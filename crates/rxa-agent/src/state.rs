//! What the agent is doing right now, shared between the network thread and the
//! menu bar.
//!
//! The agent serves one gateway at a time (see the module docs in `main.rs`), so
//! this is a single optional connection rather than a list. It exists because
//! the menu bar has to answer "is anybody watching my screen?" from the main
//! thread, while the connection that would know lives on the tokio runtime.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The gateway connection the agent is currently serving.
#[derive(Clone, Debug)]
pub struct Connection {
    /// Distinguishes this connection from the one it evicted.
    ///
    /// Without it, a session that ends *after* its replacement has already been
    /// recorded would clear the newer connection's state, and the menu would
    /// claim nobody is connected while a session is happily running.
    pub id: u64,
    pub peer: SocketAddr,
    pub since: Instant,
}

#[derive(Default)]
pub struct AgentState {
    connection: Mutex<Option<Connection>>,
    /// Only ever incremented, so an id is never reused within a run.
    last_id: AtomicU64,
}

impl AgentState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a newly accepted gateway, returning the id to hand back to
    /// [`AgentState::disconnected`] when its session ends.
    pub fn connected(&self, peer: SocketAddr, now: Instant) -> u64 {
        let id = self.last_id.fetch_add(1, Ordering::Relaxed) + 1;
        *self.connection.lock().unwrap() = Some(Connection { id, peer, since: now });
        id
    }

    /// Clear the connection — but only if `id` is still the current one.
    pub fn disconnected(&self, id: u64) {
        let mut current = self.connection.lock().unwrap();
        if current.as_ref().is_some_and(|c| c.id == id) {
            *current = None;
        }
    }

    pub fn current(&self) -> Option<Connection> {
        self.connection.lock().unwrap().clone()
    }

    pub fn is_connected(&self) -> bool {
        self.connection.lock().unwrap().is_some()
    }
}

/// The one line the menu bar leads with: is anyone connected, and for how long.
///
/// The port is left out deliberately — it is a different ephemeral number on
/// every reconnect and tells the user nothing, whereas the address answers "is
/// that me on the laptop, or something I don't recognise?".
pub fn describe(connection: Option<&Connection>, now: Instant) -> String {
    match connection {
        Some(c) => format!(
            "Sharing this screen with {} ({})",
            c.peer.ip(),
            human_duration(now.saturating_duration_since(c.since))
        ),
        None => "Not connected".to_owned(),
    }
}

/// A duration at the precision a glance wants: seconds, then minutes, then
/// hours. Nobody reading a menu bar needs "1h 3m 12s".
fn human_duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let minutes = secs / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    format!("{}h {}m", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn a_fresh_state_has_no_connection() {
        let state = AgentState::new();
        assert!(!state.is_connected());
        assert!(state.current().is_none());
    }

    #[test]
    fn connecting_and_disconnecting_round_trips() {
        let state = AgentState::new();
        let id = state.connected(peer("10.0.0.2:41234"), Instant::now());
        assert!(state.is_connected());
        assert_eq!(state.current().unwrap().peer, peer("10.0.0.2:41234"));

        state.disconnected(id);
        assert!(!state.is_connected());
    }

    // The eviction case, which is the whole reason connections carry an id: the
    // gateway reconnects, the agent records the new connection and aborts the
    // old session, and the old session's cleanup runs last.
    #[test]
    fn a_stale_session_does_not_clear_the_connection_that_replaced_it() {
        let state = AgentState::new();
        let old = state.connected(peer("10.0.0.2:1"), Instant::now());
        let new = state.connected(peer("10.0.0.3:2"), Instant::now());
        assert_ne!(old, new);

        // The evicted session finishes and reports its own disconnect.
        state.disconnected(old);
        assert!(state.is_connected(), "the newer connection must survive");
        assert_eq!(state.current().unwrap().peer, peer("10.0.0.3:2"));

        state.disconnected(new);
        assert!(!state.is_connected());
    }

    #[test]
    fn disconnecting_an_unknown_id_is_harmless() {
        let state = AgentState::new();
        state.disconnected(42);
        assert!(!state.is_connected());

        let id = state.connected(peer("10.0.0.2:1"), Instant::now());
        state.disconnected(id + 1000);
        assert!(state.is_connected());
    }

    #[test]
    fn the_summary_names_the_peer_without_its_ephemeral_port() {
        let now = Instant::now();
        let connection = Connection {
            id: 1,
            peer: peer("192.168.1.10:52344"),
            since: now - Duration::from_secs(125),
        };
        let text = describe(Some(&connection), now);
        assert!(text.contains("192.168.1.10"), "{text}");
        assert!(!text.contains("52344"), "the port is noise: {text}");
        assert!(text.contains("2m"), "{text}");
    }

    #[test]
    fn the_summary_says_so_when_nothing_is_connected() {
        assert_eq!(describe(None, Instant::now()), "Not connected");
    }

    // A clock that has gone backwards (or a connection recorded a hair after the
    // instant we compare against) must not panic — `Instant::sub` does.
    #[test]
    fn a_future_start_time_does_not_panic() {
        let now = Instant::now();
        let connection = Connection {
            id: 1,
            peer: peer("10.0.0.2:1"),
            since: now + Duration::from_secs(60),
        };
        assert!(describe(Some(&connection), now).contains("0s"));
    }

    #[test]
    fn durations_read_at_the_precision_a_glance_wants() {
        assert_eq!(human_duration(Duration::from_secs(0)), "0s");
        assert_eq!(human_duration(Duration::from_secs(59)), "59s");
        assert_eq!(human_duration(Duration::from_secs(60)), "1m");
        assert_eq!(human_duration(Duration::from_secs(3599)), "59m");
        assert_eq!(human_duration(Duration::from_secs(3600)), "1h 0m");
        assert_eq!(human_duration(Duration::from_secs(3600 * 5 + 60 * 7)), "5h 7m");
    }
}
