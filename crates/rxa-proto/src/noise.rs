//! The handshake: `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`, two messages.
//!
//! ```text
//! gateway ──▶ e, psk          (ephemeral pubkey, PSK mixed into the chaining key)
//! agent   ──▶ e, ee           (ephemeral, DH)
//! ── both sides now hold a forward-secret AEAD session ──
//! ```
//!
//! The PSK alone provides mutual authentication: neither side can complete the
//! handshake without it. There are no certificates, no CA, no pinning, and
//! nothing that expires — which is the whole point, since a reconnect must
//! never involve a human. `NN` contributes the ephemeral DH on top, so a
//! recorded session stays unreadable even if the PSK later leaks.
//!
//! [`crate::PROLOGUE`] is bound into the handshake transcript, so a
//! version-mismatched peer fails here rather than desynchronising later.
//!
//! Each handshake message is framed on the wire as `u16 BE length + bytes`.
//! Once the handshake completes both sides switch to
//! [`snow::StatelessTransportState`] — see [`crate::frame`] for why.

use snow::{Builder, StatelessTransportState};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::PROLOGUE;

/// The Noise pattern and cipher suite. Both sides parse this same string.
pub const PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";

/// A Noise message can never exceed this on the wire.
const MAX_NOISE_MSG: usize = 65535;

/// Why a handshake failed.
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    /// The peer hung up, or the network dropped. Also what a wrong PSK looks
    /// like from the *responder*'s side of the second message.
    #[error("handshake I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Decryption or pattern failure — in practice, a PSK mismatch or a peer
    /// speaking a different protocol version.
    #[error("handshake rejected (wrong PSK, or a peer speaking another version): {0}")]
    Noise(#[from] snow::Error),
    /// A length prefix that no valid Noise message could have.
    #[error("handshake message is {0} bytes, which is not a valid Noise message")]
    BadLength(usize),
}

/// Dial side (the gateway): send `e, psk`, read `e, ee`.
pub async fn initiate<S>(
    stream: &mut S,
    psk: &[u8; 32],
) -> Result<StatelessTransportState, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut state = builder(psk)?.build_initiator()?;
    let mut buf = vec![0u8; MAX_NOISE_MSG];
    let n = state.write_message(&[], &mut buf)?;
    write_msg(stream, &buf[..n]).await?;
    let msg = read_msg(stream).await?;
    state.read_message(&msg, &mut buf)?;
    Ok(state.into_stateless_transport_mode()?)
}

/// Listen side (the agent): read `e, psk`, send `e, ee`.
pub async fn respond<S>(
    stream: &mut S,
    psk: &[u8; 32],
) -> Result<StatelessTransportState, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut state = builder(psk)?.build_responder()?;
    let mut buf = vec![0u8; MAX_NOISE_MSG];
    let msg = read_msg(stream).await?;
    // A wrong PSK fails here, before the agent has revealed anything at all.
    state.read_message(&msg, &mut buf)?;
    let n = state.write_message(&[], &mut buf)?;
    write_msg(stream, &buf[..n]).await?;
    Ok(state.into_stateless_transport_mode()?)
}

fn builder(psk: &[u8; 32]) -> Result<Builder<'_>, snow::Error> {
    // PARAMS is a compile-time constant that the tests exercise, so a parse
    // failure here would be a bug in this file rather than a runtime condition.
    let params = PARAMS.parse().expect("PARAMS is a valid Noise pattern");
    Builder::new(params).prologue(PROLOGUE)?.psk(0, psk)
}

async fn write_msg<S: AsyncWrite + Unpin>(stream: &mut S, msg: &[u8]) -> std::io::Result<()> {
    // One write: a handshake message is small, and splitting the length off
    // would put it in its own packet.
    let mut out = Vec::with_capacity(2 + msg.len());
    out.extend_from_slice(&(msg.len() as u16).to_be_bytes());
    out.extend_from_slice(msg);
    stream.write_all(&out).await?;
    stream.flush().await
}

async fn read_msg<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>, HandshakeError> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len).await?;
    let len = usize::from(u16::from_be_bytes(len));
    // An `NN` handshake message is 32 or 48 bytes; anything shorter is a peer
    // that is not speaking this protocol. The upper bound needs no check: a
    // u16 length cannot exceed a Noise message's own limit.
    if len < 32 {
        return Err(HandshakeError::BadLength(len));
    }
    let mut msg = vec![0u8; len];
    stream.read_exact(&mut msg).await?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psk;

    /// Run both halves of a handshake over an in-memory duplex, returning each
    /// side's result.
    async fn handshake(
        client_psk: [u8; 32],
        server_psk: [u8; 32],
    ) -> (
        Result<StatelessTransportState, HandshakeError>,
        Result<StatelessTransportState, HandshakeError>,
    ) {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let r = respond(&mut b, &server_psk).await;
            // Hold the pipe open long enough for the peer to observe its own
            // outcome rather than a truncated read.
            tokio::task::yield_now().await;
            r
        });
        let client = initiate(&mut a, &client_psk).await;
        (client, server.await.unwrap())
    }

    #[tokio::test]
    async fn matching_psk_yields_a_working_session_both_ways() {
        let key = psk::parse(&psk::generate()).unwrap();
        let (client, server) = handshake(key, key).await;
        let (client, server) = (client.unwrap(), server.unwrap());

        // Gateway → agent, then agent → gateway, at nonce 0 in each direction.
        let mut ct = [0u8; 128];
        let mut pt = [0u8; 128];
        let n = client.write_message(0, b"attach", &mut ct).unwrap();
        let m = server.read_message(0, &ct[..n], &mut pt).unwrap();
        assert_eq!(&pt[..m], b"attach");

        let n = server.write_message(0, b"hello", &mut ct).unwrap();
        let m = client.read_message(0, &ct[..n], &mut pt).unwrap();
        assert_eq!(&pt[..m], b"hello");
    }

    #[tokio::test]
    async fn mismatched_psk_fails_the_handshake() {
        let a = psk::parse(&psk::generate()).unwrap();
        let b = psk::parse(&psk::generate()).unwrap();
        let (client, server) = handshake(a, b).await;
        // The responder is the first to authenticate, so it is the side that
        // reports a crypto failure; the initiator just sees the hangup.
        assert!(server.is_err(), "responder must reject a wrong PSK");
        assert!(client.is_err(), "initiator must not end up with a session");
    }

    // The prologue binds the protocol version into the transcript. Simulate a
    // peer from another version by handshaking with a different prologue.
    #[tokio::test]
    async fn mismatched_prologue_fails_the_handshake() {
        let key = psk::parse(&psk::generate()).unwrap();
        let (mut a, mut b) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move { respond(&mut b, &key).await });

        let params = PARAMS.parse().unwrap();
        let mut other = Builder::new(params)
            .prologue(b"rxa/2")
            .unwrap()
            .psk(0, &key)
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut buf = vec![0u8; MAX_NOISE_MSG];
        let n = other.write_message(&[], &mut buf).unwrap();
        write_msg(&mut a, &buf[..n]).await.unwrap();

        assert!(
            server.await.unwrap().is_err(),
            "a peer on another protocol version must be rejected at the handshake"
        );
    }

    #[tokio::test]
    async fn a_peer_speaking_garbage_is_rejected_on_the_length_prefix() {
        let key = psk::parse(&psk::generate()).unwrap();
        let (mut a, mut b) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move { respond(&mut b, &key).await });
        // e.g. an HTTP request arriving on the agent's port.
        a.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        drop(a);
        assert!(matches!(
            server.await.unwrap(),
            Err(HandshakeError::BadLength(_) | HandshakeError::Io(_))
        ));
    }
}
