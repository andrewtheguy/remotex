//! The graphics pipeline's H.264 payloads: what wraps a bitstream on the wire.
//!
//! A `WireToSurface1` in the AVC420 codec carries an `RFX_AVC420_BITMAP_STREAM`: a
//! metablock naming the rectangles of the surface the picture is to be shown in,
//! each with the quantization the host encoded it at, and then one H.264 access
//! unit in Annex B byte-stream form, whose picture is the whole surface rounded up
//! to macroblocks. The rectangles are the region mask — the picture outside them
//! is whatever the encoder left there and is not shown.
//!
//! The AVC444 codecs wrap two of those, a *luma* subframe and a *chroma* subframe,
//! behind one word saying which are present and how long the first is; both go
//! through one H.264 decoder as one stream. What the two views hold and how they
//! are put back together is [`super::super::avc`]'s business, not the wire's.
//!
//! [MS-RDPEGFX] 2.2.4.4, 2.2.4.5 and 2.2.4.6.
//!
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0

use super::gfx::{Rect16, rect16};
use super::wire::{Malformed, Reader};

const WHAT: &str = "an AVC420 bitmap stream";
const WHAT_444: &str = "an AVC444 bitmap stream";

/// `RFX_AVC420_METABLOCK` holds an `RDPGFX_RECT16` and an `RDPGFX_AVC420_QUANT_QUALITY`
/// per rectangle.
const REGION_BYTES: u64 = 8 + 2;

/// One rectangle of the region mask and what the host says about it.
///
/// The quantization is informational — [MS-RDPEGFX] 2.2.4.4.1 says the decoder is
/// not to use it, and the H.264 stream carries its own — so it is read and kept for
/// the log and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    /// In surface coordinates, exclusive right and bottom.
    pub rect: Rect16,
    /// The quantization parameter, six bits.
    pub qp: u8,
    /// Whether the rectangle was progressively encoded.
    pub progressive: bool,
    /// The quality level, 0 to 100.
    pub quality: u8,
}

/// `RFX_AVC420_BITMAP_STREAM`: the region mask and the access unit it masks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Avc420<'a> {
    pub regions: Vec<Region>,
    /// One access unit, Annex B.
    pub bitstream: &'a [u8],
}

/// `RFX_AVC444_BITMAP_STREAM` and `RFX_AVC444V2_BITMAP_STREAM`, which are the same
/// on the wire: which of the two views this PDU carries. At least one is present;
/// the `LC` value that would have neither is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Avc444<'a> {
    /// The YUV420 view: the picture's luma and half its chroma.
    pub luma: Option<Avc420<'a>>,
    /// The Chroma420 view: the other half of the chroma, packed into a YUV420
    /// picture of its own.
    pub chroma: Option<Avc420<'a>>,
}

/// Read an `RFX_AVC420_BITMAP_STREAM`.
pub fn avc420(data: &[u8]) -> Result<Avc420<'_>, Malformed> {
    let mut r = Reader::new(WHAT, data);
    let count = r.u32_le()?;
    // Each region is ten bytes of metablock, so a count the block has no bytes for
    // is refused before anything is allocated for it.
    if u64::from(count) * REGION_BYTES > r.rest().len() as u64 {
        return Err(r.refuse("more region rectangles than the block has bytes for,", count));
    }
    let rects: Vec<Rect16> = (0..count).map(|_| rect16(&mut r)).collect::<Result<_, _>>()?;
    let regions = rects
        .into_iter()
        .map(|rect| {
            let qp = r.u8()?;
            let quality = r.u8()?;
            Ok(Region { rect, qp: qp & 0x3F, progressive: qp & 0x80 != 0, quality })
        })
        .collect::<Result<_, Malformed>>()?;
    Ok(Avc420 { regions, bitstream: r.rest() })
}

/// Read an `RFX_AVC444_BITMAP_STREAM` or `RFX_AVC444V2_BITMAP_STREAM`.
///
/// `cbAvc420EncodedBitstream1` sizes the first stream only when both are present;
/// with one view the rest of the PDU is that view, whatever the count says, since
/// the specification has the count zero for a lone chroma view and says nothing
/// about a lone luma one.
pub fn avc444(data: &[u8]) -> Result<Avc444<'_>, Malformed> {
    let mut r = Reader::new(WHAT_444, data);
    let info = r.u32_le()?;
    let first = usize::try_from(info & 0x3FFF_FFFF).unwrap_or(usize::MAX);
    match info >> 30 {
        0 => {
            let luma = avc420(r.bytes(first)?)?;
            let chroma = avc420(r.rest())?;
            Ok(Avc444 { luma: Some(luma), chroma: Some(chroma) })
        }
        1 => Ok(Avc444 { luma: Some(avc420(r.rest())?), chroma: None }),
        2 => Ok(Avc444 { luma: None, chroma: Some(avc420(r.rest())?) }),
        lc => Err(r.refuse("LC", lc)),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rdp_client::proto::wire::Writer;

    /// A rectangle, its quantization byte and its quality.
    type Spec = ((u16, u16, u16, u16), u8, u8);

    fn metablock(regions: &[Spec]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32_le(u32::try_from(regions.len()).unwrap());
        for (rect, _, _) in regions {
            w.u16_le(rect.0);
            w.u16_le(rect.1);
            w.u16_le(rect.2);
            w.u16_le(rect.3);
        }
        for (_, qp, quality) in regions {
            w.u8(*qp);
            w.u8(*quality);
        }
        w.finish()
    }

    /// The bytes of a stream with both views, or one, as [MS-RDPEGFX] 2.2.4.5 lays
    /// them out. Crate-visible so the compositor's tests can wrap an encoded pair.
    pub(crate) fn wrap444(luma: Option<&[u8]>, chroma: Option<&[u8]>) -> Vec<u8> {
        let mut w = Writer::new();
        let (lc, first) = match (luma, chroma) {
            (Some(luma), Some(_)) => (0u32, luma.len() as u32),
            (Some(luma), None) => (1, luma.len() as u32),
            (None, Some(_)) => (2, 0),
            (None, None) => panic!("a stream with neither view"),
        };
        w.u32_le(lc << 30 | first);
        if let Some(luma) = luma {
            w.bytes(luma);
        }
        if let Some(chroma) = chroma {
            w.bytes(chroma);
        }
        w.finish()
    }

    #[test]
    fn a_metablock_reads_its_rectangles_and_quantization_then_the_bitstream_is_the_rest() {
        let mut data = metablock(&[((0, 0, 64, 48), 0x80 | 22, 90), ((64, 0, 128, 16), 30, 100)]);
        data.extend_from_slice(&[0, 0, 0, 1, 0x65, 0xAA]);
        let stream = avc420(&data).unwrap();
        assert_eq!(stream.regions, vec![
            Region { rect: Rect16 { left: 0, top: 0, right: 64, bottom: 48 }, qp: 22, progressive: true, quality: 90 },
            Region { rect: Rect16 { left: 64, top: 0, right: 128, bottom: 16 }, qp: 30, progressive: false, quality: 100 },
        ]);
        assert_eq!(stream.bitstream, &[0, 0, 0, 1, 0x65, 0xAA]);
        // No regions is a legal, empty mask.
        let stream = avc420(&[0, 0, 0, 0, 7]).unwrap();
        assert!(stream.regions.is_empty());
        assert_eq!(stream.bitstream, &[7]);
    }

    #[test]
    fn a_metablock_that_lies_about_its_length_is_refused() {
        // Three regions announced, bytes for none.
        assert!(matches!(avc420(&[3, 0, 0, 0, 1, 2]), Err(Malformed::Refused { .. })));
        // A rectangle with no width.
        let data = metablock(&[((4, 4, 4, 8), 0, 0)]);
        assert!(matches!(avc420(&data), Err(Malformed::Refused { .. })));
        // Cut off inside the quantization values: the count's bytes are checked
        // before anything is read, so this is a refusal, not a short read.
        let mut data = metablock(&[((0, 0, 1, 1), 0, 0)]);
        data.pop();
        assert!(matches!(avc420(&data), Err(Malformed::Refused { .. })));
        // Cut off inside the count itself.
        assert!(matches!(avc420(&[1, 0]), Err(Malformed::Short { .. })));
    }

    #[test]
    fn a_444_stream_splits_into_the_views_lc_names() {
        let mut luma = metablock(&[((0, 0, 16, 16), 20, 80)]);
        luma.extend_from_slice(&[1, 2, 3]);
        let mut chroma = metablock(&[((0, 0, 16, 16), 24, 80)]);
        chroma.extend_from_slice(&[4, 5]);

        let wrapped = wrap444(Some(&luma), Some(&chroma));
        let both = avc444(&wrapped).unwrap();
        assert_eq!(both.luma.as_ref().unwrap().bitstream, &[1, 2, 3]);
        assert_eq!(both.chroma.as_ref().unwrap().bitstream, &[4, 5]);
        assert_eq!(both.chroma.unwrap().regions[0].qp, 24);

        let wrapped = wrap444(Some(&luma), None);
        let alone = avc444(&wrapped).unwrap();
        assert_eq!(alone.luma.unwrap().bitstream, &[1, 2, 3]);
        assert!(alone.chroma.is_none());

        let wrapped = wrap444(None, Some(&chroma));
        let alone = avc444(&wrapped).unwrap();
        assert!(alone.luma.is_none());
        assert_eq!(alone.chroma.unwrap().bitstream, &[4, 5]);
    }

    #[test]
    fn a_444_stream_with_a_bad_lc_or_a_first_view_past_the_end_is_refused() {
        let err = avc444(&[0, 0, 0, 0xC0, 0, 0, 0, 0]).unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "LC", value: 3, .. }), "{err}");
        // LC 0 with a first-stream length past the PDU.
        let err = avc444(&[9, 0, 0, 0, 0, 0, 0, 0]).unwrap_err();
        assert!(matches!(err, Malformed::Short { .. }), "{err}");
        // Only the header.
        assert!(matches!(avc444(&[0, 0, 0]), Err(Malformed::Short { .. })));
    }
}
