//! A headless RDP client, on IronRDP's protocol crates.
//!
//! Screen, pointer, keyboard, mouse and resize, and nothing else. There is no
//! window and no drawing: [`Session::start`] connects, keeps a complete framebuffer
//! up to date in memory the caller can read, and posts an [`Event`] whenever a
//! rectangle of it changes. What the caller does with those pixels is not this
//! module's business — [`crate::rdp`] is the caller, and it encodes them.
//!
//! IronRDP supplies the protocol: the connection sequence, CredSSP, TLS, fast-path
//! and slow-path decoding, the bitmap codecs, and Display Control. This module owns
//! what a headless gateway needs on top — one thread per session, the framebuffer
//! copy, the event stream, and the resize path.
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
//! # What this does not do
//!
//! - **No sound, no clipboard, no touch.** None of those channels is opened.
//! - **No graphics pipeline.** MS-RDPEGFX is not advertised, so no surface
//!   commands, no RemoteFX Progressive and no H.264.
//! - **No certificate verification.** Any server certificate is accepted, for the
//!   session only and without storing it. That is defensible under NLA, where
//!   CredSSP binds the server's TLS public key into the credential exchange so an
//!   interceptor cannot replay the credentials; it is **not** defensible under
//!   plain TLS security, where the credentials go to whoever answered.
//! - **No Kerberos.** CredSSP runs NTLM with the target's user name and password.
//! - **One monitor.** [`Input::resize`] sends a layout of exactly one.

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
