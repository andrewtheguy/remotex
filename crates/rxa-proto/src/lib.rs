//! `rxa` — the wire protocol between remotex and its macOS agent.
//!
//! remotex reaches a Mac today over Apple's Screen Sharing (VNC), which is
//! flaky and re-prompts for credentials on every reconnect. `rxa` replaces that
//! hop with a purpose-built protocol: a Noise session over TCP carrying
//! pre-encoded screen tiles, authenticated by a long-lived keypair on each end,
//! so a dropped connection costs a two-message handshake and never a login. See
//! `docs/mac-agent-architecture.md`.
//!
//! Everything both sides must agree on lives here — framing, messages, the
//! handshake, the key format, and the DOM-code → macOS-keycode table — so the
//! two halves cannot drift. The crate is platform-independent on purpose: the
//! agent crate only compiles on macOS, so anything that needs a test lives
//! here instead.
//!
//! The stack, bottom up:
//!
//! - [`key`] — the X25519 identity keys each side pins the other's half of,
//!   in the house text format (a role-tagged prefix + base64url of 32 bytes
//!   and a CRC16) used across the author's repos
//! - [`noise`] — `Noise_IK_25519_ChaChaPoly_BLAKE2s` handshake helpers
//! - [`frame`] — the Noise transport as a reliable byte stream, plus the
//!   length-prefixed application framing on top of it
//! - [`msg`] — [`msg::AgentMsg`] / [`msg::GatewayMsg`]
//! - [`keymap`] — DOM `KeyboardEvent.code` → macOS `kVK_*` virtual keycode

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub mod frame;
pub mod key;
pub mod keymap;
pub mod msg;
pub mod noise;

/// Protocol version, bound into the Noise handshake as the prologue so a
/// mismatch fails at the handshake instead of desynchronising later.
pub const PROLOGUE: &[u8] = b"rxa/3";

/// Protocol version, carried in [`msg::GatewayMsg::Claim`] and
/// [`msg::AgentMsg::Hello`] — once in each direction, so whichever side is older
/// is the one that says so. Redundant with [`PROLOGUE`] (a mismatch already fails
/// the handshake) but cheap, and it gives both ends something to log.
///
/// 9 is the authorized list: the handshake became `Noise_IK` so the agent learns
/// which gateway key dialed and can look it up (see [`noise`]). Unlike 8, this one
/// *does* move [`PROLOGUE`] with it — the handshake pattern itself changed, so
/// there is no version of it both builds can complete, and the prologue is where a
/// pattern mismatch is supposed to be caught. The version below is then only for
/// the logs: it never gets read, because the handshake fails first.
///
/// 8 was the claim: the gateway speaks first, asking for the agent's session slot,
/// and `Hello` is the answer to that rather than an unprompted greeting. 7 was
/// WebP, where [`msg::format`] lost `PNG` and `JPEG` and an old agent's format
/// byte would have been read as the new codec.
pub const VERSION: u16 = 9;

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
        // is the intended failure mode. `rxa/3` is `Noise_IK`, where the agent
        // learns the dialing gateway's key and looks it up in a list; `rxa/2`
        // was `Noise_KK` and one pinned gateway; `rxa/1` was the pre-shared key
        // both replaced.
        assert_eq!(PROLOGUE, b"rxa/3");
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
