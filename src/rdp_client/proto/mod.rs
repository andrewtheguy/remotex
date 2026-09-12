//! RDP's wire format, written here.
//!
//! The client above this module is built on IronRDP's protocol crates. This module
//! is the replacement for them: the PDUs a connection is made of, encoded and decoded
//! against the specification rather than against a dependency. It is built and tested
//! on its own until it can carry a whole session, so that no connection is ever half
//! one stack and half the other.
//!
//! # What is here
//!
//! - [`wire`] — the primitives every PDU is spelled in: a bounds-checked reader and a
//!   writer, both with the byte order in the method name.
//! - [`der`] — tag-length-value ASN.1, which the server's certificate and the MCS
//!   connect PDUs are both written in.
//! - [`per`] — packed ASN.1, which the T.124 conference and the rest of MCS are
//!   written in.
//! - [`x224`] — TPKT framing, the X.224 connection sequence, and the security
//!   negotiation that decides whether TLS and CredSSP follow.
//! - [`tls`] — the TLS session the rest of the connection lives inside.
//! - [`credssp`] — Network Level Authentication, before the server builds a session.
//! - [`mcs`] — the channels every later PDU travels on, and the sequence that opens
//!   them.
//! - [`gcc`] — what the two sides tell each other while those channels are opened:
//!   the desktop, the colour depth, the virtual channels.
//!
//! # One kind of server
//!
//! The target is a current Windows host, and only that. This client asks for
//! `HYBRID` — TLS with the credentials checked first — and nothing else: no legacy
//! RDP encryption, no plain TLS, no Kerberos, no RDSTLS. Every protocol a server
//! might also speak is one more sequence to get right and to keep right, and none of
//! them is needed to reach a Windows desktop.
//!
//! # Where the shapes come from
//!
//! [MS-RDPBCGR] is the specification, and it is the authority. Where it is silent or
//! wrong about what servers actually do — and it is, in places, about both — the
//! reference is FreeRDP's `libfreerdp/core`, which has been talking to real Windows
//! hosts for fifteen years. Neither is a reason to accept a field this client does
//! not understand: an unexpected value ends the connection with a sentence naming the
//! field, rather than being skipped in the hope that it did not matter.
//!
//! [MS-RDPBCGR]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/5073f4ed-1e93-45e1-b039-6e30c385867c

pub mod credssp;
pub mod der;
pub mod gcc;
pub mod mcs;
pub mod per;
pub mod tls;
pub mod wire;
pub mod x224;
