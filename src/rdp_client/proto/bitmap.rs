//! Rectangles of pixels, which is all this client lets a server draw with.
//!
//! The Confirm Active of [`super::capabilities`] claims no orders and no caches, so
//! every change to the desktop arrives as a Bitmap Update: a count, and then that
//! many rectangles, each carrying where it goes and the pixels that go there.
//!
//! # One colour depth
//!
//! The session is 32 bits per pixel. That is asked for in the GCC conference, asked
//! for again in the Confirm Active, and declared by the server in its Demand Active —
//! where [`super::capabilities`] refuses anything else, so that a depth this decoder
//! cannot read is a sentence during the capability exchange rather than a picture that
//! comes out wrong. At 32 bits a compressed rectangle is the planar codec of
//! [`super::planar`], and there is no second codec to choose between: the interleaved
//! run-length coding that a 16-bit session would use cannot arrive.
//!
//! # Bottom up, and wider than it looks
//!
//! Two details produce a picture that is *nearly* right when they are missed, which is
//! the worst kind of wrong:
//!
//! - **The rows arrive bottom first.** Every bitmap in RDP is stored the way a Windows
//!   DIB is, last row first, and a decoder that ignores it paints the desktop upside
//!   down.
//! - **The bitmap can be wider than the rectangle it belongs in.** `width` is the
//!   decoded image; the destination rectangle is what belongs on the desktop, and a
//!   server rounds the first up past the second. The extra columns are on the right,
//!   and they are not part of the picture.
//!
//! [`Bitmap::decode`] answers both: what it writes is the destination rectangle, top
//! row first, and nothing else.
//!
//! [MS-RDPBCGR] 2.2.9.1.1.3.1.2.

use super::planar;
use super::wire::{Malformed, Reader};

/// `updateType`: `UPDATETYPE_BITMAP`. The field is there because the slow path uses
/// one PDU for every kind of update; fast-path has already said which this is, and it
/// says so again here.
const UPDATE_BITMAP: u16 = 0x0001;

/// `BITMAP_COMPRESSION`.
const COMPRESSED: u16 = 0x0001;

/// `NO_BITMAP_COMPRESSION_HDR`: the eight bytes of `bitmapComprHdr` are not there.
/// The General capability asked for that — the header says the length a second time
/// and nothing reads it.
const NO_COMPRESSION_HEADER: u16 = 0x0400;

/// The `bitmapComprHdr` a server sends when it was not asked to leave it out.
const COMPRESSION_HEADER: u16 = 8;

/// The only colour depth a rectangle may arrive in. See the module docs.
pub const DEPTH: u16 = 32;

/// Bytes per pixel, on the wire and in the answer alike: `BGRX` in, `RGBX` out.
const PIXEL: usize = 4;

/// The working space one session decodes in, kept across updates so that a desktop
/// changing thirty times a second allocates for none of it.
#[derive(Debug, Default)]
pub struct Scratch {
    /// The colour planes of a compressed rectangle, one after another.
    planes: Vec<u8>,
    /// The bitmap as the wire lays it out: bottom row first, blue first.
    pixels: Vec<u8>,
}

/// One rectangle of the desktop, as it arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bitmap<'a> {
    /// Where the rectangle's top left goes, in desktop pixels.
    pub x: u16,
    pub y: u16,
    /// The size of the destination rectangle — what belongs on the desktop, and what
    /// [`Bitmap::decode`] writes.
    pub paint_width: u16,
    pub paint_height: u16,
    /// The size of the bitmap itself: the shape its pixels decode to, which is never
    /// smaller than the rectangle above.
    pub width: u16,
    pub height: u16,
    pub compressed: bool,
    pub data: &'a [u8],
}

/// The rectangles of one Bitmap Update, in the order they are to be painted.
pub fn update(body: &[u8]) -> Result<Vec<Bitmap<'_>>, Malformed> {
    const WHAT: &str = "an RDP Bitmap Update";

    let mut r = Reader::new(WHAT, body);
    let kind = r.u16_le()?;
    if kind != UPDATE_BITMAP {
        return Err(r.refuse("its update type", kind));
    }
    let count = r.u16_le()?;
    let mut rectangles = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        rectangles.push(rectangle(&mut r)?);
    }
    Ok(rectangles)
}

fn rectangle<'a>(r: &mut Reader<'a>) -> Result<Bitmap<'a>, Malformed> {
    let x = r.u16_le()?;
    let y = r.u16_le()?;
    // The far edges are inclusive: a one-pixel rectangle has its right edge equal to
    // its left one.
    let right = r.u16_le()?;
    let bottom = r.u16_le()?;
    let width = r.u16_le()?;
    let height = r.u16_le()?;
    let depth = r.u16_le()?;
    let flags = r.u16_le()?;
    let mut length = r.u16_le()?;

    if depth != DEPTH {
        return Err(r.refuse("a colour depth", depth));
    }
    let compressed = flags & COMPRESSED != 0;
    if compressed && flags & NO_COMPRESSION_HEADER == 0 {
        // The header repeats the length and the row count. Stepping over it is what
        // makes the two shapes of this field one shape to everything below.
        r.skip(usize::from(COMPRESSION_HEADER))?;
        length = length
            .checked_sub(COMPRESSION_HEADER)
            .ok_or_else(|| r.refuse("a compressed length", length))?;
    }
    let data = r.bytes(usize::from(length))?;

    let paint = right.checked_sub(x).zip(bottom.checked_sub(y));
    let Some((paint_width, paint_height)) = paint.map(|(w, h)| (w + 1, h + 1)) else {
        return Err(r.refuse("a rectangle that ends before it begins", right));
    };
    if paint_width > width || paint_height > height {
        return Err(r.refuse("a rectangle larger than the bitmap in it", paint_width));
    }
    Ok(Bitmap { x, y, paint_width, paint_height, width, height, compressed, data })
}

impl Bitmap<'_> {
    /// How many bytes [`Bitmap::decode`] writes.
    pub fn painted_bytes(&self) -> usize {
        usize::from(self.paint_width) * usize::from(self.paint_height) * PIXEL
    }

    /// The destination rectangle's pixels, in `RGBX32`, top row first.
    ///
    /// `scratch` is working space kept across calls — see [`Scratch`].
    pub fn decode(&self, scratch: &mut Scratch, out: &mut Vec<u8>) -> Result<(), Malformed> {
        if self.compressed {
            let Scratch { planes, pixels } = scratch;
            planar::decompress(self.data, planes, pixels, self.width, self.height)?;
            self.turn_over(&pixels[..], out)
        } else {
            // Uncompressed rows are padded out to a multiple of four bytes, which at
            // four bytes a pixel they already are.
            self.turn_over(self.data, out)
        }
    }

    /// Take the destination rectangle out of the decoded bitmap, turning it the right
    /// way up and putting the channels in the order the framebuffer holds them.
    fn turn_over(&self, pixels: &[u8], out: &mut Vec<u8>) -> Result<(), Malformed> {
        const WHAT: &str = "a decoded RDP bitmap";

        let stride = usize::from(self.width) * PIXEL;
        let need = stride * usize::from(self.height);
        if pixels.len() < need {
            return Err(Malformed::Short { what: WHAT, len: pixels.len(), at: pixels.len(), need });
        }
        out.clear();
        out.reserve(self.painted_bytes());
        let rows = usize::from(self.height) - usize::from(self.paint_height);
        for row in (rows..usize::from(self.height)).rev() {
            let from = row * stride;
            let (row, _) = pixels[from..from + stride].as_chunks::<PIXEL>();
            for pixel in row.iter().take(usize::from(self.paint_width)) {
                out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0]);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `TS_BITMAP_DATA`, as a server writes it.
    struct Rectangle<'a> {
        x: u16,
        y: u16,
        paint: (u16, u16),
        size: (u16, u16),
        flags: u16,
        data: &'a [u8],
    }

    impl Rectangle<'_> {
        fn encode(&self) -> Vec<u8> {
            let mut w = super::super::wire::Writer::new();
            w.u16_le(self.x);
            w.u16_le(self.y);
            // The far edges are inclusive.
            w.u16_le(self.x + self.paint.0 - 1);
            w.u16_le(self.y + self.paint.1 - 1);
            w.u16_le(self.size.0);
            w.u16_le(self.size.1);
            w.u16_le(DEPTH);
            w.u16_le(self.flags);
            w.u16_le(u16::try_from(self.data.len()).unwrap());
            w.bytes(self.data);
            w.finish()
        }
    }

    fn pdu(rectangles: &[Rectangle<'_>]) -> Vec<u8> {
        let mut body = UPDATE_BITMAP.to_le_bytes().to_vec();
        body.extend_from_slice(&u16::try_from(rectangles.len()).unwrap().to_le_bytes());
        for rectangle in rectangles {
            body.extend(rectangle.encode());
        }
        body
    }

    fn plain(x: u16, y: u16, paint: (u16, u16), size: (u16, u16), data: &[u8]) -> Rectangle<'_> {
        Rectangle { x, y, paint, size, flags: 0, data }
    }

    #[test]
    fn an_update_says_where_each_of_its_rectangles_goes() {
        let body = pdu(&[
            plain(0, 0, (1, 1), (1, 1), &[0; 4]),
            plain(64, 32, (2, 1), (2, 1), &[0; 8]),
        ]);
        let rectangles = update(&body).expect("a Bitmap Update");
        assert_eq!(rectangles.len(), 2);
        assert_eq!((rectangles[1].x, rectangles[1].y), (64, 32));
        assert_eq!((rectangles[1].paint_width, rectangles[1].paint_height), (2, 1));
    }

    /// Both of the details that produce a picture that is nearly right: the rows
    /// arrive bottom first, and the channels arrive blue first.
    #[test]
    fn a_rectangle_comes_out_the_right_way_up_with_red_first() {
        let data = [
            10, 20, 30, 0, 11, 21, 31, 0, // the bottom row of the picture
            40, 50, 60, 0, 41, 51, 61, 0, // and the top one
        ];
        let body = pdu(&[plain(0, 0, (2, 2), (2, 2), &data)]);
        let rectangle = update(&body).unwrap()[0];

        let (mut scratch, mut out) = (Scratch::default(), Vec::new());
        rectangle.decode(&mut scratch, &mut out).expect("an uncompressed rectangle");
        assert_eq!(out, vec![
            60, 50, 40, 0, 61, 51, 41, 0, //
            30, 20, 10, 0, 31, 21, 11, 0,
        ]);
        assert_eq!(out.len(), rectangle.painted_bytes());
    }

    /// A server rounds the bitmap's width up past the rectangle it is painting, and
    /// the columns that buys are not part of the picture.
    #[test]
    fn a_bitmap_wider_than_its_rectangle_keeps_only_the_rectangle() {
        let mut data = Vec::new();
        for row in 0..2_u8 {
            for column in 0..4_u8 {
                data.extend_from_slice(&[column, row, 0, 0]);
            }
        }
        let body = pdu(&[plain(0, 0, (2, 1), (4, 2), &data)]);
        let rectangle = update(&body).unwrap()[0];

        let (mut scratch, mut out) = (Scratch::default(), Vec::new());
        rectangle.decode(&mut scratch, &mut out).expect("an uncompressed rectangle");
        // The top row — which arrived last — and only its first two pixels.
        assert_eq!(out, vec![0, 1, 0, 0, 0, 1, 1, 0]);
    }

    /// The header says the length a second time, and a server that was asked to leave
    /// it out may send it anyway. Both shapes have to reach the decoder as one.
    #[test]
    fn a_compression_header_is_stepped_over_when_a_server_sends_one() {
        let mut data = vec![0xFF; usize::from(COMPRESSION_HEADER)];
        // A planar stream of one flat 2x2 plane per colour, written out plainly.
        data.extend_from_slice(&[0x20, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 0]);
        let rectangle = Rectangle { x: 0, y: 0, paint: (2, 2), size: (2, 2), flags: COMPRESSED, data: &data };
        let body = pdu(&[rectangle]);
        let rectangle = update(&body).unwrap()[0];
        assert!(rectangle.compressed);

        let (mut scratch, mut out) = (Scratch::default(), Vec::new());
        rectangle.decode(&mut scratch, &mut out).expect("a compressed rectangle");
        assert_eq!(out, [1, 2, 3, 0].repeat(4));

        // And the same stream with the header left out, as this client asked for.
        let rectangle = Rectangle {
            x: 0,
            y: 0,
            paint: (2, 2),
            size: (2, 2),
            flags: COMPRESSED | NO_COMPRESSION_HEADER,
            data: &data[usize::from(COMPRESSION_HEADER)..],
        };
        let body = pdu(&[rectangle]);
        let rectangle = update(&body).unwrap()[0];
        let mut again = Vec::new();
        rectangle.decode(&mut scratch, &mut again).expect("a compressed rectangle");
        assert_eq!(again, out);
    }

    #[test]
    fn a_depth_the_session_is_not_is_refused_rather_than_guessed_at() {
        let mut body = pdu(&[plain(0, 0, (1, 1), (1, 1), &[0; 4])]);
        // The colour depth, which is the seventh field of the rectangle.
        let at = 4 + 12;
        body[at..at + 2].copy_from_slice(&16_u16.to_le_bytes());
        assert_eq!(
            update(&body).unwrap_err().to_string(),
            "an RDP Bitmap Update carries a colour depth 0x10, which this client does not accept"
        );
    }

    #[test]
    fn a_rectangle_larger_than_the_bitmap_in_it_is_refused() {
        let body = pdu(&[plain(0, 0, (4, 1), (2, 1), &[0; 8])]);
        assert_eq!(
            update(&body).unwrap_err().to_string(),
            "an RDP Bitmap Update carries a rectangle larger than the bitmap in it 0x4, which \
             this client does not accept"
        );
    }

    #[test]
    fn a_rectangle_that_ends_before_it_begins_is_refused() {
        let mut body = pdu(&[plain(8, 0, (1, 1), (1, 1), &[0; 4])]);
        // destRight, made smaller than destLeft.
        body[8..10].copy_from_slice(&4_u16.to_le_bytes());
        assert_eq!(
            update(&body).unwrap_err().to_string(),
            "an RDP Bitmap Update carries a rectangle that ends before it begins 0x4, which this \
             client does not accept"
        );
    }

    #[test]
    fn an_update_that_is_not_a_bitmap_update_is_refused() {
        let mut body = pdu(&[plain(0, 0, (1, 1), (1, 1), &[0; 4])]);
        body[0] = 0x03;
        assert_eq!(
            update(&body).unwrap_err().to_string(),
            "an RDP Bitmap Update carries its update type 0x3, which this client does not accept"
        );
    }
}
