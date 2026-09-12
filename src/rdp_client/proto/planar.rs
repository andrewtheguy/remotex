//! The codec a 32-bit session compresses its bitmaps with.
//!
//! RDP 6.0 bitmap compression, which everyone calls the planar codec because of what
//! it does first: it splits a rectangle into one plane per channel, so that a run of
//! pixels that share a colour becomes a run of equal bytes in each of three places
//! rather than an interleaved pattern no run-length coder can see.
//!
//! Each plane is then coded on its own, row by row, in two steps:
//!
//! 1. **Delta.** Every row but the first is replaced by its difference from the row
//!    above, zig-zagged so that a small negative difference is a small byte. A flat
//!    region becomes a plane of zeroes.
//! 2. **Run length.** Each row is a run of segments, each a control byte saying how
//!    many bytes follow literally and how many times the last of them repeats.
//!
//! # What a server may send, and what it may not
//!
//! The format header can also ask for two things this client did not claim: the
//! planes as luma and chroma rather than red, green and blue, and chroma at half
//! resolution. Both are lossy, both are gated on `DRAW_ALLOW_DYNAMIC_COLOR_FIDELITY`
//! and `DRAW_ALLOW_COLOR_SUBSAMPLING` in the Bitmap capability, and
//! [`super::capabilities`] claims neither — so they are refused here by name rather
//! than decoded into a desktop whose colours are quietly wrong.
//!
//! [MS-RDPEGDI] 2.2.2.5.1 and 3.1.9.

use super::wire::{Malformed, Reader};

const WHAT: &str = "an RDP planar bitmap";

/// The bottom three bits of the format header: the colour loss level. Zero means the
/// planes are red, green and blue; anything else means luma and chroma, with that
/// many bits shifted out of the chroma on the way.
const COLOR_LOSS: u8 = 0x07;

/// `CS`: the chroma planes are half size in each direction.
const SUBSAMPLED: u8 = 0x08;

/// `RLE`: the planes are run-length coded and delta-coded. Without it they are plain
/// bytes, and a padding byte follows them.
const RLE: u8 = 0x10;

/// `NA`: there is no alpha plane. The Bitmap capability's `DRAW_ALLOW_SKIP_ALPHA`
/// asked for that, and a server that sends one anyway is answered by reading it and
/// throwing it away: a desktop has no transparency to carry.
const NO_ALPHA: u8 = 0x20;

/// How many colour planes there are, always: red, green and blue.
const PLANES: usize = 3;

/// Decompress one bitmap into `pixels`, as `BGRX32` and bottom row first — the shape
/// an uncompressed bitmap already arrives in, so that everything above this sees one
/// layout.
///
/// `planes` is working space, kept across calls so that a session decoding a desktop
/// allocates for none of its rectangles.
pub fn decompress(
    src: &[u8],
    planes: &mut Vec<u8>,
    pixels: &mut Vec<u8>,
    width: u16,
    height: u16,
) -> Result<(), Malformed> {
    let mut r = Reader::new(WHAT, src);
    let header = r.u8()?;
    if header & COLOR_LOSS != 0 {
        return Err(r.refuse("a colour loss level", header & COLOR_LOSS));
    }
    if header & SUBSAMPLED != 0 {
        return Err(r.refuse("subsampled chroma", header));
    }
    let alpha = header & NO_ALPHA == 0;
    let plane = usize::from(width) * usize::from(height);
    let body = r.rest();

    let colors = if header & RLE != 0 {
        unpack(body, planes, plane, alpha, width, height)?;
        &planes[..]
    } else {
        // The alpha plane comes first and is not wanted; the padding byte after the
        // colour planes is not wanted either, and is what tells a length check that
        // the whole stream arrived.
        let skipped = if alpha { plane } else { 0 };
        let need = skipped + plane * PLANES + 1;
        if body.len() < need {
            return Err(Malformed::Short { what: WHAT, len: src.len(), at: r.at(), need });
        }
        &body[skipped..skipped + plane * PLANES]
    };

    compose(colors, pixels, plane);
    Ok(())
}

/// The three colour planes, decoded into `planes` one after another.
fn unpack(
    body: &[u8],
    planes: &mut Vec<u8>,
    plane: usize,
    alpha: bool,
    width: u16,
    height: u16,
) -> Result<(), Malformed> {
    planes.clear();
    planes.resize(plane * PLANES, 0);

    let mut at = 0;
    if alpha {
        // Decoded into the space the red plane is about to take and then written
        // over: how long a coded plane is can only be learnt by decoding it.
        at += scanlines(rest(body, at)?, &mut planes[..plane], width, height)?;
    }
    for index in 0..PLANES {
        let start = index * plane;
        let taken = scanlines(rest(body, at)?, &mut planes[start..start + plane], width, height)?;
        at += taken;
    }
    Ok(())
}

/// One whole plane: every row coded on its own, and every row but the first a
/// difference from the row above it.
fn scanlines(src: &[u8], plane: &mut [u8], width: u16, height: u16) -> Result<usize, Malformed> {
    let width = usize::from(width);
    let mut at = 0;
    for row in 0..usize::from(height) {
        let start = row * width;
        at += scanline(rest(src, at)?, &mut plane[start..start + width])?;
        if row > 0 {
            let (above, line) = plane[start - width..start + width].split_at_mut(width);
            undelta(above, line);
        }
    }
    Ok(at)
}

/// One row, as segments of literal bytes and runs of the last of them.
fn scanline(src: &[u8], row: &mut [u8]) -> Result<usize, Malformed> {
    let mut r = Reader::new(WHAT, src);
    // The byte a run repeats, which is the last literal byte of this row so far. A
    // row that opens with a run repeats zero, because each row starts afresh.
    let mut last = 0_u8;
    let mut at = 0;
    while at < row.len() {
        let control = r.u8()?;
        if control == 0 {
            return Err(r.refuse("a segment of no length", control));
        }
        let literals = usize::from(control >> 4);
        // A run field of one or two is the long form: the literal count it would
        // otherwise hold is added to the run instead, and there are no literals.
        let (literals, run) = match control & 0x0F {
            1 => (0, 16 + literals),
            2 => (0, 32 + literals),
            short => (literals, usize::from(short)),
        };
        if at + literals + run > row.len() {
            return Err(r.refuse("a segment that runs past its row", control));
        }
        row[at..at + literals].copy_from_slice(r.bytes(literals)?);
        if literals > 0 {
            last = row[at + literals - 1];
        }
        row[at + literals..at + literals + run].fill(last);
        at += literals + run;
    }
    Ok(r.at())
}

/// Put one row back from the difference it was coded as.
///
/// The difference is zig-zagged: the bottom bit is the sign, so that a difference of
/// minus one is 1 rather than 255 and stays a small byte for the run-length coder
/// above.
fn undelta(above: &[u8], row: &mut [u8]) {
    for (value, above) in row.iter_mut().zip(above) {
        let delta = if *value & 1 == 1 {
            0xFF_u8.wrapping_sub((*value - 1) >> 1)
        } else {
            *value >> 1
        };
        *value = above.wrapping_add(delta);
    }
}

/// The three planes, woven back into pixels.
fn compose(colors: &[u8], pixels: &mut Vec<u8>, plane: usize) {
    let (red, rest) = colors.split_at(plane);
    let (green, blue) = rest.split_at(plane);
    pixels.clear();
    pixels.reserve(plane * 4);
    for at in 0..plane {
        pixels.extend_from_slice(&[blue[at], green[at], red[at], 0]);
    }
}

/// What is left of a buffer from `at`, as an error rather than a panic when a plane
/// has claimed more than the bitmap carried.
fn rest(bytes: &[u8], at: usize) -> Result<&[u8], Malformed> {
    bytes.get(at..).ok_or(Malformed::Short { what: WHAT, len: bytes.len(), at, need: 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three colour planes of a 2x2 bitmap, written out plainly.
    fn raw(header: u8, planes: &[u8]) -> Vec<u8> {
        let mut stream = vec![header];
        stream.extend_from_slice(planes);
        // The padding byte a stream that was not run-length coded ends with.
        stream.push(0);
        stream
    }

    fn decoded(stream: &[u8], width: u16, height: u16) -> Vec<u8> {
        let (mut planes, mut pixels) = (Vec::new(), Vec::new());
        decompress(stream, &mut planes, &mut pixels, width, height).expect("a planar bitmap");
        pixels
    }

    /// Plane order is the one thing here that cannot be reasoned out of a hex dump:
    /// red, green and blue, and the answer is blue first because that is how an
    /// uncompressed bitmap arrives and how everything above this reads one.
    #[test]
    fn three_planes_are_woven_back_into_pixels_blue_first() {
        let stream = raw(NO_ALPHA, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(decoded(&stream, 2, 2), vec![
            9, 5, 1, 0, //
            10, 6, 2, 0, //
            11, 7, 3, 0, //
            12, 8, 4, 0,
        ]);
    }

    /// A desktop has no transparency, so an alpha plane is read past rather than
    /// kept — and reading past it is the only way to find the planes behind it.
    #[test]
    fn an_alpha_plane_is_stepped_over_rather_than_rendered() {
        let mut planes = vec![0xFF; 4];
        planes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(decoded(&raw(0, &planes), 2, 2), decoded(
            &raw(NO_ALPHA, &planes[4..]),
            2,
            2
        ));
    }

    /// One plane of an 8x2 bitmap: three literal bytes and a run of five more of the
    /// third, then a row below coded as no difference at all.
    fn plane(first: u8, second: u8, third: u8) -> Vec<u8> {
        vec![0x35, first, second, third, 0x08]
    }

    #[test]
    fn a_run_repeats_the_literal_before_it_and_a_row_of_zero_differences_repeats_the_row_above() {
        let mut stream = vec![RLE | NO_ALPHA];
        stream.extend(plane(1, 2, 3));
        stream.extend(plane(4, 5, 6));
        stream.extend(plane(7, 8, 9));
        let mut row = vec![7, 4, 1, 0, 8, 5, 2, 0];
        row.extend([9, 6, 3, 0].repeat(6));
        assert_eq!(decoded(&stream, 8, 2), [row.clone(), row].concat());
    }

    /// A difference is zig-zagged: the bottom bit is the sign, so minus one is 1.
    #[test]
    fn a_row_is_the_difference_from_the_row_above_it_and_may_go_downwards() {
        // One literal and a run of three, then four literal differences of minus one.
        let coded = |value: u8| vec![0x13, value, 0x40, 1, 1, 1, 1];
        let mut stream = vec![RLE | NO_ALPHA];
        stream.extend(coded(100));
        stream.extend(coded(0));
        stream.extend(coded(200));
        let top = [200, 0, 100, 0];
        let below = [199, 255, 99, 0];
        assert_eq!(decoded(&stream, 4, 2), [[top; 4].concat(), [below; 4].concat()].concat());
    }

    /// The long forms of the control byte, which are how a flat row of any width is
    /// coded: a run field of 1 or 2 means sixteen or thirty-two, plus what would
    /// otherwise have been the literal count.
    #[test]
    fn a_long_run_holds_its_length_in_the_field_the_literals_would_have_used() {
        let (mut planes, mut pixels) = (Vec::new(), Vec::new());
        // One literal, then a run of 32 + 15 of it, then a run of 16 more: a row of
        // 64, in four bytes. A long run carries no literals of its own, so what it
        // repeats is the literal a segment before it left behind.
        let coded = |value: u8| vec![0x10, value, 0xF2, 0x01];
        let mut stream = vec![RLE | NO_ALPHA];
        stream.extend(coded(7));
        stream.extend(coded(8));
        stream.extend(coded(9));
        decompress(&stream, &mut planes, &mut pixels, 64, 1).expect("a planar bitmap");
        assert_eq!(pixels, [9, 8, 7, 0].repeat(64));
    }

    #[test]
    fn a_lossy_stream_this_client_never_asked_for_is_refused_by_name() {
        let (mut planes, mut pixels) = (Vec::new(), Vec::new());
        let mut refuse = |header: u8| {
            decompress(&raw(header, &[0; 12]), &mut planes, &mut pixels, 2, 2)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            refuse(NO_ALPHA | 1),
            "an RDP planar bitmap carries a colour loss level 0x1, which this client does not \
             accept"
        );
        assert_eq!(
            refuse(NO_ALPHA | SUBSAMPLED),
            "an RDP planar bitmap carries subsampled chroma 0x28, which this client does not accept"
        );
    }

    #[test]
    fn a_stream_that_ends_inside_its_planes_is_short_rather_than_a_panic() {
        let (mut planes, mut pixels) = (Vec::new(), Vec::new());
        let err = decompress(&raw(NO_ALPHA, &[0; 8]), &mut planes, &mut pixels, 2, 2).unwrap_err();
        assert_eq!(
            err.to_string(),
            "an RDP planar bitmap is 10 bytes, and reading 13 more at offset 1 runs past the end"
        );

        // And a coded plane that stops in the middle of a row is the same answer.
        let err =
            decompress(&[RLE | NO_ALPHA, 0x40, 1], &mut planes, &mut pixels, 4, 2).unwrap_err();
        assert!(matches!(err, Malformed::Short { .. }), "{err}");
    }

    /// A segment that would write past the end of its row is a stream that does not
    /// describe the bitmap it was sent for.
    #[test]
    fn a_segment_wider_than_its_row_is_refused() {
        let (mut planes, mut pixels) = (Vec::new(), Vec::new());
        let stream = [RLE | NO_ALPHA, 0x28, 1, 2];
        let err = decompress(&stream, &mut planes, &mut pixels, 4, 1).unwrap_err();
        assert_eq!(
            err.to_string(),
            "an RDP planar bitmap carries a segment that runs past its row 0x28, which this \
             client does not accept"
        );
    }
}
