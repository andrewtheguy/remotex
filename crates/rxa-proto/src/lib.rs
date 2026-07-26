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

use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// Wall-clock milliseconds for clipboard activity timestamps. Saturation only
/// matters after the year 584,554,051 or if the system clock predates Unix.
pub fn unix_time_ms() -> u64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis();
    u64::try_from(elapsed).unwrap_or(u64::MAX)
}

/// Timestamp a newly observed clipboard change without moving backwards.
///
/// Advancing by at least one millisecond distinguishes repeated activity even
/// when the text is identical or the wall clock has not advanced.
pub fn next_clipboard_time(previous: Option<u64>) -> u64 {
    let now = unix_time_ms();
    previous.map_or(now, |last| now.max(last.saturating_add(1)))
}

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

    #[test]
    fn clipboard_time_advances_past_a_future_previous_value() {
        const FAR_FUTURE_MS: u64 = 32_503_680_000_000; // 3000-01-01 UTC
        let previous = FAR_FUTURE_MS;
        assert_eq!(
            next_clipboard_time(Some(previous)),
            previous.saturating_add(1)
        );
        assert_eq!(next_clipboard_time(Some(u64::MAX)), u64::MAX);
    }
}
