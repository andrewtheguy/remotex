//! VP9 as this gateway meets it: wlshare's own stream, passed to the browser as wlshare
//! coded it ([`pass`]). The gateway encodes no VP9; the picture it codes itself is
//! tiles ([`crate::tiles`]).
//!
//! What a passed frame needs here is what the browser cannot read out of it: the
//! profile and the keyframe bit of its uncompressed header ([`frame_header`]), and the
//! WebCodecs string a decoder is configured with ([`codec_string`]), since VP9 carries
//! no parameter sets. The chroma ([`Chroma`]) is the plan's, which the listing asks
//! wlshare for: 4:4:4 for a browser whose own decoder takes profile 1, which the page
//! asks it once at load (`vp9_444` on the session socket), and 4:2:0 otherwise. No
//! browser's hardware VP9 path takes profile 1, and iOS and iPadOS have no software
//! one, so a browser that says it takes no profile 1 is never sent it.

use crate::video::check_picture;

/// How a stream samples colour, which is its VP9 profile: a sample per pixel, or one
/// per 2×2 block of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chroma {
    /// 4:4:4, VP9 profile 1: a colour sample per pixel.
    Yuv444,
    /// 4:2:0, VP9 profile 0: a colour sample per 2×2 pixels.
    Yuv420,
}

impl Chroma {
    /// The VP9 profile of a stream at this chroma, eight bits, which is what
    /// [`FrameHeader::profile`] says of one of its frames.
    pub fn profile(self) -> u8 {
        match self {
            Self::Yuv444 => 1,
            Self::Yuv420 => 0,
        }
    }

    /// The codec string's chroma field: 3 is 4:4:4, and 1 is 4:2:0 with its samples
    /// sited at the luma's, WebM's default, which is what a decoder assumes of a VP9
    /// 4:2:0 picture anyway: the bitstream does not say.
    fn codec_field(self) -> &'static str {
        match self {
            Self::Yuv444 => "03",
            Self::Yuv420 => "01",
        }
    }

    /// The chroma as a sentence names it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Yuv444 => "4:4:4",
            Self::Yuv420 => "4:2:0",
        }
    }
}

/// What the opening bits of a VP9 frame say about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// The VP9 profile, 0–3: 1 for 4:4:4 and 0 for 4:2:0 ([`Chroma::profile`]).
    pub profile: u8,
    /// Whether a decoder that has seen nothing before this frame can start here.
    pub keyframe: bool,
}

/// Read the first fields of a frame's uncompressed header (VP9 bitstream §6.2):
/// `frame_marker` (two bits, always 2), `profile_low_bit`, `profile_high_bit`, a
/// reserved zero bit on profile 3, `show_existing_frame`, and `frame_type`, where 0
/// is a keyframe. A frame that only shows an earlier one is not a keyframe.
///
/// `None` for bytes that do not start a VP9 frame.
pub fn frame_header(frame: &[u8]) -> Option<FrameHeader> {
    let mut bits = frame.iter().take(2).flat_map(|byte| (0..8).rev().map(move |i| byte >> i & 1));
    let mut bit = || bits.next();
    if (bit()? << 1 | bit()?) != 2 {
        return None;
    }
    let low = bit()?;
    let profile = bit()? << 1 | low;
    if profile == 3 && bit()? != 0 {
        return None;
    }
    let show_existing = bit()? == 1;
    let keyframe = !show_existing && bit()? == 0;
    Some(FrameHeader { profile, keyframe })
}

/// VP9's levels, as `(level, max_luma_sample_rate, max_luma_picture_size,
/// max_luma_breadth)`.
///
/// Transcribed from `vp9_level_defs[]` in libvpx's `vp9/encoder/vp9_encoder.c`, and
/// not from memory: the table is the normative one, the rows are not evenly spaced,
/// and 4K30 lands on level 5.0 rather than the 5.1 a plausible guess gives. Only the
/// three fields a picture size decides are kept. A level also caps the bitrate, the
/// buffer size and the compression ratio, which wlshare's quality walk, not a level,
/// keeps within the link. So the level announced is the picture's, which is what a
/// browser reads it for: it picks a decoder by profile and size, and it refuses a
/// string whose level is not one of the fourteen — Chromium's parser rejects the
/// "undefined" 0 the file-format binding allows — so a level must be named.
///
/// The lowest row a picture fits is the level announced, which matters because a
/// level is a ceiling: a decoder that accepts 5.0 accepts every stream below it, so
/// the smallest level the picture fits is the widest claim to make.
const LEVELS: [(u8, u64, u32, u16); 14] = [
    (10, 829_440, 36_864, 512),
    (11, 2_764_800, 73_728, 768),
    (20, 4_608_000, 122_880, 960),
    (21, 9_216_000, 245_760, 1_344),
    (30, 20_736_000, 552_960, 2_048),
    (31, 36_864_000, 983_040, 2_752),
    (40, 83_558_400, 2_228_224, 4_160),
    (41, 160_432_128, 2_228_224, 4_160),
    (50, 311_951_360, 8_912_896, 8_384),
    (51, 588_251_136, 8_912_896, 8_384),
    (52, 1_176_502_272, 8_912_896, 8_384),
    (60, 1_176_502_272, 35_651_584, 16_832),
    (61, 2_353_004_544, 35_651_584, 16_832),
    (62, 4_706_009_088, 35_651_584, 16_832),
];

/// The WebCodecs codec string for a `w`×`h` stream of wlshare's at `chroma`, every
/// field of it:
/// `vp09.<profile>.<level>.<depth>.<chroma>.<primaries>.<transfer>.<matrix>.<range>`.
///
/// The chroma's profile, eight bits and the chroma, because that is what wlshare's
/// encoder produces. The optional fields are spelled out rather than left to their
/// defaults because the defaults are wrong for this stream: an omitted chroma field
/// means 4:2:0, which Chromium reads as 4:2:2 on a profile 1 string since 4:2:0 is not
/// a profile 1 picture, and omitted colour fields mean BT.709 where the keyframe
/// header says BT.601 (SMPTE 170M primaries, transfer and matrix — code 6 each, what
/// Chromium's own VP9 parser maps that header flag to) at studio swing. The level
/// comes from `LEVELS` at `fps` frames a second. `None` for a picture no VP9 level
/// covers.
pub fn codec_string(w: u16, h: u16, fps: u64, chroma: Chroma) -> Option<String> {
    let size = u32::from(w) * u32::from(h);
    let rate = u64::from(size) * fps;
    let breadth = w.max(h);
    let (level, ..) = LEVELS
        .iter()
        .find(|(_, max_rate, max_size, max_breadth)| rate <= *max_rate && size <= *max_size && breadth <= *max_breadth)
        .copied()?;
    Some(format!("vp09.{:02}.{level:02}.08.{}.06.06.06.00", chroma.profile(), chroma.codec_field()))
}

/// An access unit the remote encoded itself, as the browser is told about it.
#[derive(Debug, PartialEq, Eq)]
pub struct Passed {
    /// The configuration string a decoder for it is built with.
    pub decode: String,
    pub keyframe: bool,
    /// Which strip of the picture it is, where it is not the whole of it.
    pub strip: Option<crate::protocol::Strip>,
}

/// The frame rate a passed stream's level is figured at: wlshare's own default ceiling,
/// `max_fps = 60`, since its server rather than this gateway paces it. A level tells a decoder
/// the most it will be asked for, so the rate the remote may reach is the one to name.
const PASSED_FPS: u64 = 60;

/// Check a `w`×`h` frame of wlshare's VP9 encoding: at `chroma`, the plan's, which the
/// listing asked wlshare for, BT.601 at studio swing, declared in its keyframes. The
/// profile is read and held to the chroma's ([`Chroma::profile`]), so the configuration
/// announced for it is the one the frame needs, and the browser that said it takes no
/// 4:4:4 is never handed one.
pub fn pass(w: u16, h: u16, frame: &[u8], chroma: Chroma) -> anyhow::Result<Passed> {
    let header =
        frame_header(frame).ok_or_else(|| anyhow::anyhow!("the server's VP9 frame does not start with a VP9 header"))?;
    anyhow::ensure!(
        header.profile == chroma.profile(),
        "the server's VP9 frame is profile {}, not the profile {} of the {} this session asked for",
        header.profile,
        chroma.profile(),
        chroma.name()
    );
    check_picture((w, h))?;
    let decode = codec_string(w, h, PASSED_FPS, chroma).ok_or_else(|| anyhow::anyhow!("no VP9 level covers a {w}x{h} picture"))?;
    Ok(Passed { decode, keyframe: header.keyframe, strip: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A passed picture is held to the ceiling every picture is, so a desktop past it
    /// ends on the same refusal whichever picture it would have been.
    #[test]
    fn a_passed_frame_past_the_ceiling_is_refused() {
        let keyframe = [0xa0u8, 0, 0, 0];
        assert!(pass(1920, 1080, &keyframe, Chroma::Yuv444).is_ok());
        let refused = pass(5376, 2288, &keyframe, Chroma::Yuv444).expect_err("a 5376x2288 frame was passed");
        assert_eq!(refused.to_string(), check_picture((5376, 2288)).unwrap_err().to_string());
    }

    /// A passed frame is announced at the chroma the session asked for, and one of
    /// another profile — a server that did not code what it was asked — is refused by
    /// name rather than handed to a decoder configured for something else.
    #[test]
    fn a_passed_frame_is_held_to_the_sessions_chroma() {
        let (profile_0, profile_1) = ([0x80u8, 0, 0, 0], [0xa0u8, 0, 0, 0]);
        let full = pass(1920, 1080, &profile_1, Chroma::Yuv444).expect("a 4:4:4 frame");
        assert_eq!(full.decode, "vp09.01.41.08.03.06.06.06.00");
        let subsampled = pass(1920, 1080, &profile_0, Chroma::Yuv420).expect("a 4:2:0 frame");
        assert_eq!(subsampled.decode, "vp09.00.41.08.01.06.06.06.00");
        assert_eq!(
            pass(1920, 1080, &profile_0, Chroma::Yuv444).unwrap_err().to_string(),
            "the server's VP9 frame is profile 0, not the profile 1 of the 4:4:4 this session asked for"
        );
        assert_eq!(
            pass(1920, 1080, &profile_1, Chroma::Yuv420).unwrap_err().to_string(),
            "the server's VP9 frame is profile 1, not the profile 0 of the 4:2:0 this session asked for"
        );
    }

    /// The header's first bits: the marker, the profile either way round, a reserved
    /// bit on profile 3, a frame that shows an earlier one, and the frame type.
    #[test]
    fn a_frame_header_says_its_profile_and_whether_it_is_a_keyframe() {
        assert_eq!(frame_header(&[0x80, 0]), Some(FrameHeader { profile: 0, keyframe: true }));
        assert_eq!(frame_header(&[0x84, 0]), Some(FrameHeader { profile: 0, keyframe: false }));
        assert_eq!(frame_header(&[0xa0, 0]), Some(FrameHeader { profile: 1, keyframe: true }));
        assert_eq!(frame_header(&[0x90, 0]), Some(FrameHeader { profile: 2, keyframe: true }));
        assert_eq!(frame_header(&[0xb0, 0]), Some(FrameHeader { profile: 3, keyframe: true }));
        assert_eq!(frame_header(&[0xb8, 0]), None, "profile 3's reserved bit set");
        assert_eq!(frame_header(&[0x88, 0]), Some(FrameHeader { profile: 0, keyframe: false }), "a shown earlier frame");
        assert_eq!(frame_header(&[0x40, 0]), None, "no frame marker");
        assert_eq!(frame_header(&[]), None);
    }

    /// The level is the lowest a picture fits at the rate named, 4K30 on 5.0 and
    /// 1080p60 on 4.1, and a picture no level covers has no string.
    #[test]
    fn the_codec_string_names_the_lowest_level_the_picture_fits() {
        assert_eq!(codec_string(1920, 1080, 30, Chroma::Yuv420).as_deref(), Some("vp09.00.40.08.01.06.06.06.00"));
        assert_eq!(codec_string(1920, 1080, 60, Chroma::Yuv444).as_deref(), Some("vp09.01.41.08.03.06.06.06.00"));
        assert_eq!(codec_string(3840, 2160, 30, Chroma::Yuv420).as_deref(), Some("vp09.00.50.08.01.06.06.06.00"));
        assert_eq!(codec_string(16_832, 16_832, 60, Chroma::Yuv420), None);
    }
}
