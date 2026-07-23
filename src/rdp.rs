//! Server-side RDP session — **placeholder**.
//!
//! In Phase 1 this module owns the real RDP protocol engine
//! ([IronRDP](https://crates.io/crates/ironrdp), via `ironrdp-async`). The web
//! server never speaks RDP directly: [`crate::ws`] bridges the browser to a
//! `Session` here, which connects to the RDP host, decodes the framebuffer into
//! [`ServerMsg::Tile`] updates, and injects [`ClientMsg`] input as RDP PDUs.
//!
//! Nothing here is implemented yet — the skeleton compiles and runs without a
//! real RDP backend so the browser ⇄ WebSocket wiring can be verified. See
//! docs/phase1-mvp.md for the milestones that fill this in.
//!
//! TODO(phase1): uncomment the `ironrdp*` dependencies in Cargo.toml and
//! implement `connect` / `pump_frames` / `send_input`.

#![allow(dead_code)]

use crate::{config::AppConfig, protocol::ClientMsg};

/// An active RDP session bound to a single connected browser client.
pub struct Session {
    // TODO(phase1): hold the ironrdp-async framed connection + active stage here.
}

impl Session {
    /// Connect to the configured RDP host and complete the handshake.
    pub async fn connect(_config: &AppConfig) -> anyhow::Result<Self> {
        // TODO(phase1): ironrdp-async connect + TLS + activation sequence.
        unimplemented!("RDP connect is not implemented yet — see docs/phase1-mvp.md")
    }

    /// Inject a single input event (mouse/keyboard) into the RDP session.
    pub async fn send_input(&mut self, _input: ClientMsg) -> anyhow::Result<()> {
        // TODO(phase1): translate ClientMsg -> RDP input PDUs and send.
        unimplemented!("RDP input injection is not implemented yet")
    }
}
