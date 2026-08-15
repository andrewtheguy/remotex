//! remotex — a browser-based RDP client.
//!
//! The library exposes the pieces the binary wires together and that the
//! integration tests exercise at the protocol level. See docs/architecture.md.

pub mod audio;
pub mod auth;
pub mod camera;
pub mod classify;
pub mod cli;
pub mod config;
pub mod copies;
#[cfg(feature = "embedded-gateway")]
pub mod embedded;
pub mod encode;
pub mod engine;
pub mod error;
pub mod feedback;
pub mod keymap;
pub mod opus_stream;
pub mod pcm48;
pub mod pcm_stream;
pub mod protocol;
pub mod rdp;
pub mod rdp_audio;
pub mod rdp_camera;
pub mod rdp_clipboard;
pub mod regions;
pub mod server;
pub mod session;
pub mod tiles;
pub mod video;
pub mod vnc;
pub mod vnc_apple;
pub mod vnc_apple_clipboard;
pub mod vnc_clipboard;
pub mod vnc_encodings;
pub mod vnc_record;
pub mod vp9;
pub mod wire;
pub mod ws;
