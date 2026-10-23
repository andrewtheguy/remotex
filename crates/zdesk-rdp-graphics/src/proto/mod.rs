//! The wire format of what a host draws with.
//!
//! - [`wire`] — the primitives every PDU is spelled in: a bounds-checked reader and a
//!   writer, both with the byte order in the method name. The rest of the client's
//!   PDUs, in the gateway, are spelled with these too.
//! - [`zgfx`] — the bulk compression every graphics pipeline PDU is wrapped in.
//! - [`gfx`] — the graphics pipeline's own PDUs: surfaces, frames, and the codecs
//!   that fill them.
//! - [`bitmap`] — the rectangles of pixels a bitmap update carries, and where they
//!   go.
//! - [`planar`] — the codec a 32-bit session compresses a rectangle with.
//! - [`clear`] — ClearCodec, which a Windows desktop draws its flat regions and text
//!   in.
//! - [`nsc`] — NSCodec, as ClearCodec's subcodec for what is neither flat nor
//!   repeated.
//! - [`progressive`] — RemoteFX Progressive, which a host draws pictures and motion
//!   with.
//! - [`avc`] — what wraps the H.264 a host draws video with, in a session that
//!   takes it: the region mask and the access units, which are decoded elsewhere.

pub mod avc;
pub mod bitmap;
pub mod clear;
pub mod gfx;
pub mod nsc;
pub mod planar;
pub mod progressive;
pub mod wire;
pub mod zgfx;
