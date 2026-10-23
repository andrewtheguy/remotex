//! NSCodec, as ClearCodec's subcodec for the parts of a rectangle that are neither
//! flat nor repeated.
//!
//! NSCodec ([MS-RDPNSC]) is a planar codec: a bitmap is split into luma, two chroma
//! planes in the YCoCg space with a few low bits dropped (the *colour loss level*),
//! and an alpha plane, each plane run-length coded on its own. The chroma planes may
//! also be subsampled 2×2. On the sandbox this client is written against, Windows
//! draws most of its pictures and anti-aliased text through this subcodec inside
//! ClearCodec, so a lit desktop needs it; the standalone `NSCODEC` bitmap codec is
//! never sent and is not here.
//!
//! Decoding is FreeRDP's `nsc.c`, without its context object: the caller keeps a
//! [`Nsc`] for the plane buffers it reuses, and gets pixels back one at a time.

use super::wire::{Malformed, Reader};

const WHAT: &str = "an NSCodec bitmap";

fn refuse(field: &'static str, value: impl Into<u64>) -> Malformed {
    Malformed::Refused { what: WHAT, field, value: value.into() }
}

/// The decoder's plane buffers, reused across bitmaps.
#[derive(Default)]
pub struct Nsc {
    planes: [Vec<u8>; 4],
}

impl Nsc {
    /// Decode a `width`×`height` bitmap, handing each pixel to `put` as
    /// `(x, y, [b, g, r])`. Alpha is decoded and dropped: nothing here draws with it.
    pub fn decode(
        &mut self,
        src: &[u8],
        width: usize,
        height: usize,
        mut put: impl FnMut(usize, usize, [u8; 3]),
    ) -> Result<(), Malformed> {
        let mut r = Reader::new(WHAT, src);
        let mut plane_bytes = [0usize; 4];
        for count in &mut plane_bytes {
            *count = r.u32_le()? as usize;
        }
        let color_loss = r.u8()?;
        if !(1..=7).contains(&color_loss) {
            return Err(refuse("a colour loss level", color_loss));
        }
        let subsampled = r.u8()? != 0;
        r.u16_le()?; // reserved

        // Chroma planes are coded at a width rounded up to 8 and, when subsampled,
        // a height rounded up to 2, so a plane can be that large.
        let wide = width.div_ceil(8) * 8;
        let tall = height.div_ceil(2) * 2;
        let capacity = wide * tall;
        let mut lengths = [width * height; 4];
        if subsampled {
            lengths[0] = wide * height;
            lengths[1] = (wide / 2) * (tall / 2);
            lengths[2] = lengths[1];
        }
        for (i, plane) in self.planes.iter_mut().enumerate() {
            plane.clear();
            plane.resize(capacity, 0);
            let coded = r.bytes(plane_bytes[i])?;
            let length = lengths[i];
            if coded.is_empty() {
                plane[..length].fill(0xFF);
            } else if coded.len() < length {
                rle_decode(coded, &mut plane[..length])?;
            } else {
                plane[..length].copy_from_slice(&coded[..length]);
            }
        }

        let shift = color_loss - 1;
        let [y_plane, co_plane, cg_plane, _alpha] = &self.planes;
        for y in 0..height {
            let (y_row, c_row, c_step) = if subsampled {
                (y * wide, (y >> 1) * (wide >> 1), 2)
            } else {
                (y * width, y * width, 1)
            };
            for x in 0..width {
                let luma = i16::from(y_plane[y_row + x]);
                let c = c_row + x / c_step;
                // The chroma byte, its dropped low bits restored as zeros, read as
                // the signed value it was before they were dropped.
                let co = i16::from(((i16::from(co_plane[c]) << shift) as u16 as u8) as i8);
                let cg = i16::from(((i16::from(cg_plane[c]) << shift) as u16 as u8) as i8);
                let r = luma + co - cg;
                let g = luma + cg;
                let b = luma - co - cg;
                put(x, y, [clip(b), clip(g), clip(r)]);
            }
        }
        Ok(())
    }
}

fn clip(v: i16) -> u8 {
    v.clamp(0, 255) as u8
}

/// NSCodec's run-length coding of one plane, [MS-RDPNSC] 2.2.2.1: a byte repeated
/// is a run, its length in the byte after or, past 255, in the four after that;
/// the last four bytes of a plane are always raw.
fn rle_decode(coded: &[u8], out: &mut [u8]) -> Result<(), Malformed> {
    let mut r = Reader::new(WHAT, coded);
    let mut at = 0;
    let end = out.len();
    while end - at > 4 {
        let value = r.u8()?;
        if end - at == 5 {
            out[at] = value;
            at += 1;
            continue;
        }
        let next = r.rest().first().copied().ok_or_else(|| refuse("a run-length plane cut short at", at as u64))?;
        if next != value {
            out[at] = value;
            at += 1;
            continue;
        }
        r.u8()?;
        let short = r.u8()?;
        let run = if short < 0xFF { usize::from(short) + 2 } else { r.u32_le()? as usize };
        if run > end - at - 4 {
            return Err(refuse("a run past its plane", run as u64));
        }
        out[at..at + run].fill(value);
        at += run;
    }
    out[at..end].copy_from_slice(r.bytes(end - at)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(planes: [usize; 4], loss: u8, subsampled: bool) -> Vec<u8> {
        let mut v = Vec::new();
        for p in planes {
            v.extend_from_slice(&(p as u32).to_le_bytes());
        }
        v.extend_from_slice(&[loss, u8::from(subsampled), 0, 0]);
        v
    }

    fn decode(src: &[u8], w: usize, h: usize) -> Vec<[u8; 3]> {
        let mut out = vec![[0; 3]; w * h];
        Nsc::default().decode(src, w, h, |x, y, bgr| out[y * w + x] = bgr).unwrap();
        out
    }

    /// Raw planes, no subsampling, no colour loss: YCoCg back to RGB exactly.
    #[test]
    fn raw_planes_convert_from_ycocg() {
        // One pixel: Y = 100, Co = 20, Cg = -10 (0xF6), alpha whatever.
        let mut src = header([1, 1, 1, 1], 1, false);
        src.extend_from_slice(&[100, 20, 0xF6, 0xFF]);
        // r = y + co - cg = 130, g = y + cg = 90, b = y - co - cg = 90.
        assert_eq!(decode(&src, 1, 1), vec![[90, 90, 130]]);
    }

    /// A run-length plane: a run of one value, then the raw tail every plane ends in.
    #[test]
    fn a_run_length_plane_fills_its_run_then_copies_the_tail() {
        // A 4×4 luma plane of 16: twelve 100s as a run (100, 100, 10), then four raw.
        let y = [100, 100, 10, 1, 2, 3, 4];
        let mut src = header([y.len(), 0, 0, 0], 1, false);
        src.extend_from_slice(&y);
        let out = decode(&src, 4, 4);
        // Empty chroma planes fill with 0xFF, which as a signed byte is -1:
        // r = y - 1 + 1 = y, g = y - 1, b = y + 1 + 1 = y + 2.
        assert_eq!(out[0], [102, 99, 100]);
        assert_eq!(out[11], [102, 99, 100]);
        assert_eq!(out[12], [3, 0, 1]);
        assert_eq!(out[15], [6, 3, 4]);
    }

    /// Subsampled chroma: one chroma sample serves a 2×2 block, planes are padded to
    /// eight wide and two tall, and colour loss restores dropped bits as zeros.
    #[test]
    fn subsampled_chroma_serves_each_two_by_two_block() {
        // 2×2 pixels: luma plane 8 wide × 2, chroma 4 × 1, alpha 4. Colour loss 3
        // drops two bits: a stored Co of 5 is 20 back.
        let mut src = header([16, 4, 4, 4], 3, true);
        src.extend_from_slice(&[50, 60, 0, 0, 0, 0, 0, 0, 70, 80, 0, 0, 0, 0, 0, 0]);
        src.extend_from_slice(&[5, 0, 0, 0]); // Co
        src.extend_from_slice(&[0, 0, 0, 0]); // Cg
        src.extend_from_slice(&[0xFF; 4]);
        let out = decode(&src, 2, 2);
        assert_eq!(out, vec![[30, 50, 70], [40, 60, 80], [50, 70, 90], [60, 80, 100]]);
    }

    /// A colour loss level outside 1..=7 and a run past its plane are each refused.
    #[test]
    fn bad_headers_and_runs_are_refused() {
        let mut src = header([1, 1, 1, 1], 0, false);
        src.extend_from_slice(&[0; 4]);
        assert!(Nsc::default().decode(&src, 1, 1, |_, _, _| {}).is_err());
        let y = [7, 7, 0xFF, 100, 0, 0, 0];
        let mut src = header([y.len(), 0, 0, 0], 1, false);
        src.extend_from_slice(&y);
        assert!(Nsc::default().decode(&src, 4, 4, |_, _, _| {}).is_err());
    }
}
