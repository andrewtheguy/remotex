//! The cursor, which is a shape and never a part of the desktop.
//!
//! A server sends the pointer separately from the pixels behind it, and this client
//! keeps it that way: a decoded shape goes to the caller as a shape, so a browser can
//! wear it on its own hardware pointer and move it without asking the host. Nothing
//! here draws into the framebuffer, and nothing here needs to know where the cursor
//! is — only what it looks like.
//!
//! # Two masks and three outcomes
//!
//! Windows has drawn cursors by XORing into the screen since it drew them at all, and
//! the wire format still says so. A shape is a pair of masks of the same dimensions:
//! `xorMask`, which is a colour per pixel, and `andMask`, which is one bit per pixel.
//! Together they mean three different things, and a decoder that reads them as
//! "colour plus transparency" gets the third wrong:
//!
//! - `andMask` 0 — the pixel is the colour in `xorMask`, opaque.
//! - `andMask` 1 and the colour is black — the pixel is transparent. On anything but
//!   a 32-bit shape this is the *only* way to be transparent, there being no alpha
//!   channel to say so.
//! - `andMask` 1 and the colour is white — the pixel is the inverse of whatever is
//!   behind it. Nothing can be inverted here, because what is behind it is the
//!   browser's business and arrives later; those pixels become a checkerboard, which
//!   is what a text caret over an unknown background has always looked like when it
//!   could not be inverted.
//!
//! # Bottom up, except when it is not
//!
//! A colour `xorMask` is stored the way every other Windows bitmap is, last row
//! first. A one-bit-per-pixel one is not: it reads top row first, and so does the
//! `andMask` beside it. Every scanline of either mask, at any depth, is padded out to
//! a two-byte boundary.
//!
//! # The cache
//!
//! [`super::capabilities`] tells the server how many shapes this client holds — see
//! [`CACHE_ENTRIES`] — so the server is entitled to send a shape once and afterwards
//! name it by index. It does: a text caret that blinks twice a second is two cached indices rather than
//! two bitmaps a second. [`Cache`] is therefore not an optimisation but part of
//! reading the protocol — an index nothing was stored at is a shape that cannot be
//! drawn, not a shape that arrives later.
//!
//! [MS-RDPBCGR] 2.2.9.1.2.1.6 through 2.2.9.1.2.1.11.

use core::fmt;

use super::fastpath;
use super::wire::{Malformed, Reader};

/// The largest shape RDP allows, and the largest the Large Pointer capability of
/// [`super::capabilities`] claims this client will take.
pub const MAX_DIMENSION: u16 = 384;

/// How many shapes the server may assume are held. This is the number the Pointer
/// capability sends, and the two have to agree: a server told 32 will use index 31.
pub const CACHE_ENTRIES: usize = 32;

/// `xorBpp` values this client can read. One bit is a monochrome shape; 24 is the
/// implicit depth of a Colour Pointer Update; 32 carries its own alpha.
///
/// The four and eight bit depths are indexed, and would need the session's colour
/// palette — which a 32-bit session never sends. Sixteen would need the 5-6-5
/// unpacking that [`super::bitmap`] does not have either, for the same reason.
const MONOCHROME: u16 = 1;
const RGB: u16 = 24;
const RGBA: u16 = 32;

/// A cursor, decoded: straight-alpha `RGBA`, top row first, `width * height * 4`
/// bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Shape {
    pub width: u16,
    pub height: u16,
    /// Where the click actually lands, from the top left of the image.
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub rgba: Vec<u8>,
}

/// Hand-written, because the derived one prints every byte: a 384x384 shape is about
/// 2.3 MB of comma-separated integers, which makes the one log line somebody needed
/// unfindable.
impl fmt::Debug for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Shape {{ {}x{}, hotspot {},{}, {} bytes }}",
            self.width,
            self.height,
            self.hotspot_x,
            self.hotspot_y,
            self.rgba.len()
        )
    }
}

/// What a pointer update asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pointer {
    /// No cursor at all — a full-screen video player, say.
    Hidden,
    /// The system default arrow, which the server does not send a bitmap for.
    Default,
    /// Where the server has put the cursor. A client that moves its own pointer has
    /// no use for this; it arrives when the *host* moves it.
    Position { x: u16, y: u16 },
    /// A bitmap, newly sent or named by cache index.
    Shape(Shape),
}

/// The shapes the server has sent and may name again.
#[derive(Debug)]
pub struct Cache {
    entries: Box<[Option<Shape>]>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

impl Cache {
    pub fn new() -> Self {
        Self { entries: vec![None; CACHE_ENTRIES].into_boxed_slice() }
    }

    /// One pointer update, by the fast-path code it arrived under.
    ///
    /// Every code that carries a shape also stores it, because the server counts on
    /// that: the next update may be nothing but the index.
    pub fn update(&mut self, code: u8, body: &[u8]) -> Result<Pointer, Malformed> {
        match code {
            fastpath::POINTER_HIDDEN => Ok(Pointer::Hidden),
            fastpath::POINTER_DEFAULT => Ok(Pointer::Default),
            fastpath::POINTER_POSITION => position(body),
            fastpath::COLOR_POINTER => {
                self.store(attribute("a Colour Pointer Update", body, Some(RGB), Width::Narrow)?)
            }
            fastpath::NEW_POINTER => {
                self.store(attribute("a New Pointer Update", body, None, Width::Narrow)?)
            }
            fastpath::LARGE_POINTER => {
                self.store(attribute("a Large Pointer Update", body, None, Width::Wide)?)
            }
            fastpath::CACHED_POINTER => self.cached(body),
            _ => Err(Malformed::Refused {
                what: "a fast-path update",
                field: "a pointer update type",
                value: code.into(),
            }),
        }
    }

    fn store(&mut self, (index, shape): (u16, Shape)) -> Result<Pointer, Malformed> {
        let slot = self.entries.get_mut(usize::from(index)).ok_or(Malformed::Refused {
            what: "a pointer update",
            field: "a cache index past the cache this client claimed",
            value: index.into(),
        })?;
        *slot = Some(shape.clone());
        Ok(Pointer::Shape(shape))
    }

    fn cached(&self, body: &[u8]) -> Result<Pointer, Malformed> {
        const WHAT: &str = "a Cached Pointer Update";
        let mut r = Reader::new(WHAT, body);
        let index = r.u16_le()?;
        match self.entries.get(usize::from(index)).and_then(Option::as_ref) {
            Some(shape) => Ok(Pointer::Shape(shape.clone())),
            None => Err(Malformed::Missing { what: WHAT, field: "a shape at the index it names" }),
        }
    }
}

fn position(body: &[u8]) -> Result<Pointer, Malformed> {
    let mut r = Reader::new("a Pointer Position Update", body);
    Ok(Pointer::Position { x: r.u16_le()?, y: r.u16_le()? })
}

/// How wide the two mask lengths are. A Large Pointer Update is the same structure as
/// the other two with room for a mask that a `u16` could not measure.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Width {
    Narrow,
    Wide,
}

/// `TS_COLORPOINTERATTRIBUTE` and the two shapes built on it, which differ only in
/// whether the depth is on the wire and how wide the mask lengths are.
fn attribute(
    what: &'static str,
    body: &[u8],
    depth: Option<u16>,
    width: Width,
) -> Result<(u16, Shape), Malformed> {
    let mut r = Reader::new(what, body);
    let bpp = match depth {
        Some(bpp) => bpp,
        None => r.u16_le()?,
    };
    if !matches!(bpp, MONOCHROME | RGB | RGBA) {
        return Err(r.refuse("a pointer depth", bpp));
    }
    let index = r.u16_le()?;
    let hotspot_x = r.u16_le()?;
    let hotspot_y = r.u16_le()?;
    let shape_width = r.u16_le()?;
    let shape_height = r.u16_le()?;
    if shape_width == 0 || shape_width > MAX_DIMENSION {
        return Err(r.refuse("a pointer width", shape_width));
    }
    if shape_height == 0 || shape_height > MAX_DIMENSION {
        return Err(r.refuse("a pointer height", shape_height));
    }

    // Both masks are declared, and both are checked against the size they must be for
    // the dimensions just read. A mask that is nearly the right length would decode
    // into a shape that is nearly right, sheared by a byte a row.
    let (and_length, xor_length) = match width {
        Width::Narrow => (u32::from(r.u16_le()?), u32::from(r.u16_le()?)),
        Width::Wide => (r.u32_le()?, r.u32_le()?),
    };
    let xor_stride = stride(u32::from(shape_width) * u32::from(bpp));
    let and_stride = stride(u32::from(shape_width));
    if xor_length != xor_stride * u32::from(shape_height) {
        return Err(r.refuse("an xorMask that is not the length its own size asks for", xor_length));
    }
    // An absent andMask is a shape that is opaque wherever its colours are, which is
    // how a 32-bit shape that says everything in its alpha channel arrives.
    if and_length != 0 && and_length != and_stride * u32::from(shape_height) {
        return Err(r.refuse("an andMask that is not the length its own size asks for", and_length));
    }

    // On the wire the masks are the other way round from their lengths.
    let xor = r.bytes(xor_length as usize)?;
    let and = r.bytes(and_length as usize)?;

    let rgba = pixels(bpp, shape_width, shape_height, xor, and);
    Ok((index, Shape { width: shape_width, height: shape_height, hotspot_x, hotspot_y, rgba }))
}

/// The length of one scanline, which is padded out to a two-byte boundary whatever
/// the depth.
fn stride(bits: u32) -> u32 {
    bits.div_ceil(16) * 2
}

/// The two masks, become straight-alpha `RGBA` with the top row first.
///
/// Every length was checked by [`attribute`], so there is nothing here that can fail:
/// what is left is the row order, the three-way rule, and the depths.
fn pixels(bpp: u16, width: u16, height: u16, xor: &[u8], and: &[u8]) -> Vec<u8> {
    let xor_stride = stride(u32::from(width) * u32::from(bpp)) as usize;
    let and_stride = if and.is_empty() { 0 } else { stride(u32::from(width)) as usize };
    let mut rgba = Vec::with_capacity(usize::from(width) * usize::from(height) * 4);
    for row in 0..height {
        // A colour mask is stored last row first; a monochrome one, and the andMask
        // that goes with it, are not.
        let source = usize::from(if bpp == MONOCHROME { row } else { height - 1 - row });
        let xor_row = &xor[source * xor_stride..][..xor_stride];
        let and_row = &and[source * and_stride..][..and_stride];
        for column in 0..width {
            let colour = colour(bpp, xor_row, column);
            let pixel = match (bit(and_row, column), colour) {
                (1, [0x00, 0x00, 0x00, 0xFF]) => [0x00; 4],
                (1, [0xFF, 0xFF, 0xFF, 0xFF]) => invert(row, column),
                _ => colour,
            };
            rgba.extend_from_slice(&pixel);
        }
    }
    rgba
}

/// One pixel of an `xorMask` scanline, as `RGBA`. The wire order is blue first.
fn colour(bpp: u16, row: &[u8], at: u16) -> [u8; 4] {
    if bpp == MONOCHROME {
        return if bit(row, at) == 1 { [0xFF; 4] } else { [0x00, 0x00, 0x00, 0xFF] };
    }
    let at = usize::from(at);
    if bpp == RGB {
        [row[at * 3 + 2], row[at * 3 + 1], row[at * 3], 0xFF]
    } else {
        [row[at * 4 + 2], row[at * 4 + 1], row[at * 4], row[at * 4 + 3]]
    }
}

/// One bit of a scanline, counting from the top bit of the first byte. A row that is
/// not there — an absent `andMask` — is all zeroes, which is opaque.
fn bit(row: &[u8], at: u16) -> u8 {
    match row.get(usize::from(at) / 8) {
        Some(byte) => (byte >> (7 - at % 8)) & 1,
        None => 0,
    }
}

/// A pixel the server asked to have inverted, which cannot be: what it would be
/// inverted against is not here and will not be until a browser composites it.
///
/// A checkerboard is what the alternative was always going to look like. Painting
/// them black loses a caret over black text, and painting them white loses it over a
/// white page; alternating loses neither.
fn invert(row: u16, column: u16) -> [u8; 4] {
    if (row + column).is_multiple_of(2) { [0xFF; 4] } else { [0x00, 0x00, 0x00, 0xFF] }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A New Pointer Update: the depth, the cache index, the hotspot, the size, the
    /// two lengths, and then the masks the other way round.
    fn new_pointer(bpp: u16, index: u16, size: (u16, u16), xor: &[u8], and: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        for field in [bpp, index, 1, 2, size.0, size.1] {
            body.extend_from_slice(&field.to_le_bytes());
        }
        body.extend_from_slice(&u16::try_from(and.len()).unwrap().to_le_bytes());
        body.extend_from_slice(&u16::try_from(xor.len()).unwrap().to_le_bytes());
        body.extend_from_slice(xor);
        body.extend_from_slice(and);
        body
    }

    fn shape(update: &[u8]) -> Shape {
        let Pointer::Shape(shape) = Cache::new().update(fastpath::NEW_POINTER, update).unwrap()
        else {
            panic!("a new pointer is a shape");
        };
        shape
    }

    /// Two rows of two 32-bit pixels, bottom row first, and an all-opaque andMask.
    fn two_by_two() -> Vec<u8> {
        let xor = [
            0x10, 0x20, 0x30, 0xFF, 0x11, 0x21, 0x31, 0xFF, // the bottom row
            0x40, 0x50, 0x60, 0xFF, 0x41, 0x51, 0x61, 0xFF, // the top row
        ];
        new_pointer(RGBA, 0, (2, 2), &xor, &[0x00; 4])
    }

    #[test]
    fn a_shape_arrives_bottom_row_first_and_blue_first() {
        let shape = shape(&two_by_two());
        assert_eq!((shape.width, shape.height), (2, 2));
        assert_eq!((shape.hotspot_x, shape.hotspot_y), (1, 2));
        assert_eq!(
            shape.rgba,
            vec![
                0x60, 0x50, 0x40, 0xFF, 0x61, 0x51, 0x41, 0xFF, // the top row, RGBA
                0x30, 0x20, 0x10, 0xFF, 0x31, 0x21, 0x11, 0xFF,
            ]
        );
    }

    #[test]
    fn a_thirty_two_bit_shape_keeps_the_alpha_it_carries() {
        let xor = [0x10, 0x20, 0x30, 0x80];
        let shape = shape(&new_pointer(RGBA, 0, (1, 1), &xor, &[0x00, 0x00]));
        assert_eq!(shape.rgba, vec![0x30, 0x20, 0x10, 0x80]);
    }

    /// The three-way rule: the andMask bit does not mean "transparent", it means
    /// "not the colour you see" — and which of the two it is depends on the colour.
    #[test]
    fn an_and_bit_over_black_is_transparent_and_over_white_is_a_checkerboard() {
        // Four pixels in a row: opaque red, transparent, inverted, inverted.
        let xor = [
            0x00, 0x00, 0xFF, 0xFF, // red
            0x00, 0x00, 0x00, 0xFF, // black, under an and bit
            0xFF, 0xFF, 0xFF, 0xFF, // white, under an and bit
            0xFF, 0xFF, 0xFF, 0xFF, // white, under an and bit
        ];
        let shape = shape(&new_pointer(RGBA, 0, (4, 1), &xor, &[0b0111_0000, 0x00]));
        assert_eq!(
            shape.rgba,
            vec![
                0xFF, 0x00, 0x00, 0xFF, // the colour, opaque
                0x00, 0x00, 0x00, 0x00, // gone
                0xFF, 0xFF, 0xFF, 0xFF, // column 2 of row 0: the light square
                0x00, 0x00, 0x00, 0xFF, // column 3: the dark one
            ]
        );
    }

    /// A Colour Pointer Update has no depth field: it is always 24-bit. Three pixels
    /// of it are nine bytes, and a scanline is padded to ten.
    #[test]
    fn a_colour_pointer_is_twenty_four_bit_and_its_rows_are_padded_to_a_word() {
        let xor = [
            0x10, 0x20, 0x30, 0x11, 0x21, 0x31, 0x12, 0x22, 0x32, //
            0xEE, // the padding, which is not a pixel
        ];
        let mut body = new_pointer(RGB, 0, (3, 1), &xor, &[0x00, 0x00]);
        body.drain(..2); // the depth, which this update does not carry
        let Pointer::Shape(shape) = Cache::new().update(fastpath::COLOR_POINTER, &body).unwrap()
        else {
            panic!("a colour pointer is a shape");
        };
        assert_eq!(
            shape.rgba,
            vec![0x30, 0x20, 0x10, 0xFF, 0x31, 0x21, 0x11, 0xFF, 0x32, 0x22, 0x12, 0xFF]
        );
    }

    /// The one shape whose rows are the right way up already.
    #[test]
    fn a_monochrome_shape_reads_top_row_first() {
        // Row 0 is one white pixel then seven black; row 1 is the reverse.
        let xor = [0b1000_0000, 0x00, 0b0111_1111, 0x00];
        let shape = shape(&new_pointer(MONOCHROME, 0, (8, 2), &xor, &[0x00; 4]));
        assert_eq!(&shape.rgba[..8], &[0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xFF]);
        assert_eq!(&shape.rgba[32..40], &[0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    /// A 32-bit shape may say everything in its alpha and send no andMask at all.
    #[test]
    fn an_absent_and_mask_is_opaque_everywhere() {
        let xor = [0x10, 0x20, 0x30, 0xFF];
        let shape = shape(&new_pointer(RGBA, 0, (1, 1), &xor, &[]));
        assert_eq!(shape.rgba, vec![0x30, 0x20, 0x10, 0xFF]);
    }

    /// A Large Pointer Update is the same fields with room for a bigger mask.
    #[test]
    fn a_large_pointer_measures_its_masks_in_four_bytes() {
        let mut body = Vec::new();
        for field in [RGBA, 7_u16, 1, 2, 1, 1] {
            body.extend_from_slice(&field.to_le_bytes());
        }
        body.extend_from_slice(&2_u32.to_le_bytes());
        body.extend_from_slice(&4_u32.to_le_bytes());
        body.extend_from_slice(&[0x10, 0x20, 0x30, 0xFF]);
        body.extend_from_slice(&[0x00, 0x00]);
        let Pointer::Shape(shape) = Cache::new().update(fastpath::LARGE_POINTER, &body).unwrap()
        else {
            panic!("a large pointer is a shape");
        };
        assert_eq!(shape.rgba, vec![0x30, 0x20, 0x10, 0xFF]);
    }

    #[test]
    fn the_shapes_without_a_bitmap_are_read_from_the_code_alone() {
        let mut cache = Cache::new();
        assert_eq!(cache.update(fastpath::POINTER_HIDDEN, &[]).unwrap(), Pointer::Hidden);
        assert_eq!(cache.update(fastpath::POINTER_DEFAULT, &[]).unwrap(), Pointer::Default);
        assert_eq!(
            cache.update(fastpath::POINTER_POSITION, &[0x01, 0x00, 0x02, 0x00]).unwrap(),
            Pointer::Position { x: 1, y: 2 }
        );
    }

    #[test]
    fn a_shape_is_kept_and_handed_back_by_its_index() {
        let mut cache = Cache::new();
        let sent = cache.update(fastpath::NEW_POINTER, &new_pointer(RGBA, 5, (1, 1), &[0x10, 0x20, 0x30, 0xFF], &[0x00, 0x00])).unwrap();
        let cached = cache.update(fastpath::CACHED_POINTER, &5_u16.to_le_bytes()).unwrap();
        assert_eq!(cached, sent);
    }

    #[test]
    fn an_index_nothing_was_stored_at_is_not_a_shape() {
        let error = Cache::new().update(fastpath::CACHED_POINTER, &5_u16.to_le_bytes()).unwrap_err();
        assert_eq!(
            error,
            Malformed::Missing {
                what: "a Cached Pointer Update",
                field: "a shape at the index it names"
            }
        );
    }

    #[test]
    fn an_index_past_the_cache_this_client_claimed_is_refused() {
        let past = u16::try_from(CACHE_ENTRIES).unwrap();
        let update = new_pointer(RGBA, past, (1, 1), &[0x10, 0x20, 0x30, 0xFF], &[0x00, 0x00]);
        let error = Cache::new().update(fastpath::NEW_POINTER, &update).unwrap_err();
        assert!(matches!(error, Malformed::Refused { value, .. } if value == u64::from(past)));
    }

    #[test]
    fn a_depth_this_client_cannot_unpack_is_refused_by_name() {
        let update = new_pointer(16, 0, (1, 1), &[0x10, 0x20], &[0x00, 0x00]);
        let error = Cache::new().update(fastpath::NEW_POINTER, &update).unwrap_err();
        assert_eq!(
            error,
            Malformed::Refused {
                what: "a New Pointer Update",
                field: "a pointer depth",
                value: 16
            }
        );
    }

    /// A mask that is nearly the right length decodes into a shape that is nearly
    /// right, sheared by a byte a row — so it is refused rather than decoded.
    #[test]
    fn a_mask_that_is_not_the_length_its_size_asks_for_is_refused() {
        let short = new_pointer(RGBA, 0, (2, 2), &[0x00; 12], &[0x00; 4]);
        assert!(Cache::new().update(fastpath::NEW_POINTER, &short).is_err());
        let and = new_pointer(RGBA, 0, (2, 2), &[0x00; 16], &[0x00; 3]);
        assert!(Cache::new().update(fastpath::NEW_POINTER, &and).is_err());
    }

    #[test]
    fn a_shape_bigger_than_rdp_allows_is_refused_before_it_is_measured() {
        let update = new_pointer(RGBA, 0, (MAX_DIMENSION + 1, 1), &[], &[]);
        let error = Cache::new().update(fastpath::NEW_POINTER, &update).unwrap_err();
        assert_eq!(
            error,
            Malformed::Refused {
                what: "a New Pointer Update",
                field: "a pointer width",
                value: u64::from(MAX_DIMENSION) + 1
            }
        );
    }

    /// A 384x384 shape must not print 590 KB of integers into a log line.
    #[test]
    fn debug_prints_the_size_not_the_pixels() {
        let text = format!("{:?}", shape(&two_by_two()));
        assert_eq!(text, "Shape { 2x2, hotspot 1,2, 16 bytes }");
    }
}
