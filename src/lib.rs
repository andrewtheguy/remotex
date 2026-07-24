//! rdpweb — a browser-based RDP client.
//!
//! The library exposes the pieces the binary wires together and that the
//! integration tests exercise at the protocol level. See docs/phase1-mvp.md.

pub mod assets;
pub mod cli;
pub mod config;
pub mod error;
pub mod keymap;
pub mod protocol;
pub mod rdp;
pub mod server;
pub mod ws;
