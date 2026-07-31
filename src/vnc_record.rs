//! Apple's record layer: the AES-128-CBC framed transport every byte of an RFB
//! 003.889 session rides inside once the server has handed over a key.
//!
//! Used by the `ard-high-performance` subtype (see [`crate::vnc`] and
//! [`crate::vnc_apple`]). Nothing here knows what a rectangle is — it is a
//! transport, and the RFB above it is unchanged.
//!
//! ## How the key arrives
//!
//! Not from a handshake of its own. The client sends `SetEncryption` in cleartext
//! and the server answers with a *rekey*: a 36-byte blob delivered, of all places,
//! inside an ordinary framebuffer rectangle ([`crate::vnc_apple::ENCODING_REKEY`]).
//! Its two halves are the content key and the initial IV, each wrapped under a
//! **wrap key** that both ends already hold from authentication — for Apple's DH
//! branch that is `MD5(shared)`, the same digest that encrypted the credentials.
//! [`unwrap_rekey`] undoes it.
//!
//! ## The frame
//!
//! ```text
//! wire       u16 ciphertext_len || byte[ciphertext_len] ciphertext
//! plaintext  u16 body_len || byte[body_len] body || filler || byte[20] integrity
//!            filler_len = (-(2 + body_len + 20)) mod 16
//! integrity  SHA1( u32_be(seq) || plaintext[0 .. ciphertext_len - 20] )
//! ```
//!
//! Three properties of it are load-bearing, and each is a silent failure if
//! missed:
//!
//! - **One CBC stream per direction, spanning the whole session.** Record `N`'s
//!   last ciphertext block is record `N+1`'s IV, so the context is never reset
//!   between records. A per-record context decrypts record 0 and then produces
//!   garbage that still passes as bytes.
//! - **A sequence number that never resets.** It is not on the wire; it is
//!   prepended to the hash, so the two ends only agree while they have counted the
//!   same number of records. Counting *messages* would drift the moment a server
//!   payload spanned two records.
//! - **Plain SHA-1, not HMAC.** The key authenticates nothing here; the trailer is
//!   an integrity check over a stream only the key holder can produce.
//!
//! The integrity trailer is checked before a single body byte is handed upward
//! (see [`RecordReader`]), so nothing downstream ever sees unverified pixels.
//!
//! ## Framing is per-message going out, a stream coming in
//!
//! One record carries exactly one client→server message. That asymmetry is why
//! the write side is [`RecordWriter::frame`], taking a whole message, while the
//! read side is an [`AsyncRead`] — a large server payload may span consecutive
//! records and is reassembled by concatenating their bodies, which is exactly what
//! a byte stream is.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt as _, BlockCipherEncrypt as _, KeyInit as _};
use anyhow::Context as _;
use sha1::{Digest as _, Sha1};
use tokio::io::{AsyncRead, ReadBuf};

/// AES block size, which is also the record layer's alignment.
const BLOCK: usize = 16;
/// Bytes of SHA-1 at the end of every record's plaintext.
const TRAILER: usize = 20;
/// The `u16 body_len` at the front of every record's plaintext.
const BODY_LEN: usize = 2;
/// Length of the rekey blob: `u32 generation`, then the wrapped key and IV.
pub const REKEY_LEN: usize = 4 + BLOCK + BLOCK;
/// Largest record the `u16` length prefix can describe, rounded down to a whole
/// number of blocks.
const MAX_CIPHERTEXT: usize = (u16::MAX as usize / BLOCK) * BLOCK;
/// Largest message that fits in one record, which is what caps a single
/// client→server message. Every message this client sends is a few dozen bytes,
/// so it is a sanity bound rather than a limit anything runs into.
pub const MAX_BODY: usize = MAX_CIPHERTEXT - BODY_LEN - TRAILER;

/// The AES-128 key and IV one rekey installed.
///
/// One value for both directions: they start from the same pair and diverge from
/// there as each side's chain advances at its own rate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Keys {
    pub key: [u8; BLOCK],
    pub iv: [u8; BLOCK],
}

/// Recover the record layer's key and IV from a rekey blob.
///
/// The two halves are single AES-128-**ECB** blocks, decrypted *independently* —
/// no chaining between them, and no relation to the CBC that follows. ECB appears
/// exactly here and nowhere else in the session.
///
/// The returned key is also the wrap key for any *subsequent* rekey, which is why
/// `generation` comes back rather than being checked here: this client refuses a
/// second rekey ([`crate::vnc`]) and the caller is where that refusal reads
/// sensibly.
pub fn unwrap_rekey(wrap_key: &[u8; BLOCK], body: &[u8; REKEY_LEN]) -> (u32, Keys) {
    let cipher = Aes128::new(wrap_key.into());
    let generation = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let unwrap = |offset: usize| {
        let mut block = [0u8; BLOCK];
        block.copy_from_slice(&body[offset..offset + BLOCK]);
        cipher.decrypt_block((&mut block).into());
        block
    };
    let key = unwrap(4);
    let iv = unwrap(4 + BLOCK);
    (generation, Keys { key, iv })
}

/// One direction's cipher, its chaining block, and its record counter.
///
/// A struct rather than a function because of what it must *not* do: the chain
/// carries from one record to the next and the counter never resets, so both have
/// to outlive any single record. Two of these exist per session, one each way, and
/// they are never swapped or rebuilt.
struct Cbc {
    cipher: Aes128,
    chain: [u8; BLOCK],
    seq: u32,
}

impl Cbc {
    fn new(keys: Keys) -> Self {
        Self {
            cipher: Aes128::new(&keys.key.into()),
            chain: keys.iv,
            seq: 0,
        }
    }

    /// CBC-encrypt in place, advancing the chain. `data` must be whole blocks.
    fn encrypt(&mut self, data: &mut [u8]) {
        for block in data.chunks_exact_mut(BLOCK) {
            for (b, c) in block.iter_mut().zip(self.chain) {
                *b ^= c;
            }
            let block: &mut [u8; BLOCK] = block.try_into().expect("whole AES block");
            self.cipher.encrypt_block(block.into());
            self.chain = *block;
        }
    }

    /// CBC-decrypt in place, advancing the chain. `data` must be whole blocks.
    fn decrypt(&mut self, data: &mut [u8]) {
        for block in data.chunks_exact_mut(BLOCK) {
            let block: &mut [u8; BLOCK] = block.try_into().expect("whole AES block");
            let ciphertext = *block;
            self.cipher.decrypt_block(block.into());
            for (b, c) in block.iter_mut().zip(self.chain) {
                *b ^= c;
            }
            self.chain = ciphertext;
        }
    }

    /// The integrity trailer for a record's plaintext, and the sequence number
    /// consumed in producing it. `covered` is the plaintext up to but excluding
    /// the trailer itself.
    fn trailer(&mut self, covered: &[u8]) -> [u8; TRAILER] {
        let mut hash = Sha1::new();
        hash.update(self.seq.to_be_bytes());
        hash.update(covered);
        self.seq = self.seq.wrapping_add(1);
        hash.finalize().into()
    }
}

/// Bytes of filler between a body and its trailer: the least that rounds the
/// plaintext up to a whole number of blocks.
fn filler_len(body_len: usize) -> usize {
    (BLOCK - (BODY_LEN + body_len + TRAILER) % BLOCK) % BLOCK
}

/// Frames outgoing messages, one record each.
///
/// [`Self::frame`] hands back bytes instead of writing them, which keeps framing a
/// pure function of the message and this context — testable straight through a
/// [`RecordReader`], and with no I/O inside the part that has to be exactly right.
pub struct RecordWriter {
    cbc: Cbc,
    /// Reused across records, so a steady session frames input events without
    /// allocating.
    buf: Vec<u8>,
}

impl RecordWriter {
    pub fn new(keys: Keys) -> Self {
        Self {
            cbc: Cbc::new(keys),
            buf: Vec::new(),
        }
    }

    /// Wrap one complete client→server message in one record.
    ///
    /// Filler is zeroed rather than random. The spec allows either, the CBC chain
    /// already makes two identical messages encrypt differently, and determinism
    /// is what lets the framing be asserted byte for byte in a test — worth more
    /// than padding entropy that protects nothing.
    pub fn frame(&mut self, msg: &[u8]) -> anyhow::Result<&[u8]> {
        anyhow::ensure!(
            msg.len() <= MAX_BODY,
            "a {}-byte message does not fit in one record (at most {MAX_BODY})",
            msg.len()
        );
        let total = BODY_LEN + msg.len() + filler_len(msg.len()) + TRAILER;
        self.buf.clear();
        self.buf.reserve(2 + total);
        // The outer length prefix, which is *not* encrypted and *not* covered by
        // the trailer.
        self.buf
            .extend_from_slice(&u16::try_from(total).expect("record within u16").to_be_bytes());
        let plaintext = self.buf.len();
        self.buf
            .extend_from_slice(&u16::try_from(msg.len()).expect("body within u16").to_be_bytes());
        self.buf.extend_from_slice(msg);
        self.buf.resize(plaintext + total - TRAILER, 0);
        let trailer = self.cbc.trailer(&self.buf[plaintext..]);
        self.buf.extend_from_slice(&trailer);
        self.cbc.encrypt(&mut self.buf[plaintext..]);
        Ok(&self.buf)
    }
}

/// Which part of a record is being read.
enum Phase {
    /// The two outer length bytes.
    Len,
    /// `len` bytes of ciphertext, decrypted in place once all of them are in.
    Ciphertext { len: usize },
}

/// Reads a record-layer stream and yields the concatenation of record bodies.
///
/// That concatenation is not an approximation of the reassembly rule — it *is*
/// the rule, which is why everything above this reads plaintext with the ordinary
/// `read_u8`/`read_exact` and never learns the records are there. A rectangle
/// whose pixels span four records is one `read_exact` that happens to drive four
/// decryptions.
///
/// Construct it around the reader the cleartext phase was using, buffer and all.
/// Re-wrapping the socket instead would drop whatever that buffer had already
/// pulled in — which by then is usually the front of the first record — and the
/// loss surfaces as an integrity failure rather than as the missing bytes it is.
pub struct RecordReader<R> {
    inner: R,
    cbc: Cbc,
    /// The record in flight: the length prefix while it is being read, then the
    /// ciphertext, decrypted in place. Reused between records.
    staging: Vec<u8>,
    /// How much of the current phase's target has arrived.
    filled: usize,
    phase: Phase,
    /// The verified body inside `staging` that has not been handed upward yet.
    body: std::ops::Range<usize>,
}

impl<R> RecordReader<R> {
    pub fn new(inner: R, keys: Keys) -> Self {
        Self {
            inner,
            cbc: Cbc::new(keys),
            staging: Vec::new(),
            filled: 0,
            phase: Phase::Len,
            body: 0..0,
        }
    }

    /// Decrypt and verify a complete record, leaving its body in `staging`.
    fn accept(&mut self, len: usize) -> io::Result<std::ops::Range<usize>> {
        self.cbc.decrypt(&mut self.staging[..len]);
        let covered = len - TRAILER;
        let expected = self.cbc.trailer(&self.staging[..covered]);
        // Constant-time is not the concern — an attacker who can replay records
        // learns nothing from timing a hash comparison here — but *closing* is:
        // a record that fails this has to stop the session, not be skipped.
        if self.staging[covered..len] != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a record failed its integrity check",
            ));
        }
        let body_len = usize::from(u16::from_be_bytes([self.staging[0], self.staging[1]]));
        if BODY_LEN + body_len + TRAILER > len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a record claims a {body_len}-byte body but is only {len} bytes"),
            ));
        }
        Ok(BODY_LEN..BODY_LEN + body_len)
    }
}

/// Read into `dst`, reporting how much arrived. Zero means end of stream.
fn poll_fill<R: AsyncRead + Unpin>(
    inner: &mut R,
    cx: &mut Context<'_>,
    dst: &mut [u8],
) -> Poll<io::Result<usize>> {
    let mut buf = ReadBuf::new(dst);
    ready!(Pin::new(inner).poll_read(cx, &mut buf))?;
    Poll::Ready(Ok(buf.filled().len()))
}

/// End of stream in the middle of a record, as opposed to between two of them.
fn truncated() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "the connection ended inside a record",
    )
}

impl<R: AsyncRead + Unpin> AsyncRead for RecordReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = &mut *self;
        loop {
            // Verified bytes first, and only ever those: nothing reaches a caller
            // before its record's trailer has been checked.
            if !me.body.is_empty() {
                let n = buf.remaining().min(me.body.len());
                let from = me.body.start;
                buf.put_slice(&me.staging[from..from + n]);
                me.body.start += n;
                return Poll::Ready(Ok(()));
            }
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            match me.phase {
                Phase::Len => {
                    me.staging.resize(BODY_LEN, 0);
                    let n = ready!(poll_fill(&mut me.inner, cx, &mut me.staging[me.filled..]))?;
                    if n == 0 {
                        // A hang-up *between* records is an ordinary close, and
                        // reporting it as end-of-stream is what lets the read loop
                        // above say "the server closed the connection" in its own
                        // words rather than under a decryption error.
                        return if me.filled == 0 {
                            Poll::Ready(Ok(()))
                        } else {
                            Poll::Ready(Err(truncated()))
                        };
                    }
                    me.filled += n;
                    if me.filled == BODY_LEN {
                        let len = usize::from(u16::from_be_bytes([me.staging[0], me.staging[1]]));
                        if len == 0 || len % BLOCK != 0 {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("a record claims {len} ciphertext bytes, not a non-zero multiple of {BLOCK}"),
                            )));
                        }
                        me.staging.resize(len, 0);
                        me.filled = 0;
                        me.phase = Phase::Ciphertext { len };
                    }
                }
                Phase::Ciphertext { len } => {
                    let n = ready!(poll_fill(&mut me.inner, cx, &mut me.staging[me.filled..len]))?;
                    if n == 0 {
                        return Poll::Ready(Err(truncated()));
                    }
                    me.filled += n;
                    if me.filled == len {
                        me.body = me.accept(len)?;
                        me.filled = 0;
                        me.phase = Phase::Len;
                    }
                }
            }
        }
    }
}

/// Read exactly `REKEY_LEN` bytes as a rekey blob. A free function because the
/// caller reads it off a *cleartext* stream, before any of this exists.
pub fn rekey_body(bytes: &[u8]) -> anyhow::Result<[u8; REKEY_LEN]> {
    bytes
        .try_into()
        .with_context(|| format!("a rekey rectangle carried {} bytes, not {REKEY_LEN}", bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt as _;

    /// The spec's own test vectors are unfilled placeholders, so these are
    /// self-consistent: they pin this implementation against itself and against
    /// the arithmetic in the spec's prose. That catches a chaining or counting
    /// mistake but could not catch a byte order both halves agreed on — which is
    /// why [`the_trailer_covers_the_sequence_number_big_endian`] pins a literal
    /// digest instead of a round trip.
    fn keys() -> Keys {
        Keys {
            key: *b"0123456789abcdef",
            iv: *b"fedcba9876543210",
        }
    }

    #[test]
    fn the_rekey_halves_are_unwrapped_independently() {
        let wrap = *b"wrapwrapwrapwrap";
        let cipher = Aes128::new(&wrap.into());
        let wrapped = |mut block: [u8; BLOCK]| {
            cipher.encrypt_block((&mut block).into());
            block
        };

        let mut blob = [0u8; REKEY_LEN];
        blob[..4].copy_from_slice(&7u32.to_be_bytes());
        blob[4..20].copy_from_slice(&wrapped(keys().key));
        blob[20..].copy_from_slice(&wrapped(keys().iv));

        let (generation, recovered) = unwrap_rekey(&wrap, &blob);
        assert_eq!(generation, 7);
        assert_eq!(recovered, keys());

        // Swap the two wrapped blocks and the recovered values swap with them —
        // which is only true if each is decrypted on its own. Chain them and the
        // second would come back as noise.
        blob[4..20].copy_from_slice(&wrapped(keys().iv));
        blob[20..].copy_from_slice(&wrapped(keys().key));
        let (_, swapped) = unwrap_rekey(&wrap, &blob);
        assert_eq!(swapped.key, keys().iv);
        assert_eq!(swapped.iv, keys().key);
    }

    #[test]
    fn filler_rounds_the_plaintext_up_to_a_block() {
        // (-(2 + n + 20)) mod 16, tabulated rather than recomputed, so a change to
        // the formula has to disagree with a number written out by hand.
        for (body, filler) in [
            (0usize, 10usize),
            (1, 9),
            (10, 0),
            (11, 15),
            (14, 12),
            (15, 11),
            (16, 10),
            (26, 0),
            (300, 14),
        ] {
            assert_eq!(filler_len(body), filler, "body {body}");
            assert_eq!((BODY_LEN + body + filler + TRAILER) % BLOCK, 0, "body {body}");
        }
    }

    #[test]
    fn a_framed_record_has_the_length_the_spec_computes() {
        let mut writer = RecordWriter::new(keys());
        let framed = writer.frame(&[0xaa; 10]).unwrap().to_vec();
        // 2 body-len + 10 body + 0 filler + 20 trailer = 32, and the outer prefix
        // counts only the ciphertext.
        assert_eq!(u16::from_be_bytes([framed[0], framed[1]]), 32);
        assert_eq!(framed.len(), 2 + 32);

        // An empty message is still a record.
        let framed = writer.frame(&[]).unwrap();
        assert_eq!(u16::from_be_bytes([framed[0], framed[1]]), 32);
    }

    #[test]
    fn the_trailer_covers_the_sequence_number_big_endian() {
        // A literal digest, because a round trip cannot tell big-endian from
        // little-endian when both ends agree on the mistake. Computed here from the
        // definition: SHA1(u32_be(seq) || plaintext-before-the-trailer).
        let plaintext = {
            let mut p = vec![0u8; BODY_LEN + 4 + filler_len(4)];
            p[..2].copy_from_slice(&4u16.to_be_bytes());
            p[2..6].copy_from_slice(b"ping");
            p
        };
        let mut cbc = Cbc::new(keys());
        assert_eq!(cbc.seq, 0);
        let first = cbc.trailer(&plaintext);
        assert_eq!(cbc.seq, 1);
        let second = cbc.trailer(&plaintext);
        // The same plaintext hashes differently at seq 1 — the counter is in the
        // hash, and this is the whole reason it may never be reset.
        assert_ne!(first, second);

        let expect = |seq: u32| {
            let mut h = Sha1::new();
            h.update(seq.to_be_bytes());
            h.update(&plaintext);
            <[u8; TRAILER]>::from(h.finalize())
        };
        assert_eq!(first, expect(0));
        assert_eq!(second, expect(1));
        // And it is big-endian, not host order. Checked at seq 1, since zero reads
        // the same either way and would let the mistake through.
        assert_ne!(second, {
            let mut h = Sha1::new();
            h.update(1u32.to_le_bytes());
            h.update(&plaintext);
            <[u8; TRAILER]>::from(h.finalize())
        });
    }

    /// The chain carries between records, so a context rebuilt per record decrypts
    /// the first one and then fails. One test for the rule that a persistent
    /// context is not an optimisation.
    #[tokio::test]
    async fn a_record_only_decrypts_in_sequence() {
        let mut writer = RecordWriter::new(keys());
        let mut wire = writer.frame(b"first").unwrap().to_vec();
        let second = writer.frame(b"second").unwrap().to_vec();
        wire.extend_from_slice(&second);

        let mut reader = RecordReader::new(std::io::Cursor::new(wire), keys());
        let mut got = [0u8; 11];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"firstsecond");

        // The second record alone, through a context that never saw the first.
        let mut fresh = RecordReader::new(std::io::Cursor::new(second), keys());
        let err = fresh.read_u8().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// The property the whole read side rests on: a payload split across records
    /// is one `read_exact` to everything above.
    #[tokio::test]
    async fn a_payload_split_across_records_reads_back_as_one() {
        let blob: Vec<u8> = (0..100u8).collect();
        let mut writer = RecordWriter::new(keys());
        let mut wire = writer.frame(&blob[..37]).unwrap().to_vec();
        let rest = writer.frame(&blob[37..]).unwrap().to_vec();
        wire.extend_from_slice(&rest);

        let mut reader = RecordReader::new(std::io::Cursor::new(wire), keys());
        let mut got = vec![0u8; 100];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(got, blob);
    }

    #[tokio::test]
    async fn a_tampered_record_yields_nothing_at_all() {
        let mut writer = RecordWriter::new(keys());
        let mut wire = writer.frame(b"payload").unwrap().to_vec();
        // Flip a byte of ciphertext. CBC will still "decrypt" it, and the trailer
        // is what notices.
        let last = wire.len() - 1;
        wire[last] ^= 0x01;

        let mut reader = RecordReader::new(std::io::Cursor::new(wire), keys());
        let mut got = [0u8; 7];
        let err = reader.read_exact(&mut got).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // Not one byte of the record escaped before the check.
        assert_eq!(got, [0u8; 7]);
    }

    #[tokio::test]
    async fn a_misframed_record_is_refused() {
        for (len, what) in [(0u16, "zero"), (17, "not a whole number of blocks")] {
            let mut wire = len.to_be_bytes().to_vec();
            wire.resize(2 + usize::from(len.max(16)), 0);
            let mut reader = RecordReader::new(std::io::Cursor::new(wire), keys());
            let err = reader.read_u8().await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{what}");
        }

        // A body longer than the record that carries it.
        let mut writer = RecordWriter::new(keys());
        let framed = writer.frame(b"short").unwrap().to_vec();
        let mut forged = Cbc::new(keys());
        let mut plaintext = vec![0u8; framed.len() - 2];
        plaintext[..2].copy_from_slice(&9000u16.to_be_bytes());
        let trailer = forged.trailer(&plaintext[..plaintext.len() - TRAILER]);
        let covered = plaintext.len() - TRAILER;
        plaintext[covered..].copy_from_slice(&trailer);
        forged.encrypt(&mut plaintext);
        let mut wire = framed[..2].to_vec();
        wire.extend_from_slice(&plaintext);

        let mut reader = RecordReader::new(std::io::Cursor::new(wire), keys());
        let err = reader.read_u8().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(format!("{err}").contains("9000"), "{err}");
    }

    #[tokio::test]
    async fn a_clean_hang_up_between_records_is_end_of_stream() {
        let mut writer = RecordWriter::new(keys());
        let wire = writer.frame(b"hi").unwrap().to_vec();
        let mut reader = RecordReader::new(std::io::Cursor::new(wire), keys());
        let mut got = Vec::new();
        reader.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"hi");

        // Cut inside a record instead, and it is an error rather than a quiet end.
        let mut writer = RecordWriter::new(keys());
        let framed = writer.frame(b"hi").unwrap();
        let cut = framed[..framed.len() - 4].to_vec();
        let mut reader = RecordReader::new(std::io::Cursor::new(cut), keys());
        let err = reader.read_to_end(&mut Vec::new()).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// A reader that yields one byte per poll, which is what a real socket does
    /// under load and what a hand-written state machine gets wrong.
    struct Trickle<R>(R);

    impl<R: AsyncRead + Unpin> AsyncRead for Trickle<R> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let mut one = [0u8; 1];
            let n = ready!(poll_fill(&mut self.0, cx, &mut one))?;
            if n == 1 {
                buf.put_slice(&one);
            }
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn records_reassemble_a_byte_at_a_time() {
        let blob: Vec<u8> = (0..200u8).collect();
        let mut writer = RecordWriter::new(keys());
        let mut wire = writer.frame(&blob[..64]).unwrap().to_vec();
        let a = writer.frame(&blob[64..70]).unwrap().to_vec();
        let b = writer.frame(&blob[70..]).unwrap().to_vec();
        wire.extend_from_slice(&a);
        wire.extend_from_slice(&b);

        let mut reader = RecordReader::new(Trickle(std::io::Cursor::new(wire)), keys());
        let mut got = vec![0u8; 200];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(got, blob);
    }

    #[test]
    fn a_message_too_large_for_one_record_is_refused() {
        let mut writer = RecordWriter::new(keys());
        assert!(writer.frame(&vec![0u8; MAX_BODY]).is_ok());
        let err = writer.frame(&vec![0u8; MAX_BODY + 1]).unwrap_err();
        assert!(format!("{err:#}").contains("does not fit in one record"), "{err:#}");
    }
}
