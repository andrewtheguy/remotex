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
//! - [`x224`] — TPKT framing, the X.224 connection sequence, and the security
//!   negotiation that decides whether TLS and CredSSP follow.
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

pub mod wire;
pub mod x224;
