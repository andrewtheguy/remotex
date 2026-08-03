//! H.264 encoding: one stream over a rectangle of a [`crate::video::Mirror`].
//!
//! The other codec rather than the default one. A target asks for it by name through
//! [`crate::config::TargetConfig::video_codec`]; nothing here knows which of the two is
//! running, and a browser that cannot decode what it is configured to send says so from
//! its own `VideoDecoder`, naming the codec, rather than getting the other one. What the
//! two share — the mirror, the coded rectangle, the colour conversion — is
//! [`crate::video`]'s, and this module is only openh264.
//!
//! That containment is not tidiness. This crate asserts on odd dimensions, on a
//! mis-sized slice and on an out-of-range quantizer, and the release profile is
//! `panic = "abort"` — so an assertion reaching openh264 takes the whole gateway down
//! rather than one session. Each is made unreachable by construction below or in
//! [`crate::video`], and each has a test.

use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, QpRange,
    RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::{OpenH264API, Timestamp};

use crate::tiles::Rect;
use crate::video::{AccessUnit, I420, Mark, Mirror, QUALITY_MAX, QUALITY_MIN, coded_rect, outline};

/// The quantizer the dial spans. 51 is H.264's coarsest.
///
/// The fine end is 12 rather than 0 because openh264 clamps `iMinQp` up to its
/// `GOM_MIN_QP_MODE`, which is 12: a dial that mapped past it would have a top third
/// where turning the knob did nothing at all.
const QP_FINEST: u8 = 12;
/// See [`QP_FINEST`].
const QP_COARSEST: u8 = 51;

/// The frame rate the encoder is told to expect.
///
/// Nothing paces frames to it — the remote decides when a frame happens, and this
/// encoder is handed whatever arrives. It is set because openh264's rate control reads
/// it, and a plausible number is better than the crate's default of zero.
const NOMINAL_FPS: f32 = 30.0;

/// The bitrate the encoder is told to aim at.
///
/// At a pinned quantizer this decides nothing: the quantizer *is* the dial, and the
/// bitrate lands wherever the picture puts it. It is set because openh264 verifies it
/// against the level, and because the crate's default of 120 kbps is not a number about
/// a desktop.
const NOMINAL_BITRATE: u32 = 8_000_000;

/// The 1–100 quality dial as a constant quantizer.
///
/// The two scales run opposite ways: [`QUALITY_MIN`] is the coarsest picture and becomes
/// [`QP_COARSEST`], [`QUALITY_MAX`] the finest and becomes [`QP_FINEST`]. Out-of-range
/// input is clamped rather than refused — `config` is what rejects a bad dial, and this
/// is on the path where a panic would take the process with it.
///
/// Private, and that is the point of the dial: the quantizer is openh264's scale and
/// stops at this module's edge. VP9's is a different 0–63 and the congestion loop knows
/// neither.
fn qp_for(quality: u8) -> u8 {
    let quality = u32::from(quality.clamp(QUALITY_MIN, QUALITY_MAX));
    let span = u32::from(QP_COARSEST - QP_FINEST);
    QP_COARSEST - ((quality - 1) * span / 99) as u8
}

/// The WebCodecs codec string for an Annex-B access unit, from its SPS.
///
/// `avc1.PPCCLL` is `profile_idc`, the constraint-flags byte and `level_idc`, which are
/// the three bytes after an SPS NAL's header — so this is the encoder's own answer about
/// what it just produced, rather than a prediction. `None` for a unit with no SPS in it,
/// which every non-keyframe is: `sps_pps_strategy(SpsPpsStrategy::ConstantId)` below is
/// what puts one in front of every keyframe.
///
/// This is what `ServerMsg::VideoFormat` carries, and it is derived here rather than in
/// the client because VP9 has no in-band parameter sets at all — a client that had to
/// find its own codec string for one codec and be told it for the other would be two
/// contracts.
pub fn codec_string(unit: &[u8]) -> Option<String> {
    let mut at = 0usize;
    while at + 2 < unit.len() {
        if unit[at] != 0 || unit[at + 1] != 0 {
            at += 1;
            continue;
        }
        // Three-byte and four-byte start codes both appear in one bitstream: openh264
        // writes four ahead of a parameter set and three ahead of a slice.
        let start = match (unit[at + 2], unit.get(at + 3)) {
            (1, _) => at + 3,
            (0, Some(1)) => at + 4,
            _ => {
                at += 1;
                continue;
            }
        };
        if start + 3 < unit.len() && unit[start] & 0x1f == 7 {
            return Some(format!(
                "avc1.{:02x}{:02x}{:02x}",
                unit[start + 1],
                unit[start + 2],
                unit[start + 3]
            ));
        }
        at = start;
    }
    None
}

/// One H.264 stream over a fixed rectangle of a [`Mirror`].
///
/// The rectangle is fixed for the stream's whole life, and that is what makes an
/// inter-frame stream mean anything: every frame is expressed as a change from the last
/// one at the same place. A region that moves or grows gets a *new* stream, which is
/// [`crate::regions`]' decision to make.
pub struct Stream {
    encoder: Encoder,
    /// The region as the client knows it, and as a record header reports it.
    rect: Rect,
    /// The picture actually encoded: [`Self::rect`] grown to even sides.
    coded: Rect,
    /// The conversion in front of the encoder, reused across frames.
    yuv: I420,
    /// [`Self::coded`] cropped out of the mirror, reused for the same reason.
    scratch: Vec<u8>,
    /// The 1–100 dial in force, which [`Self::set_quality`] moves and the totals report.
    quality: u8,
    /// The quantizer that dial came out as, so a move that changes nothing costs no FFI.
    qp: u8,
    /// The codec string of the last keyframe this stream produced — what
    /// `ServerMsg::VideoFormat` announces. `None` until there has been one.
    decode: Option<String>,
    /// Where this stream's timestamps are measured from. Real elapsed time rather than a
    /// frame counter, because openh264's screen-content rate control reads it and a
    /// counter would tell it every frame arrived on schedule.
    started: std::time::Instant,
}

impl Stream {
    /// A stream over `rect` of a mirror whose coded size is `mirror`, at `quality`
    /// (1–100).
    ///
    /// The coded rectangle, and the refusal of a picture too large for it, are
    /// [`coded_rect`]'s — including the theorem that makes the evenness openh264
    /// requires something checked once rather than hoped for here.
    pub fn new(rect: Rect, mirror: (u16, u16), quality: u8) -> anyhow::Result<Self> {
        let coded = coded_rect(rect, mirror)?;
        let qp = qp_for(quality);
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
            // The shadow has already recorded these pixels as delivered, so a frame the
            // encoder decided to skip would be permanently wrong pixels.
            .skip_frames(false)
            // SPS and PPS repeat with every keyframe, which is what [`codec_string`]
            // reads and what lets a decoder be built from any keyframe.
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            // One slice on one thread: this already runs on a blocking worker, and one
            // thread makes an access unit a deterministic thing to test. Several regions
            // encode in parallel with each other, which is where the cores go.
            .num_threads(1)
            .complexity(Complexity::Low)
            // Both are refused for screen content anyway — openh264 turns them off and
            // says so on stderr — so they are off here rather than left at the crate's
            // camera defaults for it to complain about once per encoder. Adaptive
            // quantization would also have moved the quantizer off the dial.
            //
            // One warning it does still print at every init is expected and correct:
            // that a bitrate cannot be enforced without frame skipping. It cannot, and
            // it is not meant to be — the quantizer is the dial and skipping a frame is
            // the one thing this encoder may never do.
            .adaptive_quantization(false)
            .background_detection(false)
            // No fixed keyframe interval. Nothing here is ever lost in transit — the
            // link is TCP — so a periodic keyframe would buy resilience against nothing
            // and cost the bytes every time. The keyframes that do happen are asked for:
            // a repaint, a resize, a client coming back, a region that grew.
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
            yuv: I420::new(coded.w(), coded.h()),
            scratch: Vec::new(),
            quality: quality.clamp(QUALITY_MIN, QUALITY_MAX),
            qp,
            decode: None,
            started: std::time::Instant::now(),
        })
    }

    /// The region this stream is for — what a record header reports, and what a client
    /// crops the decoded picture to.
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// The dial this stream is currently encoding at.
    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// The WebCodecs codec string for this stream, once a keyframe has said what it is.
    pub fn decode_string(&self) -> Option<&str> {
        self.decode.as_deref()
    }

    /// Make the next access unit one a decoder can start from.
    pub fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    /// Move the dial on the live encoder, without a keyframe.
    ///
    /// This is how a congested link gives up quality (see `Congestion` in
    /// [`crate::encode`]), and "without a keyframe" is the whole reason it is written
    /// this way. Rebuilding the encoder with a new configuration would be safe Rust and
    /// would force an IDR on the next frame — a few hundred KB spent at the exact moment
    /// the link has run out of room, making worse the thing it is reacting to.
    ///
    /// Reading the parameters back out and handing them straight in again, rather than
    /// building a fresh `SEncParamExt`, is what keeps this from having to know every
    /// field [`Stream::new`] set: whatever the encoder is running on now is what it goes
    /// on running on, with two integers changed.
    pub fn set_quality(&mut self, quality: u8) -> anyhow::Result<()> {
        let quality = quality.clamp(QUALITY_MIN, QUALITY_MAX);
        self.quality = quality;
        let qp = qp_for(quality);
        if qp == self.qp {
            return Ok(());
        }
        let mut params = openh264_sys2::SEncParamExt::default();
        let option = openh264_sys2::ENCODER_OPTION_SVC_ENCODE_PARAM_EXT;
        // SAFETY: `params` is the type this option is documented to carry, it is
        // initialized before the get and fully populated by it, and it outlives both
        // calls. `raw_api` needs `&mut self`, which is what serializes this against the
        // encode — the same borrow that makes `encode` exclusive.
        //
        // This is the pair of calls the crate itself makes to re-tune a running encoder
        // (`Encoder::reinit`'s second-and-later branch).
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
    /// into the padding, which is the caller's job because it is once per round rather
    /// than once per stream.
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
        self.yuv.read_rgb(&self.scratch)?;

        let at = Timestamp::from_millis(self.started.elapsed().as_millis() as u64);
        let bitstream = self
            .encoder
            .encode_at(self.yuv.source(), at)
            .map_err(|e| anyhow::anyhow!("h264 encode failed: {e}"))?;
        let (keyframe, data) = (bitstream.frame_type() == FrameType::IDR, bitstream.to_vec());
        if data.is_empty() {
            return Ok(None);
        }
        if keyframe && let Some(decode) = codec_string(&data) {
            // A keyframe carries the SPS, so this is the one place the string can be
            // read. It never changes for a live stream — the picture size is fixed for
            // its whole life — but reading it every time costs one scan of one keyframe
            // and needs no argument about when it could not have changed.
            //
            // Only ever replaced by an answer, never by the lack of one: a keyframe this
            // scan could not find an SPS in would otherwise take the announced string
            // back to `None` mid-stream, leaving a live decoder that the client can no
            // longer name.
            self.decode = Some(decode);
        }
        Ok(Some(AccessUnit { data, keyframe }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rectangle from a position and a size, which is what most of these want.
    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a rectangle with a size")
    }

    /// `w`×`h` of one colour, which makes "did these pixels land here" readable a byte
    /// at a time.
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
            // about — a stream of identical frames costs nearly nothing at any quality
            // and would compare two zeroes.
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
        assert_eq!(qp_for(QUALITY_MIN), QP_COARSEST);
        assert_eq!(qp_for(QUALITY_MAX), QP_FINEST);
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
        // A small patch, deliberately: repainting the whole screen a different colour is
        // a scene change, and openh264 answers one with a keyframe of its own —
        // correctly, since intra is cheaper there than predicting from a picture that no
        // longer resembles this one. What this test is about is the *ordinary* frame,
        // where a keyframe would be bytes for nothing.
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
        // The client splits on start codes, so this is the wire contract in one
        // assertion: openh264's own four-byte one, ahead of the SPS.
        assert_eq!(&unit.data[..4], &[0, 0, 0, 1], "not an Annex-B start code");
    }

    /// The codec string against the encoder's own output rather than a hand-built NAL,
    /// which is the whole reason it is derived here: what a client is told has to be
    /// what openh264 actually produced.
    #[test]
    fn the_codec_string_comes_off_the_encoders_own_keyframe() {
        let (mut mirror, mut stream) = whole(320, 240, 60);
        mirror
            .blit(rect(0, 0, 320, 240), &flat(320, 240, [7, 7, 7]))
            .expect("a full-screen blit");
        let keyframe = stream.encode(&mirror, None).expect("an encode").expect("a unit");
        assert!(keyframe.keyframe);
        let announced = stream.decode_string().expect("a keyframe says what it is").to_owned();
        assert_eq!(codec_string(&keyframe.data).as_deref(), Some(announced.as_str()));
        let mut hex = announced.strip_prefix("avc1.").expect("an avc1 string").chars();
        assert_eq!(hex.clone().count(), 6, "avc1.PPCCLL is six hex digits: {announced}");
        assert!(hex.all(|c| c.is_ascii_hexdigit()), "not hex: {announced}");

        // A delta frame carries no SPS, so it cannot name a codec — and the stream goes
        // on reporting what its keyframe said.
        mirror
            .blit(rect(64, 64, 32, 32), &flat(32, 32, [200, 200, 200]))
            .expect("a patch inside the desktop");
        let delta = stream.encode(&mirror, None).expect("an encode").expect("a unit");
        assert!(!delta.keyframe);
        assert_eq!(codec_string(&delta.data), None, "a delta frame named a codec");
        assert_eq!(stream.decode_string(), Some(announced.as_str()));

        // Bytes that are not Annex-B at all name nothing rather than indexing off the
        // end of them.
        assert_eq!(codec_string(&[]), None);
        assert_eq!(codec_string(&[0, 0, 1]), None);
        assert_eq!(codec_string(&[0, 0, 0, 1, 0x67]), None);
        assert_eq!(codec_string(&flat(4, 1, [9, 9, 9])), None);
    }

    /// The test that holds the abort-the-process hazard closed: every openh264 assertion
    /// on this path is about even dimensions, and this is the odd case.
    #[test]
    fn an_odd_desktop_is_padded_and_still_encodes() {
        let (mut mirror, mut stream) = whole(1919, 1079, 60);
        assert_eq!(stream.rect(), mirror.rect(), "a record header carries the true region");
        mirror
            .blit(rect(0, 0, 1919, 1079), &flat(1919, 1079, [90, 90, 90]))
            .expect("a full-screen blit");
        mirror.pad_edges();
        assert!(stream.encode(&mirror, None).expect("an encode").is_some());
    }

    /// The mechanism the congestion loop rests on: quality can be given up mid-stream
    /// without spending a keyframe to do it.
    #[test]
    fn the_quality_moves_on_a_live_encoder_without_a_keyframe() {
        let (mut mirror, mut stream) = whole(320, 240, 90);
        assert_eq!(stream.quality(), 90);

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

        stream.set_quality(QUALITY_MIN).expect("the encoder to accept a new quantizer");
        assert_eq!(stream.quality(), QUALITY_MIN);
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

    /// Two streams over disjoint regions of one mirror, which is the whole point of the
    /// split: each sees its own pixels and neither sees the other's.
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
