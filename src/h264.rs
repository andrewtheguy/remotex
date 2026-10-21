//! H.264 as this gateway meets it on a `wlshare` target: the moving regions of
//! its tiles with motion, coded by the host's hardware encoder and passed to
//! the browser as wlshare coded them ([`pass`]), for a browser whose own
//! decoder takes H.264 (`h264` on the session socket). The gateway encodes no
//! H.264 and decodes none.
//!
//! What a passed region's frame needs here is what the browser cannot read out
//! of it in time: whether a decoder can start at it, and the WebCodecs string
//! its decoder is configured with, which H.264's own sequence parameter set
//! names, as RFC 6381 writes it, `avc1.` and the profile, constraints and level.
//! Each frame is one Annex B access unit, the parameter sets ahead of every
//! IDR, which is what a `VideoDecoder` configured with no `description` takes.

use crate::video::check_picture;
use crate::vnc_apple_media::parse_h264_sps;

/// A frame of a region's H.264 stream, as the browser is told about it.
#[derive(Debug, PartialEq, Eq)]
pub struct Region {
    /// The configuration string a decoder for it is built with, where the
    /// frame carries its sequence parameter set: a keyframe's.
    pub decode: Option<String>,
    /// Whether a decoder that has seen nothing before this frame can start
    /// here: it holds an IDR picture.
    pub keyframe: bool,
}

/// The NAL units of an Annex B access unit, each from its header byte, in
/// order; a unit with no start code is none.
pub fn nal_units(unit: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut at = 0;
    while at + 3 <= unit.len() {
        if unit[at] == 0 && unit[at + 1] == 0 && unit[at + 2] == 1 {
            starts.push(at + 3);
            at += 3;
        } else {
            at += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .filter_map(|(i, &from)| {
            // Up to the next start code, less the zero byte of a four-byte one.
            let mut to = starts.get(i + 1).map_or(unit.len(), |&next| next - 3);
            if to > from && unit[to - 1] == 0 && starts.get(i + 1).is_some() {
                to -= 1;
            }
            (to > from).then(|| &unit[from..to])
        })
        .collect()
}

/// Check a `w`×`h` region's frame of wlshare's H.264: Annex B with at least
/// one NAL unit, within the ceiling every passed picture is held to, and
/// where it carries a sequence parameter set, one of a picture the region's
/// size — a pixel wider or taller at most, since a 4:2:0 picture is cropped
/// in pairs and the page clips the picture to the region anyway.
pub fn pass(w: u16, h: u16, unit: &[u8]) -> anyhow::Result<Region> {
    let nals = nal_units(unit);
    anyhow::ensure!(!nals.is_empty(), "the server's H.264 frame is not an Annex B access unit");
    check_picture((w, h))?;
    let mut decode = None;
    for nal in &nals {
        if nal[0] & 0x1f == 7 {
            let params = parse_h264_sps(nal).ok_or_else(|| anyhow::anyhow!("the server's H.264 frame carries a sequence parameter set this gateway cannot read"))?;
            let (sw, sh) = params.size;
            anyhow::ensure!(
                (w..=w.saturating_add(1)).contains(&sw) && (h..=h.saturating_add(1)).contains(&sh),
                "the server's H.264 frame is of a {sw}x{sh} picture, not the {w}x{h} region's"
            );
            decode = Some(params.decode);
        }
    }
    let keyframe = nals.iter().any(|nal| nal[0] & 0x1f == 5);
    Ok(Region { decode, keyframe })
}

/// A 320×192 grey picture's first access unit as x264 codes it at High profile,
/// level 1.3 — SPS, PPS and an IDR — and the P frame after it, for the tests here
/// and the sink's.
#[cfg(test)]
pub mod sample {
    pub const KEYFRAME: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x64, 0x00, 0x0d, 0xac, 0xb4, 0x0a, 0x0c, 0xd8, 0x08, 0x80, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x1e, 0x07, 0x8a, 0x15, 0x50, 0, 0, 0, 1, 0x68, 0xef, 0x04, 0x4b, 0x22,
        0xc0, 0, 0, 0, 1, 0x65, 0x88, 0x84, 0x3f, 0xfe, 0xb5, 0xce, 0xee, 0x93, 0x79, 0x85, 0x56, 0x58, 0x76, 0x4e, 0x18, 0xef, 0xbd, 0x09, 0xa0, 0x00, 0x03, 0x98, 0xb2, 0xb3, 0x5f, 0xc7, 0x16,
        0x72, 0x48, 0x02, 0x02, 0x00, 0x02, 0x1c, 0x3e, 0xf8, 0xd2, 0x1a, 0x53, 0x88, 0xc9, 0x5e, 0x81,
    ];
    pub const FRAME: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x26, 0x3f, 0x00, 0x01, 0xc7];
}

#[cfg(test)]
mod tests {
    use super::sample::{FRAME, KEYFRAME};
    use super::*;

    #[test]
    fn nal_units_are_cut_at_their_start_codes() {
        let units = nal_units(KEYFRAME);
        assert_eq!(units.iter().map(|nal| nal[0] & 0x1f).collect::<Vec<_>>(), [7, 8, 5]);
        assert_eq!(units[0].len(), 23, "the SPS runs up to the next start code, its zero byte excluded");
        assert_eq!(nal_units(FRAME).len(), 1);
        assert!(nal_units(&[0, 0, 0, 1]).is_empty(), "a start code with nothing after it");
        assert!(nal_units(&[0x80, 0x12]).is_empty(), "a VP9 frame is no access unit");
    }

    #[test]
    fn a_keyframe_names_its_decoder_and_a_frame_names_none() {
        assert_eq!(pass(320, 192, KEYFRAME).unwrap(), Region { decode: Some("avc1.64000D".to_owned()), keyframe: true });
        assert!(pass(319, 191, KEYFRAME).unwrap().keyframe, "a region a pixel smaller than the coded picture");
        assert_eq!(pass(320, 192, FRAME).unwrap(), Region { decode: None, keyframe: false });
    }

    #[test]
    fn a_frame_that_is_not_what_wlshare_promised_is_refused() {
        assert!(pass(320, 192, &[0x80, 0, 0, 0]).is_err(), "a VP9 frame");
        assert!(pass(640, 192, KEYFRAME).is_err(), "a picture of another size");
        assert!(pass(318, 192, KEYFRAME).is_err(), "two pixels narrower is another picture");
        assert!(pass(5376, 2288, KEYFRAME).is_err(), "past the ceiling");
    }
}
