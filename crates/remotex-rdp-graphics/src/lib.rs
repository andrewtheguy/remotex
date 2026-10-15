//! The graphics of an RDP session: what a host draws with, decoded and composed.
//!
//! This is the part of the gateway's RDP client that turns a host's drawing into
//! pixels, and nothing else of it — no connection, no channels, no input. Two
//! things run it:
//!
//! - **The gateway**, whose session (`src/rdp_client`) feeds it what arrives on the
//!   connection and encodes the framebuffer it fills.
//! - **The page**, in a session started with the pipeline passed: the gateway
//!   passes the graphics pipeline's commands on instead of composing them, and the
//!   page composes them with this crate built to WebAssembly (`frontend/wasm/egfx`).
//!
//! One implementation, so the two cannot read a command differently.
//!
//! # What is here
//!
//! - [`proto`] — the wire: the graphics pipeline's PDUs and its bulk compression,
//!   the codecs, and the reader and writer every PDU is spelled with.
//! - [`gfx`] — the pipeline's state: surfaces, the bitmap cache, and the frames
//!   that carry them to the framebuffer.
//! - [`avc`] — the H.264 a passed pipeline may carry: its access units found for
//!   whoever decodes them, and the pictures that come back put into colour.
//! - [`framebuffer`] — the desktop as composed, and the rectangles of it that
//!   changed.
//! - [`compositor`] — the pipeline and a framebuffer together, fed commands that
//!   were passed on.
//! - [`fidelity`] — whether what a host has drawn with is its picture exactly.

pub mod avc;
pub mod compositor;
pub mod fidelity;
pub mod framebuffer;
pub mod gfx;
pub mod proto;

pub use compositor::{Composed, Compositor};
pub use framebuffer::{Frame, Framebuffer, Rect};
