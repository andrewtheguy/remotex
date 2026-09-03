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

/// Hextile's tile edge. Every rectangle is cut into these, the right and bottom
/// ones shrinking to whatever is left.
const HEXTILE: usize = 16;
/// The tile carries its own pixels, and the other bits mean nothing.
const HEXTILE_RAW: u8 = 0x01;
/// The tile sets the background colour the tiles after it may omit.
const HEXTILE_BACKGROUND: u8 = 0x02;
/// The same for the foreground its uncoloured sub-rectangles use.
const HEXTILE_FOREGROUND: u8 = 0x04;
/// The tile has sub-rectangles, counted by the byte after the colours.
const HEXTILE_SUBRECTS: u8 = 0x08;
/// Each sub-rectangle carries its own colour rather than using the foreground.
const HEXTILE_SUBRECTS_COLOURED: u8 = 0x10;

/// ZRLE's tile edge, four times Hextile's.
const ZRLE: usize = 64;
/// The largest palette a ZRLE tile can name: subencoding 255 means 127 entries.
const ZRLE_MAX_PALETTE: usize = 127;
/// Absolute cap on a compressed payload read off the wire, independent of the
/// geometry that justified it.
///
/// A 4K desktop makes [`zrle_ceiling`] about 34 MB, which a hostile server could
/// then claim for every rectangle. The geometric bound is the meaningful one; this
/// is the one that does not scale with the desktop.
const MAX_COMPRESSED: usize = 64 << 20;

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
    /// 16x16 tiles, each raw or painted from colours an earlier tile may have set
    /// (encoding 5).
    Hextile,
    /// A `u32` length, then that much of a *second* zlib stream, holding 64x64
    /// tiles that are run-length encoded, palettised, or both (encoding 16).
    Zrle,
}

impl Payload {
    /// What to call this in a log line.
    fn name(self) -> &'static str {
        match self {
            Payload::Raw => "raw",
            Payload::Zlib => "zlib",
            Payload::CopyRect => "copyrect",
            Payload::Rre => "rre",
            Payload::Hextile => "hextile",
            Payload::Zrle => "zrle",
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
    /// ZRLE's stream, which is a *different* one.
    ///
    /// Two streams, not one: encoding 6 and ZRLE each deflate across the whole
    /// connection and their chunks interleave on the one socket, so an inflater
    /// shared between them would read the other encoding's chunk as a stream with no
    /// header. This is the same rule that already forbids a fresh context per
    /// rectangle, applied across encodings instead of across rectangles.
    zrle: Option<Inflater>,
    /// The colours a Hextile tile may omit.
    hextile: HextileColours,
    /// Which encodings have already been announced in the log.
    seen: Vec<Payload>,
}

/// The background and foreground a Hextile tile may omit because an earlier tile
/// set them.
///
/// Black until a tile says otherwise, rather than an error: refusing a session over
/// a colour no tile has needed yet would trade a wrong pixel for a dead connection,
/// and a conforming server sets both in its first tile anyway.
#[derive(Default)]
struct HextileColours {
    background: [u8; 3],
    foreground: [u8; 3],
}

/// What reading one rectangle's payload produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    /// The rectangle's pixels, packed RGB888.
    Pixels(Vec<u8>),
    /// A CopyRect the client can perform on its own canvas: the source rectangle,
    /// whose pixels the shadow has already moved to the destination. Only ever
    /// produced when the caller said the client can do it — see [`Decoders::decode`].
    Copied(Rect),
    /// A CopyRect whose destination already held exactly those pixels. Nothing to
    /// send and nothing to draw, which is the same answer [`Shadow::accept`] gives
    /// for an update that changed nothing.
    Unchanged,
    /// The rectangle was understood but its pixels cannot be produced: a CopyRect
    /// sourced from a region the shadow never learned. The caller answers with a
    /// repaint request.
    Unavailable,
}

impl Decoders {
    /// Decode one rectangle's payload.
    ///
    /// The payload is read whatever the geometry says, including for a rectangle of
    /// no pixels: an encoding that frames itself — a length word, a subrect count,
    /// a source position — still sends that framing, and consuming it is what keeps
    /// the stream in step. The RFB stream has no framing of its own above the record
    /// layer, so walking past by the wrong number of bytes desyncs everything after
    /// it.
    ///
    /// `copy_to` is read by CopyRect and nothing else: `Some` is the destination
    /// *and* the caller's word that this client can perform a copy itself, so the
    /// shadow moves its own pixels there and the source comes back to be sent as
    /// thirteen bytes. `None` reads the source back out as ordinary pixels, which is
    /// what this gateway did for every plan before the record existed. One argument
    /// rather than two because the two are never independently useful: a destination
    /// with nothing that can act on it is not a destination.
    pub async fn decode<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        payload: Payload,
        shadow: &std::sync::Mutex<Shadow>,
        copy_to: Option<(u16, u16)>,
        w: u16,
        h: u16,
    ) -> anyhow::Result<Decoded> {
        self.note(payload);
        Ok(match payload {
            Payload::Raw => Decoded::Pixels(raw(reader, w, h).await?),
            Payload::Zlib => Decoded::Pixels(self.zlib(reader, w, h).await?),
            Payload::CopyRect => copy_rect(reader, shadow, copy_to, w, h).await?,
            Payload::Rre => Decoded::Pixels(rre(reader, w, h).await?),
            Payload::Hextile => Decoded::Pixels(self.hextile(reader, w, h).await?),
            Payload::Zrle => Decoded::Pixels(self.zrle(reader, w, h).await?),
        })
    }

    /// Encoding 16: 64x64 tiles inside a deflate stream, each tile run-length
    /// encoded, palettised, both, or neither.
    ///
    /// The tiling is what makes this beat plain zlib on interface content: the
    /// redundancy is taken out per tile *before* deflate ever sees the bytes.
    async fn zrle<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        w: u16,
        h: u16,
    ) -> anyhow::Result<Vec<u8>> {
        let cap = zrle_ceiling(w, h);
        let len = reader.read_u32().await?;
        // As for encoding 6: deflate expands a small payload, so the compressed
        // bound has to be generous. What keeps it honest is that the *inflated*
        // bytes are bounded by geometry and then spent exactly, tile by tile.
        let ceiling = (cap + cap / 64 + 1024).min(MAX_COMPRESSED);
        anyhow::ensure!(
            u64::from(len) <= ceiling as u64,
            "a zrle rect claims {len} compressed bytes for {w}x{h}, past the {ceiling} \
             its tiles could need even uncompressed"
        );
        let mut chunk = vec![0u8; len as usize];
        reader.read_exact(&mut chunk).await?;
        let inflated = self
            .zrle
            .get_or_insert_with(|| Inflater::new("zrle"))
            .capped(&chunk, cap)?;
        zrle_tiles(&inflated, w, h)
    }

    /// Encoding 5: 16x16 tiles, left to right and top to bottom, each either raw
    /// pixels or an area fill plus sub-rectangles.
    ///
    /// A tile may omit its background or foreground, meaning the one an earlier tile
    /// set. That state is [`Decoders::hextile`] rather than a local, so it carries
    /// across rectangles as it does in noVNC: a conforming server sets both in the
    /// first tile of every rectangle, which makes the choice invisible to it, and
    /// this is the reading our only reference implements.
    async fn hextile<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        w: u16,
        h: u16,
    ) -> anyhow::Result<Vec<u8>> {
        let mut out = vec![0u8; usize::from(w) * usize::from(h) * 3];
        // One tile's worth of scratch, reused: the biggest is 16x16.
        let mut tile = vec![0u8; HEXTILE * HEXTILE * 3];
        let mut raw_rgb = Vec::new();
        for ty in (0..h).step_by(HEXTILE) {
            let th = HEXTILE.min(usize::from(h - ty)) as u16;
            for tx in (0..w).step_by(HEXTILE) {
                let tw = HEXTILE.min(usize::from(w - tx)) as u16;
                let sub = reader.read_u8().await?;
                anyhow::ensure!(
                    sub & 0xe0 == 0,
                    "a hextile tile has subencoding {sub}, which sets bits RFB does not define"
                );
                if sub & HEXTILE_RAW != 0 {
                    // Raw wins outright: RFC 6143 says the other bits are ignored,
                    // and the carried colours are neither read nor changed.
                    let mut pixels = vec![0u8; usize::from(tw) * usize::from(th) * BPP];
                    reader.read_exact(&mut pixels).await?;
                    bgrx_to_rgb_into(&pixels, &mut raw_rgb);
                    blit(&mut out, w, (tx, ty), (tw, th), &raw_rgb);
                    continue;
                }
                if sub & HEXTILE_BACKGROUND != 0 {
                    self.hextile.background = colour(read_pixel(reader).await?);
                }
                if sub & HEXTILE_FOREGROUND != 0 {
                    self.hextile.foreground = colour(read_pixel(reader).await?);
                }
                fill(&mut tile, tw, (0, 0), (tw, th), self.hextile.background);
                if sub & HEXTILE_SUBRECTS != 0 {
                    let count = reader.read_u8().await?;
                    for _ in 0..count {
                        let rgb = if sub & HEXTILE_SUBRECTS_COLOURED != 0 {
                            colour(read_pixel(reader).await?)
                        } else {
                            self.hextile.foreground
                        };
                        // Two nibbles each: a position, then a size that counts from
                        // one. So a size nibble of 15 means 16, and 15 + 16 is past
                        // the largest tile there is — which is why this is checked
                        // rather than trusted.
                        let xy = reader.read_u8().await?;
                        let wh = reader.read_u8().await?;
                        let (sx, sy) = (u16::from(xy >> 4), u16::from(xy & 0x0f));
                        let (sw, sh) = (u16::from(wh >> 4) + 1, u16::from(wh & 0x0f) + 1);
                        anyhow::ensure!(
                            sx + sw <= tw && sy + sh <= th,
                            "a hextile subrect {sw}x{sh}+{sx}+{sy} leaves its {tw}x{th} tile"
                        );
                        fill(&mut tile, tw, (sx, sy), (sw, sh), rgb);
                    }
                }
                blit(&mut out, w, (tx, ty), (tw, th), &tile);
            }
        }
        Ok(out)
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

/// Most inflated bytes a ZRLE rectangle's geometry can justify.
///
/// The tiles partition the rectangle, so `Σ tw*th` is exactly `w*h` and the
/// per-pixel worst case can be counted over the whole of it: plain RLE, where a run
/// of one pixel costs a three-byte CPIXEL and a one-byte length. Raw and palette
/// tiles are strictly cheaper. Only the fixed cost — a subencoding byte and the
/// largest palette a tile may carry — has to be counted per tile.
fn zrle_ceiling(w: u16, h: u16) -> usize {
    let tiles = usize::from(w).div_ceil(ZRLE) * usize::from(h).div_ceil(ZRLE);
    tiles * (1 + ZRLE_MAX_PALETTE * 3) + usize::from(w) * usize::from(h) * 4
}

/// Paint a ZRLE rectangle's tiles out of its inflated bytes.
///
/// Sync and pure: everything the wire decides has already happened by the time this
/// runs, which is what lets the whole subencoding table be tested against
/// handwritten bytes with no socket and no compressor in the way.
fn zrle_tiles(data: &[u8], w: u16, h: u16) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Bytes { data, at: 0 };
    let mut out = vec![0u8; usize::from(w) * usize::from(h) * 3];
    let mut tile = vec![0u8; ZRLE * ZRLE * 3];
    for ty in (0..h).step_by(ZRLE) {
        let th = ZRLE.min(usize::from(h - ty)) as u16;
        for tx in (0..w).step_by(ZRLE) {
            let tw = ZRLE.min(usize::from(w - tx)) as u16;
            let pixels = usize::from(tw) * usize::from(th);
            match bytes.u8()? {
                // Raw: the tile's pixels, in order, with no compaction left.
                0 => {
                    for px in tile[..pixels * 3].as_chunks_mut::<3>().0 {
                        *px = cpixel(&mut bytes)?;
                    }
                }
                // Solid: one colour for the whole tile.
                1 => {
                    let rgb = cpixel(&mut bytes)?;
                    fill(&mut tile, tw, (0, 0), (tw, th), rgb);
                }
                // A palette, then one index per pixel packed into 1, 2 or 4 bits.
                n @ 2..=16 => {
                    let palette = read_palette(&mut bytes, usize::from(n))?;
                    let bits = palette_bits(palette.len());
                    // Rows are padded to whole bytes, so the whole block can be
                    // taken at once and indexed rather than walked with a running
                    // shift.
                    let row_bytes = (usize::from(tw) * bits).div_ceil(8);
                    let packed = bytes.take(row_bytes * usize::from(th))?;
                    let mask = (1u8 << bits) - 1;
                    for y in 0..usize::from(th) {
                        let row = &packed[y * row_bytes..(y + 1) * row_bytes];
                        for x in 0..usize::from(tw) {
                            let shift = 8 - bits - (x * bits) % 8;
                            let index = usize::from((row[x * bits / 8] >> shift) & mask);
                            let rgb = *palette
                                .get(index)
                                .with_context(|| format!("a zrle palette index of {index}"))?;
                            let at = (y * usize::from(tw) + x) * 3;
                            tile[at..at + 3].copy_from_slice(&rgb);
                        }
                    }
                }
                // Runs of full colours, laid down left to right and wrapping rows.
                128 => {
                    let mut done = 0usize;
                    while done < pixels {
                        let rgb = cpixel(&mut bytes)?;
                        let run = rle_len(&mut bytes)?;
                        anyhow::ensure!(
                            run <= pixels - done,
                            "a zrle run of {run} overruns the {pixels} pixels its tile has left"
                        );
                        for px in tile[done * 3..(done + run) * 3].as_chunks_mut::<3>().0 {
                            *px = rgb;
                        }
                        done += run;
                    }
                }
                // The same, but the runs name palette entries.
                n @ 130..=255 => {
                    let palette = read_palette(&mut bytes, usize::from(n) - 128)?;
                    let mut done = 0usize;
                    while done < pixels {
                        let byte = bytes.u8()?;
                        // The top bit says a run follows; without it the entry is
                        // one pixel and the index is the whole of it.
                        let (index, run) = if byte >= 128 {
                            (usize::from(byte - 128), rle_len(&mut bytes)?)
                        } else {
                            (usize::from(byte), 1)
                        };
                        let rgb = *palette
                            .get(index)
                            .with_context(|| format!("a zrle palette index of {index}"))?;
                        anyhow::ensure!(
                            run <= pixels - done,
                            "a zrle run of {run} overruns the {pixels} pixels its tile has left"
                        );
                        for px in tile[done * 3..(done + run) * 3].as_chunks_mut::<3>().0 {
                            *px = rgb;
                        }
                        done += run;
                    }
                }
                other => anyhow::bail!("a zrle tile has subencoding {other}, which RFB does not define"),
            }
            blit(&mut out, w, (tx, ty), (tw, th), &tile);
        }
    }
    // A server sync-flushes its stream at the end of a rectangle, so bytes left over
    // mean the tiles and the geometry disagree about what was sent. Named with a
    // count, because that is what identifies a server batching rectangles instead.
    anyhow::ensure!(
        bytes.done(),
        "a zrle rectangle left {} inflated byte(s) unread",
        bytes.data.len() - bytes.at
    );
    Ok(out)
}

/// `n` CPIXELs, which is how both palette subencodings begin.
fn read_palette(bytes: &mut Bytes, n: usize) -> anyhow::Result<Vec<[u8; 3]>> {
    (0..n).map(|_| cpixel(bytes)).collect()
}

/// Bits per index for a palette of `n`: as few as will address it.
fn palette_bits(n: usize) -> usize {
    match n {
        0..=2 => 1,
        3..=4 => 2,
        _ => 4,
    }
}

/// A ZRLE CPIXEL: three bytes rather than four.
///
/// The forced format puts all 24 colour bits in the low three bytes of a
/// little-endian pixel, which is exactly the case RFB lets ZRLE drop the fourth for
/// — and which makes them `B, G, R`.
fn cpixel(bytes: &mut Bytes) -> anyhow::Result<[u8; 3]> {
    let px = bytes.take(3)?;
    Ok([px[2], px[1], px[0]])
}

/// A run length: bytes of 255 continue it, and the total counts from one.
///
/// Cannot run away — every byte comes out of the capped inflate buffer, and the
/// caller bounds the result by the pixels its tile has left.
fn rle_len(bytes: &mut Bytes) -> anyhow::Result<usize> {
    let mut len = 1usize;
    loop {
        let byte = bytes.u8()?;
        len += usize::from(byte);
        if byte != 255 {
            return Ok(len);
        }
    }
}

/// A checked cursor over inflated bytes.
struct Bytes<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        let end = self.at + n;
        anyhow::ensure!(
            end <= self.data.len(),
            "a zrle rectangle wants {n} more inflated byte(s) than the {} it was sent",
            self.data.len()
        );
        let taken = &self.data[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn done(&self) -> bool {
        self.at == self.data.len()
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
/// sides of *that* link already, so neither has to carry them again.
///
/// With `copy_to` set, the same is true of the browser link and the saving carries
/// through: the shadow moves its own pixels and the caller sends the client a
/// `COPY` record, thirteen bytes for a rectangle that may be most of a desktop.
/// Without it — a target whose canvas is not made purely of tiles, see
/// [`crate::encode::TileSink::copies`] — the source is read back out as ordinary
/// pixels, which is still the whole of the VNC link's traffic saved.
async fn copy_rect<R: AsyncRead + Unpin>(
    reader: &mut R,
    shadow: &std::sync::Mutex<Shadow>,
    copy_to: Option<(u16, u16)>,
    w: u16,
    h: u16,
) -> anyhow::Result<Decoded> {
    // Read the source before anything can return: four bytes arrive whatever the
    // geometry says.
    let src_x = reader.read_u16().await?;
    let src_y = reader.read_u16().await?;
    let Some(src) = Rect::from_size(src_x, src_y, w, h) else {
        return Ok(Decoded::Pixels(Vec::new()));
    };
    // Not an error either way: a server that copies from a region this side never
    // learned costs one repaint, not the session. Logged because a *repeating* one
    // is the only way this becomes a problem, and the source rect is what
    // identifies it.
    let unknown =
        || debug!("vnc: copyrect source {w}x{h}+{src_x}+{src_y} is not in the shadow; repainting");
    let Some(dst) = copy_to.and_then(|(x, y)| Rect::from_size(x, y, w, h)) else {
        return Ok(match shadow.lock().unwrap().copy_out(src) {
            Some(pixels) => Decoded::Pixels(pixels),
            None => {
                unknown();
                Decoded::Unavailable
            }
        });
    };
    Ok(match shadow.lock().unwrap().copy_within(src, dst) {
        Some(true) => Decoded::Copied(src),
        Some(false) => Decoded::Unchanged,
        None => {
            unknown();
            Decoded::Unavailable
        }
    })
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

/// Copy a `size` block of packed RGB888 into `out` at `at`, where `out`'s rows are
/// `stride` pixels wide and `src`'s are `size.0` pixels wide.
///
/// `src` may be a reused scratch buffer longer than the block, so only what the
/// block claims is read.
fn blit(out: &mut [u8], stride: u16, at: (u16, u16), size: (u16, u16), src: &[u8]) {
    let stride = usize::from(stride);
    let (x, y) = (usize::from(at.0), usize::from(at.1));
    let (w, h) = (usize::from(size.0), usize::from(size.1));
    for row in 0..h {
        let to = ((y + row) * stride + x) * 3;
        let from = row * w * 3;
        out[to..to + w * 3].copy_from_slice(&src[from..from + w * 3]);
    }
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
        for px in out[start..start + w * 3].as_chunks_mut::<3>().0 {
            *px = rgb;
        }
    }
}

/// Repack BGRX pixels (our forced format on the wire) into packed RGB888.
pub fn bgrx_to_rgb(bgrx: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::new();
    bgrx_to_rgb_into(bgrx, &mut rgb);
    rgb
}

/// [`bgrx_to_rgb`] into a buffer the caller reuses, for the paths that repack in a
/// loop — hextile pays this once per 16×16 tile.
///
/// Sized writes into a zeroed buffer rather than a byte-at-a-time `extend`: the
/// fixed 4-in/3-out stride is what lets the compiler vectorize the shuffle.
pub fn bgrx_to_rgb_into(bgrx: &[u8], rgb: &mut Vec<u8>) {
    let pixels = bgrx.len() / BPP;
    rgb.clear();
    rgb.resize(pixels * 3, 0);
    for (out, px) in rgb.as_chunks_mut::<3>().0.iter_mut().zip(bgrx.as_chunks::<BPP>().0) {
        out[0] = px[2];
        out[1] = px[1];
        out[2] = px[0];
    }
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
    ///
    /// The whole chunk is fed even once `expect` bytes are out. A rectangle ends
    /// with a sync flush, whose bytes produce no output at all, and this inflater
    /// lives for the connection: bytes left unfed here are bytes the *next*
    /// rectangle's chunk is decoded as, which is a desync rather than a bad pixel. A
    /// rectangle of no pixels is the case that makes this plain — it is all flush.
    fn exact(&mut self, chunk: &[u8], expect: usize) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            expect <= MAX_INFLATED,
            "a {} rectangle wants {expect} inflated bytes, past the {MAX_INFLATED} ceiling",
            self.what
        );
        // One byte of slack: `decompress_vec` writes into spare capacity, and zlib
        // consumes no input once it has nowhere to put the output — so a buffer sized
        // exactly to `expect` would stall on the trailing flush instead of reading it.
        // The slack also turns an over-long stream into a size mismatch below rather
        // than a silent truncation.
        let mut out = Vec::with_capacity(expect + 1);
        let mut fed = 0;
        while fed < chunk.len() {
            let before = (self.inflate.total_in(), self.inflate.total_out());
            self.inflate
                .decompress_vec(&chunk[fed..], &mut out, flate2::FlushDecompress::Sync)
                .with_context(|| format!("inflating a {} rectangle", self.what))?;
            fed += (self.inflate.total_in() - before.0) as usize;
            if (self.inflate.total_in(), self.inflate.total_out()) == before {
                // Neither side moved, so feeding more of the same chunk cannot
                // help: either the stream wants output space this rectangle does
                // not claim, or it is truncated. The size check names which.
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

    /// Inflate all of `chunk`, growing the output as the stream asks for it, up to
    /// `cap`.
    ///
    /// ZRLE's inflated size is not implied by its rectangle the way encoding 6's is
    /// — a tile's byte count depends on the subencoding it chose — so the geometry
    /// gives a ceiling rather than an answer, and the tile parser is what proves the
    /// bytes were the right ones.
    fn capped(&mut self, chunk: &[u8], cap: usize) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            cap <= MAX_INFLATED,
            "a {} rectangle allows {cap} inflated bytes, past the {MAX_INFLATED} ceiling",
            self.what
        );
        let mut out = Vec::new();
        let mut fed = 0;
        loop {
            if out.len() == out.capacity() {
                // `decompress_vec` writes into spare capacity and never grows the
                // Vec itself, so a loop that does not reserve here spins forever
                // making no progress.
                let want = (out.capacity().max(4096) * 2).min(cap);
                anyhow::ensure!(
                    want > out.len(),
                    "a {} rectangle inflated past the {cap} bytes its geometry allows",
                    self.what
                );
                out.reserve_exact(want - out.len());
            }
            let before = (self.inflate.total_in(), self.inflate.total_out());
            self.inflate
                .decompress_vec(&chunk[fed..], &mut out, flate2::FlushDecompress::Sync)
                .with_context(|| format!("inflating a {} rectangle", self.what))?;
            fed += (self.inflate.total_in() - before.0) as usize;
            if (self.inflate.total_in(), self.inflate.total_out()) == before {
                // Neither side moved. With output space left, the only thing that
                // can stop the stream is running out of input — so anything unread
                // here is a chunk that ended mid-symbol. `exact` catches that with
                // its size check; this has no size to check against.
                anyhow::ensure!(
                    fed == chunk.len(),
                    "a {} rectangle's stream stalled with {} of {} compressed bytes unread",
                    self.what,
                    chunk.len() - fed,
                    chunk.len()
                );
                return Ok(out);
            }
        }
    }
}

/// Deflate `raw` into one chunk of a continuing stream, the way a server emits one
/// rectangle's worth.
///
/// Consuming the input is not the end of it: the sync flush that closes the
/// rectangle has bytes of its own, and stopping at the last input byte truncates
/// them into a chunk no decoder should accept. So this runs until the input is
/// consumed *and* the compressor had room it did not use, which is how
/// `compress_vec` says it has emitted everything it was holding.
///
/// **The obvious condition — "loop until a call produces nothing new" — does not
/// terminate**, and which zlib is linked decides whether anyone finds out. A sync
/// flush emits an empty stored block; the C zlib suppresses a second one against
/// an already-flushed stream and `miniz_oxide` emits it every time, so the same
/// loop returned on a Mac with `libz-sys` in the tree and spun forever without it.
/// This gateway lost `libz-sys` when the RDP engine moved off IronRDP, and this
/// test hung. The production deflate in `vnc_apple_clipboard.rs` was already
/// written the right way round, which is why nothing user-visible was affected.
///
/// Shared with [`crate::vnc`]'s tests rather than copied — a second copy of this
/// loop is a second chance to write the truncated version and call the decoder
/// wrong.
#[cfg(test)]
pub(crate) fn deflate_chunk(deflate: &mut flate2::Compress, raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut fed = 0;
    loop {
        out.reserve(raw.len() + 64);
        let available = out.spare_capacity_mut().len();
        let before = (deflate.total_in(), deflate.total_out());
        deflate
            .compress_vec(&raw[fed..], &mut out, flate2::FlushCompress::Sync)
            .unwrap();
        fed += (deflate.total_in() - before.0) as usize;
        if fed == raw.len() && deflate.total_out() - before.1 < available as u64 {
            return out;
        }
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

    use super::deflate_chunk as chunk;

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
            decoders.decode(&mut a.as_slice(), Payload::Zlib, &shadow, None, 32, 32).await.unwrap(),
            Decoded::Pixels(bgrx_to_rgb(&first))
        );
        assert_eq!(
            decoders.decode(&mut b.as_slice(), Payload::Zlib, &shadow, None, 32, 32).await.unwrap(),
            Decoded::Pixels(bgrx_to_rgb(&second))
        );

        // The second chunk alone, through a stream that never saw the first: no
        // zlib header, no window, nothing.
        let mut fresh = Decoders::default();
        assert!(
            fresh.decode(&mut b.as_slice(), Payload::Zlib, &shadow, None, 32, 32).await.is_err()
        );

        // A chunk that does not inflate to the size its geometry claims.
        let mut decoders = Decoders::default();
        let err = decoders
            .decode(&mut a.as_slice(), Payload::Zlib, &shadow, None, 32, 33)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("its geometry claims"), "{err:#}");
    }

    /// A rectangle of no pixels is all sync flush: no output, but input the
    /// connection's inflater still has to swallow. Stopping once `expect` bytes are
    /// out leaves those bytes for the next rectangle's chunk to be decoded as, which
    /// is a desync — every rectangle after it is wrong, not just this one.
    #[tokio::test]
    async fn a_zlib_rect_of_no_pixels_still_advances_the_stream() {
        let mut deflate = flate2::Compress::new(flate2::Compression::default(), true);
        // What a server sends for a 0x0 rect: the stream header and a flush, framing
        // no pixels at all. Then a real rectangle behind it.
        let empty = zlib_payload(&chunk(&mut deflate, &[]));
        let pixels = bgrx(64);
        let real = zlib_payload(&chunk(&mut deflate, &pixels));

        let shadow = unused_shadow();
        let mut decoders = Decoders::default();
        assert_eq!(
            decoders.decode(&mut empty.as_slice(), Payload::Zlib, &shadow, None, 0, 0).await.unwrap(),
            Decoded::Pixels(Vec::new())
        );
        assert_eq!(
            decoders.decode(&mut real.as_slice(), Payload::Zlib, &shadow, None, 8, 8).await.unwrap(),
            Decoded::Pixels(bgrx_to_rgb(&pixels)),
            "the rectangle behind the empty one"
        );
    }

    #[tokio::test]
    async fn a_zlib_rect_claiming_more_bytes_than_it_could_need_is_refused() {
        let wire = u32::MAX.to_be_bytes();
        let err = Decoders::default()
            .decode(&mut wire.as_slice(), Payload::Zlib, &unused_shadow(), None, 2, 2)
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

    /// A Hextile sub-rectangle as a test writes one: an optional colour, then the
    /// position and size that go into the two nibble bytes.
    type Subrect = (Option<[u8; 4]>, u8, u8, u8, u8);

    /// One Hextile tile: the subencoding byte, then whatever it says follows.
    struct Tile(Vec<u8>);

    impl Tile {
        fn new(sub: u8) -> Self {
            Self(vec![sub])
        }
        fn colour(mut self, bgrx: [u8; 4]) -> Self {
            self.0.extend_from_slice(&bgrx);
            self
        }
        fn subrects(mut self, subrects: &[Subrect]) -> Self {
            self.0.push(subrects.len() as u8);
            for (px, x, y, w, h) in subrects {
                if let Some(px) = px {
                    self.0.extend_from_slice(px);
                }
                self.0.push((x << 4) | y);
                self.0.push(((w - 1) << 4) | (h - 1));
            }
            self
        }
        fn pixels(mut self, bgrx: &[u8]) -> Self {
            self.0.extend_from_slice(bgrx);
            self
        }
    }

    fn hextile_payload(tiles: Vec<Tile>) -> Vec<u8> {
        tiles.into_iter().flat_map(|t| t.0).collect()
    }

    const BLUE: [u8; 4] = [0xf0, 0x00, 0x00, 0];
    const BLUE_RGB: [u8; 3] = [0x00, 0x00, 0xf0];
    const GREEN: [u8; 4] = [0x00, 0xf0, 0x00, 0];
    const GREEN_RGB: [u8; 3] = [0x00, 0xf0, 0x00];

    /// The colours are the tile's own only when it says so; otherwise they are
    /// whatever the last tile that did say left behind — across rectangles, not just
    /// across tiles.
    #[tokio::test]
    async fn hextile_carries_its_colours_between_tiles_and_between_rectangles() {
        let mut decoders = Decoders::default();

        // A 32x1 rectangle, so two tiles. The first sets both colours and paints a
        // foreground subrect; the second omits both and must reuse them.
        let wire = hextile_payload(vec![
            Tile::new(HEXTILE_BACKGROUND | HEXTILE_FOREGROUND | HEXTILE_SUBRECTS)
                .colour(BLUE)
                .colour(GREEN)
                .subrects(&[(None, 0, 0, 1, 1)]),
            Tile::new(HEXTILE_SUBRECTS).subrects(&[(None, 0, 0, 1, 1)]),
        ]);
        let rgb = decoders.hextile(&mut wire.as_slice(), 32, 1).await.unwrap();
        assert_eq!(&rgb[..3], &GREEN_RGB, "the first tile's subrect");
        assert_eq!(&rgb[3..6], &BLUE_RGB, "and its background");
        assert_eq!(&rgb[16 * 3..16 * 3 + 3], &GREEN_RGB, "the second tile reused both");
        assert_eq!(&rgb[17 * 3..17 * 3 + 3], &BLUE_RGB);

        // A whole new rectangle, still omitting both.
        let wire = hextile_payload(vec![Tile::new(HEXTILE_SUBRECTS).subrects(&[(None, 0, 0, 1, 1)])]);
        let rgb = decoders.hextile(&mut wire.as_slice(), 16, 1).await.unwrap();
        assert_eq!(&rgb[..3], &GREEN_RGB);
        assert_eq!(&rgb[3..6], &BLUE_RGB);
    }

    /// noVNC skips a blank tile that follows a raw one, calling it a server quirk
    /// (`hextile.js:80-86`). We deliberately do not: RFC 6143 gives subencoding 0
    /// one meaning, and skipping would leave a hole in a buffer this side is filling
    /// from scratch — which the shadow would then record as pixels the browser has
    /// and suppress forever after.
    #[tokio::test]
    async fn a_blank_tile_after_a_raw_one_is_still_the_background() {
        // A 33x1 rectangle, so three tiles: 16 wide, 16 wide, then the 1 left over.
        let raw_tile: Vec<u8> = std::iter::repeat_n(GREEN, 16).flatten().collect();
        let wire = hextile_payload(vec![
            Tile::new(HEXTILE_BACKGROUND).colour(BLUE),
            Tile::new(HEXTILE_RAW).pixels(&raw_tile),
            Tile::new(0),
        ]);
        let rgb = Decoders::default()
            .hextile(&mut wire.as_slice(), 33, 1)
            .await
            .unwrap();
        assert_eq!(&rgb[..3], &BLUE_RGB);
        assert_eq!(&rgb[16 * 3..16 * 3 + 3], &GREEN_RGB, "the raw tile");
        assert_eq!(&rgb[32 * 3..], &BLUE_RGB, "and the blank one after it");
    }

    /// The right and bottom tiles are whatever is left over, and a subrect is
    /// measured against that rather than against a full 16x16.
    #[tokio::test]
    async fn a_hextile_subrect_outside_its_edge_tile_is_refused() {
        // A 17x1 rectangle: a full tile, then a 1-pixel one.
        let wire = hextile_payload(vec![
            Tile::new(HEXTILE_BACKGROUND).colour(BLUE),
            Tile::new(HEXTILE_SUBRECTS).subrects(&[(None, 0, 0, 2, 1)]),
        ]);
        let err = Decoders::default()
            .hextile(&mut wire.as_slice(), 17, 1)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("leaves its 1x1 tile"), "{err:#}");
    }

    #[tokio::test]
    async fn a_hextile_tile_setting_undefined_bits_is_refused() {
        let wire = hextile_payload(vec![Tile::new(0x20)]);
        let err = Decoders::default()
            .hextile(&mut wire.as_slice(), 1, 1)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("bits RFB does not define"), "{err:#}");
    }

    /// A CPIXEL as a server writes one: three bytes, blue first.
    fn cp(rgb: [u8; 3]) -> [u8; 3] {
        [rgb[2], rgb[1], rgb[0]]
    }

    /// Repeat one colour across a whole tile's worth of expected RGB.
    fn solid_rgb(rgb: [u8; 3], pixels: usize) -> Vec<u8> {
        std::iter::repeat_n(rgb, pixels).flatten().collect()
    }

    const RED_RGB: [u8; 3] = [0xf0, 0x00, 0x00];

    #[test]
    fn a_raw_zrle_tile_is_cpixels_in_order() {
        let mut data = vec![0u8];
        data.extend(cp(RED_RGB));
        data.extend(cp(GREEN_RGB));
        assert_eq!(
            zrle_tiles(&data, 2, 1).unwrap(),
            [RED_RGB, GREEN_RGB].concat()
        );
    }

    #[test]
    fn a_solid_zrle_tile_is_one_cpixel() {
        let mut data = vec![1u8];
        data.extend(cp(BLUE_RGB));
        assert_eq!(zrle_tiles(&data, 3, 2).unwrap(), solid_rgb(BLUE_RGB, 6));
    }

    /// Palette rows are padded to whole bytes. A three-wide tile at two bits an
    /// index uses six of the eight, and the two left over must be stepped over
    /// rather than read as the next row's first pixel.
    #[test]
    fn a_palette_zrle_tile_pads_each_row_to_a_whole_byte() {
        let mut data = vec![3u8]; // three entries, so two bits an index
        for rgb in [RED_RGB, GREEN_RGB, BLUE_RGB] {
            data.extend(cp(rgb));
        }
        // Row 0: red, green, blue. Row 1: blue, green, red. Two bits spare in each.
        data.push(0b00_01_10_00);
        data.push(0b10_01_00_00);
        assert_eq!(
            zrle_tiles(&data, 3, 2).unwrap(),
            [RED_RGB, GREEN_RGB, BLUE_RGB, BLUE_RGB, GREEN_RGB, RED_RGB].concat()
        );
    }

    /// One bit an index for a palette of two, four bits for anything up to sixteen.
    #[test]
    fn palette_indices_are_as_narrow_as_the_palette_allows() {
        assert_eq!(palette_bits(2), 1);
        assert_eq!(palette_bits(3), 2);
        assert_eq!(palette_bits(4), 2);
        assert_eq!(palette_bits(5), 4);
        assert_eq!(palette_bits(16), 4);
    }

    /// Runs are laid down in pixel order and wrap rows, so a run can span the end of
    /// one and the start of the next.
    #[test]
    fn a_plain_rle_run_crosses_a_row_boundary() {
        let mut data = vec![128u8];
        data.extend(cp(RED_RGB));
        data.push(3); // 1 + 3 = a run of four, over a 3x2 tile's first two rows
        data.extend(cp(BLUE_RGB));
        data.push(1); // and two more

        let mut expected = solid_rgb(RED_RGB, 4);
        expected.extend(solid_rgb(BLUE_RGB, 2));
        assert_eq!(zrle_tiles(&data, 3, 2).unwrap(), expected);
    }

    /// A length is a sum of bytes, with 255 meaning "and more", so a run past 255
    /// takes more than one byte to say.
    #[test]
    fn a_palette_rle_run_longer_than_255_is_summed() {
        let mut data = vec![130u8]; // 130 - 128 = two palette entries
        data.extend(cp(RED_RGB));
        data.extend(cp(BLUE_RGB));
        data.push(0x80); // entry 0, with a run following
        data.extend([255, 44]); // 1 + 255 + 44 = 300
        data.push(0x81); // entry 1, with a run following
        data.extend([255, 43]); // 1 + 255 + 43 = 299
        data.push(0x00); // and one last single pixel of entry 0

        let mut expected = solid_rgb(RED_RGB, 300);
        expected.extend(solid_rgb(BLUE_RGB, 299));
        expected.extend(solid_rgb(RED_RGB, 1));
        assert_eq!(zrle_tiles(&data, 60, 10).unwrap(), expected);
    }

    /// Tiles are 64 square and run left to right, so a 65-wide rectangle has two of
    /// them and the second is one pixel wide.
    #[test]
    fn zrle_tiles_are_64_square_and_laid_out_in_reading_order() {
        let mut data = vec![1u8];
        data.extend(cp(RED_RGB));
        data.push(1);
        data.extend(cp(BLUE_RGB));

        let rgb = zrle_tiles(&data, 65, 1).unwrap();
        assert_eq!(&rgb[..3], &RED_RGB);
        assert_eq!(&rgb[63 * 3..64 * 3], &RED_RGB);
        assert_eq!(&rgb[64 * 3..], &BLUE_RGB);
    }

    #[test]
    fn undefined_zrle_subencodings_are_refused() {
        for sub in [17u8, 127, 129] {
            let err = zrle_tiles(&[sub], 1, 1).unwrap_err();
            assert!(format!("{err:#}").contains("RFB does not define"), "{sub}: {err:#}");
        }
    }

    #[test]
    fn a_zrle_run_past_the_end_of_its_tile_is_refused() {
        let mut data = vec![128u8];
        data.extend(cp(RED_RGB));
        data.push(9); // a run of ten, over a tile of four
        let err = zrle_tiles(&data, 2, 2).unwrap_err();
        assert!(format!("{err:#}").contains("overruns the 4 pixels"), "{err:#}");
    }

    #[test]
    fn a_zrle_palette_index_past_the_palette_is_refused() {
        let mut data = vec![130u8]; // two entries, so 0 and 1 are the only ones
        data.extend(cp(RED_RGB));
        data.extend(cp(BLUE_RGB));
        data.push(0x02);
        let err = zrle_tiles(&data, 1, 1).unwrap_err();
        assert!(format!("{err:#}").contains("palette index of 2"), "{err:#}");
    }

    /// A server sync-flushes at the end of a rectangle, so leftover bytes mean the
    /// tiles and the geometry disagree about what was sent.
    #[test]
    fn inflated_bytes_the_tiles_did_not_want_are_refused() {
        let mut data = vec![1u8];
        data.extend(cp(RED_RGB));
        data.push(0xff);
        let err = zrle_tiles(&data, 1, 1).unwrap_err();
        assert!(format!("{err:#}").contains("left 1 inflated byte"), "{err:#}");
    }

    #[test]
    fn a_truncated_zrle_tile_is_refused() {
        let err = zrle_tiles(&[0u8, 0x10], 1, 1).unwrap_err();
        assert!(format!("{err:#}").contains("more inflated byte"), "{err:#}");
    }

    /// Encoding 6 and ZRLE each deflate across the whole connection, and their
    /// chunks interleave on one socket. One inflater between them would read the
    /// other's chunk as a stream with no header.
    #[tokio::test]
    async fn zrle_and_zlib_do_not_share_a_stream() {
        let shadow = unused_shadow();

        // Two independent streams, as a server keeps them.
        let mut zlib_stream = flate2::Compress::new(flate2::Compression::default(), true);
        let mut zrle_stream = flate2::Compress::new(flate2::Compression::default(), true);

        let pixels = bgrx(64);
        let mut solid = vec![1u8];
        solid.extend(cp(RED_RGB));

        let mut decoders = Decoders::default();
        // Alternate them, twice each, so a shared stream would fail on the second
        // rectangle of whichever came second.
        for _ in 0..2 {
            let zlib = zlib_payload(&chunk(&mut zlib_stream, &pixels));
            assert_eq!(
                decoders.decode(&mut zlib.as_slice(), Payload::Zlib, &shadow, None, 8, 8).await.unwrap(),
                Decoded::Pixels(bgrx_to_rgb(&pixels))
            );
            let zrle = zlib_payload(&chunk(&mut zrle_stream, &solid));
            assert_eq!(
                decoders.decode(&mut zrle.as_slice(), Payload::Zrle, &shadow, None, 8, 8).await.unwrap(),
                Decoded::Pixels(solid_rgb(RED_RGB, 64))
            );
        }

        // And the negative: a ZRLE chunk offered to the zlib stream, which has a
        // window and a history of its own.
        let zrle = zlib_payload(&chunk(&mut zrle_stream, &solid));
        assert!(
            decoders.decode(&mut zrle.as_slice(), Payload::Zlib, &shadow, None, 8, 8).await.is_err()
        );
    }

    #[tokio::test]
    async fn a_zrle_rect_claiming_more_compressed_bytes_than_its_tiles_could_need_is_refused() {
        let wire = u32::MAX.to_be_bytes();
        let err = Decoders::default()
            .decode(&mut wire.as_slice(), Payload::Zrle, &unused_shadow(), None, 2, 2)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("even uncompressed"), "{err:#}");
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
        let copied = copy_rect(&mut wire.as_slice(), &shadow, None, 4, 1).await.unwrap();
        assert_eq!(copied, Decoded::Pixels(rgb[..4 * 3].to_vec()));
    }

    /// The same overlap through the client's own canvas: nothing is read out, the
    /// shadow moves its pixels where the browser is about to move its own, and what
    /// comes back is the source rectangle rather than a copy of it.
    #[tokio::test]
    async fn a_copy_the_client_can_do_moves_the_shadow_and_names_the_source() {
        let rgb: Vec<u8> = (0..8u8).flat_map(|i| [i * 0x10, 0x20, 0x30]).collect();
        let shadow = shadow_of(8, 1, &rgb);

        let wire = copy_rect_payload(0, 0);
        let copied = copy_rect(&mut wire.as_slice(), &shadow, Some((2, 0)), 4, 1).await.unwrap();
        assert_eq!(copied, Decoded::Copied(Rect::from_size(0, 0, 4, 1).unwrap()));

        // [2..6] now holds what [0..4] held, taken before the write rather than after.
        let moved = shadow
            .lock()
            .unwrap()
            .copy_out(Rect::from_size(2, 0, 4, 1).unwrap())
            .expect("the destination is known");
        assert_eq!(moved, rgb[..4 * 3]);
    }

    /// A copy onto the pixels already there is a record and a blit for nothing, and
    /// the shadow is what knows so.
    #[tokio::test]
    async fn a_copy_that_changes_nothing_is_not_sent() {
        let rgb: Vec<u8> = std::iter::repeat_n([0x10, 0x20, 0x30], 8).flatten().collect();
        let shadow = shadow_of(8, 1, &rgb);
        let wire = copy_rect_payload(0, 0);
        assert_eq!(
            copy_rect(&mut wire.as_slice(), &shadow, Some((4, 0)), 4, 1).await.unwrap(),
            Decoded::Unchanged
        );
    }

    #[tokio::test]
    async fn a_copy_rect_from_an_unknown_region_asks_for_nothing_rather_than_guessing() {
        // A shadow that knows its left half only. Built per iteration, because a copy
        // the client can do writes into the shadow and would make the right half
        // known for the reading after it.
        let rgb: Vec<u8> = (0..4u8).flat_map(|i| [i * 0x10, 0x20, 0x30]).collect();
        let half = || {
            let mut shadow = Shadow::new("test", 8, 1);
            shadow.accept(Rect::from_size(0, 0, 4, 1).unwrap(), &rgb);
            std::sync::Mutex::new(shadow)
        };

        // Both readings of a CopyRect refuse the same source, because both rest on
        // the same claim: that this side knows what is there.
        for copies in [false, true] {
            let shadow = half();
            let known = copy_rect_payload(0, 0);
            assert_ne!(
                copy_rect(&mut known.as_slice(), &shadow, copies.then_some((4, 0)), 4, 1).await.unwrap(),
                Decoded::Unavailable,
                "copies = {copies}"
            );

            // One pixel into the half nothing has ever painted.
            let shadow = half();
            let unknown = copy_rect_payload(1, 0);
            assert_eq!(
                copy_rect(&mut unknown.as_slice(), &shadow, copies.then_some((4, 0)), 4, 1).await.unwrap(),
                Decoded::Unavailable,
                "copies = {copies}"
            );
        }
    }

    /// Off the edge of the framebuffer is the same answer as unknown — one repaint,
    /// not a dead session — and the four bytes are still consumed.
    #[tokio::test]
    async fn a_copy_rect_sourced_off_the_framebuffer_is_refused_not_fatal() {
        let rgb = vec![0x10, 0x20, 0x30];
        let shadow = shadow_of(1, 1, &rgb);
        let wire = copy_rect_payload(1, 0);
        let mut reader = wire.as_slice();
        assert_eq!(
            copy_rect(&mut reader, &shadow, Some((0, 0)), 1, 1).await.unwrap(),
            Decoded::Unavailable
        );
        assert!(reader.is_empty(), "the source position is consumed either way");
    }

    /// And the other end of the same rectangle. `read_rect` bounds-checks a
    /// destination before any of this runs, so this is [`Shadow::copy_within`]'s own
    /// guard rather than a case the engine can reach — but the answer it gives is
    /// what decides whether a `COPY` record can be trusted, and "all of it changed,
    /// recorded nowhere" is what it would otherwise inherit from `accept`.
    #[tokio::test]
    async fn a_copy_rect_landing_off_the_framebuffer_is_refused_too() {
        let rgb: Vec<u8> = (0..8u8).flat_map(|i| [i * 0x10, 0x20, 0x30]).collect();
        let shadow = shadow_of(8, 1, &rgb);
        // Four pixels wide starting at 6, on a framebuffer eight wide.
        let wire = copy_rect_payload(0, 0);
        let mut reader = wire.as_slice();
        assert_eq!(
            copy_rect(&mut reader, &shadow, Some((6, 0)), 4, 1).await.unwrap(),
            Decoded::Unavailable
        );
        assert!(reader.is_empty(), "the source position is consumed either way");
    }
}
