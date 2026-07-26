//! `rxa` — the wire protocol between remotex and its macOS agent.
//!
//! remotex reaches a Mac today over Apple's Screen Sharing (VNC), which is
//! flaky and re-prompts for credentials on every reconnect. `rxa` replaces that
//! hop with a purpose-built protocol: a PSK-authenticated Noise session over
//! TCP carrying pre-encoded screen tiles, so a dropped connection costs a
//! two-message handshake and never a login. See
//! `docs/mac-agent-architecture.md`.
//!
//! Everything both sides must agree on lives here — framing, messages, the
//! handshake, the PSK format, and the DOM-code → macOS-keycode table — so the
//! two halves cannot drift. The crate is platform-independent on purpose: the
//! agent crate only compiles on macOS, so anything that needs a test lives
//! here instead.
//!
//! The stack, bottom up:
//!
//! - [`psk`] — PSK generate/parse (`rxa` prefix + base64url of 32 random bytes
//!   and a CRC16, matching the house format used across the author's repos)
//! - [`noise`] — `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` handshake helpers
//! - [`frame`] — the Noise transport as a reliable byte stream, plus the
//!   length-prefixed application framing on top of it
//! - [`msg`] — [`msg::AgentMsg`] / [`msg::GatewayMsg`]
//! - [`keymap`] — DOM `KeyboardEvent.code` → macOS `kVK_*` virtual keycode

pub mod frame;
pub mod keymap;
pub mod msg;
pub mod noise;
pub mod psk;

/// Protocol version, bound into the Noise handshake as the prologue so a
/// mismatch fails at the handshake instead of desynchronising later.
pub const PROLOGUE: &[u8] = b"rxa/1";

/// Protocol version carried in [`msg::AgentMsg::Hello`]. Redundant with
/// [`PROLOGUE`] (a mismatch already fails the handshake) but cheap, and it
/// gives the gateway something to log.
pub const VERSION: u16 = 3;

/// The protocol's default TCP port, adjacent to the web server's 52380.
pub const DEFAULT_PORT: u16 = 52381;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prologue_pins_the_version() {
        // Changing this is a breaking protocol change: an agent and a gateway
        // with different prologues cannot complete a handshake at all, which
        // is the intended failure mode.
        assert_eq!(PROLOGUE, b"rxa/1");
    }
}
