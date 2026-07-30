//! The handshake: `Noise_IK_25519_ChaChaPoly_BLAKE2s`, two messages.
//!
//! ```text
//! gateway ──▶ e, es, s, ss
//! agent   ──▶ e, ee, se
//! ── both sides now hold a forward-secret AEAD session ──
//! ```
//!
//! Each machine holds one long-lived X25519 keypair (see [`crate::key`]). The
//! gateway pins the Mac's public half as its target's `agent_public_key`; the Mac
//! keeps a *list* of gateway keys it will answer, the way `~/.ssh/authorized_keys`
//! is a list. That is the whole of the authentication: no certificates, no CA,
//! nothing that expires — which is the point, since a reconnect must never involve
//! a human. The `ee` in the second message contributes an ephemeral DH on top, so
//! a recorded session stays unreadable even if a private key later leaks.
//!
//! `IK` — WireGuard's own — because the two ends know different amounts before the
//! first byte. The gateway knows exactly which Mac it is dialing, so the
//! responder's static key is pinned (the `K` half). The Mac does *not* know which
//! of its authorized gateways is calling, so the initiator's static key travels
//! *in* message 1 (the `I` half), encrypted under `es`: the agent decrypts it,
//! hands it to the caller's lookup, and refuses the dial if it is not on the list —
//! all before message 2, so an unauthorized peer learns nothing.
//!
//! `KK` was the pattern while the agent pinned one `gateway_public_key`, and the
//! reason to move was the list and nothing else. In particular this has no bearing
//! on how many sessions the agent serves: [`crate::msg::GatewayMsg::Claim`] keys
//! the session slot on a session id rather than on any key, so a second authorized
//! key cannot become a second concurrent session.
//!
//! **The responder is the side that reports a mismatch.** Both `es` and `ss` are
//! consumed in the first message, so a wrong `agent_public_key` on the gateway
//! breaks the agent's decryption of it, and an unlisted gateway key fails the
//! lookup immediately after; either way the agent rejects the dial before
//! revealing anything at all, and the gateway sees the hangup. That is what lets
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
pub const PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

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
    /// The dialing gateway proved it holds a key, and that key is not one the
    /// responder was told to accept.
    ///
    /// Distinct from [`HandshakeError::Noise`] on purpose: the crypto succeeded
    /// and the peer is exactly who it says it is — there is simply no entry for
    /// it. The key is carried so the log can name it in the form somebody pastes
    /// into the list.
    #[error("the dialing gateway's public key {0} is not on the authorized list")]
    Unauthorized(String),
}

/// Dial side (the gateway): send `e, es, s, ss`, read `e, ee, se`.
///
/// `private` is this gateway's own key — sent inside message 1, so the responder
/// learns which gateway is calling; `peer_public` is the agent's, from the target's
/// `agent_public_key`.
pub async fn initiate<S>(
    stream: &mut S,
    private: &[u8; 32],
    peer_public: &[u8; 32],
) -> Result<StatelessTransportState, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut state = builder(private)?.remote_public_key(peer_public)?.build_initiator()?;
    let mut buf = vec![0u8; MAX_NOISE_MSG];
    let n = state.write_message(&[], &mut buf)?;
    write_msg(stream, &buf[..n]).await?;
    let msg = read_msg(stream).await?;
    state.read_message(&msg, &mut buf)?;
    Ok(state.into_stateless_transport_mode()?)
}

/// Listen side (the agent): read `e, es, s, ss`, look the dialer up, send
/// `e, ee, se`.
///
/// `private` is this Mac's own key. `authorized` is asked once, with the gateway
/// public key message 1 carried, and answers with whatever the caller wants to know
/// about the entry that matched — the comment against it, typically, so the menu bar
/// and the log can name *which* gateway is connected. `None` refuses the dial.
///
/// The lookup happens between the two messages, which is the whole point of the
/// pattern: an unauthorized peer gets no reply, so it learns neither this Mac's
/// ephemeral key nor whether anyone is watching the screen. `FnOnce`, because a
/// handshake concerns exactly one key.
pub async fn respond<S, T>(
    stream: &mut S,
    private: &[u8; 32],
    authorized: impl FnOnce(&[u8; 32]) -> Option<T>,
) -> Result<(StatelessTransportState, T), HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut state = builder(private)?.build_responder()?;
    let mut buf = vec![0u8; MAX_NOISE_MSG];
    let msg = read_msg(stream).await?;
    // A wrong `agent_public_key` on the gateway fails here, before the agent has
    // revealed anything at all: this message consumes both `es` and `ss`.
    state.read_message(&msg, &mut buf)?;
    // Present because the pattern transmits it, and 32 bytes because the pattern
    // is Curve25519 — both are properties of PARAMS above rather than of the peer.
    let peer_public: [u8; 32] = state
        .get_remote_static()
        .expect("IK carries the initiator's static key in message 1")
        .try_into()
        .expect("a Curve25519 static key is 32 bytes");
    let Some(matched) = authorized(&peer_public) else {
        return Err(HandshakeError::Unauthorized(crate::key::public_text(
            crate::key::Role::Gateway,
            &peer_public,
        )));
    };
    let n = state.write_message(&[], &mut buf)?;
    write_msg(stream, &buf[..n]).await?;
    Ok((state.into_stateless_transport_mode()?, matched))
}

/// Everything both sides configure the same way. The responder stops here; only
/// the initiator pins a remote static key, which is what `IK` means.
fn builder(private: &[u8; 32]) -> Result<Builder<'_>, snow::Error> {
    // PARAMS is a compile-time constant that the tests exercise, so a parse
    // failure here would be a bug in this file rather than a runtime condition.
    let params = PARAMS.parse().expect("PARAMS is a valid Noise pattern");
    Builder::new(params)
        .prologue(PROLOGUE)?
        .local_private_key(private)
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
    // The `IK` messages are 96 and 48 bytes — an ephemeral public key, the
    // initiator's encrypted static key in the first, and the tag over an empty
    // payload; anything shorter than a bare public key is a peer that is not
    // speaking this protocol. The upper bound needs no check: a u16 length cannot
    // exceed a Noise message's own limit.
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
    /// Each side is given its own private key and what it *believes* about its
    /// peer — which is the whole thing under test, so the two are passed
    /// separately rather than derived from one pairing. The agent's belief is now a
    /// list: `authorizes` is the keys it will answer, and the comment it hands back
    /// is what the caller learns about who dialed.
    async fn handshake(
        gateway: &Identity,
        gateway_expects: [u8; 32],
        agent: &Identity,
        authorizes: Vec<([u8; 32], &'static str)>,
    ) -> (
        Result<StatelessTransportState, HandshakeError>,
        Result<(StatelessTransportState, &'static str), HandshakeError>,
    ) {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let (agent_private, gateway_private) = (agent.private, gateway.private);
        let server = tokio::spawn(async move {
            let r = respond(&mut b, &agent_private, |dialer| {
                authorizes
                    .iter()
                    .find(|(key, _)| key == dialer)
                    .map(|(_, comment)| *comment)
            })
            .await;
            // Hold the pipe open long enough for the peer to observe its own
            // outcome rather than a truncated read.
            tokio::task::yield_now().await;
            r
        });
        let client = initiate(&mut a, &gateway_private, &gateway_expects).await;
        (client, server.await.unwrap())
    }

    /// The paired case: the gateway holds the Mac's real public key, and the Mac
    /// lists the gateway's.
    async fn paired(
        gateway: &Identity,
        agent: &Identity,
    ) -> (
        Result<StatelessTransportState, HandshakeError>,
        Result<(StatelessTransportState, &'static str), HandshakeError>,
    ) {
        handshake(
            gateway,
            agent.public,
            agent,
            vec![(gateway.public, "home server")],
        )
        .await
    }

    #[tokio::test]
    async fn matching_keys_yield_a_working_session_both_ways() {
        let (gateway, agent) = (identity(Role::Gateway), identity(Role::Agent));
        let (client, server) = paired(&gateway, &agent).await;
        let (client, (server, dialed_as)) = (client.unwrap(), server.unwrap());
        // The point of the pattern: the agent knows *which* of its authorized
        // gateways this is, not merely that the crypto worked.
        assert_eq!(dialed_as, "home server");

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
        let (client, server) = handshake(
            &gateway,
            stranger.public,
            &agent,
            vec![(gateway.public, "home server")],
        )
        .await;
        // The responder authenticates first, so it is the side that reports a
        // crypto failure; the initiator just sees the hangup. And it is a crypto
        // failure rather than a refusal: message 1 cannot even be decrypted, so
        // the list is never consulted.
        assert!(
            matches!(server, Err(HandshakeError::Noise(_))),
            "the agent must reject a mismatched dial in Noise: {server:?}"
        );
        assert!(client.is_err(), "the gateway must not end up with a session");
    }

    // The other half: the Mac's list does not have this gateway on it, however
    // well the gateway knows this Mac's public key. Authenticated, and not
    // authorized — the distinction the list introduces.
    #[tokio::test]
    async fn a_gateway_that_is_not_on_the_list_is_refused_by_name() {
        let (gateway, agent) = (identity(Role::Gateway), identity(Role::Agent));
        let stranger = identity(Role::Gateway);
        let (client, server) = handshake(
            &gateway,
            agent.public,
            &agent,
            vec![(stranger.public, "some other server")],
        )
        .await;
        let Err(HandshakeError::Unauthorized(reported)) = server else {
            panic!("an unlisted gateway must be refused as unauthorized: {server:?}");
        };
        // Reported in the form somebody pastes into the list, which is the only
        // reason to carry it at all.
        assert_eq!(
            reported,
            crate::key::public_text(Role::Gateway, &gateway.public)
        );
        assert!(client.is_err(), "the gateway must not end up with a session");
    }

    // An agent nobody has authorized yet — a first launch. Every dial is refused,
    // and the refusal still names the key, which is exactly the value that has to
    // go on the list to fix it.
    #[tokio::test]
    async fn an_empty_list_refuses_every_gateway() {
        let (gateway, agent) = (identity(Role::Gateway), identity(Role::Agent));
        let (client, server) = handshake(&gateway, agent.public, &agent, vec![]).await;
        assert!(
            matches!(server, Err(HandshakeError::Unauthorized(_))),
            "{server:?}"
        );
        assert!(client.is_err());
    }

    // More than one entry is the whole point, and each has to reach its own
    // comment: an agent that matched the first key on the list would name the
    // wrong machine in the menu bar.
    #[tokio::test]
    async fn each_listed_gateway_is_recognised_as_itself() {
        let agent = identity(Role::Agent);
        let (home, laptop) = (identity(Role::Gateway), identity(Role::Gateway));
        let list = vec![(home.public, "home server"), (laptop.public, "laptop")];

        for (dialer, expected) in [(&home, "home server"), (&laptop, "laptop")] {
            let (client, server) =
                handshake(dialer, agent.public, &agent, list.clone()).await;
            client.unwrap();
            assert_eq!(server.unwrap().1, expected);
        }
    }

    // The prologue binds the protocol version into the transcript. Simulate a
    // peer from another version by handshaking with a different prologue.
    #[tokio::test]
    async fn mismatched_prologue_fails_the_handshake() {
        let (gateway, agent) = (identity(Role::Gateway), identity(Role::Agent));
        let (agent_private, gateway_public) = (agent.private, gateway.public);
        let (mut a, mut b) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            respond(&mut b, &agent_private, |k| (*k == gateway_public).then_some(())).await
        });

        let params = PARAMS.parse().unwrap();
        let mut other = Builder::new(params)
            .prologue(b"rxa/2")
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
        let server = tokio::spawn(async move {
            respond(&mut b, &agent_private, |k| (*k == gateway_public).then_some(())).await
        });
        // e.g. an HTTP request arriving on the agent's port.
        a.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        drop(a);
        assert!(matches!(
            server.await.unwrap(),
            Err(HandshakeError::BadLength(_) | HandshakeError::Io(_))
        ));
    }
}
