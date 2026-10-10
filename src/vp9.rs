//! VP9 encoding: one stream over a [`crate::video::Mirror`].
//!
//! The video codec — this gateway streams VP9 only. It is BSD-licensed with a patent grant,
//! which is exactly the property that gets it into every browser build: a Chromium built
//! without proprietary codecs still decodes it.
//!
//! The coding is `screen-vp9`'s, the crate wlshare's own stream is coded with, pinned
//! by its release tag: the one place libvpx is spoken to for either side, so a
//! passed wlshare frame and one encoded here are the same stream by construction. What
//! this module owns is the stream over the mirror: the picture limits, the keyframe
//! owed until a frame carries it, and the WebCodecs string the browser is configured
//! with. When a round is taken is [`crate::encode`]'s business.

use anyhow::Context as _;

use crate::config::Chroma;
use crate::shadow::Rect;
use crate::video::{AccessUnit, Mirror, check_picture};

pub use screen_vp9::{FrameHeader, frame_header};

/// The frame rate the level of a stream this gateway encodes is figured at.
///
/// Nothing paces frames to it — the remote decides when a frame happens — but a VP9 level is
/// defined over a *sample rate*, so a number is needed to turn a picture size into one. 30 is
/// what `VIDEO_FRAME_INTERVAL` in [`crate::encode`] paces access units to, so it is the rate a
/// stream cannot exceed rather than a guess. A passed stream is paced by its server and figured
/// at its own rate ([`crate::stream::pass`]).
pub const ENCODED_FPS: u64 = 30;

/// The WebCodecs codec string for a `w`×`h` stream at `chroma` and `fps` — what
/// `ServerMsg::VideoFormat` carries, derived here rather than in the client because VP9
/// has no in-band parameter sets for a client to read one out of. `None` for a picture no
/// VP9 level covers, which [`check_picture`] has already refused long before this is
/// reached.
pub fn codec_string(w: u16, h: u16, chroma: Chroma, fps: u64) -> Option<String> {
    screen_vp9::codec_string(w, h, chroma.into(), fps)
}

/// One VP9 stream over a [`Mirror`]'s coded picture.
///
/// The picture size is fixed for the stream's whole life, and that is what makes an inter-frame
/// stream mean anything: every frame is expressed as a change from the last one. A desktop that
/// is resized gets a *new* stream.
pub struct Stream {
    /// The encoder and the conversion in front of it, which reads the mirror whole or
    /// where it changed.
    stream: screen_vp9::Stream,
    /// The picture encoded: the mirror's coded size, the desktop grown to even sides.
    coded: (u16, u16),
    /// Whether the next frame must be one a decoder can start from.
    keyframe_owed: bool,
    /// The WebCodecs codec string for this stream's picture, computed once at construction:
    /// VP9 carries no parameter sets, so the string follows from the picture size, which is
    /// fixed for the stream's life.
    decode: Option<String>,
    /// How many retunes [`Self::set_quality`] still refuses, for a test that needs the
    /// encoder to say no.
    #[cfg(test)]
    refusals: u32,
}

impl Stream {
    /// A stream over a mirror whose coded size is `coded`, at `quality` (1–100) and `chroma`.
    ///
    /// The refusal of a picture too large is [`check_picture`]'s. VP9 does not need even
    /// sides and is held to them anyway — see the note there.
    pub fn new(coded: (u16, u16), quality: u8, chroma: Chroma) -> anyhow::Result<Self> {
        check_picture(coded)?;
        // Every core but one for the one stream, which has nothing to overlap with. See
        // `video::threads`.
        let stream = screen_vp9::Stream::new(coded.0, coded.1, chroma.into(), quality, crate::video::threads())
            .with_context(|| format!("vp9 encoder for a {}x{} picture", coded.0, coded.1))?;
        Ok(Self {
            stream,
            coded,
            keyframe_owed: false,
            decode: codec_string(coded.0, coded.1, chroma, ENCODED_FPS),
            #[cfg(test)]
            refusals: 0,
        })
    }

    /// The dial this stream is currently encoding at.
    pub fn quality(&self) -> u8 {
        self.stream.quality()
    }

    /// The coarsest dial any block of the client's picture was last encoded at: a frame
    /// encoded where the mirror changed leaves every other block at the dial it had, so
    /// this trails [`Self::quality`] until a whole frame, or enough changes, have coded
    /// them all since.
    pub fn coarsest(&self) -> u8 {
        self.stream.coarsest()
    }

    /// The WebCodecs codec string for this stream, known from construction.
    pub fn decode_string(&self) -> Option<&str> {
        self.decode.as_deref()
    }

    /// Make the next access unit one a decoder can start from.
    ///
    /// Recorded rather than done, because libvpx takes it as a flag on the next encode —
    /// there is nothing to tell the encoder in advance.
    pub fn force_keyframe(&mut self) {
        self.keyframe_owed = true;
    }

    /// Make the next `count` retunes that would change the dial fail, as a libvpx refusal
    /// would.
    #[cfg(test)]
    pub fn refuse_retunes(&mut self, count: u32) {
        self.refusals = count;
    }

    /// Move the dial on the live encoder, without a keyframe.
    ///
    /// This is how a congested link gives up quality (the walk in `screen_vp9::walk`), and
    /// "without a keyframe" is the whole reason it is a retune rather than a rebuild — which
    /// would force a keyframe on the next frame, spending a few hundred KB at the exact moment
    /// the link has run out of room.
    pub fn set_quality(&mut self, quality: u8) -> anyhow::Result<()> {
        #[cfg(test)]
        if quality.clamp(crate::video::QUALITY_MIN, crate::video::QUALITY_MAX) != self.stream.quality() && self.refusals > 0 {
            self.refusals -= 1;
            anyhow::bail!("a retune refused on the test's orders");
        }
        self.stream.set_quality(quality).context("retuning the VP9 encoder")
    }

    /// Encode `mirror` as it stands.
    ///
    /// `changed` is where the mirror differs from the one read for the last access
    /// unit, or `None` for one that may differ anywhere: only those rectangles are
    /// converted and only the blocks they touch encoded, the rest of the frame being
    /// the picture the client holds, at the quality it holds it. A keyframe and a
    /// stream's first frame are the whole mirror whatever `changed` says.
    ///
    /// `None` means the encoder produced no bitstream. The caller must then leave its dirty flag
    /// set, so those pixels ride on the next frame — which is what keeps a frame that produced
    /// nothing from becoming pixels the client never gets. With no lag and no dropped frames it
    /// should be unreachable; it is a return value rather than an assertion because the caller
    /// has to be ready for it anyway.
    ///
    /// The mirror must have been padded ([`Mirror::pad_edges`]), which is the caller's job.
    pub fn encode(&mut self, mirror: &Mirror, changed: Option<&[Rect]>) -> anyhow::Result<Option<AccessUnit>> {
        self.read(mirror, changed.filter(|_| !self.keyframe_owed))?;
        let mut data = Vec::new();
        let keyframe = self.stream.encode(self.keyframe_owed, &mut data).context("encoding a VP9 frame")?;
        Ok(self.unit(keyframe, data))
    }

    /// Settle `mirror` at `quality`: encode it whole and at that quality, as one
    /// inter frame unless a keyframe is owed, and leave the dial where it was.
    ///
    /// What a desktop that went quiet while the link had it coarse is sent once
    /// ([`crate::encode`]): every block sharpened, and the rounds after it at what
    /// the link bears again. `None` and the mirror's padding are as
    /// [`Self::encode`] has them. An encoder that would not move its dial back is an
    /// error: it stays at the [`Self::quality`] it reports.
    pub fn settle(&mut self, mirror: &Mirror, quality: u8) -> anyhow::Result<Option<AccessUnit>> {
        self.read(mirror, None)?;
        let mut data = Vec::new();
        let keyframe = self.stream.settle(quality, self.keyframe_owed, &mut data).context("settling a VP9 stream")?;
        Ok(self.unit(keyframe, data))
    }

    /// Read `mirror` into the stream where it `changed`, or whole.
    fn read(&mut self, mirror: &Mirror, changed: Option<&[Rect]>) -> anyhow::Result<()> {
        anyhow::ensure!(
            mirror.coded() == self.coded,
            "a {}x{} vp9 stream was handed a {}x{} mirror",
            self.coded.0,
            self.coded.1,
            mirror.coded().0,
            mirror.coded().1
        );
        let changed: Option<Vec<screen_vp9::Rect>> = changed.map(|rects| rects.iter().map(|rect| coded_rect(mirror, *rect)).collect());
        self.stream.read_rgb(mirror.picture(), changed.as_deref()).context("reading the mirror for a VP9 frame")
    }

    /// The access unit an encode produced, if it produced one.
    fn unit(&mut self, keyframe: Option<bool>, data: Vec<u8>) -> Option<AccessUnit> {
        // Cleared only when something came out: a frame that produced no bitstream still
        // owes its keyframe, and the caller's dirty flag is what brings it back.
        keyframe.map(|keyframe| {
            self.keyframe_owed = false;
            AccessUnit { data, keyframe }
        })
    }
}

/// `rect` of the desktop as the encoder is told of it: with the mirror's padding column
/// or row beside it where it reaches the desktop's edge, since [`Mirror::pad_edges`]
/// repeats that edge into the padding and a change to one is a change to the other.
fn coded_rect(mirror: &Mirror, rect: Rect) -> screen_vp9::Rect {
    let (size, coded) = (mirror.size(), mirror.coded());
    let padded = |far: u16, size: u16, coded: u16| u16::from(far + 1 == size && coded != size);
    screen_vp9::Rect {
        x: rect.left,
        y: rect.top,
        width: rect.w() + padded(rect.right, size.0, coded.0),
        height: rect.h() + padded(rect.bottom, size.1, coded.1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rectangle from a position and a size, which is what most of these want.
    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a rectangle with a size")
    }

    /// `w`×`h` of one colour.
    fn flat(w: u16, h: u16, colour: [u8; 3]) -> Vec<u8> {
        colour.iter().copied().cycle().take(usize::from(w) * usize::from(h) * 3).collect()
    }

    /// A mirror and the stream over it.
    fn whole(w: u16, h: u16, quality: u8) -> (Mirror, Stream) {
        let mirror = Mirror::new(w, h).expect("a mirror");
        let stream = Stream::new(mirror.coded(), quality, Chroma::Subsampled).expect("a stream");
        (mirror, stream)
    }

    /// Blit a moving block and encode, so there is something for the quantizer to be coarse
    /// about.
    fn moving(mirror: &mut Mirror, stream: &mut Stream, step: u16) -> AccessUnit {
        let mut picture = flat(320, 240, [30, 60, 90]);
        let stride = 320 * 3;
        for row in 0..80 {
            let at = (usize::from(step) * 3 + row) * stride + usize::from(step) * 9;
            picture[at..at + 300].fill(230);
        }
        mirror.blit(rect(0, 0, 320, 240), &picture).expect("a full-screen blit");
        stream.encode(mirror, None).expect("an encode").expect("an access unit")
    }

    /// The keyframe is owed until a frame carries it, and not a frame longer.
    #[test]
    fn the_first_frame_is_a_keyframe_and_another_can_be_asked_for() {
        let (mut mirror, mut stream) = whole(320, 240, 60);
        assert!(moving(&mut mirror, &mut stream, 0).keyframe, "a decoder has to be able to start somewhere");
        assert!(!moving(&mut mirror, &mut stream, 1).keyframe, "an unasked-for keyframe is bytes for nothing");
        assert!(!moving(&mut mirror, &mut stream, 2).keyframe);
        stream.force_keyframe();
        let asked = moving(&mut mirror, &mut stream, 3);
        assert!(asked.keyframe, "force_keyframe did not reach the encoder");
        assert_eq!(frame_header(&asked.data), Some(FrameHeader { profile: 0, keyframe: true }));
        // And it is not sticky: the frame after a forced keyframe is an ordinary one.
        let after = moving(&mut mirror, &mut stream, 4);
        assert!(!after.keyframe);
        assert_eq!(frame_header(&after.data), Some(FrameHeader { profile: 0, keyframe: false }));
    }

    /// The dial reaches the running encoder without a keyframe, and the test's refusal
    /// hook stands in for libvpx's.
    #[test]
    fn the_quality_moves_on_a_live_encoder_and_a_refusal_leaves_it_where_it_was() {
        let (mut mirror, mut stream) = whole(320, 240, 90);
        assert_eq!(stream.quality(), 90);
        moving(&mut mirror, &mut stream, 0);
        let fine: usize = (1..5).map(|step| moving(&mut mirror, &mut stream, step).data.len()).sum();
        stream.set_quality(crate::video::QUALITY_MIN).expect("the encoder to accept a new quantizer");
        assert_eq!(stream.quality(), crate::video::QUALITY_MIN);
        let coarse: Vec<_> = (5..9).map(|step| moving(&mut mirror, &mut stream, step)).collect();
        assert!(coarse.iter().map(|unit| unit.data.len()).sum::<usize>() < fine, "the quantizer did not reach the running encoder");
        assert!(!coarse.iter().any(|unit| unit.keyframe), "moving the quantizer cost a keyframe");

        stream.refuse_retunes(1);
        assert!(stream.set_quality(60).is_err(), "the refusal did not fire");
        assert_eq!(stream.quality(), crate::video::QUALITY_MIN, "a refused retune moved the dial");
        stream.set_quality(60).expect("the refusal was one retune's");
        assert_eq!(stream.quality(), 60);
    }

    /// The stream's profile and colour fields follow the config's chroma into the string
    /// the browser is configured with, at the rate this gateway paces to.
    #[test]
    fn the_codec_string_follows_the_chroma() {
        assert_eq!(codec_string(1920, 1080, Chroma::Subsampled, ENCODED_FPS).as_deref(), Some("vp09.00.40.08.01.06.06.06.00"));
        assert_eq!(codec_string(1920, 1080, Chroma::Full, ENCODED_FPS).as_deref(), Some("vp09.01.40.08.03.06.06.06.00"));
        let (_, stream) = whole(1920, 1080, 60);
        assert_eq!(stream.decode_string(), Some("vp09.00.40.08.01.06.06.06.00"));
    }

    /// The odd case, which is where a chroma plane would be half a pixel wide if the mirror
    /// were not held at even sides.
    #[test]
    fn an_odd_desktop_is_padded_and_still_encodes() {
        let (mut mirror, mut stream) = whole(1919, 1079, 60);
        mirror.blit(rect(0, 0, 1919, 1079), &flat(1919, 1079, [90, 90, 90])).expect("a full-screen blit");
        mirror.pad_edges();
        let unit = stream.encode(&mirror, None).expect("an encode").expect("a unit");
        assert_eq!(frame_header(&unit.data).map(|header| header.profile), Some(0));
    }

    /// A change that reaches an odd desktop's last column or row takes the mirror's
    /// padding beside it, and no other change is widened.
    #[test]
    fn a_change_at_an_odd_desktops_edge_takes_the_padding_with_it() {
        let odd = Mirror::new(319, 239).expect("a mirror");
        let coded = |x, y, width, height| screen_vp9::Rect { x, y, width, height };
        assert_eq!(coded_rect(&odd, rect(10, 20, 30, 40)), coded(10, 20, 30, 40));
        assert_eq!(coded_rect(&odd, rect(300, 20, 19, 40)), coded(300, 20, 20, 40));
        assert_eq!(coded_rect(&odd, rect(10, 230, 30, 9)), coded(10, 230, 30, 10));
        assert_eq!(coded_rect(&odd, rect(0, 0, 319, 239)), coded(0, 0, 320, 240));
        let even = Mirror::new(320, 240).expect("a mirror");
        assert_eq!(coded_rect(&even, rect(0, 0, 320, 240)), coded(0, 0, 320, 240));
    }

    #[test]
    fn a_picture_too_large_is_refused_by_name() {
        let Err(refused) = Stream::new((5120, 2880), 60, Chroma::Subsampled) else {
            panic!("a 5K picture was accepted");
        };
        let message = format!("{refused:#}");
        assert!(message.contains("5120x2880"), "the message does not say what was asked for");
        assert!(message.contains("3840"), "the message does not say what the limit is");
        assert!(message.contains("resize"), "the message does not say what to do instead");
    }

    /// A stream is built for one picture size, and a mirror of any other is refused rather
    /// than read past its end.
    #[test]
    fn a_mirror_of_another_size_is_refused() {
        let (_, mut stream) = whole(320, 240, 60);
        let other = Mirror::new(640, 480).expect("a mirror");
        assert!(stream.encode(&other, None).is_err());
    }
}
