//! What the agent is doing right now, shared between the network thread and the
//! menu bar.
//!
//! The agent serves one *session* at a time (see the module docs in `main.rs`),
//! so this is a single optional connection rather than a list. That is a limit on
//! concurrency and not on who may connect: several gateways can be entitled to
//! reach this Mac, and the slot below is what makes exactly one of them the one
//! holding it. It exists because the menu bar has to answer "is anybody watching
//! my screen?" from the main thread, while the connection that would know lives
//! on the tokio runtime.

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
    ///
    /// Local to this process and unrelated to [`Connection::session`]: this
    /// counts connections the agent has served, that one names the session on
    /// the other end of them.
    pub id: u64,
    /// Whose session holds the slot, from
    /// [`rxa_proto::msg::GatewayMsg::Claim`]. Not a credential and not an
    /// address — it is the one value that distinguishes a session reconnecting
    /// to itself from a different one arriving, which is the whole of
    /// [`decide`].
    pub session: [u8; 16],
    /// What this Mac's authorized list calls the gateway that dialed, from the
    /// comment on its entry — `None` when the entry carried none.
    ///
    /// Known because the handshake is `Noise_IK`: the agent learns which key is
    /// calling and looks it up (see [`crate::authorized`]). It answers a question
    /// the address alone could not, now that more than one gateway may be listed —
    /// *which* of them is watching.
    pub gateway: Option<String>,
    pub peer: SocketAddr,
    pub since: Instant,
}

impl Connection {
    /// This connection as a refused claim is told about it: who holds the slot,
    /// and for how many seconds.
    ///
    /// The name off the list when there is one, and the bare address otherwise. The
    /// name is the better answer to "who is on my Mac?" — it is what its owner chose
    /// to call that gateway — and the peer being told is itself on the same list, so
    /// there is nothing here it is not already entitled to know.
    ///
    /// Either way without the ephemeral port, for the same reason [`describe`]
    /// leaves it out: it is a different number on every reconnect and answers
    /// nothing. Saturating, because a clock that has gone backwards must not panic
    /// the accept loop.
    pub fn holder(&self, now: Instant) -> (String, u32) {
        let held = now.saturating_duration_since(self.since).as_secs();
        (
            self.name().unwrap_or_else(|| self.peer.ip().to_string()),
            u32::try_from(held).unwrap_or(u32::MAX),
        )
    }

    /// The list's name for this gateway, ignoring one that is only whitespace.
    fn name(&self) -> Option<String> {
        let name = self.gateway.as_deref()?.trim();
        (!name.is_empty()).then(|| name.to_owned())
    }
}

/// What a claim on the session slot should do to whoever is in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nobody holds the slot.
    Take,
    /// The session in the slot is the one asking — it is reconnecting to itself.
    /// Every ordinary interruption arrives here: a dropped link, a target switch,
    /// a browser takeover on the gateway, and a half-open connection the agent
    /// has not yet reaped. None of them is a second client, so none of them may
    /// cost the user a prompt.
    Reclaim,
    /// A different session, and a person chose it over the incumbent.
    TakeOver,
    /// A different session that did not ask to take over. The incumbent keeps the
    /// slot and the newcomer is told who has it.
    Refuse,
}

/// Judge a claim against the slot. Pure, because this is the rule the whole
/// design rests on and it deserves to be readable and tested without a socket.
///
/// Note what is *not* an input: which key authenticated the claim, and where it
/// dialed from. Authentication decided whether this peer may ask at all; the
/// session id decides whose turn it is. Keying this on the peer's identity would
/// make "one active session" mean "one permitted gateway" — see
/// `docs/roadmap.md`.
pub fn decide(held: Option<[u8; 16]>, claiming: [u8; 16], force: bool) -> Decision {
    match held {
        None => Decision::Take,
        // Checked before `force`, so a client that asked to take over and turns
        // out to be the holder anyway is an ordinary reconnect. It costs the
        // incumbent nothing either way — they are the same session — but the log
        // line should say what actually happened.
        Some(current) if current == claiming => Decision::Reclaim,
        Some(_) if force => Decision::TakeOver,
        Some(_) => Decision::Refuse,
    }
}

#[derive(Default)]
pub struct AgentState {
    connection: Mutex<Option<Connection>>,
    /// A fatal worker failure that left the menu bar alive but stopped serving.
    ///
    /// Startup and the AppKit shell have separate lifetimes: the status item is
    /// the user's way to diagnose and quit the agent, so a dead network worker
    /// must not take it down with itself.
    failure: Mutex<Option<String>>,
    /// Only ever incremented, so an id is never reused within a run.
    last_id: AtomicU64,
}

impl AgentState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a newly accepted gateway, returning the id to hand back to
    /// [`AgentState::disconnected`] when its session ends.
    pub fn connected(
        &self,
        session: [u8; 16],
        gateway: Option<String>,
        peer: SocketAddr,
        now: Instant,
    ) -> u64 {
        let id = self.last_id.fetch_add(1, Ordering::Relaxed) + 1;
        *self.connection.lock().unwrap() = Some(Connection {
            id,
            session,
            gateway,
            peer,
            since: now,
        });
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

    /// Leave the UI alive and record why the agent can no longer serve.
    pub fn failed(&self, error: impl Into<String>) {
        *self.failure.lock().unwrap() = Some(error.into());
    }

    pub fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
    }
}

/// The one line the menu bar leads with: is anyone watching, and for how long.
///
/// Both halves answer the same question — "is somebody looking at my screen?" —
/// and both make the *gateway* the subject, because a bare "Not connected" makes
/// the agent the subject: as the top line of a menu that reads like an agent which
/// has stopped, when the line under it says it is listening. "Gateway" rather than
/// "client" to stay with the one word the logs, the docs and the config comments
/// all already use for the thing that dials in.
///
/// It leads with the list's name for that gateway when there is one, and keeps the
/// address beside it: the name is what the user recognises, and the address is what
/// they check it against. An entry with no comment falls back to the address alone,
/// which is what this line said before there was a list to name anything.
///
/// The port is left out deliberately — it is a different ephemeral number on
/// every reconnect and tells the user nothing, whereas the address answers "is
/// that me on the laptop, or something I don't recognise?".
pub fn describe(connection: Option<&Connection>, now: Instant) -> String {
    let Some(c) = connection else {
        return "No gateway connected".to_owned();
    };
    let held = human_duration(now.saturating_duration_since(c.since));
    match c.name() {
        Some(name) => format!("Sharing this screen with {name} ({}, {held})", c.peer.ip()),
        None => format!("Sharing this screen with {} ({held})", c.peer.ip()),
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
        assert!(state.failure().is_none());
    }

    #[test]
    fn a_worker_failure_is_retained_for_the_menu_bar() {
        let state = AgentState::new();
        state.failed("cannot build the network runtime");
        assert_eq!(
            state.failure().as_deref(),
            Some("cannot build the network runtime")
        );
    }

    const SESSION_A: [u8; 16] = [0xa1; 16];
    const SESSION_B: [u8; 16] = [0xb2; 16];

    #[test]
    fn connecting_and_disconnecting_round_trips() {
        let state = AgentState::new();
        let id = state.connected(
            SESSION_A,
            Some("home server".to_owned()),
            peer("10.0.0.2:41234"),
            Instant::now(),
        );
        assert!(state.is_connected());
        assert_eq!(state.current().unwrap().peer, peer("10.0.0.2:41234"));
        assert_eq!(state.current().unwrap().session, SESSION_A);
        assert_eq!(state.current().unwrap().gateway.as_deref(), Some("home server"));

        state.disconnected(id);
        assert!(!state.is_connected());
    }

    // The eviction case, which is the whole reason connections carry an id: the
    // gateway reconnects, the agent records the new connection and aborts the
    // old session, and the old session's cleanup runs last.
    #[test]
    fn a_stale_session_does_not_clear_the_connection_that_replaced_it() {
        let state = AgentState::new();
        let old = state.connected(SESSION_A, None, peer("10.0.0.2:1"), Instant::now());
        let new = state.connected(SESSION_B, None, peer("10.0.0.3:2"), Instant::now());
        assert_ne!(old, new);

        // The evicted session finishes and reports its own disconnect.
        state.disconnected(old);
        assert!(state.is_connected(), "the newer connection must survive");
        assert_eq!(state.current().unwrap().peer, peer("10.0.0.3:2"));
        // And the slot names the session that holds it, not the one that left —
        // this is what the next claim is judged against.
        assert_eq!(state.current().unwrap().session, SESSION_B);

        state.disconnected(new);
        assert!(!state.is_connected());
    }

    #[test]
    fn disconnecting_an_unknown_id_is_harmless() {
        let state = AgentState::new();
        state.disconnected(42);
        assert!(!state.is_connected());

        let id = state.connected(SESSION_A, None, peer("10.0.0.2:1"), Instant::now());
        state.disconnected(id + 1000);
        assert!(state.is_connected());
    }

    // The whole design in one table. Note the two inputs that are absent: which
    // key authenticated the claim, and where it came from.
    #[test]
    fn the_slot_is_decided_by_the_session_asking_and_nothing_else() {
        // A free slot goes to whoever asks, forced or not.
        assert_eq!(decide(None, SESSION_A, false), Decision::Take);
        assert_eq!(decide(None, SESSION_A, true), Decision::Take);

        // The holder coming back — a dropped link, a target switch, a browser
        // takeover on the gateway, a half-open connection not yet reaped. Never a
        // prompt, and never a refusal.
        assert_eq!(decide(Some(SESSION_A), SESSION_A, false), Decision::Reclaim);
        // Including when that client asked to take over: it turns out to be the
        // incumbent, so what happened was a reconnect.
        assert_eq!(decide(Some(SESSION_A), SESSION_A, true), Decision::Reclaim);

        // A different session is the only case a person has to answer.
        assert_eq!(decide(Some(SESSION_A), SESSION_B, false), Decision::Refuse);
        assert_eq!(decide(Some(SESSION_A), SESSION_B, true), Decision::TakeOver);
    }

    // A refusal has to name the incumbent to a person, and the port is noise for
    // the same reason it is in `describe`.
    #[test]
    fn the_holder_a_refusal_names_leaves_out_the_ephemeral_port() {
        let since = Instant::now();
        let connection = Connection {
            id: 1,
            session: SESSION_A,
            gateway: None,
            peer: peer("192.168.1.10:52344"),
            since,
        };
        let (holder, held) = connection.holder(since + Duration::from_secs(754));
        assert_eq!(holder, "192.168.1.10");
        assert_eq!(held, 754);

        // A clock that went backwards reads as "just now" rather than panicking
        // in the accept loop.
        let (_, held) = connection.holder(since - Duration::from_secs(5));
        assert_eq!(held, 0);
    }

    // Once the incumbent has a name off the authorized list, that is what a
    // refusal reports: "in use from home server" answers whose turn it is, where
    // an address makes the person on the other client work it out. The peer being
    // told is on the same list, so this is nothing it may not know.
    #[test]
    fn a_named_gateway_is_reported_by_name_rather_than_address() {
        let since = Instant::now();
        let mut connection = Connection {
            id: 1,
            session: SESSION_A,
            gateway: Some("home server".to_owned()),
            peer: peer("192.168.1.10:52344"),
            since,
        };
        assert_eq!(connection.holder(since).0, "home server");

        // A comment that is only whitespace is no name at all, and must not become
        // a blank where the holder should be.
        connection.gateway = Some("   ".to_owned());
        assert_eq!(connection.holder(since).0, "192.168.1.10");
    }

    #[test]
    fn the_summary_names_the_peer_without_its_ephemeral_port() {
        // Built forwards from `since`: subtracting from a fresh `Instant` panics
        // on a machine that booted less than 125s ago.
        let since = Instant::now();
        let now = since + Duration::from_secs(125);
        let connection = Connection {
            id: 1,
            session: SESSION_A,
            gateway: None,
            peer: peer("192.168.1.10:52344"),
            since,
        };
        let text = describe(Some(&connection), now);
        assert_eq!(text, "Sharing this screen with 192.168.1.10 (2m)");
        assert!(!text.contains("52344"), "the port is noise: {text}");

        // With a name on the list, that leads and the address stays beside it: the
        // name is what is recognised, the address is what it is checked against.
        let named = Connection {
            gateway: Some("home server".to_owned()),
            ..connection
        };
        assert_eq!(
            describe(Some(&named), now),
            "Sharing this screen with home server (192.168.1.10, 2m)"
        );
    }

    // The idle line has to read as "running, and nobody is looking" rather than as
    // a fault — it is the first thing in the menu, and a user who reads it as
    // "stopped" goes looking for a way to start something that is already running.
    #[test]
    fn the_idle_summary_does_not_read_as_a_stopped_agent() {
        let text = describe(None, Instant::now());
        assert_eq!(text, "No gateway connected");
        // It has to name what is absent — the gateway — rather than describe this
        // side as being in a state.
        assert!(text.contains("gateway"), "{text}");
        for alarming in ["not connected", "disconnected", "stopped", "error"] {
            assert!(!text.to_lowercase().contains(alarming), "{text}");
        }
    }

    // A clock that has gone backwards (or a connection recorded a hair after the
    // instant we compare against) must not panic — `Instant::sub` does.
    #[test]
    fn a_future_start_time_does_not_panic() {
        let now = Instant::now();
        let connection = Connection {
            id: 1,
            session: SESSION_A,
            gateway: None,
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
