//! Framing: application messages over the Noise transport.
//!
//! A Noise transport message caps at 65535 bytes on the wire (65519 of
//! plaintext, after the 16-byte AEAD tag), and a full-screen JPEG keyframe is
//! far larger than that. Rather than inventing a chunking flag in the message
//! definitions, this module treats the Noise transport as a **reliable byte
//! stream**: writes are split into ≤65519-byte Noise messages transparently,
//! reads reassemble them. Application framing then sits on top as plain
//! `u32 LE length + payload`, exactly as if it were a TCP stream — where
//! `payload[0]` is the message type byte (see [`crate::msg`]) and `length`
//! counts the whole payload.
//!
//! ## Why the transport is *stateless*
//!
//! Both sides need to read tiles and write input **concurrently**, from
//! separate tasks. [`snow::TransportState`] takes `&mut self` for both
//! directions, so a shared one would force a mutex that the reader holds while
//! parked on the socket — starving the writer. [`snow::StatelessTransportState`]
//! takes `&self` and an explicit nonce instead, so the two halves can each own
//! their own counter behind a shared `Arc` and never block one another. TCP
//! delivers in order, so the two counters stay in lockstep with the peer's by
//! construction.

use std::sync::Arc;

use snow::StatelessTransportState;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// The AEAD tag ChaChaPoly appends to every Noise message.
const TAG_LEN: usize = 16;

/// Largest plaintext that fits in one Noise transport message.
pub const MAX_NOISE_PAYLOAD: usize = 65535 - TAG_LEN;

/// Largest application frame accepted on read. Everything here is
/// authenticated, so this is not a security boundary — it is a guard against a
/// bug on the far side turning into a multi-gigabyte allocation. A full-screen
/// Retina keyframe is a few megabytes, so this leaves ample headroom.
pub const MAX_FRAME_LEN: usize = 32 * 1024 * 1024;

/// Why a frame could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame decryption failed: {0}")]
    Noise(#[from] snow::Error),
    #[error("peer announced a {0}-byte frame, over the {MAX_FRAME_LEN}-byte limit")]
    TooLarge(usize),
    #[error("peer sent an empty frame")]
    Empty,
    #[error("peer sent a {0}-byte Noise message, too short to hold an AEAD tag")]
    Truncated(usize),
}

/// The two halves of an established session, ready to hand to separate tasks.
pub fn split<R, W>(
    reader: R,
    writer: W,
    transport: StatelessTransportState,
) -> (FrameReader<R>, FrameWriter<W>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let transport = Arc::new(transport);
    (
        FrameReader {
            inner: reader,
            transport: Arc::clone(&transport),
            nonce: 0,
            plain: Vec::new(),
            plain_pos: 0,
            cipher: Vec::new(),
        },
        FrameWriter {
            inner: writer,
            transport,
            nonce: 0,
            chunk: Vec::new(),
            cipher: Vec::new(),
        },
    )
}

/// Reads application frames off the Noise transport.
pub struct FrameReader<R> {
    inner: R,
    transport: Arc<StatelessTransportState>,
    /// Receive-direction nonce; must match the peer's send counter exactly.
    nonce: u64,
    /// Decrypted bytes of the current Noise message.
    plain: Vec<u8>,
    plain_pos: usize,
    /// Scratch for the ciphertext, reused across messages.
    cipher: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Read one complete application frame, reassembling across Noise messages.
    ///
    /// **Not cancel-safe:** dropping the returned future mid-frame leaves the
    /// stream positioned partway through a message. Callers that need to wait
    /// on something else at the same time (`tokio::select!`) must drive this
    /// from its own task and forward frames over a channel.
    pub async fn recv(&mut self) -> Result<Vec<u8>, FrameError> {
        let mut header = [0u8; 4];
        self.read_exact(&mut header).await?;
        let len = u32::from_le_bytes(header) as usize;
        if len == 0 {
            return Err(FrameError::Empty);
        }
        if len > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge(len));
        }
        let mut payload = vec![0u8; len];
        self.read_exact(&mut payload).await?;
        Ok(payload)
    }

    /// Pull exactly `out.len()` plaintext bytes off the stream.
    async fn read_exact(&mut self, out: &mut [u8]) -> Result<(), FrameError> {
        let mut filled = 0;
        while filled < out.len() {
            if self.plain_pos == self.plain.len() {
                self.fill().await?;
            }
            let available = &self.plain[self.plain_pos..];
            let take = available.len().min(out.len() - filled);
            out[filled..filled + take].copy_from_slice(&available[..take]);
            self.plain_pos += take;
            filled += take;
        }
        Ok(())
    }

    /// Read and decrypt the next Noise message into `self.plain`.
    async fn fill(&mut self) -> Result<(), FrameError> {
        let mut len = [0u8; 2];
        self.inner.read_exact(&mut len).await?;
        let len = usize::from(u16::from_be_bytes(len));
        if len < TAG_LEN {
            return Err(FrameError::Truncated(len));
        }
        self.cipher.resize(len, 0);
        self.inner.read_exact(&mut self.cipher).await?;
        // Plaintext is exactly `len - TAG_LEN`; size the buffer for it.
        self.plain.resize(len - TAG_LEN, 0);
        let n = self
            .transport
            .read_message(self.nonce, &self.cipher, &mut self.plain)?;
        self.nonce += 1;
        self.plain.truncate(n);
        self.plain_pos = 0;
        Ok(())
    }
}

/// Writes application frames onto the Noise transport.
pub struct FrameWriter<W> {
    inner: W,
    transport: Arc<StatelessTransportState>,
    /// Send-direction nonce; must match the peer's receive counter exactly.
    nonce: u64,
    /// Accumulates up to one Noise message worth of plaintext.
    chunk: Vec<u8>,
    /// Scratch for the length prefix + ciphertext, reused across messages.
    cipher: Vec<u8>,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    /// Write one application frame: a `u32 LE` length followed by `payload`,
    /// split across as many Noise messages as it takes.
    pub async fn send(&mut self, payload: &[u8]) -> Result<(), FrameError> {
        if payload.is_empty() {
            return Err(FrameError::Empty);
        }
        if payload.len() > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge(payload.len()));
        }
        let header = (payload.len() as u32).to_le_bytes();

        // Move the scratch buffer out so the write borrow below is disjoint.
        let mut chunk = std::mem::take(&mut self.chunk);
        chunk.clear();
        for part in [header.as_slice(), payload] {
            let mut rest = part;
            while !rest.is_empty() {
                let take = rest.len().min(MAX_NOISE_PAYLOAD - chunk.len());
                chunk.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                if chunk.len() == MAX_NOISE_PAYLOAD {
                    let result = self.write_noise_msg(&chunk).await;
                    chunk.clear();
                    if let Err(e) = result {
                        self.chunk = chunk;
                        return Err(e);
                    }
                }
            }
        }
        let result = if chunk.is_empty() {
            Ok(())
        } else {
            self.write_noise_msg(&chunk).await
        };
        self.chunk = chunk;
        result?;
        self.inner.flush().await?;
        Ok(())
    }

    /// Encrypt one ≤[`MAX_NOISE_PAYLOAD`] plaintext chunk and write it with its
    /// `u16 BE` length prefix, in a single `write_all`.
    async fn write_noise_msg(&mut self, plain: &[u8]) -> Result<(), FrameError> {
        self.cipher.resize(2 + plain.len() + TAG_LEN, 0);
        let n = self
            .transport
            .write_message(self.nonce, plain, &mut self.cipher[2..])?;
        self.nonce += 1;
        self.cipher[..2].copy_from_slice(&(n as u16).to_be_bytes());
        self.inner.write_all(&self.cipher[..2 + n]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{self, Role};
    use crate::noise;

    /// A connected pair of framed endpoints over an in-memory duplex, with the
    /// real handshake underneath.
    async fn pair() -> (
        (FrameReader<impl AsyncRead + Unpin>, FrameWriter<impl AsyncWrite + Unpin>),
        (FrameReader<impl AsyncRead + Unpin>, FrameWriter<impl AsyncWrite + Unpin>),
    ) {
        let mint = |role| key::parse_private(role, &key::generate_private(role)).unwrap();
        let (gateway, agent): ([u8; 32], [u8; 32]) = (mint(Role::Gateway), mint(Role::Agent));
        let (gateway_public, agent_public) = (key::public_of(&gateway), key::public_of(&agent));
        // A small duplex buffer on purpose: it forces multi-megabyte frames to
        // interleave reads and writes, exercising reassembly under back-pressure.
        let (mut a, mut b) = tokio::io::duplex(8 * 1024);
        let server = tokio::spawn(async move {
            let t = noise::respond(&mut b, &agent, &gateway_public).await.unwrap();
            (b, t)
        });
        let client_t = noise::initiate(&mut a, &gateway, &agent_public).await.unwrap();
        let (b, server_t) = server.await.unwrap();

        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let (a_read, a_write) = split(ar, aw, client_t);
        let (b_read, b_write) = split(br, bw, server_t);
        ((a_read, a_write), (b_read, b_write))
    }

    #[tokio::test]
    async fn small_frames_roundtrip_in_order() {
        let ((_ar, mut aw), (mut br, _bw)) = pair().await;
        let sender = tokio::spawn(async move {
            for i in 0u8..16 {
                aw.send(&[i, i, i]).await.unwrap();
            }
        });
        for i in 0u8..16 {
            assert_eq!(br.recv().await.unwrap(), vec![i, i, i]);
        }
        sender.await.unwrap();
    }

    // The reason this module exists: a keyframe is much larger than one Noise
    // message, and must survive the split/reassemble transparently.
    #[tokio::test]
    async fn a_frame_far_larger_than_one_noise_message_roundtrips() {
        let ((_ar, mut aw), (mut br, _bw)) = pair().await;
        // ~3 MB, deliberately not a multiple of MAX_NOISE_PAYLOAD.
        let big: Vec<u8> = (0..3_000_001usize).map(|i| (i % 251) as u8).collect();
        let expected = big.clone();
        let sender = tokio::spawn(async move { aw.send(&big).await.unwrap() });
        assert_eq!(br.recv().await.unwrap(), expected);
        sender.await.unwrap();
    }

    // A frame whose length prefix lands exactly on a Noise message boundary is
    // the classic off-by-one; so is one byte either side of it.
    #[tokio::test]
    async fn frames_at_the_noise_message_boundary_roundtrip() {
        for len in [
            MAX_NOISE_PAYLOAD - 5,
            MAX_NOISE_PAYLOAD - 4,
            MAX_NOISE_PAYLOAD - 3,
            MAX_NOISE_PAYLOAD,
            MAX_NOISE_PAYLOAD + 1,
            2 * MAX_NOISE_PAYLOAD - 4,
        ] {
            let ((_ar, mut aw), (mut br, _bw)) = pair().await;
            let payload: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
            let expected = payload.clone();
            let sender = tokio::spawn(async move { aw.send(&payload).await.unwrap() });
            assert_eq!(br.recv().await.unwrap(), expected, "len {len}");
            sender.await.unwrap();
        }
    }

    // Both directions are independent nonce sequences; interleaving must not
    // desynchronise either one.
    #[tokio::test]
    async fn both_directions_run_independently() {
        let ((mut ar, mut aw), (mut br, mut bw)) = pair().await;
        for i in 0u8..8 {
            aw.send(&[i]).await.unwrap();
            assert_eq!(br.recv().await.unwrap(), vec![i]);
            bw.send(&[i, 0xFF]).await.unwrap();
            assert_eq!(ar.recv().await.unwrap(), vec![i, 0xFF]);
        }
    }

    #[tokio::test]
    async fn an_absurd_length_prefix_is_rejected_rather_than_allocated() {
        let ((_ar, mut aw), (mut br, _bw)) = pair().await;
        // Forge an oversized header directly on the transport, bypassing send's
        // own guard, to prove the read side refuses it too.
        let header = ((MAX_FRAME_LEN + 1) as u32).to_le_bytes();
        aw.write_noise_msg(&header).await.unwrap();
        aw.inner.flush().await.unwrap();
        assert!(matches!(br.recv().await, Err(FrameError::TooLarge(_))));
    }

    #[tokio::test]
    async fn a_hangup_mid_frame_surfaces_as_io_and_not_a_hang() {
        let ((ar, mut aw), (mut br, _bw)) = pair().await;
        // Announce a 64-byte frame and then vanish, the shape a Wi-Fi drop
        // takes mid-tile. Both halves have to go for the peer to see EOF.
        aw.write_noise_msg(&64u32.to_le_bytes()).await.unwrap();
        aw.inner.flush().await.unwrap();
        drop((ar, aw));
        assert!(matches!(br.recv().await, Err(FrameError::Io(_))));
    }

    #[tokio::test]
    async fn empty_frames_are_refused_on_both_sides() {
        let ((_ar, mut aw), (mut br, _bw)) = pair().await;
        assert!(matches!(aw.send(&[]).await, Err(FrameError::Empty)));
        aw.write_noise_msg(&0u32.to_le_bytes()).await.unwrap();
        aw.inner.flush().await.unwrap();
        assert!(matches!(br.recv().await, Err(FrameError::Empty)));
    }
}
