//! H.264 encoding: the framebuffer copy every stream reads, and one stream over a
//! rectangle of it.
//!
//! Two render dials arrive here and they differ only in how many rectangles they
//! ask for. `render_type = "video"` asks for one covering the whole desktop.
//! `render_motion_subtype = "h264"` asks for one per coalesced moving region, with
//! the still codecs carrying everything else. Which rectangles, and when they start
//! and stop, is [`crate::regions`]' business; this module knows only how to encode
//! one.
//!
//! Three things about the shape follow from the rest of the gateway rather than
//! from H.264:
//!
//! - **A frame may never be dropped.** [`crate::tiles::Shadow`] records source pixels
//!   as delivered the moment it accepts them, so a frame the encoder skipped is
//!   permanently wrong pixels — nothing re-sends it. Hence `skip_frames(false)`, and
//!   hence an empty bitstream leaves the caller's dirty flag alone instead of
//!   clearing it.
//! - **This is fed rectangles, not frames.** Damage arrives as rectangles, and VNC's
//!   can only be cropped out of the one it just decoded, so the [`Mirror`] holds the
//!   whole framebuffer and every stream encodes a crop of it when the engine says a
//!   frame has ended. One mirror rather than one per stream is also what lets a
//!   stream start in the middle of a session: its region's pixels are already there,
//!   whether or not they have changed recently.
//! - **The mirror is the newest truth, so a cleanup needs no stash.** Everything
//!   blitted is exact source, moving or not, so a region that settles is re-sent
//!   crisp by cropping it out of here — never from a remembered frame that has since
//!   been overtaken. See [`crate::regions`].
//!
//! A detach drops frames outright (`SessionManager::pump`), so a client coming back
//! has missed part of every stream and cannot decode from where it left off. It
//! recovers because every attach injects a repaint, which is one of the moments
//! [`Stream::force_keyframe`] is called.
//!
//! Everything openh264 lives here, and that containment is not tidiness. This crate
//! asserts on odd dimensions, on a mis-sized slice and on an out-of-range quantizer,
//! and the release profile is `panic = "abort"` — so an assertion reaching openh264
//! takes the whole gateway down rather than one session. Each is made unreachable by
//! construction below, and each has a test.

use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, QpRange,
    RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::formats::{RgbSliceU8, YUVBuffer};
use openh264::{OpenH264API, Timestamp};

use crate::tiles::Rect;

/// The longest side openh264 will encode, and the longest the *other* side may then
/// be.
///
/// Checked either way up, because the limit is on the picture and not on width:
/// 3840×2160 and 2160×3840 are both legal, and 3840×3840 is not. A `w <= 3840 &&
/// h <= 2160` test would read almost the same and would refuse a portrait desktop
/// the encoder is perfectly happy with.
const MAX_LONG_SIDE: u16 = 3840;
/// See [`MAX_LONG_SIDE`].
const MAX_SHORT_SIDE: u16 = 2160;

/// The quantizer the dial spans. 51 is H.264's coarsest.
///
/// The fine end is 12 rather than 0 because openh264 clamps `iMinQp` up to its
/// `GOM_MIN_QP_MODE`, which is 12: a dial that mapped past it would have a top third
/// where turning the knob did nothing at all.
const QP_FINEST: u8 = 12;
/// See [`QP_FINEST`]. Public because the congestion loop in [`crate::encode`] walks
/// the quantizer up towards it, and there should be one statement of where it stops.
pub const QP_COARSEST: u8 = 51;

/// The frame rate the encoder is told to expect.
///
/// Nothing paces frames to it — the remote decides when a frame happens, and this
/// encoder is handed whatever arrives. It is set because openh264's rate control
/// reads it, and a plausible number is better than the crate's default of zero.
const NOMINAL_FPS: f32 = 30.0;

/// The bitrate the encoder is told to aim at.
///
/// At a pinned quantizer this decides nothing: the quantizer *is* the dial, and the
/// bitrate lands wherever the picture puts it. It is set because openh264 verifies it
/// against the level, and because the crate's default of 120 kbps is not a number
/// about a desktop.
const NOMINAL_BITRATE: u32 = 8_000_000;

/// The 1–100 quality dial as a constant quantizer.
///
/// The two scales run opposite ways: 1 is the coarsest picture and becomes
/// [`QP_COARSEST`], 100 the finest and becomes [`QP_FINEST`]. Out-of-range input is
/// clamped rather than refused — `config` is what rejects a bad dial, and this is on
/// the path where a panic would take the process with it.
pub fn qp_for(quality: u8) -> u8 {
    let quality = u32::from(quality.clamp(1, 100));
    let span = u32::from(QP_COARSEST - QP_FINEST);
    QP_COARSEST - ((quality - 1) * span / 99) as u8
}

/// One encoded frame: a complete Annex-B access unit, and whether a decoder that has
/// seen nothing before it can start here.
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// A `render_motion_debug` outline drawn round the picture a stream encodes.
///
/// Painted on the crop, never on the [`Mirror`]: the mirror is what
/// [`crate::tiles::Shadow`] has already recorded as delivered and what a cleanup
/// crops, so a mark left there would be restored as though it were content and
/// nothing would ever take it off again. On the crop it lives exactly as long as the
/// stream does, and the cleanup that follows erases it.
#[derive(Clone, Copy)]
pub struct Mark {
    pub colour: [u8; 3],
    /// Border thickness in pixels. Owned by the caller because the still path draws
    /// the same outline round its own pieces, and one thickness should mean one
    /// thickness.
    pub px: u16,
}

/// The whole framebuffer as packed RGB888, and the one copy every stream reads.
///
/// Held at the desktop size rounded up to even sides. Only the encoder ever sees
/// those extra pixels — a client is told the true size and crops — and they are
/// filled from their neighbours rather than left black, because a hard black edge
/// beside content is a strong feature the encoder would pay for in every frame.
pub struct Mirror {
    /// The desktop as the client knows it, and as a record header reports it.
    size: (u16, u16),
    /// The picture actually held: [`Self::size`] rounded up to even sides.
    coded: (u16, u16),
    rgb: Vec<u8>,
}

impl Mirror {
    /// A mirror for a `w`×`h` desktop. ~6 MB at 1080p.
    pub fn new(w: u16, h: u16) -> anyhow::Result<Self> {
        anyhow::ensure!(w > 0 && h > 0, "an h264 mirror cannot hold a {w}x{h} desktop");
        // Saturating rather than wrapping: a 65535-wide desktop is not real, but
        // wrapping to 0 here would hand openh264 a zero-sized picture, and the point
        // of this constructor is that nothing invalid gets that far.
        let coded = (w.saturating_add(w % 2), h.saturating_add(h % 2));
        let (cw, ch) = (usize::from(coded.0), usize::from(coded.1));
        Ok(Self { size: (w, h), coded, rgb: vec![0; cw * ch * 3] })
    }

    /// The desktop this mirror is for.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// The picture actually held: up to one pixel wider and one taller than
    /// [`Self::size`].
    pub fn coded(&self) -> (u16, u16) {
        self.coded
    }

    /// The whole desktop as a rectangle — the region a `video` target streams.
    pub fn rect(&self) -> Rect {
        Rect::from_size(0, 0, self.size.0, self.size.1).expect("a desktop with pixels")
    }

    /// Copy `rgb` — packed RGB888 for `rect` — into the mirror.
    ///
    /// A rectangle outside the desktop is refused rather than clipped: it means this
    /// mirror and the engine disagree about how big the framebuffer is, and taking
    /// part of the update would put that disagreement on the screen instead of in
    /// the log.
    pub fn blit(&mut self, rect: Rect, rgb: &[u8]) -> anyhow::Result<()> {
        let (w, h) = (usize::from(rect.w()), usize::from(rect.h()));
        anyhow::ensure!(
            rgb.len() == w * h * 3,
            "an h264 blit is {} bytes, expected {} for {w}x{h} RGB",
            rgb.len(),
            w * h * 3
        );
        anyhow::ensure!(
            rect.right < self.size.0 && rect.bottom < self.size.1,
            "an h264 blit of {w}x{h} at ({},{}) falls outside a {}x{} desktop",
            rect.left,
            rect.top,
            self.size.0,
            self.size.1
        );

        let stride = usize::from(self.coded.0) * 3;
        for row in 0..h {
            let at = (usize::from(rect.top) + row) * stride + usize::from(rect.left) * 3;
            self.rgb[at..at + w * 3].copy_from_slice(&rgb[row * w * 3..(row + 1) * w * 3]);
        }
        Ok(())
    }

    /// `rect`'s pixels as packed RGB888, into a buffer the caller reuses.
    ///
    /// `rect` may reach into the padding — that is exactly what a stream at the
    /// desktop's odd edge does — so this is bounded by [`Self::coded`] rather than
    /// by [`Self::size`]. Out of those bounds it refuses: the alternative is an
    /// index panic, and this binary aborts on one.
    pub fn crop_into(&self, rect: Rect, out: &mut Vec<u8>) -> anyhow::Result<()> {
        anyhow::ensure!(
            rect.right < self.coded.0 && rect.bottom < self.coded.1,
            "an h264 crop of {}x{} at ({},{}) falls outside a {}x{} mirror",
            rect.w(),
            rect.h(),
            rect.left,
            rect.top,
            self.coded.0,
            self.coded.1
        );
        let stride = usize::from(self.coded.0) * 3;
        let (w, h) = (usize::from(rect.w()), usize::from(rect.h()));
        out.clear();
        out.reserve(w * h * 3);
        for row in 0..h {
            let at = (usize::from(rect.top) + row) * stride + usize::from(rect.left) * 3;
            out.extend_from_slice(&self.rgb[at..at + w * 3]);
        }
        Ok(())
    }

    /// Fill the at-most-one padding column and row from their neighbours.
    ///
    /// Only an odd-sized desktop has any. Called once per encode round rather than
    /// per stream: at most one stream can reach the pad on each axis, and finding
    /// out which is more work than repeating a column copy.
    pub fn pad_edges(&mut self) {
        let stride = usize::from(self.coded.0) * 3;
        if self.coded.0 != self.size.0 {
            let last = usize::from(self.size.0 - 1) * 3;
            for row in 0..usize::from(self.size.1) {
                let at = row * stride + last;
                self.rgb.copy_within(at..at + 3, at + 3);
            }
        }
        if self.coded.1 != self.size.1 {
            let last = usize::from(self.size.1 - 1) * stride;
            self.rgb.copy_within(last..last + stride, last + stride);
        }
    }
}

/// One H.264 stream over a fixed rectangle of a [`Mirror`].
///
/// The rectangle is fixed for the stream's whole life, and that is what makes an
/// inter-frame stream mean anything: every frame is expressed as a change from the
/// last one at the same place. A region that moves or grows gets a *new* stream,
/// which is [`crate::regions`]' decision to make.
pub struct Stream {
    encoder: Encoder,
    /// The region as the client knows it, and as a record header reports it.
    rect: Rect,
    /// The picture actually encoded: [`Self::rect`] grown to even sides.
    coded: Rect,
    /// Reused across frames so a 1080p conversion is not a 3 MB allocation apiece.
    yuv: YUVBuffer,
    /// [`Self::coded`] cropped out of the mirror, reused for the same reason.
    scratch: Vec<u8>,
    /// The quantizer in force, which [`Self::set_qp`] moves and the totals report.
    qp: u8,
    /// Where this stream's timestamps are measured from. Real elapsed time rather
    /// than a frame counter, because openh264's screen-content rate control reads it
    /// and a counter would tell it every frame arrived on schedule.
    started: std::time::Instant,
}

impl Stream {
    /// A stream over `rect` of a mirror whose coded size is `mirror`, at `quality`
    /// (1–100).
    ///
    /// **The evenness of the coded rectangle is a theorem, not a hope, and this is
    /// where it is checked.** H.264 needs even sides. A region is a union of whole
    /// grid cells clipped to the desktop, and `CELL_W`/`CELL_H` are both even, so a
    /// region's origin is always even and its size is odd only where its right or
    /// bottom edge is the desktop's own and the desktop is odd there — in which case
    /// the mirror's own padding is exactly the column or row needed. So the coded
    /// rectangle is the region grown right and down, it never leaves the mirror, and
    /// it never contains a pixel that was invented. A whole-desktop `video` stream is
    /// the same statement with the region set to the desktop.
    ///
    /// Fails for a picture openh264 will not encode. That cannot be a config-time
    /// refusal — only the remote knows its own size, and it may change mid-session —
    /// so the message has to carry the whole explanation to wherever it surfaces.
    pub fn new(rect: Rect, mirror: (u16, u16), quality: u8) -> anyhow::Result<Self> {
        // Checked rather than plain arithmetic, though a rectangle ending at the last
        // representable pixel needs a desktop wider than `u16` can describe: this
        // binary aborts on an arithmetic overflow, and the point of this constructor
        // is that nothing invalid gets as far as an abort.
        let (right, bottom) = rect
            .right
            .checked_add(rect.w() % 2)
            .zip(rect.bottom.checked_add(rect.h() % 2))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "an h264 stream over {}x{} at ({},{}) cannot be grown to even sides \
                     without leaving the framebuffer's coordinate space",
                    rect.w(),
                    rect.h(),
                    rect.left,
                    rect.top
                )
            })?;
        let coded = Rect { left: rect.left, top: rect.top, right, bottom };
        anyhow::ensure!(
            coded.right < mirror.0 && coded.bottom < mirror.1,
            "an h264 stream over {}x{} at ({},{}) needs even sides, and growing it to \
             {}x{} leaves a {}x{} mirror — a region is a union of grid cells, so this \
             is a region that was not",
            rect.w(),
            rect.h(),
            rect.left,
            rect.top,
            coded.w(),
            coded.h(),
            mirror.0,
            mirror.1
        );
        let (long, short) = (coded.w().max(coded.h()), coded.w().min(coded.h()));
        anyhow::ensure!(
            long <= MAX_LONG_SIDE && short <= MAX_SHORT_SIDE,
            "h264 cannot encode a {}x{} picture: openh264 refuses one with a long side \
             over {MAX_LONG_SIDE} or a short side over {MAX_SHORT_SIDE}. Only the \
             remote knows its own size, so check-config cannot catch this — give this \
             target a still render_type (\"fixed-quality\" with render_subtype = \
             \"webp\"), or ask the remote for a smaller desktop",
            rect.w(),
            rect.h()
        );

        let qp = qp_for(quality).clamp(QP_FINEST, QP_COARSEST);
        let config = EncoderConfig::new()
            // What this encoder is actually looking at. Screen content is mostly flat
            // colour, hard edges and text, none of which a camera preset expects.
            .usage_type(UsageType::ScreenContentRealTime)
            // Quality mode with the quantizer pinned top and bottom is the constant-QP
            // encode: openh264 clamps every per-picture decision into this range, so a
            // range of one value is one quantizer. It is *not* `RateControlMode::Off`,
            // which reads its quantizer from a spatial-layer field the safe wrapper
            // cannot set — the dial would silently do nothing.
            //
            // The pin is ±2 rather than absolute: openh264 adjusts by up to two on a
            // scene change. That is fine for a dial and not worth fighting.
            .rate_control_mode(RateControlMode::Quality)
            .qp(QpRange::new(qp, qp))
            // The shadow has already recorded these pixels as delivered, so a frame
            // the encoder decided to skip would be permanently wrong pixels.
            .skip_frames(false)
            // SPS and PPS repeat with every keyframe, so a client can build its
            // decoder from any keyframe and needs nothing out of band.
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            // One slice on one thread: this already runs on a blocking worker, and one
            // thread makes an access unit a deterministic thing to test. Several
            // regions encode in parallel with each other, which is where the cores go.
            .num_threads(1)
            .complexity(Complexity::Low)
            // Both are refused for screen content anyway — openh264 turns them off and
            // says so on stderr — so they are off here rather than left at the crate's
            // camera defaults for it to complain about once per encoder. Adaptive
            // quantization would also have moved the quantizer off the dial.
            //
            // One warning it does still print at every init is expected and correct:
            // that a bitrate cannot be enforced without frame skipping. It cannot, and
            // it is not meant to be — the quantizer is the dial and skipping a frame
            // is the one thing this encoder may never do.
            .adaptive_quantization(false)
            .background_detection(false)
            // No fixed keyframe interval. Nothing here is ever lost in transit — the
            // link is TCP — so a periodic keyframe would buy resilience against
            // nothing and cost the bytes every time. The keyframes that do happen are
            // asked for: a repaint, a resize, a client coming back, a region that grew.
            .intra_frame_period(IntraFramePeriod::auto())
            .max_frame_rate(FrameRate::from_hz(NOMINAL_FPS))
            .bitrate(BitRate::from_bps(NOMINAL_BITRATE));
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config).map_err(|e| {
            anyhow::anyhow!("h264 encoder for a {}x{} picture: {e}", coded.w(), coded.h())
        })?;

        Ok(Self {
            encoder,
            rect,
            coded,
            // Even by construction, which is what keeps this from asserting.
            yuv: YUVBuffer::new(usize::from(coded.w()), usize::from(coded.h())),
            scratch: Vec::new(),
            qp,
            started: std::time::Instant::now(),
        })
    }

    /// The region this stream is for — what a record header reports, and what a
    /// client crops the decoded picture to.
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// The quantizer this stream is currently encoding at.
    pub fn qp(&self) -> u8 {
        self.qp
    }

    /// Make the next access unit one a decoder can start from.
    pub fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    /// Move the quantizer on the live encoder, without a keyframe.
    ///
    /// This is how a congested link gives up quality (see `Congestion` in
    /// [`crate::encode`]), and "without a keyframe" is the whole reason it is written
    /// this way. Rebuilding the encoder with a new configuration would be safe Rust
    /// and would force an IDR on the next frame — a few hundred KB spent at the exact
    /// moment the link has run out of room, making worse the thing it is reacting to.
    ///
    /// Reading the parameters back out and handing them straight in again, rather than
    /// building a fresh `SEncParamExt`, is what keeps this from having to know every
    /// field [`Stream::new`] set: whatever the encoder is running on now is what it
    /// goes on running on, with two integers changed.
    pub fn set_qp(&mut self, qp: u8) -> anyhow::Result<()> {
        let qp = qp.clamp(QP_FINEST, QP_COARSEST);
        if qp == self.qp {
            return Ok(());
        }
        let mut params = openh264_sys2::SEncParamExt::default();
        let option = openh264_sys2::ENCODER_OPTION_SVC_ENCODE_PARAM_EXT;
        // SAFETY: `params` is the type this option is documented to carry, it is
        // initialized before the get and fully populated by it, and it outlives both
        // calls. `raw_api` needs `&mut self`, which is what serializes this against
        // the encode — the same borrow that makes `encode` exclusive.
        //
        // This is the pair of calls the crate itself makes to re-tune a running
        // encoder (`Encoder::reinit`'s second-and-later branch).
        unsafe {
            let api = self.encoder.raw_api();
            let got = api.get_option(option, std::ptr::from_mut(&mut params).cast());
            anyhow::ensure!(got == openh264_sys2::cmResultSuccess, "h264 get params: {got}");
            params.iMinQp = i32::from(qp);
            params.iMaxQp = i32::from(qp);
            let set = api.set_option(option, std::ptr::from_mut(&mut params).cast());
            anyhow::ensure!(set == openh264_sys2::cmResultSuccess, "h264 set qp {qp}: {set}");
        }
        self.qp = qp;
        Ok(())
    }

    /// Encode this stream's rectangle of `mirror` as it stands.
    ///
    /// `None` means the encoder produced no bitstream. The caller must then leave its
    /// dirty flag set, so those pixels ride on the next frame — which is what keeps a
    /// frame that produced nothing from becoming pixels the client never gets.
    ///
    /// The mirror must have been padded ([`Mirror::pad_edges`]) if any stream reaches
    /// into the padding, which is the caller's job because it is once per round
    /// rather than once per stream.
    pub fn encode(
        &mut self,
        mirror: &Mirror,
        mark: Option<Mark>,
    ) -> anyhow::Result<Option<AccessUnit>> {
        mirror.crop_into(self.coded, &mut self.scratch)?;
        let coded = (usize::from(self.coded.w()), usize::from(self.coded.h()));
        if let Some(mark) = mark {
            outline(&mut self.scratch, coded, mark);
        }
        // Both even, and the slice is exactly `w * h * 3` — the three things
        // `RgbSliceU8::new` and `YUVBuffer` assert on, and an assertion here aborts
        // the process.
        anyhow::ensure!(
            self.scratch.len() == coded.0 * coded.1 * 3,
            "an h264 crop came back {} bytes for a {}x{} picture",
            self.scratch.len(),
            coded.0,
            coded.1
        );
        self.yuv.read_rgb8(RgbSliceU8::new(&self.scratch, coded));

        let at = Timestamp::from_millis(self.started.elapsed().as_millis() as u64);
        let bitstream = self
            .encoder
            .encode_at(&self.yuv, at)
            .map_err(|e| anyhow::anyhow!("h264 encode failed: {e}"))?;
        let (keyframe, data) = (bitstream.frame_type() == FrameType::IDR, bitstream.to_vec());
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(AccessUnit { data, keyframe }))
    }
}

/// Paint `mark`'s border round a `w`×`h` packed-RGB888 picture, in place.
fn outline(rgb: &mut [u8], (w, h): (usize, usize), mark: Mark) {
    let t = usize::from(mark.px);
    let mut paint = |row: usize, from: usize, to: usize| {
        for x in from..to {
            rgb[(row * w + x) * 3..][..3].copy_from_slice(&mark.colour);
        }
    };
    for y in 0..h {
        if y < t || y + t >= h {
            paint(y, 0, w);
        } else {
            paint(y, 0, t.min(w));
            paint(y, w.saturating_sub(t), w);
        }
    }
}

#[cfg(test)]
impl Mirror {
    /// The pixel at `(x, y)`, for the tests about blitting and padding.
    fn pixel(&self, x: u16, y: u16) -> [u8; 3] {
        let at = (usize::from(y) * usize::from(self.coded.0) + usize::from(x)) * 3;
        [self.rgb[at], self.rgb[at + 1], self.rgb[at + 2]]
    }

    fn len(&self) -> usize {
        self.rgb.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rectangle from a position and a size, which is what most of these want.
    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a rectangle with a size")
    }

    /// `w`×`h` of one colour, which makes "did these pixels land here" readable a
    /// byte at a time.
    fn flat(w: u16, h: u16, colour: [u8; 3]) -> Vec<u8> {
        colour
            .iter()
            .copied()
            .cycle()
            .take(usize::from(w) * usize::from(h) * 3)
            .collect()
    }

    /// A mirror and one stream over the whole of it, which is the `video` shape.
    fn whole(w: u16, h: u16, quality: u8) -> (Mirror, Stream) {
        let mirror = Mirror::new(w, h).expect("a mirror");
        let stream = Stream::new(mirror.rect(), mirror.coded(), quality).expect("a stream");
        (mirror, stream)
    }

    /// Blit `frames` differing full-screen pictures and return what they encoded to.
    fn stream_bytes(quality: u8, frames: u8) -> usize {
        let (mut mirror, mut stream) = whole(320, 240, quality);
        let mut total = 0;
        for frame in 0..frames {
            // A moving block, so there is something for the quantizer to be coarse
            // about — a stream of identical frames costs nearly nothing at any
            // quality and would compare two zeroes.
            let mut picture = flat(320, 240, [20, 90, 160]);
            let stride = 320 * 3;
            for row in 0..60 {
                let at = (usize::from(frame) * 2 + row) * stride + usize::from(frame) * 6;
                picture[at..at + 180].fill(240);
            }
            mirror.blit(rect(0, 0, 320, 240), &picture).expect("a full-screen blit");
            total += stream
                .encode(&mirror, None)
                .expect("an encode")
                .expect("an access unit")
                .data
                .len();
        }
        total
    }

    #[test]
    fn the_quality_dial_maps_onto_the_encoders_whole_range() {
        assert_eq!(qp_for(1), QP_COARSEST);
        assert_eq!(qp_for(100), QP_FINEST);
        // Every value in range, and every value out of it, has to be something
        // `QpRange::new` will accept — it asserts, and this binary aborts on a panic.
        for quality in 0..=u8::MAX {
            let qp = qp_for(quality);
            assert!(
                (QP_FINEST..=QP_COARSEST).contains(&qp),
                "quality {quality} gave qp {qp}, outside the range QpRange accepts"
            );
        }
        // Monotone: a higher dial is never a coarser picture.
        for quality in 1..100u8 {
            assert!(qp_for(quality) >= qp_for(quality + 1));
        }
    }

    #[test]
    fn a_lower_quality_makes_a_smaller_stream() {
        let coarse = stream_bytes(5, 4);
        let fine = stream_bytes(90, 4);
        assert!(
            coarse < fine,
            "quality 5 encoded {coarse} bytes and quality 90 encoded {fine}; the dial \
             is not reaching the encoder"
        );
    }

    #[test]
    fn the_first_frame_is_a_keyframe_and_another_can_be_asked_for() {
        let (mut mirror, mut stream) = whole(320, 240, 60);
        mirror
            .blit(rect(0, 0, 320, 240), &flat(320, 240, [10, 40, 70]))
            .expect("a full-screen blit");
        // A small patch, deliberately: repainting the whole screen a different colour
        // is a scene change, and openh264 answers one with a keyframe of its own —
        // correctly, since intra is cheaper there than predicting from a picture that
        // no longer resembles this one. What this test is about is the *ordinary*
        // frame, where a keyframe would be bytes for nothing.
        let patch = |mirror: &mut Mirror, stream: &mut Stream, shade: u8| {
            mirror
                .blit(rect(64, 64, 32, 32), &flat(32, 32, [shade, shade, shade]))
                .expect("a patch inside the desktop");
            stream.encode(mirror, None).expect("an encode").expect("an access unit").keyframe
        };
        assert!(
            stream.encode(&mirror, None).expect("an encode").expect("an access unit").keyframe,
            "a decoder has to be able to start somewhere"
        );
        assert!(
            !patch(&mut mirror, &mut stream, 200),
            "an unasked-for keyframe is bytes for nothing"
        );
        assert!(!patch(&mut mirror, &mut stream, 210));
        stream.force_keyframe();
        assert!(
            patch(&mut mirror, &mut stream, 220),
            "force_keyframe did not reach the encoder"
        );
    }

    #[test]
    fn an_access_unit_is_annex_b() {
        let (mut mirror, mut stream) = whole(320, 240, 60);
        mirror
            .blit(rect(0, 0, 320, 240), &flat(320, 240, [7, 7, 7]))
            .expect("a full-screen blit");
        let unit = stream.encode(&mirror, None).expect("an encode").expect("an access unit");
        // Both clients split on start codes, so this is the wire contract in one
        // assertion: openh264's own four-byte one, ahead of the SPS.
        assert_eq!(&unit.data[..4], &[0, 0, 0, 1], "not an Annex-B start code");
    }

    #[test]
    fn a_blit_lands_where_the_rectangle_says() {
        let mut mirror = Mirror::new(640, 480).expect("a mirror");
        mirror
            .blit(rect(33, 41, 17, 9), &flat(17, 9, [200, 100, 50]))
            .expect("a blit inside the desktop");
        assert_eq!(mirror.pixel(33, 41), [200, 100, 50]);
        assert_eq!(mirror.pixel(49, 49), [200, 100, 50]);
        // One pixel outside each edge is still the black the mirror started as.
        assert_eq!(mirror.pixel(32, 41), [0, 0, 0]);
        assert_eq!(mirror.pixel(50, 41), [0, 0, 0]);
        assert_eq!(mirror.pixel(33, 40), [0, 0, 0]);
        assert_eq!(mirror.pixel(33, 50), [0, 0, 0]);
    }

    #[test]
    fn a_blit_outside_the_desktop_is_refused_rather_than_clipped() {
        let mut mirror = Mirror::new(640, 480).expect("a mirror");
        let too_far = mirror.blit(rect(630, 470, 20, 20), &flat(20, 20, [1, 2, 3]));
        assert!(too_far.is_err(), "a rectangle off the edge of the desktop was accepted");
        let wrong_size = mirror.blit(rect(0, 0, 16, 16), &flat(16, 15, [1, 2, 3]));
        assert!(wrong_size.is_err(), "pixels that are not the rectangle's were accepted");
    }

    #[test]
    fn a_crop_reads_the_rectangle_it_names() {
        let mut mirror = Mirror::new(640, 480).expect("a mirror");
        mirror
            .blit(rect(320, 64, 320, 64), &flat(320, 64, [9, 8, 7]))
            .expect("a cell-sized blit");
        let mut out = Vec::new();
        mirror.crop_into(rect(320, 64, 320, 64), &mut out).expect("a crop of that cell");
        assert_eq!(out, flat(320, 64, [9, 8, 7]));
        // Its neighbour is untouched, which is the same statement from the other side.
        mirror.crop_into(rect(0, 64, 320, 64), &mut out).expect("a crop of the cell beside it");
        assert_eq!(out, flat(320, 64, [0, 0, 0]));
        // A rectangle the mirror does not hold is refused rather than indexed.
        assert!(mirror.crop_into(rect(600, 400, 64, 128), &mut out).is_err());
    }

    /// The test that holds the abort-the-process hazard closed: every openh264
    /// assertion on this path is about even dimensions, and this is the odd case.
    #[test]
    fn an_odd_desktop_is_padded_and_still_reports_its_true_size() {
        let (mut mirror, mut stream) = whole(1919, 1079, 60);
        assert_eq!(mirror.size(), (1919, 1079), "a client is told the real desktop");
        assert_eq!(mirror.coded(), (1920, 1080), "the encoder is given even sides");
        assert_eq!(mirror.len(), 1920 * 1080 * 3);
        assert_eq!(stream.rect(), mirror.rect(), "a record header carries the true region");
        mirror
            .blit(rect(0, 0, 1919, 1079), &flat(1919, 1079, [90, 90, 90]))
            .expect("a full-screen blit");
        mirror.pad_edges();
        assert!(stream.encode(&mirror, None).expect("an encode").is_some());
    }

    #[test]
    fn the_pad_repeats_the_edge_rather_than_leaving_it_black() {
        let mut mirror = Mirror::new(1919, 1079).expect("an odd-sized mirror");
        mirror
            .blit(rect(0, 0, 1919, 1079), &flat(1919, 1079, [255, 255, 255]))
            .expect("a full-screen blit");
        mirror.pad_edges();
        assert_eq!(mirror.pixel(1919, 0), [255, 255, 255], "the pad column is a black seam");
        assert_eq!(mirror.pixel(0, 1079), [255, 255, 255], "the pad row is a black seam");
        assert_eq!(mirror.pixel(1919, 1079), [255, 255, 255], "the pad corner is black");
    }

    /// The geometry theorem `Stream::new` rests on, from both sides: an interior
    /// region is even because the cell grid is, and a region at an odd desktop's edge
    /// finds the pad already there.
    #[test]
    fn a_region_at_the_desktops_odd_edge_grows_into_the_mirrors_pad() {
        let mirror = Mirror::new(1919, 1079).expect("an odd-sized mirror");
        // The bottom-right region of a 1919x1079 desktop: cells from (320*5, 64*16)
        // to the edge, so 319 wide and 55 tall — odd on both axes, and the only way
        // a region can be.
        let edge = Rect { left: 1600, top: 1024, right: 1918, bottom: 1078 };
        assert_eq!((edge.w() % 2, edge.h() % 2), (1, 1), "this test wants the odd case");
        assert!(
            Stream::new(edge, mirror.coded(), 60).is_ok(),
            "a region at the edge of an odd desktop had nowhere to grow"
        );
        // An interior region is even without help, whatever cells it covers.
        let inside = Rect { left: 320, top: 64, right: 959, bottom: 191 };
        assert_eq!((inside.w() % 2, inside.h() % 2), (0, 0));
        assert!(Stream::new(inside, mirror.coded(), 60).is_ok());
        // The rule from both sides, at the very edge: a single-pixel-wide rectangle
        // one column short of the mirror's width grows into the padding and is fine,
        // and the same rectangle one column further right has nowhere to grow and is
        // refused rather than reaching an openh264 assertion.
        let ragged = Rect { left: 1918, top: 0, right: 1918, bottom: 63 };
        assert!(
            Stream::new(ragged, (1920, 1080), 60).is_ok(),
            "growing right is inside a padded mirror"
        );
        let past = Rect { left: 1919, top: 0, right: 1919, bottom: 63 };
        assert!(
            Stream::new(past, (1920, 1080), 60).is_err(),
            "a region with nowhere left to grow was accepted"
        );
    }

    #[test]
    fn a_picture_too_large_is_refused_by_name() {
        // Straight to `Stream::new` with the mirror's size rather than through a
        // 44 MB allocation: a mirror will hold whatever it is asked to, and the
        // refusal being tested is the encoder's.
        let Err(refused) = Stream::new(rect(0, 0, 5120, 2880), (5120, 2880), 60) else {
            panic!("a 5K picture was accepted");
        };
        let message = format!("{refused:#}");
        assert!(message.contains("5120x2880"), "the message does not say what was asked for");
        assert!(message.contains("3840"), "the message does not say what the limit is");
        assert!(message.contains("webp"), "the message does not say what to do instead");
        // The limit is on the picture, not on width: turning a legal desktop on its
        // side does not make it illegal.
        assert!(
            Stream::new(rect(0, 0, 2160, 3840), (2160, 3840), 60).is_ok(),
            "a portrait 4K desktop was refused"
        );
        assert!(Mirror::new(0, 1080).is_err(), "a desktop with no pixels was accepted");
    }

    /// The mechanism the congestion loop rests on: quality can be given up mid-stream
    /// without spending a keyframe to do it.
    #[test]
    fn the_quantizer_moves_on_a_live_encoder_without_a_keyframe() {
        let (mut mirror, mut stream) = whole(320, 240, 90);
        assert_eq!(stream.qp(), qp_for(90));

        let moving = |mirror: &mut Mirror, stream: &mut Stream, step: u16| {
            let mut picture = flat(320, 240, [30, 60, 90]);
            let stride = 320 * 3;
            for row in 0..80 {
                let at = (usize::from(step) * 3 + row) * stride + usize::from(step) * 9;
                picture[at..at + 300].fill(230);
            }
            mirror.blit(rect(0, 0, 320, 240), &picture).expect("a full-screen blit");
            stream.encode(mirror, None).expect("an encode").expect("an access unit")
        };

        moving(&mut mirror, &mut stream, 0);
        let fine: usize = (1..5)
            .map(|step| moving(&mut mirror, &mut stream, step).data.len())
            .sum();

        stream.set_qp(QP_COARSEST).expect("the encoder to accept a new quantizer");
        assert_eq!(stream.qp(), QP_COARSEST);
        let coarse: Vec<_> = (5..9)
            .map(|step| moving(&mut mirror, &mut stream, step))
            .collect();

        assert!(
            coarse.iter().map(|unit| unit.data.len()).sum::<usize>() < fine,
            "the quantizer did not reach the running encoder"
        );
        assert!(
            !coarse.iter().any(|unit| unit.keyframe),
            "moving the quantizer cost a keyframe, which is what this avoids"
        );
    }

    /// Two streams over disjoint regions of one mirror, which is the whole point of
    /// the split: each sees its own pixels and neither sees the other's.
    #[test]
    fn two_regions_of_one_mirror_encode_independently() {
        let mut mirror = Mirror::new(640, 128).expect("a mirror");
        let left = Rect { left: 0, top: 0, right: 319, bottom: 127 };
        let right = Rect { left: 320, top: 0, right: 639, bottom: 127 };
        let mut a = Stream::new(left, mirror.coded(), 60).expect("a stream");
        let mut b = Stream::new(right, mirror.coded(), 60).expect("a stream");

        mirror.blit(left, &flat(320, 128, [200, 30, 30])).expect("a blit");
        mirror.blit(right, &flat(320, 128, [30, 30, 200])).expect("a blit");
        assert!(a.encode(&mirror, None).expect("an encode").expect("a unit").keyframe);
        assert!(b.encode(&mirror, None).expect("an encode").expect("a unit").keyframe);

        // Only the left region changes. The right one still encodes — nothing here
        // decides whether it should, that is the caller's dirty flag — but it has
        // nothing to describe, so it costs a fraction of what a changed one does.
        mirror.blit(left, &flat(320, 128, [30, 200, 30])).expect("a blit");
        let changed = a.encode(&mirror, None).expect("an encode").expect("a unit").data.len();
        let unchanged = b.encode(&mirror, None).expect("an encode").expect("a unit").data.len();
        assert!(
            unchanged < changed,
            "the right region cost {unchanged} bytes against the changed one's \
             {changed}; the two streams are not seeing different pixels"
        );
    }
}
