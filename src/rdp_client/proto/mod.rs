//! RDP's wire format, written here.
//!
//! Every PDU a session is made of, encoded and decoded against the specification
//! rather than against a dependency. The client above this module — [`super::Session`]
//! — is built on nothing else.
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
//! - [`frame`] — one whole frame off the socket, of whichever of the two framings it
//!   happens to wear.
//! - [`tls`] — the TLS session the rest of the connection lives inside.
//! - [`credssp`] — Network Level Authentication, before the server builds a session.
//! - [`mcs`] — the channels every later PDU travels on, and the sequence that opens
//!   them.
//! - [`gcc`] — what the two sides tell each other while those channels are opened:
//!   the desktop, the colour depth, the virtual channels.
//! - [`channel`] — the header a static virtual channel's data wears, and the chunks a
//!   long one is split into.
//! - [`dvc`] — the dynamic channels a server opens over one of those while the session
//!   is live.
//! - [`display`] — the dynamic channel a desktop is resized over.
//! - [`zgfx`] — the bulk compression every graphics pipeline PDU is wrapped in.
//! - [`gfx`] — the graphics pipeline's own PDUs: surfaces, frames, and the codecs
//!   that fill them.
//! - [`cliprdr`] — the clipboard, on a static channel of its own.
//! - [`share`] — the headers every PDU wears once the channels are open, and the
//!   dispatch that says which kind has arrived.
//! - [`info`] — the logon: who the session belongs to and what it should look like.
//! - [`input`] — keystrokes and mouse events, on the fast path back to the host.
//! - [`license`] — the one licensing PDU a Windows host sends, which says none is
//!   needed.
//! - [`capabilities`] — what the two ends agree to send each other, exchanged once
//!   before any pixel moves.
//! - [`finalization`] — the four PDUs between a confirmed share and a desktop.
//! - [`fastpath`] — the framing the server's updates arrive in once it is live.
//! - [`bitmap`] — the rectangles of pixels those updates carry, and where they go.
//! - [`planar`] — the codec a 32-bit session compresses a rectangle with.
//! - [`pointer`] — the cursor, which travels as its own shape and is never drawn
//!   into the desktop.
//! - [`desktop`] — what a client asks of a desktop that is already up, and the last
//!   thing a server says about one.
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

pub mod bitmap;
pub mod capabilities;
pub mod channel;
pub mod cliprdr;
pub mod credssp;
pub mod der;
pub mod desktop;
pub mod display;
pub mod dvc;
pub mod fastpath;
pub mod frame;
pub mod finalization;
pub mod gcc;
pub mod gfx;
pub mod info;
pub mod input;
pub mod license;
pub mod mcs;
pub mod per;
pub mod planar;
pub mod pointer;
pub mod share;
pub mod tls;
pub mod wire;
pub mod x224;
pub mod zgfx;
