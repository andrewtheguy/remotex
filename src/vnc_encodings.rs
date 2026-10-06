//! The RFB pixel encodings, decoded to packed RGB888.
//!
//! Two of them: ZRLE, the one encoding every target is asked for, and Raw, which
//! RFB lets a server send whatever the list says. Both return the rectangle's
//! pixels in the one format the tile path takes — `w * h * 3`, no padding, no
//! stride — so [`crate::vnc`]'s rectangle reader has one bounds check, one shadow
//! comparison, one crop and one sink call. Nothing here knows about tiles, sinks or
//! the browser, which is what lets a decoder be tested against handwritten wire
//! bytes.
//!
//! ## The colour order, which is the thing to get wrong
//!
//! `set_pixel_format` forces 32 bits per pixel, depth 24, little-endian, with red
//! shifted 16, green 8 and blue 0. So **every colour on the wire is `B, G, R, X`**
//! — a raw pixel and, less its fourth byte, a ZRLE CPIXEL. A grey test pixel
//! cannot catch a swapped channel, so the tests in this module use asymmetric
//! colours throughout.
//!
//! ## What is deliberately absent
//!
//! Every other encoding. CopyRect, RRE, Hextile and zlib are what a server falls
//! back to without ZRLE, and the servers this gateway is used with all have it;
//! Tight and TightPNG are vendor encodings; JPEG and H.264 are lossy. Advertising
//! an encoding is a promise to decode it, so none of them is listed, and a
//! rectangle in one ends the session.

use anyhow::Context as _;
use log::info;
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::vnc::{BPP, discard};

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

/// ZRLE's tile edge.
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
/// so one bounds check and one tile path serve both.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// `w * h` pixels in the forced wire format (encoding 0).
    Raw,
    /// A `u32` length, then that much of the connection's deflate stream, holding
    /// 64x64 tiles that are run-length encoded, palettised, or both (encoding 16).
    Zrle,
}

impl Payload {
    /// What to call this in a log line.
    fn name(self) -> &'static str {
        match self {
            Payload::Raw => "raw",
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
    /// ZRLE's inflate stream. Created on the first ZRLE rectangle and never reset
    /// — see [`Inflater`].
    zrle: Option<Inflater>,
    /// A ZRLE rectangle has been stepped over uninflated ([`Self::step_over`]), so
    /// ZRLE's stream has a gap no later chunk can be inflated across.
    zrle_gap: bool,
    /// Which encodings have already been announced in the log.
    seen: Vec<Payload>,
}

impl Decoders {
    /// Decode one rectangle's payload.
    ///
    /// The payload is read whatever the geometry says, including for a rectangle of
    /// no pixels: ZRLE still sends its length word and its flush, and consuming
    /// them is what keeps the stream in step. The RFB stream has no framing of its
    /// own above the record layer, so walking past by the wrong number of bytes
    /// desyncs everything after it.
    pub async fn decode<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        payload: Payload,
        w: u16,
        h: u16,
    ) -> anyhow::Result<Vec<u8>> {
        self.note(payload);
        match payload {
            Payload::Raw => raw(reader, w, h).await,
            Payload::Zrle => self.zrle(reader, w, h).await,
        }
    }

    /// Read past one rectangle's payload for a session that shows none of them,
    /// producing no pixels.
    ///
    /// Both are stepped over by their length alone, and ZRLE's stream is left
    /// uninflated: since its chunks deflate across the connection, no ZRLE
    /// rectangle can be decoded after one has been stepped over, and one that is
    /// ends the session rather than inflating garbage.
    pub async fn step_over<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        payload: Payload,
        w: u16,
        h: u16,
    ) -> anyhow::Result<()> {
        self.note(payload);
        match payload {
            Payload::Raw => discard(reader, (usize::from(w) * usize::from(h) * BPP) as u64).await,
            Payload::Zrle => {
                let len = zrle_length(reader, w, h).await?;
                self.zrle_gap = true;
                discard(reader, u64::from(len)).await
            }
        }
    }

    /// Encoding 16: 64x64 tiles inside a deflate stream, each tile run-length
    /// encoded, palettised, both, or neither.
    ///
    /// The redundancy is taken out per tile *before* deflate ever sees the bytes.
    async fn zrle<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        w: u16,
        h: u16,
    ) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            !self.zrle_gap,
            "a zrle rect to decode after one was stepped over, which its stream cannot inflate across"
        );
        let cap = zrle_ceiling(w, h);
        let len = zrle_length(reader, w, h).await?;
        let mut chunk = vec![0u8; len as usize];
        reader.read_exact(&mut chunk).await?;
        let inflated = self
            .zrle
            .get_or_insert_with(|| Inflater::new("zrle"))
            .capped(&chunk, cap)?;
        zrle_tiles(&inflated, w, h)
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
}

/// A ZRLE rectangle's compressed length, held to what its geometry can justify.
async fn zrle_length<R: AsyncRead + Unpin>(reader: &mut R, w: u16, h: u16) -> anyhow::Result<u32> {
    let cap = zrle_ceiling(w, h);
    let len = reader.read_u32().await?;
    // Deflate *expands* a small payload — the stream header and one sync flush
    // cost more than a 1x1 rectangle's pixels — so the compressed bound has to be
    // generous. What keeps it honest is that the *inflated*
    // bytes are bounded by geometry and then spent exactly, tile by tile.
    let ceiling = (cap + cap / 64 + 1024).min(MAX_COMPRESSED);
    anyhow::ensure!(
        u64::from(len) <= ceiling as u64,
        "a zrle rect claims {len} compressed bytes for {w}x{h}, past the {ceiling} \
         its tiles could need even uncompressed"
    );
    Ok(len)
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

/// Encoding 0: `w * h` pixels in the forced wire format, which a server may send
/// whatever the client listed.
async fn raw<R: AsyncRead + Unpin>(reader: &mut R, w: u16, h: u16) -> anyhow::Result<Vec<u8>> {
    let mut pixels = vec![0u8; usize::from(w) * usize::from(h) * BPP];
    reader.read_exact(&mut pixels).await?;
    Ok(bgrx_to_rgb(&pixels))
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
/// A `size` of no pixels is a no-op rather than an error: RFB lets a
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
///
/// Sized writes into a zeroed buffer rather than a byte-at-a-time `extend`: the
/// fixed 4-in/3-out stride is what lets the compiler vectorize the shuffle.
pub fn bgrx_to_rgb(bgrx: &[u8]) -> Vec<u8> {
    let mut rgb = vec![0u8; bgrx.len() / BPP * 3];
    for (out, px) in rgb.as_chunks_mut::<3>().0.iter_mut().zip(bgrx.as_chunks::<BPP>().0) {
        out[0] = px[2];
        out[1] = px[1];
        out[2] = px[0];
    }
    rgb
}

/// An inflater whose lifetime is chosen by the encoding that owns it.
///
/// Keep this private: the connection-lifetime stream belongs to [`Decoders`], while
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
    /// ZRLE's inflated size is not implied by its rectangle — a tile's byte count
    /// depends on the subencoding it chose — so the geometry
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
                // making no progress. One byte past `cap`, because zlib consumes
                // no input once it has nowhere to put the output: a rectangle of
                // no pixels is all flush, and with no room at all the flush would
                // be left for the next rectangle's chunk to be decoded as.
                let want = (out.capacity().max(4096) * 2).min(cap + 1);
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
                anyhow::ensure!(
                    out.len() <= cap,
                    "a {} rectangle inflated past the {cap} bytes its geometry allows",
                    self.what
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

    /// A ZRLE rectangle as it arrives: the `u32` length, then the chunk.
    fn zrle_payload(chunk: &[u8]) -> Vec<u8> {
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

    const BLUE_RGB: [u8; 3] = [0x00, 0x00, 0xf0];
    const GREEN_RGB: [u8; 3] = [0x00, 0xf0, 0x00];

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

    /// One stream across rectangles: two chunks of one deflate stream inflate in
    /// sequence and fail separately.
    #[tokio::test]
    async fn zrle_inflates_across_rectangles_but_not_out_of_order() {
        let mut deflate = flate2::Compress::new(flate2::Compression::default(), true);
        let mut red = vec![1u8];
        red.extend(cp(RED_RGB));
        let mut blue = vec![1u8];
        blue.extend(cp(BLUE_RGB));
        let a = zrle_payload(&chunk(&mut deflate, &red));
        let b = zrle_payload(&chunk(&mut deflate, &blue));

        let mut decoders = Decoders::default();
        assert_eq!(
            decoders.decode(&mut a.as_slice(), Payload::Zrle, 8, 8).await.unwrap(),
            solid_rgb(RED_RGB, 64)
        );
        assert_eq!(
            decoders.decode(&mut b.as_slice(), Payload::Zrle, 8, 8).await.unwrap(),
            solid_rgb(BLUE_RGB, 64)
        );

        // The second chunk alone, through a stream that never saw the first: no
        // zlib header, no window, nothing.
        let mut fresh = Decoders::default();
        assert!(fresh.decode(&mut b.as_slice(), Payload::Zrle, 8, 8).await.is_err());
    }

    /// A rectangle of no pixels is all sync flush: no output, but input the
    /// connection's inflater still has to swallow. Leaving those bytes unfed would
    /// have the next rectangle's chunk decoded as them, which is a desync — every
    /// rectangle after it is wrong, not just this one.
    #[tokio::test]
    async fn a_zrle_rect_of_no_pixels_still_advances_the_stream() {
        let mut deflate = flate2::Compress::new(flate2::Compression::default(), true);
        // What a server sends for a 0x0 rect: the stream header and a flush, framing
        // no tiles at all. Then a real rectangle behind it.
        let empty = zrle_payload(&chunk(&mut deflate, &[]));
        let mut solid = vec![1u8];
        solid.extend(cp(RED_RGB));
        let real = zrle_payload(&chunk(&mut deflate, &solid));

        let mut decoders = Decoders::default();
        assert_eq!(
            decoders.decode(&mut empty.as_slice(), Payload::Zrle, 0, 0).await.unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(
            decoders.decode(&mut real.as_slice(), Payload::Zrle, 8, 8).await.unwrap(),
            solid_rgb(RED_RGB, 64),
            "the rectangle behind the empty one"
        );
    }

    /// A session that shows none of the rectangles steps over ZRLE by its length,
    /// leaving the bytes behind it in step, and then refuses to decode one: the
    /// chunk it never inflated is part of the stream the next would inflate across.
    #[tokio::test]
    async fn a_stepped_over_zrle_rect_is_never_inflated_and_leaves_a_gap() {
        // Not a deflate stream at all, which an inflater would refuse.
        let mut wire = zrle_payload(&[0xff; 8]);
        wire.extend_from_slice(&bgrx(4));
        wire.push(0xaa);
        let mut reader = wire.as_slice();

        let mut decoders = Decoders::default();
        decoders.step_over(&mut reader, Payload::Zrle, 2, 2).await.unwrap();
        decoders.step_over(&mut reader, Payload::Raw, 2, 2).await.unwrap();
        assert_eq!(reader, [0xaa], "each stepped over by exactly its length");
        assert!(decoders.zrle.is_none(), "no inflater was ever made");

        let mut zrle_stream = flate2::Compress::new(flate2::Compression::default(), true);
        let mut solid = vec![1u8];
        solid.extend(cp(RED_RGB));
        let zrle = zrle_payload(&chunk(&mut zrle_stream, &solid));
        let err = decoders
            .decode(&mut zrle.as_slice(), Payload::Zrle, 8, 8)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("stepped over"), "{err:#}");
    }

    #[tokio::test]
    async fn a_zrle_rect_claiming_more_compressed_bytes_than_its_tiles_could_need_is_refused() {
        let wire = u32::MAX.to_be_bytes();
        let err = Decoders::default()
            .decode(&mut wire.as_slice(), Payload::Zrle, 2, 2)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("even uncompressed"), "{err:#}");
    }
}
