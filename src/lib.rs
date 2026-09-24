//! remotex — a browser-based RDP client.
//!
//! The library exposes the pieces the binary wires together and that the
//! integration tests exercise at the protocol level. See docs/architecture.md.

#[cfg(feature = "airplay")]
pub mod airplay;
#[cfg(not(feature = "airplay"))]
#[path = "airplay/disabled.rs"]
pub mod airplay;
pub mod assets;
pub mod audio;
pub mod auth;
pub mod camera;
pub mod cli;
pub mod config;
// The control plane is a Unix process graph and has not been ported; see the
// `tui` arm in src/main.rs.
#[cfg(all(feature = "embedded-gateway", unix))]
pub mod embedded;
pub mod encode;
pub mod engine;
pub mod error;
pub mod feedback;
pub mod keymap;
pub mod mic;
pub mod opus_stream;
pub mod pcm48;
pub mod pcm_stream;
pub mod protocol;
pub mod rdp;
pub mod rdp_camera;
pub mod rdp_client;
pub mod rdp_clipboard;
pub mod rdp_mic;
pub mod server;
pub mod session;
pub mod shadow;
pub mod stream;
pub mod throughput;
pub mod video;
pub mod vnc;
pub mod vnc_apple;
pub mod vnc_apple_clipboard;
pub mod vnc_audio;
pub mod vnc_camera;
pub mod vnc_clipboard;
pub mod vnc_encodings;
pub mod vnc_mic;
pub mod vnc_record;
pub mod vnc_rsa_aes;
pub mod vp9;
pub mod wire;
pub mod ws;
