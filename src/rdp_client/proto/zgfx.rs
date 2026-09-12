//! RDP 8 bulk compression, which every graphics pipeline PDU is wrapped in.
//!
//! MS-RDPEGFX puts each message on the wire inside an `RDP_SEGMENTED_DATA`: a
//! descriptor byte saying whether one segment follows or several, and then the
//! segments, each of which is either plain bytes or a bitstream in the RDP 8 bulk
//! compressor's own scheme. That scheme is an LZ77 variant with a fixed Huffman
//! table: a literal byte, a match of `count` bytes found `distance` bytes back in
//! everything decoded so far, or a run of unencoded bytes. "Everything decoded so
//! far" is the point — the history is a 2.5 MB ring shared by every PDU on the
//! channel for the life of the channel, which is why [`Zgfx`] is a value kept and
//! not a function called.
//!
//! # One direction only
//!
//! Only the server compresses. A Windows host reads the RDPGFX header straight off
//! the channel for a client's own PDUs — the caps advertise, the frame
//! acknowledgement — so those go out raw and never pass through here; a client that
//! wrapped them would have the host read the descriptor byte as a command id and
//! fail its graphics subsystem. [`wrap`] builds the simplest valid segmented data —
//! one uncompressed segment, the shape the compressor emits when it declines to
//! compress — which is how a test stands in for a server that sent plain bytes.
//!
//! A port of FreeRDP's `libfreerdp/codec/zgfx.c`, against [MS-RDPEGFX] 2.2.5.
//!
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0

use super::wire::{Malformed, Reader};

const WHAT: &str = "a ZGFX segmented data PDU";

/// How far back a match may reach: the compressor's own history is this large, so a
/// decompressor that keeps less would miss matches a server is entitled to make.
pub const HISTORY_SIZE: usize = 2_500_000;

/// The most one segment decompresses to. The specification says 65535; the extra
/// byte is FreeRDP's own buffer, and a stream that reaches either is not one a real
/// server produced.
const SEGMENT_MAX: usize = 65_536;

/// `RDP_SEGMENTED_DATA` descriptors.
const SINGLE: u8 = 0xE0;
const MULTIPART: u8 = 0xE1;

/// `RDP_DATA_SEGMENT` header bits: `PACKET_COMPR_TYPE_RDP8`, which is the only
/// compression type this channel has, and `PACKET_COMPRESSED`.
const RDP8: u8 = 0x04;
const COMPRESSED: u8 = 0x20;

/// One row of the Huffman table: a prefix of `length` bits equal to `code` names
/// either a literal (`Literal`, whose value is `base` plus `bits` more bits) or a
/// match distance (`Match`, the same sum) — and a distance of zero is not a match at
/// all but a run of unencoded bytes.
struct Token {
    length: u32,
    code: u32,
    bits: u32,
    kind: Kind,
    base: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Literal,
    Match,
}

/// [MS-RDPEGFX] 2.2.5.2.1.1, in the order FreeRDP walks it: by prefix length, so the
/// first row whose prefix matches is the row.
const TOKENS: [Token; 40] = [
    Token { length: 1, code: 0, bits: 8, kind: Kind::Literal, base: 0 },
    Token { length: 5, code: 17, bits: 5, kind: Kind::Match, base: 0 },
    Token { length: 5, code: 18, bits: 7, kind: Kind::Match, base: 32 },
    Token { length: 5, code: 19, bits: 9, kind: Kind::Match, base: 160 },
    Token { length: 5, code: 20, bits: 10, kind: Kind::Match, base: 672 },
    Token { length: 5, code: 21, bits: 12, kind: Kind::Match, base: 1696 },
    Token { length: 5, code: 24, bits: 0, kind: Kind::Literal, base: 0x00 },
    Token { length: 5, code: 25, bits: 0, kind: Kind::Literal, base: 0x01 },
    Token { length: 6, code: 44, bits: 14, kind: Kind::Match, base: 5792 },
    Token { length: 6, code: 45, bits: 15, kind: Kind::Match, base: 22176 },
    Token { length: 6, code: 52, bits: 0, kind: Kind::Literal, base: 0x02 },
    Token { length: 6, code: 53, bits: 0, kind: Kind::Literal, base: 0x03 },
    Token { length: 6, code: 54, bits: 0, kind: Kind::Literal, base: 0xFF },
    Token { length: 7, code: 92, bits: 18, kind: Kind::Match, base: 54944 },
    Token { length: 7, code: 93, bits: 20, kind: Kind::Match, base: 317_088 },
    Token { length: 7, code: 110, bits: 0, kind: Kind::Literal, base: 0x04 },
    Token { length: 7, code: 111, bits: 0, kind: Kind::Literal, base: 0x05 },
    Token { length: 7, code: 112, bits: 0, kind: Kind::Literal, base: 0x06 },
    Token { length: 7, code: 113, bits: 0, kind: Kind::Literal, base: 0x07 },
    Token { length: 7, code: 114, bits: 0, kind: Kind::Literal, base: 0x08 },
    Token { length: 7, code: 115, bits: 0, kind: Kind::Literal, base: 0x09 },
    Token { length: 7, code: 116, bits: 0, kind: Kind::Literal, base: 0x0A },
    Token { length: 7, code: 117, bits: 0, kind: Kind::Literal, base: 0x0B },
    Token { length: 7, code: 118, bits: 0, kind: Kind::Literal, base: 0x3A },
    Token { length: 7, code: 119, bits: 0, kind: Kind::Literal, base: 0x3B },
    Token { length: 7, code: 120, bits: 0, kind: Kind::Literal, base: 0x3C },
    Token { length: 7, code: 121, bits: 0, kind: Kind::Literal, base: 0x3D },
    Token { length: 7, code: 122, bits: 0, kind: Kind::Literal, base: 0x3E },
    Token { length: 7, code: 123, bits: 0, kind: Kind::Literal, base: 0x3F },
    Token { length: 7, code: 124, bits: 0, kind: Kind::Literal, base: 0x40 },
    Token { length: 7, code: 125, bits: 0, kind: Kind::Literal, base: 0x80 },
    Token { length: 8, code: 188, bits: 20, kind: Kind::Match, base: 1_365_664 },
    Token { length: 8, code: 189, bits: 21, kind: Kind::Match, base: 2_414_240 },
    Token { length: 8, code: 252, bits: 0, kind: Kind::Literal, base: 0x0C },
    Token { length: 8, code: 253, bits: 0, kind: Kind::Literal, base: 0x38 },
    Token { length: 8, code: 254, bits: 0, kind: Kind::Literal, base: 0x39 },
    Token { length: 8, code: 255, bits: 0, kind: Kind::Literal, base: 0x66 },
    Token { length: 9, code: 380, bits: 22, kind: Kind::Match, base: 4_511_392 },
    Token { length: 9, code: 381, bits: 23, kind: Kind::Match, base: 8_705_696 },
    Token { length: 9, code: 382, bits: 24, kind: Kind::Match, base: 17_094_304 },
];

/// The decompressor for one channel: its history, and nothing else.
pub struct Zgfx {
    history: History,
}

impl Default for Zgfx {
    fn default() -> Self {
        Self::new()
    }
}

impl Zgfx {
    pub fn new() -> Self {
        Self { history: History { ring: vec![0; HISTORY_SIZE], at: 0 } }
    }

    /// Unwrap one channel PDU into `out`, which is cleared first.
    ///
    /// A multipart PDU announces its total, and a total the segments do not come to
    /// is refused rather than passed on short: the RDPGFX PDUs inside are
    /// length-delimited, and a truncated buffer would end mid-header.
    pub fn decompress(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> Result<(), Malformed> {
        out.clear();
        let mut r = Reader::new(WHAT, bytes);
        match r.u8()? {
            SINGLE => self.segment(r.rest(), out),
            MULTIPART => {
                let count = r.u16_le()?;
                let total = usize::try_from(r.u32_le()?).unwrap_or(usize::MAX);
                out.reserve(total.min(SEGMENT_MAX * usize::from(count)));
                for _ in 0..count {
                    let size = usize::try_from(r.u32_le()?).unwrap_or(usize::MAX);
                    let segment = r.bytes(size)?;
                    self.segment(segment, out)?;
                    if out.len() > total {
                        let length = u32::try_from(out.len()).unwrap_or(u32::MAX);
                        return Err(r.refuse("segments coming to more than the announced", length));
                    }
                }
                if out.len() != total {
                    let length = u32::try_from(out.len()).unwrap_or(u32::MAX);
                    return Err(r.refuse("segments coming to", length));
                }
                Ok(())
            }
            other => Err(r.refuse("a descriptor", other)),
        }
    }

    /// One `RDP_DATA_SEGMENT`, appended to `out`.
    fn segment(&mut self, segment: &[u8], out: &mut Vec<u8>) -> Result<(), Malformed> {
        let mut r = Reader::new(WHAT, segment);
        let flags = r.u8()?;
        if flags & RDP8 == 0 {
            return Err(r.refuse("a compression type in the segment header", flags));
        }
        let data = r.rest();
        if flags & COMPRESSED == 0 {
            if data.len() > SEGMENT_MAX {
                let length = u32::try_from(data.len()).unwrap_or(u32::MAX);
                return Err(r.refuse("an uncompressed segment of", length));
            }
            self.history.write(data);
            out.extend_from_slice(data);
            return Ok(());
        }
        // The last byte says how many bits of the one before it are padding.
        let Some((&padding, body)) = data.split_last() else {
            return Err(Malformed::Short { what: WHAT, len: segment.len(), at: r.at(), need: 1 });
        };
        let bits = body.len() * 8;
        if usize::from(padding) > bits {
            return Err(r.refuse("a padding count past the segment's bits", padding));
        }
        let mut bits = Bits { bytes: body, at: 0, acc: 0, have: 0, remaining: bits - usize::from(padding) };
        let start = out.len();
        while bits.remaining > 0 {
            let token = bits.token()?;
            match token.kind {
                Kind::Literal => {
                    let value = token.base + bits.take(token.bits)?;
                    let byte = u8::try_from(value).map_err(|_| bits.refuse("a literal", value))?;
                    if out.len() - start >= SEGMENT_MAX {
                        return Err(bits.refuse("a segment longer than", SEGMENT_MAX as u32));
                    }
                    out.push(byte);
                    self.history.write(&[byte]);
                }
                Kind::Match => {
                    let distance = token.base + bits.take(token.bits)?;
                    if distance != 0 {
                        let count = bits.match_length()?;
                        if out.len() - start + count > SEGMENT_MAX {
                            return Err(bits.refuse("a segment longer than", SEGMENT_MAX as u32));
                        }
                        self.history.copy_match(&bits, distance, count, out)?;
                    } else {
                        // A run of bytes written as they are, starting at the next
                        // whole byte of the stream.
                        let count = bits.take(15)? as usize;
                        let raw = bits.aligned_bytes(count)?;
                        if out.len() - start + count > SEGMENT_MAX {
                            return Err(bits.refuse("a segment longer than", SEGMENT_MAX as u32));
                        }
                        out.extend_from_slice(raw);
                        self.history.write(raw);
                    }
                }
            }
        }
        Ok(())
    }
}

/// The bitstream of one compressed segment, most significant bit first.
struct Bits<'a> {
    bytes: &'a [u8],
    /// The next byte to load into `acc`.
    at: usize,
    /// Bits loaded and not yet taken, `have` of them, in the low end.
    acc: u32,
    have: u32,
    /// Bits left to decode in the whole segment, padding excluded.
    remaining: usize,
}

impl Bits<'_> {
    /// The next `n` bits as a number. `n` is at most 24, which is the widest field
    /// in the table.
    fn take(&mut self, n: u32) -> Result<u32, Malformed> {
        if n as usize > self.remaining {
            return Err(Malformed::Short {
                what: WHAT,
                len: self.bytes.len(),
                at: self.at,
                need: (n as usize - self.remaining).div_ceil(8),
            });
        }
        while self.have < n {
            // A byte past the end reads as zero — the padding count already keeps
            // the decoder from taking those bits as data.
            let byte = self.bytes.get(self.at).copied().unwrap_or(0);
            self.at += 1;
            self.acc = (self.acc << 8) | u32::from(byte);
            self.have += 8;
        }
        self.remaining -= n as usize;
        self.have -= n;
        let value = self.acc >> self.have;
        self.acc &= (1_u32 << self.have) - 1;
        Ok(value)
    }

    /// The next token, by reading one prefix bit at a time until a row matches.
    fn token(&mut self) -> Result<&'static Token, Malformed> {
        let mut prefix = 0;
        let mut have = 0;
        for token in &TOKENS {
            while have < token.length {
                prefix = (prefix << 1) | self.take(1)?;
                have += 1;
            }
            if prefix == token.code {
                return Ok(token);
            }
        }
        Err(self.refuse("a token prefix", prefix))
    }

    /// How long a match is: three, or four and up in a doubling code.
    fn match_length(&mut self) -> Result<usize, Malformed> {
        if self.take(1)? == 0 {
            return Ok(3);
        }
        let mut count = 4_usize;
        let mut extra = 2;
        while self.take(1)? == 1 {
            count *= 2;
            extra += 1;
            if extra > 24 {
                return Err(self.refuse("a match length code of", extra));
            }
        }
        Ok(count + self.take(extra)? as usize)
    }

    /// `count` bytes from the next whole byte of the stream, dropping whatever bits
    /// were loaded ahead.
    fn aligned_bytes(&mut self, count: usize) -> Result<&[u8], Malformed> {
        self.remaining = self.remaining.saturating_sub(self.have as usize);
        self.have = 0;
        self.acc = 0;
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len());
        let Some(end) = end.filter(|_| count * 8 <= self.remaining) else {
            return Err(Malformed::Short {
                what: WHAT,
                len: self.bytes.len(),
                at: self.at,
                need: count,
            });
        };
        let raw = &self.bytes[self.at..end];
        self.at = end;
        self.remaining -= count * 8;
        Ok(raw)
    }

    fn refuse(&self, field: &'static str, value: u32) -> Malformed {
        Malformed::Refused { what: WHAT, field, value: u64::from(value) }
    }
}

/// Everything decoded on the channel so far, the most recent [`HISTORY_SIZE`] bytes
/// of it, as a ring.
struct History {
    ring: Vec<u8>,
    /// Where the next byte goes.
    at: usize,
}

impl History {
    fn write(&mut self, mut src: &[u8]) {
        let size = self.ring.len();
        if src.len() > size {
            // Only the tail can still be reached by a match.
            let residue = src.len() - size;
            src = &src[residue..];
            self.at = (self.at + residue) % size;
        }
        let front = (size - self.at).min(src.len());
        self.ring[self.at..self.at + front].copy_from_slice(&src[..front]);
        self.ring[..src.len() - front].copy_from_slice(&src[front..]);
        self.at = (self.at + src.len()) % size;
    }

    /// Append `count` bytes found `distance` back to `out`, and to the history. A
    /// match longer than its distance repeats itself, which is how a run is coded.
    fn copy_match(
        &mut self,
        bits: &Bits<'_>,
        distance: u32,
        count: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), Malformed> {
        let size = self.ring.len();
        let distance = usize::try_from(distance).unwrap_or(usize::MAX);
        if distance > size {
            return Err(bits.refuse("a match further back than the history", distance as u32));
        }
        let start = out.len();
        let mut index = (self.at + size - distance) % size;
        for _ in 0..count.min(distance) {
            out.push(self.ring[index]);
            index = (index + 1) % size;
        }
        for i in distance..count {
            let byte = out[start + i - distance];
            out.push(byte);
        }
        self.write(&out[start..]);
        Ok(())
    }
}

/// A single uncompressed segment wrapping `payload` — the simplest valid
/// `RDP_SEGMENTED_DATA`. Used to build a server's PDUs in tests; the client's own
/// PDUs go out raw (see the module docs).
#[cfg(test)]
pub fn wrap(payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(payload.len() + 2);
    bytes.push(SINGLE);
    bytes.push(RDP8);
    bytes.extend_from_slice(payload);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample in [MS-RDPEGFX] 4.1, as FreeRDP's own tests carry it.
    const FOX: &[u8] = b"The quick brown fox jumps over the lazy dog";

    const FOX_SINGLE: &[u8] =
        b"\xE0\x04\x54\x68\x65\x20\x71\x75\x69\x63\x6B\x20\x62\x72\x6F\x77\
          \x6E\x20\x66\x6F\x78\x20\x6A\x75\x6D\x70\x73\x20\x6F\x76\x65\x72\
          \x20\x74\x68\x65\x20\x6C\x61\x7A\x79\x20\x64\x6F\x67";

    /// Three segments, the third of them compressed: a match back into what the
    /// first two put in the history.
    const FOX_MULTIPART: &[u8] =
        b"\xE1\x03\x00\x2B\x00\x00\x00\x11\x00\x00\x00\x04\x54\x68\x65\x20\
          \x71\x75\x69\x63\x6B\x20\x62\x72\x6F\x77\x6E\x20\x0E\x00\x00\x00\
          \x04\x66\x6F\x78\x20\x6A\x75\x6D\x70\x73\x20\x6F\x76\x65\x10\x00\
          \x00\x00\x24\x39\x08\x0E\x91\xF8\xD8\x61\x3D\x1E\x44\x06\x43\x79\
          \x9C\x02";

    fn decompressed(zgfx: &mut Zgfx, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        zgfx.decompress(bytes, &mut out).expect("a well-formed PDU");
        out
    }

    #[test]
    fn a_single_uncompressed_segment_is_its_own_bytes() {
        assert_eq!(decompressed(&mut Zgfx::new(), FOX_SINGLE), FOX);
    }

    /// The vector that exercises the bit decoder: the third segment codes "r the
    /// lazy dog" against the history the first two left behind.
    #[test]
    fn a_multipart_pdu_is_its_segments_joined_with_matches_into_the_history() {
        assert_eq!(decompressed(&mut Zgfx::new(), FOX_MULTIPART), FOX);
    }

    /// The history is the channel's, not the PDU's: a match in one PDU may reach
    /// into the PDU before it. Two copies of the fox, the second coded as a single
    /// match of 43 bytes at distance 43.
    #[test]
    fn the_history_outlives_one_pdu() {
        let mut zgfx = Zgfx::new();
        assert_eq!(decompressed(&mut zgfx, FOX_SINGLE), FOX);

        // A match token: prefix 10010 (distance base 32, 7 bits) with 11 more = 43,
        // then the length code for 43: 1, then 1 1 1 0 (count 32, extra 5), then 5
        // bits of 11 = 01011. Bits: 10010 0001011 1 1110 01011 = 22 bits.
        let bits = 0b10_0100_0010_1111_1100_1011_u32;
        let padded = bits << 2; // to 24 bits, 2 bits of padding
        let mut pdu = vec![SINGLE, RDP8 | COMPRESSED];
        pdu.extend_from_slice(&padded.to_be_bytes()[1..]);
        pdu.push(2);
        assert_eq!(decompressed(&mut zgfx, &pdu), FOX);
    }

    /// A match longer than its distance repeats what it copies, which is how a run
    /// of one byte is written.
    #[test]
    fn a_match_longer_than_its_distance_repeats_itself() {
        let mut zgfx = Zgfx::new();
        assert_eq!(decompressed(&mut zgfx, &wrap(b"ab")), b"ab");
        // Distance 2 (prefix 10001, 5 bits = 00010), length 6: 1, then 0 (count 4,
        // extra 2), then 10. Bits: 10001 00010 1 0 10 = 14 bits, 2 of padding.
        let bits = 0b10_0010_0010_1010_u32 << 2;
        let mut pdu = vec![SINGLE, RDP8 | COMPRESSED];
        pdu.extend_from_slice(&(bits as u16).to_be_bytes());
        pdu.push(2);
        assert_eq!(decompressed(&mut zgfx, &pdu), b"ababab");
    }

    /// The other end's PDUs are single uncompressed segments, and they read back as
    /// what went in.
    #[test]
    fn what_is_wrapped_here_is_the_shape_the_server_sends_too() {
        assert_eq!(wrap(b"abc"), vec![0xE0, 0x04, b'a', b'b', b'c']);
        assert_eq!(decompressed(&mut Zgfx::new(), &wrap(FOX)), FOX);
    }

    #[test]
    fn a_multipart_total_the_segments_do_not_come_to_is_refused() {
        let mut short = FOX_MULTIPART.to_vec();
        short[3] = 0x2C; // one more than the segments hold
        let err = Zgfx::new().decompress(&short, &mut Vec::new()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "a ZGFX segmented data PDU carries segments coming to 0x2b, which this client does \
             not accept"
        );
    }

    #[test]
    fn a_descriptor_that_is_neither_single_nor_multipart_is_refused() {
        let err = Zgfx::new().decompress(&[0xE2, 0x04, 1], &mut Vec::new()).unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "a descriptor", value: 0xE2, .. }));
        let err = Zgfx::new().decompress(&[], &mut Vec::new()).unwrap_err();
        assert!(matches!(err, Malformed::Short { .. }));
    }

    /// A bitstream that ends inside a token is short, not a panic and not a loop.
    #[test]
    fn a_stream_that_runs_out_mid_token_is_short() {
        // One byte of bits, all of them claimed as data: a literal prefix 0 and only
        // seven bits of its eight-bit value.
        let pdu = [SINGLE, RDP8 | COMPRESSED, 0x00, 0x00];
        let err = Zgfx::new().decompress(&pdu, &mut Vec::new()).unwrap_err();
        assert!(matches!(err, Malformed::Short { .. }), "{err}");
    }

    /// The ring wraps: bytes written past its end land at its start, and a match
    /// across the seam reads both halves.
    #[test]
    fn the_history_ring_wraps_around() {
        let mut history = History { ring: vec![0; 8], at: 6 };
        history.write(b"abcd");
        assert_eq!(history.ring, b"cd\0\0\0\0ab");
        assert_eq!(history.at, 2);
        let bits = Bits { bytes: &[], at: 0, acc: 0, have: 0, remaining: 0 };
        let mut out = Vec::new();
        history.copy_match(&bits, 4, 4, &mut out).unwrap();
        assert_eq!(out, b"abcd");
        // A write longer than the ring keeps only its tail.
        history.write(b"0123456789");
        assert_eq!(history.ring.iter().filter(|b| b.is_ascii_digit()).count(), 8);
        let mut out = Vec::new();
        history.copy_match(&bits, 8, 8, &mut out).unwrap();
        assert_eq!(out, b"23456789");
    }
}
