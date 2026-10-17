//! The page's decoder for a Mac's Full-quality picture: one of the Mac's
//! rectangles in, the framebuffer it paints out.
//!
//! A session started with Standard's Full quality lists zlib ahead of ZRLE, as
//! Apple's viewer does for it, and a Mac sends only the first it knows of them:
//! RFB's zlib (encoding 6). The gateway passes each rectangle as it came, so the
//! page inflates them here, against a framebuffer of its own.
//!
//! A body is a run of one deflate stream, the zlib header at the session's first,
//! that the Mac flushes at each rectangle's end and never finishes: the session's
//! rectangles share it, so each is inflated on what the ones before it left of
//! the stream's window. It inflates to the rectangle's pixels, row after row, each
//! four bytes of the pixel format the gateway set: blue, green, red, unused.

use std::fmt;

use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};

/// The encoding a Mac's Full-quality picture comes in.
pub const ENCODING: i32 = 6;

/// Bytes of one pixel in the format the gateway set.
const PIXEL: usize = 4;

/// Room past a rectangle's pixels for the inflater to write into, so that a body
/// that inflates to more than its rectangle is caught rather than cut short.
const SLACK: usize = 64;

const BLACK: u32 = 0xff00_0000;

/// Why a rectangle could not be decoded. The framebuffer keeps whatever the
/// rectangles before it drew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A rectangle that does not fit in the framebuffer.
    Bounds { x: u16, y: u16, w: u16, h: u16, width: usize, height: usize },
    /// A body the inflater could not read: the stream is broken from here on.
    Inflate,
    /// A body that inflated to more or fewer bytes than its rectangle's pixels.
    Size { want: usize, got: usize },
    /// A body with bytes the stream could not take after its pixels.
    Trailing(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bounds { x, y, w, h, width, height } => {
                write!(f, "a rectangle {w}x{h} at {x},{y} on a {width}x{height} framebuffer")
            }
            Self::Inflate => write!(f, "a body that does not inflate"),
            Self::Size { want, got } => write!(f, "a body that inflated to {got} bytes, where its pixels are {want}"),
            Self::Trailing(left) => write!(f, "a body with {left} bytes left after its pixels"),
        }
    }
}

impl std::error::Error for Error {}

/// One Mac's Full-quality picture: its framebuffer and the deflate stream its
/// rectangles share.
pub struct Decoder {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
    stream: Box<InflateState>,
    /// What a rectangle inflates to, before it is drawn.
    rows: Vec<u8>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            pixels: Vec::new(),
            stream: InflateState::new_boxed(DataFormat::Zlib),
            rows: Vec::new(),
        }
    }

    /// A black framebuffer of a new size. The stream outlives it, as the Mac's
    /// does.
    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.pixels = vec![BLACK; width * height];
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The framebuffer, row after row, each pixel R, G, B, A in memory.
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    /// Inflate one rectangle's body, the bytes after its `u32` length, onto the
    /// framebuffer. Returns whether it drew: one of no area does not.
    pub fn decode(&mut self, x: u16, y: u16, w: u16, h: u16, body: &[u8]) -> Result<bool, Error> {
        let (x0, y0, w0, h0) = (usize::from(x), usize::from(y), usize::from(w), usize::from(h));
        if x0 + w0 > self.width || y0 + h0 > self.height {
            return Err(Error::Bounds { x, y, w, h, width: self.width, height: self.height });
        }
        let want = w0 * h0 * PIXEL;
        self.rows.resize(want + SLACK, 0);
        let got = self.inflate(body)?;
        if got != want {
            return Err(Error::Size { want, got });
        }
        if want == 0 {
            return Ok(false);
        }
        for (row, line) in self.rows[..want].chunks_exact(w0 * PIXEL).enumerate() {
            let start = (y0 + row) * self.width + x0;
            let (bytes, _) = line.as_chunks::<PIXEL>();
            for (pixel, &[b, g, r, _]) in self.pixels[start..start + w0].iter_mut().zip(bytes) {
                *pixel = BLACK | u32::from(b) << 16 | u32::from(g) << 8 | u32::from(r);
            }
        }
        Ok(true)
    }

    /// Feed the whole body to the stream, and say how many bytes it inflated to.
    /// The inflater is called until it takes no more of the body and writes no
    /// more: it can hand back output it held without reading anything.
    fn inflate(&mut self, body: &[u8]) -> Result<usize, Error> {
        let mut input = body;
        let mut written = 0;
        loop {
            let out = &mut self.rows[written..];
            if out.is_empty() {
                // Past the slack: more than the rectangle by far.
                return Err(Error::Size { want: self.rows.len() - SLACK, got: written });
            }
            let result = inflate(&mut self.stream, input, out, MZFlush::None);
            input = &input[result.bytes_consumed..];
            written += result.bytes_written;
            match result.status {
                // Nothing more to do with what is left: the body is used up.
                Err(MZError::Buf) => break,
                Err(_) => return Err(Error::Inflate),
                Ok(MZStatus::StreamEnd) => break,
                Ok(_) => {}
            }
            if result.bytes_consumed == 0 && result.bytes_written == 0 {
                break;
            }
        }
        if !input.is_empty() {
            return Err(Error::Trailing(input.len()));
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use miniz_oxide::deflate::core::{CompressorOxide, create_comp_flags_from_zip_params};
    use miniz_oxide::deflate::stream::deflate;

    /// The Mac's side: one zlib stream, flushed at each rectangle's end.
    struct Mac {
        compressor: Box<CompressorOxide>,
    }

    impl Mac {
        fn new() -> Self {
            let flags = create_comp_flags_from_zip_params(6, 15, 0);
            Self { compressor: Box::new(CompressorOxide::new(flags)) }
        }

        /// A rectangle's pixels, each blue, green, red and an unused byte, deflated
        /// as the next run of the stream.
        fn rect(&mut self, pixels: &[[u8; 3]]) -> Vec<u8> {
            let raw: Vec<u8> = pixels.iter().flat_map(|&[r, g, b]| [b, g, r, 0]).collect();
            self.deflate(&raw)
        }

        fn deflate(&mut self, raw: &[u8]) -> Vec<u8> {
            let mut out = vec![0; raw.len() * 2 + 64];
            let result = deflate(&mut self.compressor, raw, &mut out, MZFlush::Sync);
            assert_eq!(result.bytes_consumed, raw.len());
            out.truncate(result.bytes_written);
            out
        }
    }

    fn rgba(pixel: u32) -> [u8; 3] {
        let [r, g, b, _] = pixel.to_le_bytes();
        [r, g, b]
    }

    /// Each rectangle continues the stream the one before it began, and lands where
    /// it says, in R, G, B, A: the bytes the gateway's pixel format puts blue first.
    #[test]
    fn rectangles_inflate_onto_the_framebuffer_on_one_stream() {
        let mut mac = Mac::new();
        let mut decoder = Decoder::new();
        decoder.resize(8, 4);

        let first: Vec<[u8; 3]> = (0..8).map(|i| [i * 30, 7, 200]).collect();
        assert_eq!(decoder.decode(0, 0, 4, 2, &mac.rect(&first)), Ok(true));
        assert_eq!(rgba(decoder.pixels()[0]), [0, 7, 200]);
        assert_eq!(rgba(decoder.pixels()[1]), [30, 7, 200]);
        assert_eq!(rgba(decoder.pixels()[8]), [120, 7, 200], "the second row starts a framebuffer row down");
        assert_eq!(decoder.pixels()[4], BLACK, "nothing drawn past the rectangle");
        assert_eq!(decoder.pixels()[0] >> 24, 0xff, "opaque");

        // The same pixels again come as back-references into the first's window.
        let again = mac.rect(&first);
        assert!(again.len() < 16, "{} bytes", again.len());
        assert_eq!(decoder.decode(4, 2, 4, 2, &again), Ok(true));
        assert_eq!(rgba(decoder.pixels()[2 * 8 + 4]), [0, 7, 200]);
        assert_eq!(rgba(decoder.pixels()[3 * 8 + 7]), [210, 7, 200]);
    }

    /// A rectangle of no area draws nothing, and the stream goes on.
    #[test]
    fn a_rectangle_of_no_area_draws_nothing() {
        let mut mac = Mac::new();
        let mut decoder = Decoder::new();
        decoder.resize(2, 2);
        assert_eq!(decoder.decode(0, 0, 0, 0, &mac.rect(&[])), Ok(false));
        assert_eq!(decoder.decode(1, 1, 1, 1, &mac.rect(&[[1, 2, 3]])), Ok(true));
        assert_eq!(rgba(decoder.pixels()[3]), [1, 2, 3]);
    }

    /// A resize leaves the framebuffer black and the stream where it was.
    #[test]
    fn a_resize_keeps_the_stream() {
        let mut mac = Mac::new();
        let mut decoder = Decoder::new();
        decoder.resize(2, 1);
        decoder.decode(0, 0, 2, 1, &mac.rect(&[[9, 9, 9], [9, 9, 9]])).unwrap();
        decoder.resize(3, 1);
        assert!(decoder.pixels().iter().all(|&pixel| pixel == BLACK));
        assert_eq!(decoder.decode(2, 0, 1, 1, &mac.rect(&[[4, 5, 6]])), Ok(true));
        assert_eq!(rgba(decoder.pixels()[2]), [4, 5, 6]);
    }

    /// A body that inflates to other than its rectangle's pixels, one that does not
    /// inflate, and a rectangle off the framebuffer are each refused.
    #[test]
    fn what_does_not_fit_its_rectangle_is_refused() {
        let mut mac = Mac::new();
        let mut decoder = Decoder::new();
        decoder.resize(4, 4);
        let two = mac.rect(&[[1, 1, 1], [2, 2, 2]]);
        assert_eq!(decoder.decode(0, 0, 3, 1, &two), Err(Error::Size { want: 12, got: 8 }));

        let mut mac = Mac::new();
        let mut decoder = Decoder::new();
        decoder.resize(4, 4);
        let many = mac.rect(&[[1, 1, 1]; 16]);
        assert!(matches!(decoder.decode(0, 0, 1, 1, &many), Err(Error::Size { want: 4, .. })));

        let mut decoder = Decoder::new();
        decoder.resize(4, 4);
        assert_eq!(decoder.decode(0, 0, 1, 1, &[0x78, 0x9c, 0xff, 0xff, 0xff]), Err(Error::Inflate));

        let mut decoder = Decoder::new();
        decoder.resize(4, 4);
        assert!(matches!(decoder.decode(3, 0, 2, 1, &[]), Err(Error::Bounds { .. })));
    }
}
