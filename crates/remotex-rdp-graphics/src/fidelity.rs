//! Whether a host draws its pipeline exactly.
//!
//! A host says nothing of how faithfully it draws: a Windows host's image quality
//! is its own policy (*Configure image quality for RemoteFX Adaptive Graphics*), and
//! no capability or PDU carries it. What the policy changes is the codecs the host
//! draws with, and those every command names. Measured against a Windows 11 host,
//! one set to Lossless drew with planar and uncompressed rectangles alone, and one
//! left at its default with RemoteFX Progressive and ClearCodec carrying NSCodec.
//!
//! So the codecs a pipeline has drawn with say whether its picture is exact so far:
//!
//! - **Exact:** uncompressed rectangles; planar, which this client takes without
//!   colour loss or subsampling ([`crate::proto::planar`] refuses both); the alpha
//!   codec; and ClearCodec whose subcodecs are raw or RLEX.
//! - **Not:** RemoteFX, RemoteFX Progressive, every H.264, and ClearCodec carrying
//!   NSCodec. NSCodec's chroma is halved into its YCoCg planes even at colour loss
//!   level 1 (MS-RDPEGDI 3.1.9.1.2), so it is not exact at any level.
//!
//! A codec this list does not name is counted as not exact: "exact" is said only of
//! what is known to be.

use std::collections::BTreeSet;

use crate::proto::gfx;
use crate::proto::wire::Reader;

/// What one draw was made with, as far as its exactness goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Drawn {
    Uncompressed,
    Planar,
    Alpha,
    /// ClearCodec with no NSCodec subcodec in it.
    ClearCodec,
    /// ClearCodec with at least one NSCodec subcodec.
    ClearCodecNsc,
    RemoteFx,
    Progressive,
    Avc420,
    Avc444,
    Avc444v2,
    Other(u16),
}

impl Drawn {
    /// What a draw in `codec`, with this payload, was made with.
    pub fn of(codec: u16, data: &[u8]) -> Self {
        match codec {
            gfx::CODEC_UNCOMPRESSED => Self::Uncompressed,
            gfx::CODEC_PLANAR => Self::Planar,
            gfx::CODEC_ALPHA => Self::Alpha,
            gfx::CODEC_CLEARCODEC if carries_nsc(data) => Self::ClearCodecNsc,
            gfx::CODEC_CLEARCODEC => Self::ClearCodec,
            gfx::CODEC_CAVIDEO => Self::RemoteFx,
            gfx::CODEC_CAPROGRESSIVE | gfx::CODEC_CAPROGRESSIVE_V2 => Self::Progressive,
            gfx::CODEC_AVC420 => Self::Avc420,
            gfx::CODEC_AVC444 => Self::Avc444,
            gfx::CODEC_AVC444_V2 => Self::Avc444v2,
            other => Self::Other(other),
        }
    }

    /// Whether what was drawn this way is the host's picture exactly.
    pub fn exact(self) -> bool {
        matches!(self, Self::Uncompressed | Self::Planar | Self::Alpha | Self::ClearCodec)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Uncompressed => "Uncompressed",
            Self::Planar => "Planar",
            Self::Alpha => "Alpha",
            Self::ClearCodec => "ClearCodec",
            Self::ClearCodecNsc => "ClearCodec with NSCodec",
            Self::RemoteFx => "RemoteFX",
            Self::Progressive => "RemoteFX Progressive",
            Self::Avc420 => "AVC420",
            Self::Avc444 => "AVC444",
            Self::Avc444v2 => "AVC444v2",
            Self::Other(codec) => gfx::codec_name(codec),
        }
    }
}

/// Whether a ClearCodec payload carries an NSCodec subcodec, read off its headers
/// alone ([MS-RDPEGFX] 2.2.4.1 and 2.2.4.3). A payload too short or malformed to
/// say is taken not to: the decoder is what judges it, and refuses it there.
fn carries_nsc(data: &[u8]) -> bool {
    const FLAG_GLYPH_INDEX: u8 = 0x01;
    const NSCODEC: u8 = 1;
    let read = || -> Option<bool> {
        let mut r = Reader::new("a ClearCodec rectangle", data);
        let flags = r.u8().ok()?;
        r.u8().ok()?;
        if flags & FLAG_GLYPH_INDEX != 0 {
            r.u16_le().ok()?;
        }
        if r.rest().len() < 12 {
            return Some(false);
        }
        let residual = r.u32_le().ok()? as usize;
        let bands = r.u32_le().ok()? as usize;
        let subcodec = r.u32_le().ok()? as usize;
        r.skip(residual).ok()?;
        r.skip(bands).ok()?;
        let mut section = Reader::new("a ClearCodec subcodec layer", r.bytes(subcodec).ok()?);
        while !section.is_empty() {
            section.skip(8).ok()?;
            let count = section.u32_le().ok()? as usize;
            if section.u8().ok()? == NSCODEC {
                return Some(true);
            }
            section.skip(count).ok()?;
        }
        Some(false)
    };
    read().unwrap_or(false)
}

/// What a pipeline has drawn with so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fidelity {
    seen: BTreeSet<Drawn>,
}

impl Fidelity {
    /// Count one draw, and say whether it was made with something not seen before.
    pub fn saw(&mut self, codec: u16, data: &[u8]) -> bool {
        self.seen.insert(Drawn::of(codec, data))
    }

    /// Whether everything drawn so far is the host's picture exactly. True of a
    /// pipeline that has drawn nothing, which [`Self::is_empty`] tells apart.
    pub fn exact(&self) -> bool {
        self.seen.iter().all(|drawn| drawn.exact())
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// The codecs drawn with, by name, in a fixed order.
    pub fn names(&self) -> Vec<&'static str> {
        self.seen.iter().map(|drawn| drawn.name()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::wire::Writer;

    /// A ClearCodec payload with only a subcodec layer, one subcodec of each id
    /// given, each over a 1×1 rectangle.
    fn clear(glyph: bool, subcodecs: &[u8]) -> Vec<u8> {
        let mut layer = Writer::new();
        for &id in subcodecs {
            layer.zeros(4);
            layer.u16_le(1);
            layer.u16_le(1);
            layer.u32_le(3);
            layer.u8(id);
            layer.zeros(3);
        }
        let layer = layer.finish();
        let mut w = Writer::new();
        w.u8(if glyph { 0x01 } else { 0 });
        w.u8(0);
        if glyph {
            w.u16_le(7);
        }
        w.u32_le(2);
        w.u32_le(0);
        w.u32_le(layer.len() as u32);
        w.zeros(2);
        w.bytes(&layer);
        w.finish()
    }

    #[test]
    fn what_a_lossless_host_draws_with_is_exact() {
        let mut fidelity = Fidelity::default();
        assert!(fidelity.saw(gfx::CODEC_PLANAR, &[]));
        assert!(fidelity.saw(gfx::CODEC_UNCOMPRESSED, &[]));
        assert!(!fidelity.saw(gfx::CODEC_PLANAR, &[0x10]));
        assert!(fidelity.exact());
        assert_eq!(fidelity.names(), ["Uncompressed", "Planar"]);
    }

    #[test]
    fn what_a_default_host_draws_with_is_not() {
        let mut fidelity = Fidelity::default();
        fidelity.saw(gfx::CODEC_CLEARCODEC, &clear(false, &[2, 1]));
        assert!(!fidelity.exact());
        fidelity.saw(gfx::CODEC_CAPROGRESSIVE, &[]);
        assert_eq!(fidelity.names(), ["ClearCodec with NSCodec", "RemoteFX Progressive"]);
    }

    #[test]
    fn clearcodec_is_exact_only_without_nscodec() {
        assert_eq!(Drawn::of(gfx::CODEC_CLEARCODEC, &clear(false, &[0, 2])), Drawn::ClearCodec);
        assert_eq!(Drawn::of(gfx::CODEC_CLEARCODEC, &clear(true, &[2])), Drawn::ClearCodec);
        assert_eq!(Drawn::of(gfx::CODEC_CLEARCODEC, &clear(true, &[0, 1])), Drawn::ClearCodecNsc);
        // A glyph hit, with no composition header behind it.
        assert_eq!(Drawn::of(gfx::CODEC_CLEARCODEC, &[0x03, 0, 7, 0]), Drawn::ClearCodec);
        assert!(Drawn::ClearCodec.exact());
        assert!(!Drawn::ClearCodecNsc.exact());
    }

    #[test]
    fn every_lossy_codec_and_an_unknown_one_is_not_exact() {
        for codec in [
            gfx::CODEC_CAVIDEO,
            gfx::CODEC_CAPROGRESSIVE,
            gfx::CODEC_CAPROGRESSIVE_V2,
            gfx::CODEC_AVC420,
            gfx::CODEC_AVC444,
            gfx::CODEC_AVC444_V2,
            0x0042,
        ] {
            assert!(!Drawn::of(codec, &[]).exact(), "{}", gfx::codec_name(codec));
        }
    }
}
