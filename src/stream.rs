//! The one stream a session runs: the whole desktop, from the first pixel to the
//! last, and the mirror behind it.
//!
//! The mirror is double-buffered so the engine's read loop never waits an encode
//! out: a round takes the up-to-date mirror and the encoder to a blocking worker,
//! and the spare takes the blits meanwhile. [`DesktopStream::put_back`] brings them
//! home, and the rectangles staged while the round was away are what the next take
//! syncs across and what re-dirties the stream.

use crate::config::Chroma;
use crate::protocol::{Held, VideoUnit};
use crate::shadow::Rect;
use crate::video::Mirror;
use crate::vp9::Stream;

/// Most rectangles the staged-damage list holds before collapsing to a bounding
/// box — see [`DesktopStream::stage`].
const STAGED_CAP: usize = 32;

/// The live encoder and what it owes.
struct Live {
    stream: Stream,
    /// The dial this encoder is *known* to be running at — recorded only on a
    /// successful `set_quality`, so it can trail [`DesktopStream::quality`] after a
    /// refusal. What [`DesktopStream::put_back`] compares against, so an unchanged
    /// dial costs a returned stream nothing and a failed retune is retried instead of
    /// believed.
    quality: u8,
    /// Whether anything has been blitted since the last access unit.
    dirty: bool,
    /// Whether the next access unit must be one a decoder can start from.
    keyframe_owed: bool,
    /// The configuration string already announced to the client, if any.
    ///
    /// Cleared by [`DesktopStream::force_keyframe`], which is what makes a reattach
    /// re-announce: the browser that just arrived never saw the original text frame,
    /// and its decoder cannot be configured from the units alone.
    announced: Option<String>,
}

/// The mirror, the encoder, and the double buffer between them.
pub struct DesktopStream {
    /// The 1–100 dial the encoder runs at — the config's, unless the congestion loop
    /// has moved it. A stream rebuilt after a resize starts here, so one built while
    /// the link is behind starts where the link left off.
    quality: u8,
    /// The chroma sampling the encoder is built with — the target's, for its whole
    /// session; nothing moves it.
    chroma: Chroma,
    /// The desktop, learned from [`crate::protocol::ServerMsg::Resize`]. `None` until
    /// the engine has announced one, which it always does before any damage.
    size: Option<(u16, u16)>,
    mirror: Option<Mirror>,
    /// The mirror's double buffer. While a round is away being encoded it holds that
    /// round's mirror's *twin*: [`Self::take_round`] hands the up-to-date mirror to
    /// the encode and installs this one as current, so blits keep landing while the
    /// encode runs instead of waiting out the whole of it under the lock.
    spare: Option<Mirror>,
    /// Rectangles blitted into `mirror` since `spare` last matched it — what
    /// [`Self::take_round`] copies across before the swap, and what
    /// [`Self::put_back`] replays as a dirty mark for damage that arrived while the
    /// encoder was away.
    staged: Vec<Rect>,
    /// Whether a round is away on a blocking worker. At most one ever is: taking a
    /// second would hand two encoders one chain of frames.
    round_out: bool,
    /// Bumped whenever the desktop changes size. A returning round stamped with an
    /// older epoch is state for a desktop that no longer exists, and is discarded
    /// rather than restored.
    epoch: u64,
    live: Option<Live>,
    /// Pixels have been blitted that no encoder has taken on: the first of a
    /// desktop, or ones that landed while there was no live encoder — a resize
    /// while a round was out drops it. [`Self::take_round`] builds the encoder
    /// they are owed.
    owed: bool,
}

impl DesktopStream {
    pub fn new(quality: u8, chroma: Chroma) -> Self {
        Self {
            quality,
            chroma,
            size: None,
            mirror: None,
            spare: None,
            staged: Vec::new(),
            round_out: false,
            epoch: 0,
            live: None,
            owed: false,
        }
    }

    /// Adopt the desktop the client is about to be told about.
    ///
    /// Cannot fail, and is all that happens on the message path. A different size
    /// drops the mirror and the encoder: an encoder cannot change picture size
    /// without starting over anyway, and the next blit builds both afresh.
    pub fn want(&mut self, w: u16, h: u16) {
        if self.size != Some((w, h)) {
            self.size = Some((w, h));
            self.mirror = None;
            self.spare = None;
            self.staged.clear();
            self.live = None;
            self.owed = false;
            // A round away on a worker carries an encoder and a mirror this is
            // dropping; the epoch is what tells `put_back` not to bring them back.
            self.epoch += 1;
        }
    }

    /// Whether the stream holds pixels no access unit has carried yet.
    ///
    /// The question `VideoSink::due_at` answers for the engines, and the reason a
    /// deferred frame is safe: these pixels are already counted as delivered by the
    /// shadow, so something has to come back for them.
    pub fn dirty(&self) -> bool {
        match &self.live {
            Some(live) => live.dirty,
            None => self.owed && !self.round_out,
        }
    }

    /// The mirror, built if it is not there yet.
    ///
    /// Construction is deferred to the pixel path — rather than done where the size
    /// arrives — because it can fail, and the size arrives inside `VideoSink::msg`,
    /// whose error every caller reads as "the browser has gone" and answers by
    /// returning without a word. Every caller of this is on the engines' `?` path,
    /// which ends the session with the message attached.
    fn mirror_mut(&mut self) -> anyhow::Result<&mut Mirror> {
        if self.mirror.is_none() {
            let (w, h) = self
                .size
                .ok_or_else(|| anyhow::anyhow!("the video mirror was asked for pixels before a desktop size"))?;
            let mirror = Mirror::new(w, h)?;
            // Refused here rather than when the encoder is built, so a desktop the
            // stream will not take fails the blit that first asked for it.
            crate::video::check_picture(mirror.coded())?;
            self.mirror = Some(mirror);
        }
        Ok(self.mirror.as_mut().expect("just built"))
    }

    /// Copy a changed rectangle's source pixels into the mirror.
    ///
    /// While a round is out the live slot is empty because the encoder is away,
    /// not because there is none; [`Self::put_back`] replays the staged rects as
    /// its dirty mark. `owed` covers the case where it does not come back.
    pub fn blit(&mut self, rect: Rect, rgb: &[u8]) -> anyhow::Result<()> {
        self.mirror_mut()?.blit(rect, rgb)?;
        self.stage(rect);
        match &mut self.live {
            Some(live) => live.dirty = true,
            None => self.owed = true,
        }
        Ok(())
    }

    /// Note that `rect` now differs between the current mirror and the spare.
    ///
    /// A rectangle that continues the last one is merged into it rather than pushed.
    /// Exactly, not approximately: same left and right, and starting on the row after
    /// it ends, so the pair covers what the two covered and nothing else.
    ///
    /// Capped: past [`STAGED_CAP`] the list collapses to one bounding box, so the
    /// sync copies some slop that did not change — bounded by what the shadow
    /// already paid to compare.
    fn stage(&mut self, rect: Rect) {
        if let Some(last) = self.staged.last_mut()
            && last.left == rect.left
            && last.right == rect.right
            && last.bottom.checked_add(1) == Some(rect.top)
        {
            last.bottom = rect.bottom;
            return;
        }
        if self.staged.len() >= STAGED_CAP {
            let mut whole = rect;
            for r in &self.staged {
                whole.left = whole.left.min(r.left);
                whole.top = whole.top.min(r.top);
                whole.right = whole.right.max(r.right);
                whole.bottom = whole.bottom.max(r.bottom);
            }
            self.staged.clear();
            self.staged.push(whole);
        } else {
            self.staged.push(rect);
        }
    }

    /// Whether a round is away being encoded. While one is, the encoder and the hot
    /// mirror are on the worker, and a second round cannot be taken.
    pub fn round_out(&self) -> bool {
        self.round_out
    }

    /// Take the mirror and the encoder, for an encode on a blocking worker.
    ///
    /// `None` when the stream has nothing waiting — so a still screen costs neither
    /// the hand-off nor the encode — and while a previous round is still away, which
    /// is what keeps the inter-frame chain serial now that the caller no longer
    /// waits the encode out.
    ///
    /// The spare mirror is brought up to date — rect by rect, bounded by the damage
    /// since the last round rather than by the desktop — and installed as current,
    /// so blits land somewhere real while the encode runs.
    ///
    /// Fails only when the encoder the owed pixels need cannot be built.
    pub fn take_round(&mut self) -> anyhow::Result<Option<Round>> {
        if self.round_out || !self.dirty() {
            return Ok(None);
        }
        let live = match self.live.take() {
            Some(live) => live,
            None => {
                let coded = self.mirror.as_ref().expect("owed pixels mean a mirror").coded();
                self.owed = false;
                Live {
                    stream: Stream::new(coded, self.quality, self.chroma)?,
                    quality: self.quality,
                    // Its whole picture is owed: nothing has carried these pixels yet.
                    dirty: true,
                    keyframe_owed: true,
                    announced: None,
                }
            }
        };
        let current = self.mirror.take().expect("a live stream means a mirror");
        let spare = match self.spare.take() {
            Some(mut spare) => {
                for rect in &self.staged {
                    spare.adopt(&current, *rect);
                }
                spare
            }
            // The first round of a session (or the first after a resize) clones
            // whole: there is no spare yet to sync.
            None => current.clone(),
        };
        self.staged.clear();
        self.mirror = Some(spare);
        self.round_out = true;
        Ok(Some(Round { mirror: current, live, skipped: 0, epoch: self.epoch }))
    }

    /// Put back what [`Self::take_round`] took.
    pub fn put_back(&mut self, round: Round) {
        self.round_out = false;
        if round.epoch != self.epoch {
            // The desktop was resized while this round was encoding: its mirror and
            // encoder describe a framebuffer that no longer exists. Its access unit
            // was still delivered — the ordered queue puts it ahead of the resize the
            // client hears about — but nothing here is worth keeping. Whatever the new
            // desktop has blitted since is `owed`, and the next take starts its stream.
            return;
        }
        // The returned mirror is stale by exactly the rects blitted since the swap,
        // which is what `staged` has been accumulating; it becomes the spare and the
        // next take's sync settles the difference.
        self.spare = Some(round.mirror);
        let mut live = round.live;
        // Damage that arrived while the round was out marked nothing dirty — the
        // live slot was empty — so it is replayed from the staged rects.
        if !self.staged.is_empty() {
            live.dirty = true;
        }
        self.owed = false;
        // And the congestion loop may have moved the dial while the encoder was out
        // of reach — compared against what it is *known* to run at, so an unchanged
        // dial costs nothing here. A failure to retune keeps the quality it already
        // has, the same answer `adjust` gives, and leaves `live.quality` alone so the
        // next round tries again.
        if live.quality != self.quality {
            match live.stream.set_quality(self.quality) {
                Ok(()) => live.quality = self.quality,
                Err(e) => log::warn!("video: a returned stream refused quality {}: {e:#}", self.quality),
            }
        }
        self.live = Some(live);
    }

    /// Move the encoder's quality, for the congestion loop.
    ///
    /// Recorded only once the encoder has taken it, so a failure leaves one
    /// consistent quality in force and a caller retrying sees the old one. While a
    /// round is out there is no encoder here to refuse, and [`Self::put_back`] brings
    /// the returning one to it.
    pub fn set_quality(&mut self, quality: u8) -> anyhow::Result<()> {
        if let Some(live) = &mut self.live {
            live.stream.set_quality(quality)?;
            live.quality = quality;
        }
        self.quality = quality;
        Ok(())
    }

    /// The dial the stream is encoding at, for the totals.
    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// Mark the stream dirty over pixels it has already carried, so the next round
    /// re-encodes them at the quality now in force.
    ///
    /// The settle ([`crate::encode`]): a screen that stopped changing while the link
    /// had the dial walked down would otherwise keep that coarse picture until it
    /// next changed. An inter frame over the unchanged mirror at a finer quantizer
    /// sharpens it; no keyframe is needed.
    pub fn refresh(&mut self) {
        if let Some(live) = &mut self.live {
            live.dirty = true;
        }
    }

    /// Make the live encoder refuse its next `count` retunes.
    #[cfg(test)]
    pub fn refuse_retunes(&mut self, count: u32) {
        if let Some(live) = &mut self.live {
            live.stream.refuse_retunes(count);
        }
    }

    /// Arm a keyframe. Its callers are exactly the moments a client's decoder has to
    /// be able to start over.
    pub fn force_keyframe(&mut self) {
        if let Some(live) = &mut self.live {
            live.keyframe_owed = true;
            // And the format is owed again, for the same reason the keyframe is: the
            // client this is for may be one that has never seen either. A
            // `VideoFormat` costs a short text frame, against a keyframe's hundreds of
            // kilobytes, so there is nothing to weigh.
            live.announced = None;
            // Without this a keyframe would wait for the desktop to change again,
            // which on a paused desktop is never — and a client that just attached
            // would sit in front of nothing.
            live.dirty = true;
        }
    }
}

/// One round of encoding: the mirror and the encoder, away from [`DesktopStream`]
/// for the duration of a blocking worker.
pub struct Round {
    mirror: Mirror,
    live: Live,
    skipped: u64,
    /// The [`DesktopStream::epoch`] this round was taken under, so
    /// [`DesktopStream::put_back`] can tell a round that outlived its desktop from one
    /// worth restoring.
    epoch: u64,
}

impl Round {
    /// The quality the encoder is running at — which can sit below
    /// [`DesktopStream::quality`] while a stream that refused a retune waits for
    /// [`DesktopStream::put_back`] to try again.
    pub fn quality(&self) -> u8 {
        self.live.quality
    }

    /// Encode the mirror. Blocking: call it on a worker.
    ///
    /// A stream the encoder produced no bitstream for keeps its dirty flag and its
    /// keyframe, so those pixels ride the next round — which is what stops a frame
    /// that produced nothing from becoming pixels the client never gets.
    pub fn encode(&mut self) -> anyhow::Result<Produced> {
        self.mirror.pad_edges();
        let live = &mut self.live;
        if live.keyframe_owed {
            live.stream.force_keyframe();
        }
        let mut produced = Produced { format: None, unit: None };
        let Some(unit) = live.stream.encode(&self.mirror)? else {
            self.skipped += 1;
            return Ok(produced);
        };
        live.dirty = false;
        live.keyframe_owed = false;
        // The announcement goes out ahead of the unit, which is the contract
        // `ServerMsg::VideoFormat` states.
        if let Some(decode) = live.stream.decode_string()
            && live.announced.as_deref() != Some(decode)
        {
            live.announced = Some(decode.to_owned());
            produced.format = Some(decode.to_owned());
        }
        let (w, h) = self.mirror.size();
        produced.unit = Some(VideoUnit {
            w,
            h,
            keyframe: unit.keyframe,
            data: unit.data,
            held: Held::default(),
        });
        Ok(produced)
    }

    /// Encodes that yielded no bitstream. Must stay zero: `skip_frames(false)` should
    /// make it unreachable, and a non-zero count is how we would find out that it is
    /// not.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }
}

#[cfg(test)]
impl Round {
    /// The mirror this round would encode, for tests asserting what the double
    /// buffer hands the worker.
    fn mirror(&self) -> &Mirror {
        &self.mirror
    }
}

/// What one round produced: the configuration owed ahead of the unit, and the unit.
pub struct Produced {
    /// The `ServerMsg::VideoFormat` owed because the stream is new, was re-armed, or
    /// its configuration string changed.
    pub format: Option<String>,
    pub unit: Option<VideoUnit>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(w: u16, h: u16) -> DesktopStream {
        let mut stream = DesktopStream::new(60, Chroma::Subsampled);
        stream.want(w, h);
        stream
    }

    fn flat(w: u16, h: u16, value: u8) -> Vec<u8> {
        vec![value; usize::from(w) * usize::from(h) * 3]
    }

    fn placed(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a rectangle with a size")
    }

    /// Rows reported one under another stage as the one rectangle they make, so how
    /// a report was cut on the way in does not decide how much the sync copies.
    #[test]
    fn adjoining_rows_stage_as_one_rectangle() {
        let mut stream = stream(320, 256);
        let whole = placed(0, 0, 320, 256);
        for top in (0..256).step_by(64) {
            let band = placed(0, top, 320, 64);
            stream.blit(band, &flat(band.w(), band.h(), 7)).expect("a blit");
        }
        assert_eq!(stream.staged, vec![whole], "four rows are the one rectangle");
    }

    /// Past the cap the list collapses to a bounding box rather than growing forever.
    #[test]
    fn the_staged_list_is_capped() {
        let mut stream = stream(64, 2048);
        for k in 0..=STAGED_CAP as u16 {
            let report = placed(0, k * 60 + 8, 64, 8);
            stream.blit(report, &flat(64, 8, 7)).expect("a blit");
        }
        assert_eq!(stream.staged.len(), 1, "the list outgrew its cap");
    }

    /// The double buffer, end to end: rounds stay serial, damage that lands while a
    /// round is away re-dirties the returning stream, and the pixels it carried
    /// reach the next round's mirror — including through the spare-sync at the swap.
    #[test]
    fn damage_during_a_round_survives_into_the_next() {
        let mut stream = stream(64, 64);
        let whole = placed(0, 0, 64, 64);
        stream.blit(whole, &flat(64, 64, 10)).expect("a blit");
        let first = stream.take_round().unwrap().expect("a dirty stream means a round");
        assert!(stream.take_round().unwrap().is_none(), "two rounds out would race one chain");

        // Damage lands while the round is away...
        stream.blit(whole, &flat(64, 64, 20)).expect("a blit mid-round");
        assert!(stream.take_round().unwrap().is_none(), "still away");
        stream.put_back(first);

        // ...and the returning stream is dirty with it, over the newer pixels.
        let second = stream.take_round().unwrap().expect("the staged damage re-dirtied the stream");
        let mut out = Vec::new();
        second.mirror().crop_into(whole, &mut out).expect("a crop");
        assert!(out.iter().all(|&b| b == 20), "the double buffer lost mid-round damage");
        stream.put_back(second);

        // The third round encodes from the spare that was synced at the last swap:
        // anything still 10 in it would be the sync not happening.
        let corner = placed(0, 0, 4, 4);
        stream.blit(corner, &flat(4, 4, 30)).expect("a corner blit");
        let third = stream.take_round().unwrap().expect("dirty again");
        third.mirror().crop_into(corner, &mut out).expect("a crop");
        assert!(out.iter().all(|&b| b == 30));
        let elsewhere = placed(32, 32, 4, 4);
        third.mirror().crop_into(elsewhere, &mut out).expect("a crop");
        assert!(out.iter().all(|&b| b == 20), "the spare was not synced before the swap");
    }

    /// A resize while a round is away: the returning round is state for a desktop
    /// that no longer exists, and none of it may come back.
    #[test]
    fn a_round_that_outlives_its_desktop_is_dropped_on_return() {
        let mut stream = stream(64, 64);
        stream.blit(placed(0, 0, 64, 64), &flat(64, 64, 10)).expect("a blit");
        let stale = stream.take_round().unwrap().expect("a round");
        stream.want(32, 32);
        stream.put_back(stale);
        assert!(stream.take_round().unwrap().is_none(), "a stale round was restored");

        // The new desktop starts clean and streams its own pixels.
        let small = placed(0, 0, 32, 32);
        stream.blit(small, &flat(32, 32, 40)).expect("a blit at the new size");
        let fresh = stream.take_round().unwrap().expect("a stream over the new desktop");
        let mut out = Vec::new();
        fresh.mirror().crop_into(small, &mut out).expect("a crop");
        assert!(out.iter().all(|&b| b == 40));
    }

    /// A resize and the new desktop's first pixels both landing while a round is
    /// away: the returning round is dropped, and the pixels it could not carry still
    /// start a stream of their own. Without that the new desktop is never encoded
    /// until something on it changes again.
    #[test]
    fn pixels_blitted_while_a_stale_round_is_out_still_start_a_stream() {
        let mut stream = stream(64, 64);
        stream.blit(placed(0, 0, 64, 64), &flat(64, 64, 10)).expect("a blit");
        let stale = stream.take_round().unwrap().expect("a round");
        stream.want(32, 32);
        let small = placed(0, 0, 32, 32);
        stream.blit(small, &flat(32, 32, 40)).expect("a blit at the new size, mid-round");
        assert!(!stream.dirty(), "nothing can be encoded while the round is out");
        stream.put_back(stale);
        assert!(stream.dirty(), "the new desktop's pixels are owed a stream");
        let fresh = stream.take_round().unwrap().expect("a stream over the new desktop");
        let mut out = Vec::new();
        fresh.mirror().crop_into(small, &mut out).expect("a crop");
        assert!(out.iter().all(|&b| b == 40));
    }
}
