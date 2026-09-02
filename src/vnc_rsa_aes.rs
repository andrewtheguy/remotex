//! RealVNC's RSA-AES security types (RFB 5 and 129): an authenticated,
//! encrypted RFB session over an ordinary 3.8 wire, and the one way a plain
//! `vnc` target can tell a server *who* is connecting.
//!
//! The types are RealVNC's, documented in the community `rfbproto` and
//! implemented on the open side by TigerVNC (client and server) and neatvnc —
//! which is what wayvnc is, and the first server this was written against. A
//! server offering them typically offers nothing this client used to speak:
//! wayvnc with `enable_auth` lists VeNCrypt X509Plain and the two RSA-AES types
//! and no more, so without this the target was unreachable rather than merely
//! anonymous. VeNCrypt is the alternative, and it costs a TLS stack; RSA-AES
//! costs an RSA key pair, SHA-1 or SHA-256, and AES-EAX, all pure Rust.
//!
//! ## The exchange
//!
//! ```text
//! server → client   u32 bits || modulus || exponent        the server's RSA key
//! client → server   u32 bits || modulus || exponent        this side's, fresh per session
//! client → server   u16 len  || RSA-PKCS1v15(server key, client random)
//! server → client   u16 len  || RSA-PKCS1v15(client key, server random)
//! ```
//!
//! Both randoms known, each direction gets its own AES key:
//!
//! ```text
//! client → server   H(server random || client random)[..key]
//! server → client   H(client random || server random)[..key]
//! ```
//!
//! `H` is SHA-1 for the 128-bit type and SHA-256 for the 256-bit one, the random
//! is 16 or 32 bytes to match, and from here every byte in both directions is a
//! [frame](Sealer::frame). Inside the frames: the client's hash of the two public
//! keys, the server's hash of them the other way round (which is what binds the
//! session to the keys that were actually sent — a middle-man's key substitution
//! shows up here), a one-byte subtype saying whether the server wants a
//! username, then the credentials, then RFB's own SecurityResult and everything
//! after it.
//!
//! ## What the server's key is worth
//!
//! Nothing this client verifies. RealVNC's viewer shows the key's fingerprint
//! and asks the operator to compare it, once; here the fingerprint is logged in
//! the same eight-byte hex form so an operator *can* compare it against the
//! server's own, and the session proceeds. Against an active middle-man that is
//! the same protection VncAuth had, which is none; against a passive one it is
//! an encrypted session VncAuth never was.
//!
//! ## The frame
//!
//! ```text
//! wire   u16 len || byte[len] AES-EAX ciphertext || byte[16] tag
//! nonce  a 128-bit little-endian counter, from zero, one per frame per direction
//! aad    the two length bytes
//! ```
//!
//! A frame is a transport unit and not a message: TigerVNC cuts outgoing data at
//! 8192 bytes and a server's rectangle spans as many frames as it needs, so the
//! read side is an [`AsyncRead`] yielding the concatenation of frame bodies and
//! the write side takes a whole message and cuts it. The counter is the nonce,
//! so a frame is decrypted exactly once and in order; a reader that has failed
//! is finished.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use aes::{Aes128, Aes256};
use eax::Eax;
use eax::aead::{Aead as _, KeyInit as _, Payload};
use log::{debug, info};
use rand::Rng as _;
use rsa::traits::PublicKeyParts as _;
use rsa::{BoxedUint, Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};

/// RFB security type `RA2`: RSA key exchange, AES-128-EAX, SHA-1.
pub const SECURITY_RSA_AES_128: u8 = 5;
/// RFB security type `RA2_256`: the same with AES-256-EAX and SHA-256.
pub const SECURITY_RSA_AES_256: u8 = 129;

/// Bits in the key pair generated for each session. What TigerVNC's server
/// generates and what wayvnc's `rsa_private_key_file` holds; the exchange
/// allows the two ends' sizes to differ, so the server's does not decide it.
const CLIENT_KEY_BITS: usize = 2048;
/// Bounds on the server's key, TigerVNC's. Below the lower one the random it
/// carries is not protected; above the upper one a bogus length turns into a
/// very large allocation and a very slow exponentiation.
const MIN_SERVER_KEY_BITS: u32 = 1024;
const MAX_SERVER_KEY_BITS: u32 = 8192;
/// The most plaintext put in one outgoing frame. TigerVNC's `MaxMessageSize`,
/// and neatvnc's receive buffer is the same 8192 — a longer frame is not a
/// protocol error there, it is a frame the server never finishes reading.
const MAX_FRAME_BODY: usize = 8192;
/// Length prefix and tag around a frame's ciphertext.
const HEADER: usize = 2;
const TAG: usize = 16;

/// RSA-AES subtype 1: the server wants a username and a password.
const SUBTYPE_USER_PASS: u8 = 1;
/// RSA-AES subtype 2: a password alone.
const SUBTYPE_PASS: u8 = 2;

/// Which of the two types was chosen, deciding the hash, the key length and
/// the random's size together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strength {
    Aes128,
    Aes256,
}

impl Strength {
    /// The strength behind an RFB security type, if it is one of the two.
    pub fn of(security_type: u8) -> Option<Self> {
        match security_type {
            SECURITY_RSA_AES_128 => Some(Self::Aes128),
            SECURITY_RSA_AES_256 => Some(Self::Aes256),
            _ => None,
        }
    }

    /// Bytes of random each side contributes, which is also the AES key length.
    fn random_len(self) -> usize {
        match self {
            Self::Aes128 => 16,
            Self::Aes256 => 32,
        }
    }

    /// The type's hash over the given pieces, in order. SHA-1 yields 20 bytes
    /// of which the 128-bit key takes the first 16; SHA-256 yields exactly the
    /// 256-bit key.
    fn hash(self, parts: &[&[u8]]) -> Vec<u8> {
        match self {
            Self::Aes128 => {
                let mut h = Sha1::new();
                parts.iter().for_each(|p| h.update(p));
                h.finalize().to_vec()
            }
            Self::Aes256 => {
                let mut h = Sha256::new();
                parts.iter().for_each(|p| h.update(p));
                h.finalize().to_vec()
            }
        }
    }

    fn cipher(self, key: &[u8]) -> Cipher {
        match self {
            Self::Aes128 => Cipher::Aes128(Eax::<Aes128>::new_from_slice(key).expect("a 16-byte key")),
            Self::Aes256 => Cipher::Aes256(Eax::<Aes256>::new_from_slice(key).expect("a 32-byte key")),
        }
    }
}

/// An RSA public key as it travels: `u32 bits || modulus || exponent`, the
/// two numbers big-endian and each padded to the modulus' byte length. Kept in
/// wire form because both the key hashes and the fingerprint are over exactly
/// these bytes.
#[derive(Clone, PartialEq, Eq)]
struct WireKey(Vec<u8>);

impl WireKey {
    fn encode(bits: u32, modulus: &[u8], exponent: &[u8]) -> Self {
        let size = (bits as usize).div_ceil(8);
        let mut wire = Vec::with_capacity(4 + 2 * size);
        wire.extend_from_slice(&bits.to_be_bytes());
        wire.extend_from_slice(&left_pad(modulus, size));
        wire.extend_from_slice(&left_pad(exponent, size));
        Self(wire)
    }

    fn of_public(key: &RsaPublicKey) -> Self {
        Self::encode(key.n().bits(), &key.n_bytes(), &key.e_bytes())
    }

    /// The key's byte length, which is what its ciphertexts are long.
    fn size(&self) -> usize {
        (self.0.len() - 4) / 2
    }

    fn modulus(&self) -> &[u8] {
        &self.0[4..4 + self.size()]
    }

    fn exponent(&self) -> &[u8] {
        &self.0[4 + self.size()..]
    }

    /// RealVNC's display of a key: the first eight bytes of its SHA-1, dashed.
    fn fingerprint(&self) -> String {
        let digest = Sha1::digest(&self.0);
        digest[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join("-")
    }

    fn public_key(&self) -> anyhow::Result<RsaPublicKey> {
        // Public inputs, so the variable-time decoder is the right one.
        RsaPublicKey::new(
            BoxedUint::from_be_slice_vartime(self.modulus()),
            BoxedUint::from_be_slice_vartime(self.exponent()),
        )
        .map_err(|e| anyhow::anyhow!("the server's RSA key is invalid: {e}"))
    }
}

fn left_pad(bytes: &[u8], size: usize) -> Vec<u8> {
    let bytes = bytes.iter().copied().skip_while(|&b| b == 0).collect::<Vec<_>>();
    assert!(bytes.len() <= size, "a {}-byte number in a {size}-byte field", bytes.len());
    let mut out = vec![0u8; size - bytes.len()];
    out.extend_from_slice(&bytes);
    out
}

/// Both directions' ciphers after a successful exchange, each already advanced
/// past the handshake frames it carried. [`Opener`] holds whatever the server
/// sent beyond the handshake, so wrap it around the same reader.
pub struct Session {
    pub sealer: Sealer,
    pub opener: Opener,
}

/// Run the exchange on a freshly chosen RSA-AES type: key exchange in the
/// clear, then the key hashes, subtype and credentials inside the frames.
///
/// Returns once the credentials are sent; the SecurityResult that answers them
/// is the caller's to read, through [`Session::opener`] like everything after.
pub async fn authenticate<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    strength: Strength,
    username: &str,
    password: &str,
) -> anyhow::Result<Session> {
    // Key generation is the expensive step, and it does not depend on anything
    // the server says: start it before the server's key is read so the two
    // overlap, and off the runtime because a 2048-bit search is real CPU time.
    let client_key = tokio::task::spawn_blocking(|| RsaPrivateKey::new(&mut rand::rng(), CLIENT_KEY_BITS));

    let server_wire = read_server_key(reader).await?;
    let server_key = server_wire.public_key()?;
    info!(
        "vnc: RSA-AES {} server key, fingerprint {}",
        server_wire.size() * 8,
        server_wire.fingerprint()
    );

    let client_key = client_key
        .await
        .map_err(|e| anyhow::anyhow!("RSA key generation task failed: {e}"))?
        .map_err(|e| anyhow::anyhow!("RSA key generation failed: {e}"))?;
    let client_wire = WireKey::of_public(client_key.as_public_key());

    let mut client_random = vec![0u8; strength.random_len()];
    rand::rng().fill_bytes(&mut client_random);
    let sealed_random = server_key
        .encrypt(&mut rand::rng(), Pkcs1v15Encrypt, &client_random)
        .map_err(|e| anyhow::anyhow!("encrypting the client random failed: {e}"))?;
    anyhow::ensure!(
        sealed_random.len() == server_wire.size(),
        "the client random encrypted to {} bytes under a {}-byte server key",
        sealed_random.len(),
        server_wire.size()
    );
    let mut out = client_wire.0.clone();
    out.extend_from_slice(&u16::try_from(sealed_random.len())?.to_be_bytes());
    out.extend_from_slice(&sealed_random);
    writer.write_all(&out).await?;

    let server_random = read_server_random(reader, &client_key, client_wire.size(), strength).await?;

    let send_key = strength.hash(&[&server_random, &client_random]);
    let recv_key = strength.hash(&[&client_random, &server_random]);
    let key_len = strength.random_len();
    let mut sealer = Sealer::new(strength.cipher(&send_key[..key_len]));
    let mut frames = FrameReader::new(reader, Opener::new(strength.cipher(&recv_key[..key_len])));

    let client_hash = strength.hash(&[&client_wire.0, &server_wire.0]);
    writer.write_all(&sealer.frame(&client_hash)).await?;

    let mut server_hash = vec![0u8; client_hash.len()];
    frames.read_exact(&mut server_hash).await?;
    anyhow::ensure!(
        server_hash == strength.hash(&[&server_wire.0, &client_wire.0]),
        "the server's RSA-AES key hash does not match the keys exchanged — \
         the connection was tampered with"
    );

    let subtype = frames.read_u8().await?;
    let credentials = match subtype {
        SUBTYPE_USER_PASS => {
            anyhow::ensure!(
                !username.is_empty(),
                "the VNC server asks RSA-AES for a username and a password, and the target \
                 has no username"
            );
            credentials(username, password)?
        }
        SUBTYPE_PASS => {
            if !username.is_empty() {
                debug!("vnc: RSA-AES server wants a password alone; the username is not sent");
            }
            credentials("", password)?
        }
        other => anyhow::bail!("unknown RSA-AES credential subtype {other}"),
    };
    writer.write_all(&sealer.frame(&credentials)).await?;

    let (_, opener) = frames.into_parts();
    Ok(Session { sealer, opener })
}

async fn read_server_key<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<WireKey> {
    let bits = reader.read_u32().await?;
    anyhow::ensure!(
        (MIN_SERVER_KEY_BITS..=MAX_SERVER_KEY_BITS).contains(&bits),
        "the server's RSA key is {bits} bits; this client accepts \
         {MIN_SERVER_KEY_BITS} to {MAX_SERVER_KEY_BITS}"
    );
    let size = (bits as usize).div_ceil(8);
    let mut wire = vec![0u8; 4 + 2 * size];
    wire[..4].copy_from_slice(&bits.to_be_bytes());
    reader.read_exact(&mut wire[4..]).await?;
    Ok(WireKey(wire))
}

async fn read_server_random<R: AsyncRead + Unpin>(
    reader: &mut R,
    client_key: &RsaPrivateKey,
    client_key_size: usize,
    strength: Strength,
) -> anyhow::Result<Vec<u8>> {
    let len = usize::from(reader.read_u16().await?);
    anyhow::ensure!(
        len == client_key_size,
        "the server random is {len} bytes, not the {client_key_size} of the client key"
    );
    let mut sealed = vec![0u8; len];
    reader.read_exact(&mut sealed).await?;
    let random = client_key
        .decrypt(Pkcs1v15Encrypt, &sealed)
        .map_err(|e| anyhow::anyhow!("decrypting the server random failed: {e}"))?;
    anyhow::ensure!(
        random.len() == strength.random_len(),
        "the server random is {} bytes, not {}",
        random.len(),
        strength.random_len()
    );
    Ok(random)
}

/// `u8 len || username || u8 len || password`, UTF-8, each under 256 bytes.
fn credentials(username: &str, password: &str) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + username.len() + password.len());
    for (what, field) in [("username", username), ("password", password)] {
        let len = u8::try_from(field.len())
            .map_err(|_| anyhow::anyhow!("the target's {what} is over 255 bytes"))?;
        out.push(len);
        out.extend_from_slice(field.as_bytes());
    }
    Ok(out)
}

/// One direction's AES-EAX, at whichever width the type chose.
enum Cipher {
    Aes128(Eax<Aes128>),
    Aes256(Eax<Aes256>),
}

impl Cipher {
    fn seal(&self, nonce: &[u8; 16], aad: &[u8], msg: &[u8]) -> Vec<u8> {
        let payload = Payload { msg, aad };
        let sealed = match self {
            Self::Aes128(c) => c.encrypt(nonce.into(), payload),
            Self::Aes256(c) => c.encrypt(nonce.into(), payload),
        };
        sealed.expect("AES-EAX encryption of an in-memory buffer cannot fail")
    }

    fn open(&self, nonce: &[u8; 16], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        let payload = Payload { msg: sealed, aad };
        match self {
            Self::Aes128(c) => c.decrypt(nonce.into(), payload),
            Self::Aes256(c) => c.decrypt(nonce.into(), payload),
        }
        .ok()
    }
}

/// The 128-bit little-endian frame counter. Never resets; a frame is numbered
/// once.
fn bump(counter: &mut [u8; 16]) {
    for byte in counter.iter_mut() {
        *byte = byte.wrapping_add(1);
        if *byte != 0 {
            break;
        }
    }
}

/// The client → server direction: takes a message, returns the frames that
/// carry it.
pub struct Sealer {
    cipher: Cipher,
    counter: [u8; 16],
}

impl Sealer {
    fn new(cipher: Cipher) -> Self {
        Self {
            cipher,
            counter: [0; 16],
        }
    }

    /// Frame a message, cutting it at [`MAX_FRAME_BODY`]. An empty message is
    /// no frame at all — nothing has ever needed to send one, and a reader that
    /// receives one has only a counter to advance for it.
    pub fn frame(&mut self, msg: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(msg.len() + (msg.len() / MAX_FRAME_BODY + 1) * (HEADER + TAG));
        for body in msg.chunks(MAX_FRAME_BODY) {
            let header = (body.len() as u16).to_be_bytes();
            let sealed = self.cipher.seal(&self.counter, &header, body);
            bump(&mut self.counter);
            out.extend_from_slice(&header);
            out.extend_from_slice(&sealed);
        }
        out
    }
}

enum Phase {
    /// The two length bytes.
    Len,
    /// `len` bytes of ciphertext and the tag behind them.
    Sealed { len: usize },
}

/// The server → client direction: the cipher, the counter, and whatever has
/// been read but not yet handed on. Separate from the reader it feeds so the
/// handshake can run it over a borrowed socket and the session over the owned
/// one, without a byte falling between — a server sends SecurityResult on the
/// heels of the credentials, and it may already be here.
pub struct Opener {
    cipher: Cipher,
    counter: [u8; 16],
    /// The frame in flight: the header while it is being read, then the sealed
    /// body and tag.
    staging: Vec<u8>,
    filled: usize,
    phase: Phase,
    /// Opened bytes not yet handed upward.
    body: Vec<u8>,
    body_pos: usize,
    /// A frame failed, which is terminal: the counter that would decrypt the
    /// next one has moved on from the one that was never accepted.
    failed: bool,
}

impl Opener {
    fn new(cipher: Cipher) -> Self {
        Self {
            cipher,
            counter: [0; 16],
            staging: Vec::new(),
            filled: 0,
            phase: Phase::Len,
            body: Vec::new(),
            body_pos: 0,
            failed: false,
        }
    }

    /// Open the complete frame in `staging`, leaving its body ready to hand on.
    fn accept(&mut self, len: usize) -> io::Result<()> {
        let (header, sealed) = self.staging.split_at(HEADER);
        let body = self.cipher.open(&self.counter, header, &sealed[..len + TAG]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "an RSA-AES frame failed its authentication tag",
            )
        })?;
        bump(&mut self.counter);
        self.body = body;
        self.body_pos = 0;
        Ok(())
    }

    fn poll_read<R: AsyncRead + Unpin>(
        &mut self,
        inner: &mut R,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.failed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the RSA-AES transport already failed",
            )));
        }
        loop {
            if self.body_pos < self.body.len() {
                let n = buf.remaining().min(self.body.len() - self.body_pos);
                buf.put_slice(&self.body[self.body_pos..self.body_pos + n]);
                self.body_pos += n;
                return Poll::Ready(Ok(()));
            }
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            match self.phase {
                Phase::Len => {
                    self.staging.resize(HEADER, 0);
                    let n = ready!(poll_fill(inner, cx, &mut self.staging[self.filled..]))?;
                    if n == 0 {
                        // Between frames a hang-up is an ordinary close.
                        return if self.filled == 0 {
                            Poll::Ready(Ok(()))
                        } else {
                            Poll::Ready(Err(truncated()))
                        };
                    }
                    self.filled += n;
                    if self.filled == HEADER {
                        let len = usize::from(u16::from_be_bytes([self.staging[0], self.staging[1]]));
                        self.staging.resize(HEADER + len + TAG, 0);
                        self.phase = Phase::Sealed { len };
                    }
                }
                Phase::Sealed { len } => {
                    let end = HEADER + len + TAG;
                    let n = ready!(poll_fill(inner, cx, &mut self.staging[self.filled..end]))?;
                    if n == 0 {
                        return Poll::Ready(Err(truncated()));
                    }
                    self.filled += n;
                    if self.filled == end {
                        if let Err(e) = self.accept(len) {
                            self.failed = true;
                            return Poll::Ready(Err(e));
                        }
                        self.filled = 0;
                        self.phase = Phase::Len;
                    }
                }
            }
        }
    }
}

fn poll_fill<R: AsyncRead + Unpin>(
    inner: &mut R,
    cx: &mut Context<'_>,
    dst: &mut [u8],
) -> Poll<io::Result<usize>> {
    let mut buf = ReadBuf::new(dst);
    ready!(Pin::new(inner).poll_read(cx, &mut buf))?;
    Poll::Ready(Ok(buf.filled().len()))
}

fn truncated() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "the connection ended inside an RSA-AES frame",
    )
}

/// An [`Opener`] over a reader: the plaintext stream, as the RFB layer reads it.
pub struct FrameReader<R> {
    inner: R,
    opener: Opener,
}

impl<R> FrameReader<R> {
    pub fn new(inner: R, opener: Opener) -> Self {
        Self { inner, opener }
    }

    /// Take the transport apart again, with everything read so far still in
    /// the [`Opener`].
    pub fn into_parts(self) -> (R, Opener) {
        (self.inner, self.opener)
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for FrameReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        me.opener.poll_read(&mut me.inner, cx, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(strength: Strength) -> (Sealer, Opener) {
        let key = vec![0x42u8; strength.random_len()];
        (
            Sealer::new(strength.cipher(&key)),
            Opener::new(strength.cipher(&key)),
        )
    }

    #[tokio::test]
    async fn frames_round_trip_across_both_widths_and_split_reads() {
        for strength in [Strength::Aes128, Strength::Aes256] {
            let (mut sealer, opener) = pair(strength);
            let big: Vec<u8> = (0..20_000u32).map(|i| (i * 7) as u8).collect();
            let mut wire = sealer.frame(b"hello");
            wire.extend(sealer.frame(&big));
            wire.extend(sealer.frame(b"!"));
            // Three frames for the big message and one each for the others.
            assert_eq!(wire.len(), 5 + big.len() + 1 + 5 * (HEADER + TAG));

            // Delivered a byte at a time, read in odd sizes: still one stream.
            let (mut tx, rx) = tokio::io::duplex(1);
            let feeder = tokio::spawn(async move {
                for b in wire {
                    tx.write_all(&[b]).await.unwrap();
                }
            });
            let mut reader = FrameReader::new(rx, opener);
            let mut got = Vec::new();
            let mut chunk = [0u8; 777];
            loop {
                let n = reader.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&chunk[..n]);
            }
            feeder.await.unwrap();
            let mut want = b"hello".to_vec();
            want.extend(&big);
            want.push(b'!');
            assert_eq!(got, want);
        }
    }

    #[tokio::test]
    async fn a_tampered_frame_closes_the_transport() {
        let (mut sealer, opener) = pair(Strength::Aes128);
        let mut wire = sealer.frame(b"first");
        let second = sealer.frame(b"second");
        wire.extend(&second);
        wire[HEADER + 1] ^= 1;
        let mut reader = FrameReader::new(std::io::Cursor::new(wire), opener);
        let mut buf = [0u8; 16];
        let err = reader.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        // Not retried into the next frame, which the moved-on counter could not
        // open anyway.
        let err = reader.read(&mut buf).await.unwrap_err();
        assert!(err.to_string().contains("already failed"), "{err}");
    }

    #[tokio::test]
    async fn a_frame_is_numbered_once() {
        // The same bytes under the counter the sealer has moved past do not open.
        let (mut sealer, opener) = pair(Strength::Aes256);
        let first = sealer.frame(b"once");
        let mut wire = first.clone();
        wire.extend(&first);
        let mut reader = FrameReader::new(std::io::Cursor::new(wire), opener);
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"once");
        assert!(reader.read(&mut buf).await.is_err());
    }

    #[tokio::test]
    async fn a_hang_up_between_frames_is_a_close_and_inside_one_is_not() {
        let (mut sealer, opener) = pair(Strength::Aes128);
        let wire = sealer.frame(b"whole");
        let mut reader = FrameReader::new(std::io::Cursor::new(wire.clone()), opener);
        let mut got = Vec::new();
        reader.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"whole");

        let (_, opener) = pair(Strength::Aes128);
        let mut reader = FrameReader::new(std::io::Cursor::new(&wire[..wire.len() - 1]), opener);
        let err = reader.read_to_end(&mut Vec::new()).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "{err}");
    }

    #[test]
    fn the_counter_is_little_endian_with_carry() {
        let mut c = [0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        bump(&mut c);
        assert_eq!(c, [0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn wire_key_pads_and_fingerprints_like_realvnc() {
        let key = WireKey::encode(1024, &[0, 0, 0x01, 0x02], &[0x01, 0x00, 0x01]);
        assert_eq!(key.0.len(), 4 + 256);
        assert_eq!(&key.0[..4], &[0, 0, 4, 0]);
        assert_eq!(key.size(), 128);
        assert_eq!(&key.modulus()[126..], &[0x01, 0x02]);
        assert_eq!(&key.exponent()[125..], &[0x01, 0x00, 0x01]);
        let fp = key.fingerprint();
        assert_eq!(fp.len(), 8 * 2 + 7, "{fp}");
        assert_eq!(fp.matches('-').count(), 7, "{fp}");
    }

    #[test]
    fn credentials_are_length_prefixed_and_bounded() {
        assert_eq!(credentials("andrew", "pw").unwrap(), b"\x06andrew\x02pw");
        assert_eq!(credentials("", "pw").unwrap(), b"\x00\x02pw");
        let long = "x".repeat(256);
        assert!(credentials(&long, "pw").unwrap_err().to_string().contains("username"));
        assert!(credentials("u", &long).unwrap_err().to_string().contains("password"));
    }

    /// The server side of the exchange, written from the specification rather
    /// than by calling the client's helpers, so a mistake in one cannot be
    /// agreed with by the other. Returns the credentials it received and hands
    /// back a SecurityResult followed by a message it expects echoed.
    async fn scripted_server<S: AsyncRead + AsyncWrite + Unpin>(
        mut sock: S,
        strength: Strength,
        server_key: RsaPrivateKey,
        subtype: u8,
    ) -> (Vec<u8>, Vec<u8>) {
        use rsa::traits::PublicKeyParts as _;
        let bits = server_key.n().bits();
        let size = (bits as usize).div_ceil(8);
        let mut server_wire = bits.to_be_bytes().to_vec();
        server_wire.extend(left_pad(&server_key.n_bytes(), size));
        server_wire.extend(left_pad(&server_key.e_bytes(), size));
        sock.write_all(&server_wire).await.unwrap();

        let client_bits = sock.read_u32().await.unwrap();
        let client_size = (client_bits as usize).div_ceil(8);
        let mut client_wire = client_bits.to_be_bytes().to_vec();
        client_wire.resize(4 + 2 * client_size, 0);
        sock.read_exact(&mut client_wire[4..]).await.unwrap();
        let client_key = RsaPublicKey::new(
            BoxedUint::from_be_slice_vartime(&client_wire[4..4 + client_size]),
            BoxedUint::from_be_slice_vartime(&client_wire[4 + client_size..]),
        )
        .unwrap();

        let random_len = strength.random_len();
        let mut server_random = vec![0u8; random_len];
        rand::rng().fill_bytes(&mut server_random);
        let sealed = client_key
            .encrypt(&mut rand::rng(), Pkcs1v15Encrypt, &server_random)
            .unwrap();
        sock.write_u16(sealed.len() as u16).await.unwrap();
        sock.write_all(&sealed).await.unwrap();

        let len = usize::from(sock.read_u16().await.unwrap());
        assert_eq!(len, size);
        let mut sealed = vec![0u8; len];
        sock.read_exact(&mut sealed).await.unwrap();
        let client_random = server_key.decrypt(Pkcs1v15Encrypt, &sealed).unwrap();
        assert_eq!(client_random.len(), random_len);

        // The server's send key is H(client || server); its receive key the reverse.
        let send = strength.hash(&[&client_random, &server_random]);
        let recv = strength.hash(&[&server_random, &client_random]);
        let mut sealer = Sealer::new(strength.cipher(&send[..random_len]));
        let mut frames = FrameReader::new(&mut sock, Opener::new(strength.cipher(&recv[..random_len])));

        let hash_len = strength.hash(&[]).len();
        let mut client_hash = vec![0u8; hash_len];
        frames.read_exact(&mut client_hash).await.unwrap();
        assert_eq!(client_hash, strength.hash(&[&client_wire, &server_wire]));
        let (sock, opener) = frames.into_parts();
        let server_hash = strength.hash(&[&server_wire, &client_wire]);
        let mut out = sealer.frame(&server_hash);
        out.extend(sealer.frame(&[subtype]));
        sock.write_all(&out).await.unwrap();

        let mut frames = FrameReader::new(sock, opener);
        let ulen = usize::from(frames.read_u8().await.unwrap());
        let mut credentials = vec![0u8; ulen];
        frames.read_exact(&mut credentials).await.unwrap();
        let plen = usize::from(frames.read_u8().await.unwrap());
        let mut password = vec![0u8; plen];
        frames.read_exact(&mut password).await.unwrap();
        credentials.push(b':');
        credentials.extend(password);

        // SecurityResult and a message, both inside one frame, then an echo back.
        let (sock, opener) = frames.into_parts();
        let mut after = 0u32.to_be_bytes().to_vec();
        after.extend_from_slice(b"after");
        sock.write_all(&sealer.frame(&after)).await.unwrap();
        let mut frames = FrameReader::new(sock, opener);
        let mut echo = [0u8; 4];
        frames.read_exact(&mut echo).await.unwrap();
        (credentials, echo.to_vec())
    }

    async fn exchange(strength: Strength, subtype: u8, username: &str, password: &str) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        let server_key = tokio::task::spawn_blocking(|| RsaPrivateKey::new(&mut rand::rng(), 1024).unwrap())
            .await
            .unwrap();
        let (client_sock, server_sock) = tokio::io::duplex(4096);
        let server = tokio::spawn(scripted_server(server_sock, strength, server_key, subtype));

        let (read_half, mut write_half) = tokio::io::split(client_sock);
        let mut reader = tokio::io::BufReader::new(read_half);
        let Session { mut sealer, opener } =
            authenticate(&mut reader, &mut write_half, strength, username, password).await?;
        let mut frames = FrameReader::new(reader, opener);
        assert_eq!(frames.read_u32().await?, 0, "SecurityResult");
        let mut after = [0u8; 5];
        frames.read_exact(&mut after).await?;
        assert_eq!(&after, b"after");
        write_half.write_all(&sealer.frame(b"echo")).await?;
        Ok(server.await.unwrap())
    }

    #[tokio::test]
    async fn the_exchange_authenticates_against_a_server_written_from_the_specification() {
        for strength in [Strength::Aes128, Strength::Aes256] {
            let (credentials, echo) = exchange(strength, SUBTYPE_USER_PASS, "andrew", "hunter2")
                .await
                .unwrap();
            assert_eq!(credentials, b"andrew:hunter2");
            assert_eq!(echo, b"echo");
        }
    }

    #[tokio::test]
    async fn a_password_only_server_gets_no_username_and_a_user_pass_one_needs_one() {
        let (credentials, _) = exchange(Strength::Aes128, SUBTYPE_PASS, "andrew", "hunter2")
            .await
            .unwrap();
        assert_eq!(credentials, b":hunter2");
        let err = exchange(Strength::Aes128, SUBTYPE_USER_PASS, "", "hunter2")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no username"), "{err:#}");
    }

    #[tokio::test]
    async fn a_short_server_key_is_refused_before_anything_is_sent() {
        let mut offer = 512u32.to_be_bytes().to_vec();
        offer.resize(4 + 128, 1);
        let mut sent = Vec::new();
        let err = authenticate(&mut offer.as_slice(), &mut sent, Strength::Aes128, "u", "p")
            .await
            .err()
            .expect("a 512-bit key is refused");
        assert!(err.to_string().contains("512 bits"), "{err:#}");
        assert!(sent.is_empty());
    }
}
