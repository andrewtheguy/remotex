//! A headless RDP client, protocol and all.
//!
//! Screen, pointer, keyboard, mouse, resize and the clipboard, and nothing else.
//! There is no window and no drawing: [`Session::start`] connects, keeps a complete framebuffer
//! up to date in memory the caller can read, and posts an [`Event`] whenever a
//! rectangle of it changes. What the caller does with those pixels is not this
//! module's business — [`crate::rdp`] is the caller, and it encodes them.
//!
//! [`proto`] is the wire: the connection sequence, CredSSP, TLS, fast-path and
//! slow-path decoding, the bitmap codec, the cursor, and Display Control, all
//! written here against [MS-RDPBCGR] rather than taken from a dependency. This
//! module is what a headless gateway needs on top — one thread per session, the
//! framebuffer copy, the event stream, and the resize path.
//!
//! [MS-RDPBCGR]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/5073f4ed-1e93-45e1-b039-6e30c385867c
//!
//! # The two threads
//!
//! ```text
//!   caller  ──  Session, Input                  UnboundedReceiver<Event>  ──  caller
//!      │  command queue                                    ▲
//!      ▼                                                   │
//!   the "rdp" thread: a current-thread runtime, select! over the socket and the queue
//! ```
//!
//! Every session gets an OS thread of its own, and it keeps it until the session
//! ends. Decoding a desktop is real CPU work, and on its own thread it runs beside
//! the caller's encoding rather than in turns with it. Input goes onto a queue the
//! thread drains between PDUs, so nothing outside that thread ever touches the
//! connection.
//!
//! # Graphics
//!
//! The server draws with plain bitmap updates, and answers a monitor layout with a
//! Deactivation-Reactivation Sequence — it tears the desktop down and builds it
//! again at the new size, which surfaces here as one [`Event::Resize`].
//!
//! # The clipboard
//!
//! MS-RDPECLIP, on a static virtual channel of its own, and only for a session that
//! asked for it with [`Connect::clipboard`]. Both directions are lazy: a copy on
//! either end announces *which formats* it can be had in, and the bytes cost a second
//! round trip that happens only when somebody pastes. This module carries format ids
//! and bytes and decides nothing about either — which format is text, and what its
//! bytes mean, is [`crate::rdp_clipboard`]'s.
//!
//! # What this does not do
//!
//! - **No sound and no touch.** Neither channel is opened.
//! - **No graphics pipeline.** MS-RDPEGFX is not advertised, so no surface
//!   commands, no RemoteFX Progressive and no H.264.
//! - **NLA and nothing else.** The security negotiation offers `HYBRID` alone, so a
//!   server that cannot do CredSSP is refused rather than logged on to some other
//!   way.
//! - **No certificate verification.** Any server certificate is accepted, for the
//!   session only and without storing it. That is defensible under NLA, where
//!   CredSSP binds the server's TLS public key into the credential exchange so an
//!   interceptor cannot replay the credentials; it is **not** defensible under
//!   plain TLS security, where the credentials go to whoever answered.
//! - **No Kerberos.** CredSSP runs NTLM with the target's user name and password.
//! - **One monitor.** [`Input::resize`] sends a layout of exactly one.

mod connect;
mod error;
mod framebuffer;
mod input;
mod pointer;
pub mod proto;
mod session;

pub use error::Error;
pub use framebuffer::{Frame, Framebuffer, Rect};
pub use input::{Input, MouseButton, sanitise_scale, sanitise_size};
pub use pointer::{Cursor, CursorImage};
pub use session::{Connect, Event, Session};
