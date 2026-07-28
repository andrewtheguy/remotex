//! The handshake: `Noise_KK_25519_ChaChaPoly_BLAKE2s`, two messages.
//!
//! ```text
//! gateway ──▶ e, es, ss
//! agent   ──▶ e, ee, se
//! ── both sides now hold a forward-secret AEAD session ──
//! ```
//!
//! Each machine holds one long-lived X25519 keypair and pins the other's public
//! half in its own config file (see [`crate::key`]), the way WireGuard pairs an
//! interface with a peer. That is the whole of the authentication: no
//! certificates, no CA, nothing that expires — which is the point, since a
//! reconnect must never involve a human. The `ee` in the second message
//! contributes an ephemeral DH on top, so a recorded session stays unreadable
//! even if a private key later leaks.
//!
//! `KK` rather than `IK` (WireGuard's own) because each side pins exactly one
//! peer: both static keys are known before the first byte, so authentication
//! happens entirely inside Noise with nothing for this crate to compare. `IK`
//! would have the agent learn the dialing gateway's key from the first message
//! and check it here — the shape that admits a *list* of accepted gateways,
//! which is the opposite of the one-gateway-at-a-time invariant the agent is
//! built on.
//!
//! **The responder is the side that reports a mismatch.** Both `es` and `ss` are
//! consumed in the first message, so a wrong key on *either* end breaks the
//! agent's decryption of it; the agent rejects the dial before revealing
//! anything at all, and the gateway sees the hangup. That is what lets
//! `src/rxa.rs` treat an initial connect failure as fatal and report it.
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
pub const PARAMS: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";

/// A Noise message can never exceed this on the wire.
const MAX_NOISE_MSG: usize = 65535;

/// Why a handshake failed.
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    /// The peer hung up, or the network dropped. Also what a key mismatch looks
    /// like from the *initiator*'s side, since the responder rejects first.
    #[error("handshake I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Decryption or pattern failure — in practice, a peer whose public key is
    /// not the one configured here, or one speaking a different version.
    #[error(
        "handshake rejected (the peer's public key is not the one configured, \
         or a peer speaking another version): {0}"
    )]
    Noise(#[from] snow::Error),
    /// A length prefix that no valid Noise message could have.
    #[error("handshake message is {0} bytes, which is not a valid Noise message")]
    BadLength(usize),
}

/// Dial side (the gateway): send `e, es, ss`, read `e, ee, se`.
///
/// `private` is this gateway's own key; `peer_public` is the agent's, from the
/// target's `agent_public_key`.
pub async fn initiate<S>(
    stream: &mut S,
    private: &[u8; 32],
    peer_public: &[u8; 32],
) -> Result<StatelessTransportState, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut state = builder(private, peer_public)?.build_initiator()?;
    let mut buf = vec![0u8; MAX_NOISE_MSG];
    let n = state.write_message(&[], &mut buf)?;
    write_msg(stream, &buf[..n]).await?;
    let msg = read_msg(stream).await?;
    state.read_message(&msg, &mut buf)?;
    Ok(state.into_stateless_transport_mode()?)
}

/// Listen side (the agent): read `e, es, ss`, send `e, ee, se`.
///
/// `private` is this Mac's own key; `peer_public` is the gateway's, from the
/// agent config's `gateway_public_key`.
pub async fn respond<S>(
    stream: &mut S,
    private: &[u8; 32],
    peer_public: &[u8; 32],
) -> Result<StatelessTransportState, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut state = builder(private, peer_public)?.build_responder()?;
    let mut buf = vec![0u8; MAX_NOISE_MSG];
    let msg = read_msg(stream).await?;
    // A mismatch on either side fails here, before the agent has revealed
    // anything at all: this message consumes both `es` and `ss`.
    state.read_message(&msg, &mut buf)?;
    let n = state.write_message(&[], &mut buf)?;
    write_msg(stream, &buf[..n]).await?;
    Ok(state.into_stateless_transport_mode()?)
}

fn builder<'a>(
    private: &'a [u8; 32],
    peer_public: &'a [u8; 32],
) -> Result<Builder<'a>, snow::Error> {
    // PARAMS is a compile-time constant that the tests exercise, so a parse
    // failure here would be a bug in this file rather than a runtime condition.
    let params = PARAMS.parse().expect("PARAMS is a valid Noise pattern");
    Builder::new(params)
        .prologue(PROLOGUE)?
        .local_private_key(private)?
        .remote_public_key(peer_public)
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
    // Both `KK` handshake messages are 48 bytes — an ephemeral public key and
    // the tag over an empty payload; anything shorter is a peer that is not
    // speaking this protocol. The upper bound needs no check: a u16 length
    // cannot exceed a Noise message's own limit.
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
    use crate::key::{self, Role};

    /// One machine's identity: its private key and the public half its peer has
    /// to be configured with.
    struct Identity {
        private: [u8; 32],
        public: [u8; 32],
    }

    fn identity(role: Role) -> Identity {
        let private = key::parse_private(role, &key::generate_private(role)).unwrap();
        Identity {
            public: key::public_of(&private),
            private,
        }
    }

    /// Run both halves of a handshake over an in-memory duplex, returning each
    /// side's result.
    ///
    /// Each side is given its own private key and the public key it *believes*
    /// its peer has — which is the whole thing under test, so the two are
    /// passed separately rather than derived from one pairing.
    async fn handshake(
        gateway: &Identity,
        gateway_expects: [u8; 32],
        agent: &Identity,
        agent_expects: [u8; 32],
    ) -> (
        Result<StatelessTransportState, HandshakeError>,
        Result<StatelessTransportState, HandshakeError>,
    ) {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let (agent_private, gateway_private) = (agent.private, gateway.private);
        let server = tokio::spawn(async move {
            let r = respond(&mut b, &agent_private, &agent_expects).await;
            // Hold the pipe open long enough for the peer to observe its own
            // outcome rather than a truncated read.
            tokio::task::yield_now().await;
            r
        });
        let client = initiate(&mut a, &gateway_private, &gateway_expects).await;
        (client, server.await.unwrap())
    }

    /// The paired case: each side configured with the other's real public key.
    async fn paired(
        gateway: &Identity,
        agent: &Identity,
    ) -> (
        Result<StatelessTransportState, HandshakeError>,
        Result<StatelessTransportState, HandshakeError>,
    ) {
        handshake(gateway, agent.public, agent, gateway.public).await
    }

    #[tokio::test]
    async fn matching_keys_yield_a_working_session_both_ways() {
        let (gateway, agent) = (identity(Role::Gateway), identity(Role::Agent));
        let (client, server) = paired(&gateway, &agent).await;
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

    // A gateway dialing a Mac whose public key it has wrong — a stale
    // `agent_public_key` after the agent regenerated its identity.
    #[tokio::test]
    async fn a_gateway_holding_the_wrong_agent_key_is_rejected() {
        let (gateway, agent) = (identity(Role::Gateway), identity(Role::Agent));
        let stranger = identity(Role::Agent);
        let (client, server) = handshake(&gateway, stranger.public, &agent, gateway.public).await;
        // The responder authenticates first, so it is the side that reports a
        // crypto failure; the initiator just sees the hangup.
        assert!(server.is_err(), "the agent must reject a mismatched dial");
        assert!(client.is_err(), "the gateway must not end up with a session");
    }

    // The other half: the Mac is paired with some other gateway, so the one
    // dialing it is a stranger however well it knows this Mac's public key.
    #[tokio::test]
    async fn an_agent_holding_the_wrong_gateway_key_rejects_the_dial() {
        let (gateway, agent) = (identity(Role::Gateway), identity(Role::Agent));
        let stranger = identity(Role::Gateway);
        let (client, server) = handshake(&gateway, agent.public, &agent, stranger.public).await;
        assert!(server.is_err(), "the agent must reject an unpaired gateway");
        assert!(client.is_err(), "the gateway must not end up with a session");
    }

    // The prologue binds the protocol version into the transcript. Simulate a
    // peer from another version by handshaking with a different prologue.
    #[tokio::test]
    async fn mismatched_prologue_fails_the_handshake() {
        let (gateway, agent) = (identity(Role::Gateway), identity(Role::Agent));
        let (agent_private, gateway_public) = (agent.private, gateway.public);
        let (mut a, mut b) = tokio::io::duplex(4096);
        let server =
            tokio::spawn(async move { respond(&mut b, &agent_private, &gateway_public).await });

        let params = PARAMS.parse().unwrap();
        let mut other = Builder::new(params)
            .prologue(b"rxa/1")
            .unwrap()
            .local_private_key(&gateway.private)
            .unwrap()
            .remote_public_key(&agent.public)
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
        let (gateway, agent) = (identity(Role::Gateway), identity(Role::Agent));
        let (agent_private, gateway_public) = (agent.private, gateway.public);
        let (mut a, mut b) = tokio::io::duplex(4096);
        let server =
            tokio::spawn(async move { respond(&mut b, &agent_private, &gateway_public).await });
        // e.g. an HTTP request arriving on the agent's port.
        a.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        drop(a);
        assert!(matches!(
            server.await.unwrap(),
            Err(HandshakeError::BadLength(_) | HandshakeError::Io(_))
        ));
    }
}
