//! The RFB pixel encodings, decoded to packed RGB888.
//!
//! Every decoder here returns the rectangle's pixels in the one format the tile
//! path takes — `w * h * 3`, no padding, no stride — so [`crate::vnc`]'s rectangle
//! reader has one bounds check, one shadow comparison, one crop and one sink call
//! whatever the server chose. Nothing here knows about tiles, sinks or the browser,
//! which is what lets a decoder be tested against handwritten wire bytes.
//!
//! ## The colour order, which is the thing to get wrong
//!
//! `set_pixel_format` forces 32 bits per pixel, depth 24, little-endian, with red
//! shifted 16, green 8 and blue 0. So **every colour on the wire is `B, G, R, X`**
//! — a raw pixel, an RRE background, a Hextile foreground, a ZRLE CPIXEL. noVNC,
//! which these decoders are ported from, forces the mirror format and copies its
//! bytes straight through; every byte-order line in the reference is reversed here.
//! A grey test pixel cannot catch a swapped channel, so the tests in this module
//! use asymmetric colours throughout.
//!
//! ## What is deliberately absent
//!
//! Tight and TightPNG are vendor encodings; JPEG and H.264 are lossy. A gateway
//! that re-encodes every tile for the browser anyway gains nothing by receiving
//! pixels that have already lost information, and advertising an encoding is a
//! promise to decode it.

use anyhow::Context as _;
use log::{debug, info};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::tiles::{Rect, Shadow};
use crate::vnc::BPP;

/// Ceiling on one inflated payload, so a hostile or broken stream cannot be
/// answered with unbounded memory.
///
/// An 8192x8192 framebuffer at four bytes a pixel, which is past any real desktop
/// and comfortably past the 4480x1800 (31 MiB) a two-display Mac session
/// synthesizes. A 64 MiB cap would be tighter than the raw path and reject a
/// rectangle solely because it arrived compressed. The real bound on either path is
/// the rectangle's bounds check against the announced desktop; this stops only
/// wildly bogus geometry from turning into an allocation.
pub const MAX_INFLATED: usize = 8192 * 8192 * 4;

/// How a rectangle's pixels arrive.
///
/// Decided from the encoding number before the bounds check, and acted on after it,
/// so one bounds check and one tile path serve every encoding.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// `w * h` pixels in the forced wire format (encoding 0).
    Raw,
    /// A `u32` length, then that much of the connection's zlib stream, holding the
    /// same raw pixels (encoding 6).
    Zlib,
    /// No pixels at all: a position in the framebuffer whose pixels this side has
    /// to read back out of the shadow, because a client here is sent tiles rather
    /// than draw commands (encoding 1).
    CopyRect,
    /// A background colour and a run of coloured sub-rectangles over it
    /// (encoding 2).
    Rre,
}

impl Payload {
    /// What to call this in a log line.
    fn name(self) -> &'static str {
        match self {
            Payload::Raw => "raw",
            Payload::Zlib => "zlib",
            Payload::CopyRect => "copyrect",
            Payload::Rre => "rre",
        }
    }
}

/// Decoder state that outlives a single rectangle.
///
/// Owned by the read loop rather than by `Shared`: nothing else touches it, and a
/// lock on the pixel path to say so would be a lock that never contends.
#[derive(Default)]
pub struct Decoders {
    /// The connection's encoding-6 inflate stream. Created on the first zlib
    /// rectangle and never reset — see [`Inflater`].
    zlib: Option<Inflater>,
    /// Which encodings have already been announced in the log.
    seen: Vec<Payload>,
}

impl Decoders {
    /// Decode one rectangle's payload into packed RGB888.
    ///
    /// The payload is read whatever the geometry says, including for a rectangle of
    /// no pixels: an encoding that frames itself — a length word, a subrect count,
    /// a source position — still sends that framing, and consuming it is what keeps
    /// the stream in step. The RFB stream has no framing of its own above the record
    /// layer, so walking past by the wrong number of bytes desyncs everything after
    /// it.
    ///
    /// `None` means the rectangle was understood but its pixels cannot be produced,
    /// which only CopyRect can say: its source is a region the shadow never learned.
    /// The caller answers that with a repaint request.
    pub async fn decode<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        payload: Payload,
        shadow: &std::sync::Mutex<Shadow>,
        w: u16,
        h: u16,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        self.note(payload);
        match payload {
            Payload::Raw => raw(reader, w, h).await.map(Some),
            Payload::Zlib => self.zlib(reader, w, h).await.map(Some),
            Payload::CopyRect => copy_rect(reader, shadow, w, h).await,
            Payload::Rre => rre(reader, w, h).await.map(Some),
        }
    }

    /// Say once per connection which encodings the server actually chose.
    ///
    /// The advertised list is a preference, not an instruction, so this is the only
    /// way to know from here what a server settled on — and the first thing worth
    /// knowing when a session paints wrongly.
    fn note(&mut self, payload: Payload) {
        if !self.seen.contains(&payload) {
            self.seen.push(payload);
            info!("vnc: server is sending {} rectangles", payload.name());
        }
    }

    /// Encoding 6: a `u32` length, then that much of the connection's single
    /// deflate stream, inflating to the rectangle's raw pixels.
    async fn zlib<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        w: u16,
        h: u16,
    ) -> anyhow::Result<Vec<u8>> {
        let expect = usize::from(w) * usize::from(h) * BPP;
        let len = reader.read_u32().await?;
        // Deflate can *expand*, and on a small rectangle it always does: the stream
        // header and one sync flush cost more than a 1x1 rectangle's four pixels
        // (measured: nine bytes for four). So bounding this at `expect` would refuse
        // legitimate rectangles. A generous multiple still bounds the read, which is
        // all this check is for — the inflated size is checked exactly, below.
        let ceiling = expect + expect / 64 + 1024;
        anyhow::ensure!(
            u64::from(len) <= ceiling as u64,
            "a zlib rect claims {len} compressed bytes for {expect} of pixels, past the \
             {ceiling} that even an incompressible one would take"
        );
        let mut chunk = vec![0u8; len as usize];
        reader.read_exact(&mut chunk).await?;
        let inflated = self
            .zlib
            .get_or_insert_with(|| Inflater::new("zlib"))
            .exact(&chunk, expect)?;
        Ok(bgrx_to_rgb(&inflated))
    }
}

/// Encoding 2: one background colour, then a run of coloured sub-rectangles laid
/// over it.
///
/// RFC 6143 calls this obsolete and it is: a rectangle of any complexity costs more
/// than Hextile's tiling of the same picture. It is here because it is trivial once
/// the shared fill exists, and because it is the only compressed encoding some very
/// old servers have.
async fn rre<R: AsyncRead + Unpin>(reader: &mut R, w: u16, h: u16) -> anyhow::Result<Vec<u8>> {
    let count = reader.read_u32().await?;
    let pixels = usize::from(w) * usize::from(h);
    // One subrectangle per pixel is the point at which raw would have been smaller,
    // so anything past it is a bogus length rather than a picture — and this runs
    // before the loop allocates or reads anything on its behalf.
    anyhow::ensure!(
        count as usize <= pixels,
        "an rre rect claims {count} subrectangles for {pixels} pixels, past the point \
         where raw would have been smaller"
    );
    let mut out = vec![0u8; pixels * 3];
    fill(&mut out, w, (0, 0), (w, h), colour(read_pixel(reader).await?));
    for _ in 0..count {
        let rgb = colour(read_pixel(reader).await?);
        let sx = reader.read_u16().await?;
        let sy = reader.read_u16().await?;
        let sw = reader.read_u16().await?;
        let sh = reader.read_u16().await?;
        anyhow::ensure!(
            u32::from(sx) + u32::from(sw) <= u32::from(w)
                && u32::from(sy) + u32::from(sh) <= u32::from(h),
            "an rre subrect {sw}x{sh}+{sx}+{sy} leaves its {w}x{h} rectangle"
        );
        fill(&mut out, w, (sx, sy), (sw, sh), rgb);
    }
    Ok(out)
}

/// Encoding 1: two `u16`s naming where in the framebuffer these pixels already
/// are.
///
/// A server sends this for a scroll or a window move — the pixels are on both
/// sides of the link already, so neither has to carry them again. The browser and
/// the Swift client take tiles and cannot blit, so the saving stops at this
/// gateway: the source is read out of the shadow and forwarded as ordinary pixels,
/// which is still the whole of the VNC link's traffic saved.
async fn copy_rect<R: AsyncRead + Unpin>(
    reader: &mut R,
    shadow: &std::sync::Mutex<Shadow>,
    w: u16,
    h: u16,
) -> anyhow::Result<Option<Vec<u8>>> {
    // Read the source before anything can return: four bytes arrive whatever the
    // geometry says.
    let src_x = reader.read_u16().await?;
    let src_y = reader.read_u16().await?;
    let Some(src) = Rect::from_size(src_x, src_y, w, h) else {
        return Ok(Some(Vec::new()));
    };
    let copied = shadow.lock().unwrap().copy_out(src);
    if copied.is_none() {
        // Not an error: a server that copies from a region this side never learned
        // costs one repaint, not the session. Logged because a *repeating* one is
        // the only way this becomes a problem, and the source rect is what
        // identifies it.
        debug!("vnc: copyrect source {w}x{h}+{src_x}+{src_y} is not in the shadow; repainting");
    }
    Ok(copied)
}

/// Encoding 0: `w * h` pixels in the forced wire format, and the fallback every
/// server has.
async fn raw<R: AsyncRead + Unpin>(reader: &mut R, w: u16, h: u16) -> anyhow::Result<Vec<u8>> {
    let mut pixels = vec![0u8; usize::from(w) * usize::from(h) * BPP];
    reader.read_exact(&mut pixels).await?;
    Ok(bgrx_to_rgb(&pixels))
}

/// One pixel in the forced wire format.
async fn read_pixel<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<[u8; BPP]> {
    let mut px = [0u8; BPP];
    reader.read_exact(&mut px).await?;
    Ok(px)
}

/// A wire pixel as RGB. See the module header: the wire order is `B, G, R, X`.
fn colour(px: [u8; BPP]) -> [u8; 3] {
    [px[2], px[1], px[0]]
}

/// Paint `size` at `at` in an RGB888 buffer `stride` pixels wide.
///
/// Shared by every encoding that paints areas rather than pixels, which is all of
/// them but raw. A `size` of no pixels is a no-op rather than an error: RFB lets a
/// server send one and it means the same thing either way.
fn fill(out: &mut [u8], stride: u16, at: (u16, u16), size: (u16, u16), rgb: [u8; 3]) {
    let stride = usize::from(stride);
    let (x, y) = (usize::from(at.0), usize::from(at.1));
    let (w, h) = (usize::from(size.0), usize::from(size.1));
    for row in y..y + h {
        let start = (row * stride + x) * 3;
        for px in out[start..start + w * 3].chunks_exact_mut(3) {
            px.copy_from_slice(&rgb);
        }
    }
}

/// Repack BGRX pixels (our forced format on the wire) into packed RGB888.
pub fn bgrx_to_rgb(bgrx: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(bgrx.len() / BPP * 3);
    for px in bgrx.chunks_exact(BPP) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    rgb
}

/// An inflater whose lifetime is chosen by the encoding that owns it.
///
/// Keep this private: the connection-lifetime streams belong to [`Decoders`], while
/// self-contained payloads call [`inflate_independent`] and so cannot accidentally
/// inherit connection-wide state.
struct Inflater {
    inflate: flate2::Decompress,
    what: &'static str,
}

impl Inflater {
    /// One deflate stream for the life of the connection, chunked across
    /// rectangles: the sliding window carries over, so this context is created once
    /// and never reset. A fresh one per rectangle decodes the first rectangle and
    /// then fails — or, worse, succeeds with the wrong pixels.
    fn new(what: &'static str) -> Self {
        Self {
            inflate: flate2::Decompress::new(true),
            what,
        }
    }

    /// Inflate one chunk to exactly `expect` bytes.
    ///
    /// `expect` is known from the geometry the rectangle already declared, so a
    /// payload that wants to expand past it is a protocol violation rather than a
    /// buffer to grow — which is also what keeps a compression bomb from being
    /// answered with memory.
    fn exact(&mut self, chunk: &[u8], expect: usize) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            expect <= MAX_INFLATED,
            "a {} rectangle wants {expect} inflated bytes, past the {MAX_INFLATED} ceiling",
            self.what
        );
        let mut out = Vec::with_capacity(expect);
        let mut fed = 0;
        while fed < chunk.len() && out.len() < expect {
            let before = (self.inflate.total_in(), self.inflate.total_out());
            self.inflate
                .decompress_vec(&chunk[fed..], &mut out, flate2::FlushDecompress::Sync)
                .with_context(|| format!("inflating a {} rectangle", self.what))?;
            fed += (self.inflate.total_in() - before.0) as usize;
            if (self.inflate.total_in(), self.inflate.total_out()) == before {
                // Neither side moved, so feeding more of the same chunk cannot
                // help: either the stream wants output space this rectangle does
                // not claim, or it is truncated.
                break;
            }
        }
        anyhow::ensure!(
            out.len() == expect,
            "a {} rectangle inflated to {} bytes, not the {expect} its geometry claims",
            self.what,
            out.len()
        );
        Ok(out)
    }
}

/// Inflate a payload that carries its own complete deflate stream.
pub fn inflate_independent(
    what: &'static str,
    chunk: &[u8],
    expect: usize,
) -> anyhow::Result<Vec<u8>> {
    Inflater::new(what).exact(chunk, expect)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deflate `raw` into one chunk of a continuing stream, the way a server emits
    /// one rectangle's worth.
    fn chunk(deflate: &mut flate2::Compress, raw: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(raw.len());
        let mut fed = 0;
        while fed < raw.len() {
            let before = deflate.total_in();
            out.reserve(raw.len());
            deflate
                .compress_vec(&raw[fed..], &mut out, flate2::FlushCompress::Sync)
                .unwrap();
            fed += (deflate.total_in() - before) as usize;
        }
        out
    }

    /// A zlib rectangle as it arrives: the `u32` length, then the chunk.
    fn zlib_payload(chunk: &[u8]) -> Vec<u8> {
        let mut wire = (chunk.len() as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(chunk);
        wire
    }

    /// Pixels whose channels all differ, so a swapped one cannot pass.
    fn bgrx(pixels: usize) -> Vec<u8> {
        std::iter::repeat_n([0x30, 0x20, 0x10, 0x00], pixels)
            .flatten()
            .collect()
    }

    /// A shadow for the encodings that never look at one.
    fn unused_shadow() -> std::sync::Mutex<Shadow> {
        std::sync::Mutex::new(Shadow::new("test", 1, 1))
    }

    /// A shadow that knows `rgb` as its whole contents.
    fn shadow_of(w: u16, h: u16, rgb: &[u8]) -> std::sync::Mutex<Shadow> {
        let mut shadow = Shadow::new("test", w, h);
        shadow.accept(Rect::from_size(0, 0, w, h).unwrap(), rgb);
        std::sync::Mutex::new(shadow)
    }

    /// A CopyRect payload: the source position, and nothing else.
    fn copy_rect_payload(src_x: u16, src_y: u16) -> Vec<u8> {
        let mut wire = src_x.to_be_bytes().to_vec();
        wire.extend_from_slice(&src_y.to_be_bytes());
        wire
    }

    #[test]
    fn bgrx_repacks_to_rgb() {
        // Two pixels: pure red and pure blue in BGRX order.
        let bgrx = [0, 0, 255, 0, 255, 0, 0, 0];
        assert_eq!(bgrx_to_rgb(&bgrx), vec![255, 0, 0, 0, 0, 255]);
    }

    #[tokio::test]
    async fn raw_pixels_arrive_as_rgb_not_bgr() {
        let wire = bgrx(2);
        let rgb = raw(&mut wire.as_slice(), 2, 1).await.unwrap();
        assert_eq!(rgb, vec![0x10, 0x20, 0x30, 0x10, 0x20, 0x30]);
    }

    /// One stream across rectangles, which is the rule that makes zlib usable at
    /// all here. Two chunks of one deflate stream inflate in sequence and fail
    /// separately.
    #[tokio::test]
    async fn zlib_inflates_across_rectangles_but_not_out_of_order() {
        let first = bgrx(1024);
        let second = bgrx(1024);
        let mut deflate = flate2::Compress::new(flate2::Compression::default(), true);
        let a = zlib_payload(&chunk(&mut deflate, &first));
        let b = zlib_payload(&chunk(&mut deflate, &second));

        let shadow = unused_shadow();
        let mut decoders = Decoders::default();
        assert_eq!(
            decoders.decode(&mut a.as_slice(), Payload::Zlib, &shadow, 32, 32).await.unwrap(),
            Some(bgrx_to_rgb(&first))
        );
        assert_eq!(
            decoders.decode(&mut b.as_slice(), Payload::Zlib, &shadow, 32, 32).await.unwrap(),
            Some(bgrx_to_rgb(&second))
        );

        // The second chunk alone, through a stream that never saw the first: no
        // zlib header, no window, nothing.
        let mut fresh = Decoders::default();
        assert!(
            fresh.decode(&mut b.as_slice(), Payload::Zlib, &shadow, 32, 32).await.is_err()
        );

        // A chunk that does not inflate to the size its geometry claims.
        let mut decoders = Decoders::default();
        let err = decoders
            .decode(&mut a.as_slice(), Payload::Zlib, &shadow, 32, 33)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("its geometry claims"), "{err:#}");
    }

    #[tokio::test]
    async fn a_zlib_rect_claiming_more_bytes_than_it_could_need_is_refused() {
        let wire = u32::MAX.to_be_bytes();
        let err = Decoders::default()
            .decode(&mut wire.as_slice(), Payload::Zlib, &unused_shadow(), 2, 2)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("even an incompressible one"), "{err:#}");
    }

    /// An RRE payload: the subrect count, the background, then the subrects.
    fn rre_payload(bgrx_background: [u8; 4], subrects: &[([u8; 4], u16, u16, u16, u16)]) -> Vec<u8> {
        let mut wire = (subrects.len() as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(&bgrx_background);
        for (px, x, y, w, h) in subrects {
            wire.extend_from_slice(px);
            for value in [x, y, w, h] {
                wire.extend_from_slice(&value.to_be_bytes());
            }
        }
        wire
    }

    #[tokio::test]
    async fn rre_paints_the_background_then_the_subrects_over_it() {
        // A 3x2 rectangle: blue background, one green pixel-column in the middle.
        let wire = rre_payload([0xf0, 0x00, 0x00, 0], &[([0x00, 0xf0, 0x00, 0], 1, 0, 1, 2)]);
        let rgb = rre(&mut wire.as_slice(), 3, 2).await.unwrap();

        let blue = [0x00, 0x00, 0xf0];
        let green = [0x00, 0xf0, 0x00];
        let expected: Vec<u8> = [blue, green, blue, blue, green, blue]
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(rgb, expected);
    }

    #[tokio::test]
    async fn an_rre_subrect_outside_its_rectangle_is_refused() {
        let wire = rre_payload([0, 0, 0, 0], &[([0xff, 0xff, 0xff, 0], 2, 0, 2, 1)]);
        let err = rre(&mut wire.as_slice(), 3, 2).await.unwrap_err();
        assert!(format!("{err:#}").contains("leaves its 3x2 rectangle"), "{err:#}");
    }

    /// One subrectangle per pixel is where raw would have been smaller, so a count
    /// past it is a bogus length — and it is refused before anything is allocated
    /// or read on its behalf.
    #[tokio::test]
    async fn an_rre_rect_claiming_more_subrects_than_pixels_is_refused() {
        let wire = 7u32.to_be_bytes();
        let err = rre(&mut wire.as_slice(), 2, 3).await.unwrap_err();
        assert!(format!("{err:#}").contains("raw would have been smaller"), "{err:#}");
    }

    /// The source is read out before the destination is written, so a copy that
    /// overlaps itself moves the original pixels rather than smearing the ones it
    /// has just laid down.
    #[tokio::test]
    async fn an_overlapping_copy_rect_moves_the_original_pixels() {
        // Eight pixels across, each a different red channel.
        let rgb: Vec<u8> = (0..8u8).flat_map(|i| [i * 0x10, 0x20, 0x30]).collect();
        let shadow = shadow_of(8, 1, &rgb);

        // Copy [0..4] two to the right, over [2..6].
        let wire = copy_rect_payload(0, 0);
        let copied = copy_rect(&mut wire.as_slice(), &shadow, 4, 1)
            .await
            .unwrap()
            .expect("the whole shadow is known");
        assert_eq!(copied, rgb[..4 * 3]);
    }

    #[tokio::test]
    async fn a_copy_rect_from_an_unknown_region_asks_for_nothing_rather_than_guessing() {
        // A shadow that knows its left half only.
        let rgb: Vec<u8> = (0..4u8).flat_map(|i| [i * 0x10, 0x20, 0x30]).collect();
        let mut shadow = Shadow::new("test", 8, 1);
        shadow.accept(Rect::from_size(0, 0, 4, 1).unwrap(), &rgb);
        let shadow = std::sync::Mutex::new(shadow);

        let known = copy_rect_payload(0, 0);
        assert!(copy_rect(&mut known.as_slice(), &shadow, 4, 1).await.unwrap().is_some());

        // One pixel into the half nothing has ever painted.
        let unknown = copy_rect_payload(1, 0);
        assert!(copy_rect(&mut unknown.as_slice(), &shadow, 4, 1).await.unwrap().is_none());
    }

    /// Off the edge of the framebuffer is the same answer as unknown — one repaint,
    /// not a dead session — and the four bytes are still consumed.
    #[tokio::test]
    async fn a_copy_rect_sourced_off_the_framebuffer_is_refused_not_fatal() {
        let rgb = vec![0x10, 0x20, 0x30];
        let shadow = shadow_of(1, 1, &rgb);
        let wire = copy_rect_payload(1, 0);
        let mut reader = wire.as_slice();
        assert!(copy_rect(&mut reader, &shadow, 1, 1).await.unwrap().is_none());
        assert!(reader.is_empty(), "the source position is consumed either way");
    }
}
