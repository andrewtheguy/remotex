//! zdesk — a browser-based RDP client.
//!
//! The library exposes the pieces the binary wires together and that the
//! integration tests exercise at the protocol level. See docs/architecture.md.

pub mod aac_eld;
pub mod assets;
pub mod audio;
pub mod auth;
pub mod camera;
pub mod combined;
pub mod cli;
pub mod config;
#[cfg(feature = "embedded-gateway")]
pub mod embedded;
pub mod encode;
pub mod engine;
pub mod error;
pub mod feedback;
pub mod h264;
pub mod hevc_wasm;
#[cfg(feature = "embedded-gateway")]
pub mod keychain;
pub mod keymap;
pub mod mic;
pub mod opus_stream;
pub mod pcm48;
pub mod protocol;
pub mod rdp;
pub mod rdp_camera;
pub mod rdp_client;
pub mod rdp_clipboard;
pub mod rdp_mic;
pub mod server;
pub mod session;
pub mod shadow;
pub mod throughput;
pub mod tiles;
pub mod vault;
pub mod video;
pub mod vnc;
pub mod vnc_apple;
pub mod vnc_apple_clipboard;
pub mod vnc_apple_media;
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
