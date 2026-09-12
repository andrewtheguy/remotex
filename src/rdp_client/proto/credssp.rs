//! Proving who we are, before the server builds a session.
//!
//! Network Level Authentication means the credentials are checked before anything
//! else: inside the TLS session, and before the first MCS PDU. A current Windows host
//! requires it, and this client offers nothing else — so this is the gate every
//! connection passes through.
//!
//! The exchange is CredSSP ([MS-CSSP]): a handful of DER-encoded `TSRequest` messages
//! carrying NTLM tokens, then the server's public key signed by the session key, then
//! the credentials themselves. `sspi` does that, and it keeps doing it here — see the
//! note on the dependency in `Cargo.toml`. What this module owns is the part that
//! belongs to RDP rather than to the mechanism: reading one `TSRequest` at a time off
//! a TLS stream that has no framing of its own, and turning a refusal into a sentence
//! a person can act on.
//!
//! # NTLM, not Kerberos
//!
//! A target here is a user name and a password, which is what NTLM takes. Kerberos
//! would need a ticket from a KDC — a second network dependency, reachable from the
//! gateway, for a domain the gateway may not be joined to. `mstsc` sends plain NTLM
//! inside CredSSP whenever Kerberos is not available, so servers expect it.
//!
//! [MS-CSSP]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cssp/9664994d-0784-4659-b85b-83b8d54c2336

use anyhow::{Context as _, Result, bail};
use sspi::credssp::{ClientMode, ClientState, CredSspClient, CredSspMode, TsRequest};
use sspi::{AuthIdentity, Username};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::tls::Stream;

/// Who to log on as.
pub struct Credentials<'a> {
    pub username: &'a str,
    pub password: &'a str,
    /// The NetBIOS domain, when the target names one. A user name already written
    /// `DOMAIN\user` or `user@realm` carries its own and needs nothing here.
    pub domain: Option<&'a str>,
}

/// Run the CredSSP exchange to its end.
///
/// Returns once the server has accepted the credentials. The same stream then carries
/// the MCS connection sequence, with no further framing change.
pub async fn authenticate(
    stream: &mut Stream,
    server_name: &str,
    credentials: &Credentials<'_>,
    server_public_key: Vec<u8>,
) -> Result<()> {
    let username = Username::new(credentials.username, credentials.domain)
        .with_context(|| format!("{:?} is not a user name", credentials.username))?;
    let identity =
        AuthIdentity { username, password: credentials.password.to_owned().into() };

    let mut client = CredSspClient::new(
        server_public_key,
        identity.into(),
        CredSspMode::WithCredentials,
        // Plain NTLM rather than SPNEGO — see the module docs.
        ClientMode::Ntlm(sspi::ntlm::NtlmConfig::default()),
        // The name the server checks the ticket against. NTLM does not use it, but
        // CredSSP carries it and a server may log it.
        format!("TERMSRV/{server_name}"),
    )
    .context("starting CredSSP")?;

    // The first message is produced from an empty request: there is nothing from the
    // server yet, and the client speaks first.
    let mut from_server = TsRequest::default();
    loop {
        let state = client
            .process(from_server)
            .resolve_to_result()
            .context("the logon attempt was refused")?;
        let (reply, done) = match state {
            ClientState::ReplyNeeded(reply) => (reply, false),
            // The last message carries the credentials themselves. It is sent and not
            // answered: the server's next PDU is the MCS Connect Response.
            ClientState::FinalMessage(reply) => (reply, true),
        };
        send(stream, &reply).await?;
        if done {
            return Ok(());
        }
        from_server = receive(stream).await?;
    }
}

async fn send(stream: &mut Stream, request: &TsRequest) -> Result<()> {
    let mut bytes = Vec::with_capacity(usize::from(request.buffer_len()));
    request.encode_ts_request(&mut bytes).context("encoding a CredSSP request")?;
    stream.write_all(&bytes).await.context("sending a CredSSP request")?;
    stream.flush().await.context("sending a CredSSP request")?;
    Ok(())
}

/// Read exactly one `TSRequest`.
///
/// TLS delivers a byte stream, and a `TSRequest` is self-delimiting only in the sense
/// that its DER header says how long it is — so the length has to be discovered from a
/// prefix and the rest read against it, rather than by reading "a message".
async fn receive(stream: &mut Stream) -> Result<TsRequest> {
    /// The most bytes it can take to learn the length: the outer tag, and the longest
    /// length encoding that can follow it. Reading further than the length says would
    /// swallow bytes belonging to the next PDU.
    const HEADER: usize = 6;

    // Byte at a time until the DER header is complete, because until it is there is
    // no way to know how much of the stream this message owns.
    let mut bytes = Vec::with_capacity(512);
    let length = loop {
        let mut byte = [0_u8; 1];
        if stream.read(&mut byte).await.context("reading a CredSSP response")? == 0 {
            bail!(
                "the server closed the connection during the logon attempt, after {} bytes",
                bytes.len()
            );
        }
        bytes.push(byte[0]);
        match TsRequest::read_length(bytes.as_slice()) {
            Ok(length) => break length,
            // Not a length yet. `sspi` reports anything it cannot read as an early
            // end of input, so the byte count is what separates "more to come" from
            // "this is not a TSRequest at all".
            Err(_) if bytes.len() < HEADER => {}
            Err(e) => {
                return Err(e).context("the server's answer is not a CredSSP response");
            }
        }
    };
    if length < bytes.len() {
        bail!("a CredSSP response announced {length} bytes, fewer than its own header");
    }

    let already = bytes.len();
    bytes.resize(length, 0);
    stream.read_exact(&mut bytes[already..]).await.context("reading a CredSSP response")?;
    TsRequest::from_buffer(&bytes).context("decoding a CredSSP response")
}
