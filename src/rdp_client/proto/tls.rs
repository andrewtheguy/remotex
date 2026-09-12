//! The TLS session everything after the negotiation happens inside.
//!
//! Once the server has answered the X.224 negotiation with `HYBRID`, the connection
//! stops being RDP for a moment: a TLS handshake runs over the same socket, CredSSP
//! runs inside it, and only then does the MCS connection sequence start — still inside
//! the TLS session, which carries every PDU for the rest of the connection.
//!
//! # The certificate is not verified
//!
//! Any certificate is accepted, for the session only and without storing it. That is
//! defensible *because* this client insists on NLA: CredSSP binds the credential
//! exchange to the public key of the certificate that terminated this very handshake,
//! so a machine in the middle holding a certificate of its own cannot complete the
//! exchange, and cannot replay the credentials onward. It would not be defensible
//! under plain TLS, where the credentials travel in the logon PDU to whoever answered
//! — which is one of the reasons this client does not offer plain TLS.
//!
//! [`public_key`] is what makes that true, so it is not an accessory to the handshake:
//! a connection that cannot read the server's public key has nothing to bind to and
//! must not continue.

use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use tokio::net::TcpStream;
use tokio_rustls::rustls;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};

use super::der;

/// The upgraded socket.
pub type Stream = tokio_rustls::client::TlsStream<TcpStream>;

/// Hand the socket to TLS.
///
/// `server_name` is the host as configured, without the brackets an IPv6 literal is
/// written with: rustls takes a name or an address, not the `[..]` form a URL uses.
pub async fn upgrade(tcp: TcpStream, server_name: &str) -> Result<Stream> {
    install_crypto_provider();

    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate))
        .with_no_client_auth();
    // Nothing resumes: one session per connection, and a resumed handshake would
    // hand back a certificate this client never saw the server present.
    config.resumption = rustls::client::Resumption::disabled();

    let name = ServerName::try_from(server_name.to_owned())
        .with_context(|| format!("{server_name} is not a name or address TLS can be asked for"))?;
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .context("the TLS handshake failed")
}

/// The server's public key, for CredSSP to bind the credential exchange to.
pub fn public_key(stream: &Stream) -> Result<Vec<u8>> {
    let (_, connection) = stream.get_ref();
    let certificate = connection
        .peer_certificates()
        .and_then(<[CertificateDer<'_>]>::first)
        .ok_or_else(|| anyhow!("the server presented no certificate"))?;
    der::certificate_public_key(certificate).context("reading the server's public key")
}

/// rustls needs a process-wide crypto provider before the first handshake, and `ring`
/// is the one in the tree.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // An error means something else in the process installed one first, which is
        // just as good.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// See the module docs: the binding CredSSP does is what this connection trusts, not
/// the certificate chain.
#[derive(Debug)]
struct AcceptAnyCertificate;

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    /// Every scheme, because none of them is being checked. Naming a short list here
    /// would not add safety — it would only make some servers fail to handshake.
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::{
            ECDSA_NISTP256_SHA256, ECDSA_NISTP384_SHA384, ECDSA_NISTP521_SHA512, ED448, ED25519,
            RSA_PKCS1_SHA1, RSA_PKCS1_SHA256, RSA_PKCS1_SHA384, RSA_PKCS1_SHA512, RSA_PSS_SHA256,
            RSA_PSS_SHA384, RSA_PSS_SHA512,
        };
        vec![
            RSA_PKCS1_SHA1,
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            ECDSA_NISTP521_SHA512,
            ED25519,
            ED448,
        ]
    }
}
