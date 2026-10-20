//! A headless RDP client, protocol and all.
//!
//! Screen, pointer, keyboard, mouse, resize, clipboard and sound, with a camera
//! and microphone in the other direction.
//! There is no window and no drawing of the desktop's pipeline: [`Session::start`]
//! connects and posts an [`Event`] for what the host sent — the graphics pipeline's
//! commands, handed on for the caller to have composed, or, from a host that draws
//! with bitmap updates, the rectangles of a framebuffer it keeps in memory the
//! caller can read. What the caller does with either is not this module's business:
//! [`crate::rdp`] is the caller, and it passes the one and encodes the other.
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
//! # The two graphics paths
//!
//! [`Connect::egfx`] chooses. With the graphics pipeline (MS-RDPEGFX), the server
//! draws through surfaces on a dynamic channel, whose commands are handed on
//! whole ([`Event::Graphics`]) for a [`remotex_rdp_graphics::Compositor`] somewhere
//! else — the page's — to compose; it marks its frames ([`Event::Frame`]), and
//! answers a monitor layout with a graphics reset, which surfaces here as one
//! [`Event::Resize`]. Without it, the server sends plain bitmap updates on the
//! share, decoded here into the framebuffer, and the desktop keeps its opening
//! size: Display Control is the pipeline's alone.
//!
//! # The clipboard
//!
//! MS-RDPECLIP, on a static virtual channel of its own, which every session asks
//! for. Both directions are lazy: a copy on
//! either end announces *which formats* it can be had in, and the bytes cost a second
//! round trip that happens only when somebody pastes. This module carries format ids
//! and bytes and decides nothing about either — which format is text, and what its
//! bytes mean, is [`crate::rdp_clipboard`]'s.
//!
//! # The camera
//!
//! MS-RDPECAM, for a session that asked for it with [`Connect::camera`]: a camera on
//! this end, which the host installs as a device of its own. The caller plugs it and
//! hands it samples through [`Session::camera`] — H.264 already encoded, which this
//! module passes through without looking inside — and the host's decisions about it,
//! starting the stream and stopping it, reach a [`CameraSink`] on the session thread.
//! [`proto::rdpecam`] is the channel; the samples are metered out one per request the
//! host makes, as the specification requires.
//!
//! # The microphone
//!
//! MS-RDPEAI, for a session that asked for it with [`Connect::microphone`]: a microphone
//! on this end, recorded from by applications on the host. The caller hands PCM to
//! [`Session::microphone`], and the host's decisions — it started recording, in this
//! format, or stopped — reach a [`MicrophoneSink`] on the session thread.
//! [`proto::rdpeai`] is the channel, which cuts the PCM into the packets the host asked for.
//!
//! # What this does not do
//!
//! - **No touch.** MS-RDPEI is never asked for.
//! - **No decoding of the pipeline.** Its codecs, H.264 among them where
//!   [`Connect::h264`] lets the host draw with it, are the compositor's.
//! - **NLA, or TLS without the pipeline, and nothing older.** The security
//!   negotiation offers `HYBRID`, and `SSL` beside it only when [`Connect::egfx`] is
//!   off, as mstsc and FreeRDP do: a Windows host chooses NLA, and a server without
//!   CredSSP, xrdp say, chooses TLS. Legacy RDP security is refused.
//! - **No certificate verification.** Any server certificate is accepted, for the
//!   session only and without storing it. Under NLA, CredSSP binds the server's TLS
//!   public key into the credential exchange so an interceptor cannot replay the
//!   credentials; under plain TLS nothing does, and the credentials go to whoever
//!   answered.
//! - **No Kerberos.** CredSSP runs NTLM with the target's user name and password.
//! - **One monitor, or a second against it.** [`Connect::monitors`] asks for
//!   same-sized monitors at connect, the second against the edge of the first that
//!   [`Connect::placement`] names, and each layout after names each monitor's own
//!   size; the framebuffer spans them, and the caller shows what it will of it.

mod camera;
mod connect;
mod error;
mod input;
mod microphone;
mod pointer;
pub mod proto;
mod session;

pub use camera::{Camera, CameraFeed, CameraSink, Fed};
pub use connect::private_address;
pub use microphone::{MicrophoneFeed, MicrophoneSink};
pub use error::Error;
pub use input::{Input, MouseButton, sanitise_scale, sanitise_size};
pub use pointer::{Cursor, CursorImage};
pub use remotex_rdp_graphics::fidelity::Fidelity;
pub use remotex_rdp_graphics::{Composed, Compositor, Frame, Framebuffer, Rect};
pub use session::{AudioSink, Connect, Event, Placed, Session};
