//! The session layer: the single session slot and the protocol-engine seam
//! (docs/architecture.md).
//!
//! ## The engine seam
//!
//! Every engine exposes the same contract: an async
//! `run(config, input_rx, frame_tx)` that connects to the target, consumes
//! browser input as [`ClientMsg`], emits the uniform [`ServerMsg`] stream
//! (resize, tiles, error), and returns when the session ends. That shared
//! signature *is* the seam — with two engines and no dynamic dispatch, a
//! `match` beats a trait object (which IronRDP's non-`Send` futures could not
//! implement cleanly anyway).
//!
//! ## The single session slot
//!
//! [`SessionManager`] decouples the engine session (backend ↔ remote host)
//! from the browser attachment (backend ↔ WebSocket). The slot also holds the
//! **selected target**: `None` is the post-login *picker* state (authenticated,
//! no connection started), `Some` is a live desktop. Which target is selected
//! is slot state, so a takeover inherits it — the new browser lands on the
//! picker or the desktop exactly where the previous holder was.
//!
//! - **Claim** (`POST /api/session`): a browser obtains the slot token. If
//!   another browser's WebSocket is live, the claim needs `force` (takeover)
//!   or the current token (reclaim after a network drop).
//!   Claiming evicts the previous WebSocket but *keeps the engine running*.
//! - **Attach** (`/ws?session=<token>`): the WebSocket joins the slot. Attach
//!   does *not* start an engine — it reports the current state to the browser
//!   ([`ServerMsg::Picker`] or [`ServerMsg::Connected`]). A reattach to a
//!   running engine sends it [`ClientMsg::Refresh`] (re-announce the size and
//!   repaint from the server-owned copy).
//! - **Connect** ([`ClientMsg::Connect`]): the browser picks a target; the
//!   engine is spawned for it and survives detach — closing the browser leaves
//!   the remote session alive.
//! - **Disconnect** ([`ClientMsg::Disconnect`], "switch target"): the engine is
//!   torn down and the slot returns to the picker, without dropping the
//!   WebSocket. An engine that ends on its own (remote hung up, connect
//!   failure) returns the slot to the picker the same way.
//! - **Detach**: the WebSocket went away. Frames keep flowing from the engine
//!   and are dropped here; the engine's framebuffer stays current, so the
//!   next attach starts from a full repaint, not a replay.
//!
//! One slot, permanently: takeover replaces the attached browser, never adds
//! one (see the tenet in docs/architecture.md).

use std::sync::{Arc, Mutex};

use log::{info, warn};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::{Protocol, TargetConfig};
use crate::protocol::{ClientMsg, ServerMsg};
use crate::{rdp, vnc};

/// Capacity of the engine→client frame channels. Bounded so a slow browser
/// link backpressures the engine instead of buffering unboundedly.
const FRAME_BUFFER: usize = 64;

/// What an attached WebSocket receives from the session.
#[derive(Debug)]
pub enum AttachEvent {
    /// A message from the engine (tiles, resize, error).
    Msg(ServerMsg),
    /// Another browser took the slot; close the WebSocket (code 4001).
    Evicted,
}

/// A claim was refused because another browser's WebSocket holds the slot.
#[derive(Debug, thiserror::Error)]
#[error("another browser holds the session")]
pub struct SessionBusy;

/// An attach was refused: the token is not the current claim.
#[derive(Debug, thiserror::Error)]
#[error("invalid or superseded session token")]
pub struct InvalidToken;

/// A [`SessionManager::connect`] was refused.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The attachment is no longer the slot's current client (superseded or
    /// evicted since it attached).
    #[error("attachment is not the current session client")]
    NotCurrent,
    /// No `[[targets]]` profile has this name.
    #[error("no target named {0:?}")]
    UnknownTarget(String),
    /// A target session is already running; disconnect before connecting again.
    #[error("a session is already connected")]
    AlreadyConnected,
}

/// One WebSocket's live handle on the session slot, returned by
/// [`SessionManager::attach`]. Browser input is routed back through
/// [`SessionManager::forward_input`] (keyed by [`Attachment::id`]) rather than a
/// direct engine sender, so it always reaches the *current* engine — or is
/// dropped in the picker state — with no stale handle to manage across
/// connect/disconnect.
pub struct Attachment {
    /// Identifies this attachment for [`SessionManager::detach`],
    /// [`SessionManager::forward_input`], and the connect/disconnect calls.
    pub id: u64,
    /// Session output: engine frames, the picker/connected status messages, and
    /// the eviction signal. Ends when the slot drops this client.
    pub events: mpsc::Receiver<AttachEvent>,
}

/// Spawns a protocol engine. Injectable so the manager's unit tests can run
/// against a scripted fake instead of a real RDP/VNC connect.
type EngineSpawner = Box<
    dyn Fn(TargetConfig, mpsc::UnboundedReceiver<ClientMsg>, mpsc::Sender<ServerMsg>)
        + Send
        + Sync,
>;

struct EngineSlot {
    input_tx: mpsc::UnboundedSender<ClientMsg>,
    /// Guards the pump's cleanup against clearing a *newer* engine.
    generation: u64,
}

struct ClientSlot {
    attach_id: u64,
    event_tx: mpsc::Sender<AttachEvent>,
}

#[derive(Default)]
struct State {
    /// The current claim token. Persists across WebSocket closes so the same
    /// browser can reattach without a takeover prompt.
    claim: Option<String>,
    /// The selected target: `None` is the picker state, `Some` is a live (or
    /// just-ended) desktop. Slot state, so a takeover inherits it.
    selected: Option<TargetConfig>,
    /// The running engine, if any. Survives detach; cleared on disconnect or
    /// when it dies.
    engine: Option<EngineSlot>,
    /// The attached WebSocket, if any.
    client: Option<ClientSlot>,
    next_attach_id: u64,
    next_generation: u64,
}

/// The single session slot: owns the engine lifecycle and routes its frames
/// to whichever browser currently holds the attachment.
pub struct SessionManager {
    /// Every target profile the browser may pick from the picker.
    targets: Vec<TargetConfig>,
    spawn_engine: EngineSpawner,
    // std Mutex: every critical section is short and never held across an await.
    state: Mutex<State>,
}

impl SessionManager {
    pub fn new(targets: Vec<TargetConfig>) -> Self {
        Self::with_spawner(targets, Box::new(spawn_engine))
    }

    /// Test seam: run the manager against a scripted engine.
    fn with_spawner(targets: Vec<TargetConfig>, spawn_engine: EngineSpawner) -> Self {
        Self {
            targets,
            spawn_engine,
            state: Mutex::new(State::default()),
        }
    }

    /// Claim the session slot, returning the new token: a live attachment
    /// blocks the claim unless `force` (takeover) or `token` is the current
    /// claim (the same browser reclaiming after a drop). Both
    /// evict the previous WebSocket; the engine keeps running either way.
    pub fn claim(&self, force: bool, token: Option<&str>) -> Result<String, SessionBusy> {
        let (id, evicted) = {
            let mut st = self.state.lock().unwrap();
            let owns = token.is_some() && st.claim.as_deref() == token;
            if st.client.is_some() && !force && !owns {
                return Err(SessionBusy);
            }
            let id = Uuid::new_v4().to_string();
            st.claim = Some(id.clone());
            (id, st.client.take())
        };
        if let Some(client) = evicted {
            info!("session: evicting the attached browser (slot claimed anew)");
            // Deliver the eviction behind any frames already buffered for the
            // old WebSocket; awaiting (not try_send) means a full buffer can't
            // drop it. If the socket is already dead the send just fails.
            tokio::spawn(async move {
                let _ = client.event_tx.send(AttachEvent::Evicted).await;
            });
        }
        Ok(id)
    }

    /// Attach a WebSocket holding `token` to the slot. Does **not** start an
    /// engine — it reports the current slot state to the browser:
    /// [`ServerMsg::Connected`] when a target session is running (and asks it to
    /// [`ClientMsg::Refresh`] for a full repaint), else [`ServerMsg::Picker`].
    /// The browser drives what happens next with [`Self::connect`] /
    /// [`Self::disconnect`].
    pub fn attach(self: &Arc<Self>, token: &str) -> Result<Attachment, InvalidToken> {
        let mut st = self.state.lock().unwrap();
        if st.claim.as_deref() != Some(token) {
            return Err(InvalidToken);
        }
        // A second attach on the same token supersedes the first (e.g. the
        // browser reconnected before its stale socket timed out).
        if let Some(old) = st.client.take() {
            info!("session: superseding the previous attachment");
            let _ = old.event_tx.try_send(AttachEvent::Evicted);
        }

        let (event_tx, events) = mpsc::channel(FRAME_BUFFER);
        st.next_attach_id += 1;
        let id = st.next_attach_id;

        // Tell the freshly attached browser which post-login state it is in. The
        // channel is empty, so try_send always lands.
        let status = match (&st.selected, &st.engine) {
            (Some(target), Some(engine)) => {
                info!("session: reattached to the running engine, requesting a repaint");
                let _ = engine.input_tx.send(ClientMsg::Refresh);
                ServerMsg::Connected { name: target.name.clone() }
            }
            // No engine (idle, or an engine that ended): the picker.
            _ => ServerMsg::Picker,
        };
        let _ = event_tx.try_send(AttachEvent::Msg(status));

        st.client = Some(ClientSlot { attach_id: id, event_tx });
        Ok(Attachment { id, events })
    }

    /// Reliably deliver a session status message to `client` — a spawned
    /// awaiting send (like [`Self::claim`]'s eviction) rather than `try_send`,
    /// so a status transition isn't silently discarded when the frame channel is
    /// momentarily full (a stalled browser socket). No-op when detached.
    fn notify(client: Option<&ClientSlot>, msg: ServerMsg) {
        if let Some(client) = client {
            let tx = client.event_tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(AttachEvent::Msg(msg)).await;
            });
        }
    }

    /// Pick a target and start its engine (the picker's "connect"). The browser
    /// is told [`ServerMsg::Connected`]; the engine then paints. Refused if this
    /// attachment is no longer the current client, the name is unknown, or a
    /// session is already connected — each refusal (except a stale attachment,
    /// which isn't the current browser) tells the browser with a
    /// [`ServerMsg::Error`] so a rejected pick never hangs the picker.
    pub fn connect(
        self: &Arc<Self>,
        attach_id: u64,
        target_name: &str,
    ) -> Result<(), ConnectError> {
        let mut st = self.state.lock().unwrap();
        if st.client.as_ref().map(|c| c.attach_id) != Some(attach_id) {
            return Err(ConnectError::NotCurrent);
        }
        if st.engine.is_some() {
            Self::notify(
                st.client.as_ref(),
                ServerMsg::Error { message: "already connected to a target".to_owned() },
            );
            return Err(ConnectError::AlreadyConnected);
        }
        let target = match self.targets.iter().find(|t| t.name == target_name).cloned() {
            Some(target) => target,
            None => {
                Self::notify(
                    st.client.as_ref(),
                    ServerMsg::Error { message: format!("no target named {target_name:?}") },
                );
                return Err(ConnectError::UnknownTarget(target_name.to_owned()));
            }
        };

        info!("session: connecting to target {:?}", target.name);
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (frame_tx, frame_rx) = mpsc::channel(FRAME_BUFFER);
        st.next_generation += 1;
        let generation = st.next_generation;
        st.engine = Some(EngineSlot { input_tx, generation });
        (self.spawn_engine)(target.clone(), input_rx, frame_tx);
        tokio::spawn(Self::pump(Arc::clone(self), frame_rx, generation));

        let name = target.name.clone();
        st.selected = Some(target);
        // try_send is safe and ordered here: this runs under the state lock
        // before the just-spawned pump can acquire it, and with no engine until
        // now nothing else feeds this channel — so the buffer holds at most the
        // attach status, never 64 frames, and Connected lands before any tile.
        if let Some(client) = &st.client {
            let _ = client.event_tx.try_send(AttachEvent::Msg(ServerMsg::Connected { name }));
        }
        Ok(())
    }

    /// Tear the current engine down and return the slot to the picker ("switch
    /// target"). The WebSocket stays attached and is told [`ServerMsg::Picker`].
    /// A no-op if this attachment is not the current client.
    pub fn disconnect(&self, attach_id: u64) {
        let mut st = self.state.lock().unwrap();
        if st.client.as_ref().map(|c| c.attach_id) != Some(attach_id) {
            return;
        }
        // Dropping the EngineSlot closes the engine's input channel, which ends
        // the engine (both engines exit their loop when input_rx closes); its
        // pump then finds a newer/absent generation and does nothing.
        let had_engine = st.engine.take().is_some();
        st.selected = None;
        if had_engine {
            info!("session: disconnected; returning to the picker");
        }
        // Reliable send: the engine may have left the frame channel full, so a
        // try_send could drop the picker transition and strand the browser on a
        // dead desktop.
        Self::notify(st.client.as_ref(), ServerMsg::Picker);
    }

    /// Route one browser input message to the current engine, dropping it in the
    /// picker state or if `attach_id` is no longer the current client (so an
    /// evicted-but-lingering socket can't inject input). Session-control
    /// messages ([`ClientMsg::Connect`] / [`ClientMsg::Disconnect`]) are handled
    /// by the ws bridge and never reach here.
    pub fn forward_input(&self, attach_id: u64, msg: ClientMsg) {
        let st = self.state.lock().unwrap();
        if st.client.as_ref().map(|c| c.attach_id) != Some(attach_id) {
            return;
        }
        if let Some(engine) = &st.engine {
            let _ = engine.input_tx.send(msg);
        }
    }

    /// The WebSocket for attachment `id` went away. The engine keeps running
    /// (detached); its frames are dropped until the next attach.
    pub fn detach(&self, id: u64) {
        let mut st = self.state.lock().unwrap();
        if st.client.as_ref().is_some_and(|c| c.attach_id == id) {
            st.client = None;
            if st.engine.is_some() {
                info!("session: browser detached; engine keeps running");
            }
        }
    }

    /// Forward one engine's frames to whichever browser is attached, dropping
    /// them while detached. Ends when the engine dies, returning the slot to the
    /// picker (keeping the WebSocket) so the browser can pick again.
    async fn pump(mgr: Arc<Self>, mut frame_rx: mpsc::Receiver<ServerMsg>, generation: u64) {
        while let Some(msg) = frame_rx.recv().await {
            let event_tx = {
                let st = mgr.state.lock().unwrap();
                match &st.engine {
                    // Current engine: forward to the attached browser (if any).
                    Some(e) if e.generation == generation => {
                        st.client.as_ref().map(|c| c.event_tx.clone())
                    }
                    // Detached (engine current, no client) or superseded (a
                    // disconnect/takeover replaced this engine): drop the frame.
                    _ => None,
                }
            };
            let Some(event_tx) = event_tx else {
                continue; // detached/superseded: drop the frame, the engine owns the framebuffer
            };
            // A send error means that client is gone mid-frame; it will detach
            // itself, so just drop the frame like the detached case.
            let _ = event_tx.send(AttachEvent::Msg(msg)).await;
        }
        info!("session: engine ended");
        // If this is still the current engine (not a disconnect that already
        // replaced/cleared it), return the slot to the picker: clear the engine
        // and selection and tell the browser, but keep it attached — a fatal
        // engine `Error` reached it just before this, and now it lands on the
        // picker rather than a dropped socket.
        let event_tx = {
            let mut st = mgr.state.lock().unwrap();
            if !st.engine.as_ref().is_some_and(|e| e.generation == generation) {
                return;
            }
            st.engine = None;
            st.selected = None;
            st.client.as_ref().map(|c| c.event_tx.clone())
        };
        if let Some(event_tx) = event_tx {
            let _ = event_tx.send(AttachEvent::Msg(ServerMsg::Picker)).await;
        }
    }
}

/// Spawn the protocol engine for `target` on its own thread.
///
/// The engine runs on a dedicated thread with a current-thread runtime:
/// IronRDP's `read_pdu` future is not `Send`-general (it holds a
/// `&dyn PduHint` across await), so it can't live on the shared multi-thread
/// runtime via `tokio::spawn`; a current-thread runtime imposes no `Send`
/// bound. The VNC engine doesn't need this, but sharing the one spawn path
/// keeps the seam uniform. The engine ends when the remote host disconnects
/// (the session outlives any one browser — see [`SessionManager`]).
///
/// Scalability: this costs one OS thread + one current-thread runtime per
/// engine — fine here, since multi session is permanently out of scope
/// (single user, one active session at a time; see CLAUDE.md).
fn spawn_engine(
    target: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                warn!("session: failed to build engine runtime: {e}");
                return;
            }
        };
        match target.protocol {
            Protocol::Rdp => rt.block_on(rdp::run(target, input_rx, frame_tx)),
            Protocol::Vnc => rt.block_on(vnc::run(target, input_rx, frame_tx)),
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;

    use super::*;
    use crate::config::Security;

    /// A scripted engine: each spawn hands its channel ends to the test, which
    /// plays the engine role directly (no task, no sockets).
    type EngineEnds = (mpsc::UnboundedReceiver<ClientMsg>, mpsc::Sender<ServerMsg>);

    fn fake_target(name: &str) -> TargetConfig {
        TargetConfig {
            name: name.to_owned(),
            protocol: Protocol::Vnc,
            host: "127.0.0.1".to_owned(),
            port: 1,
            username: String::new(),
            password: String::new(),
            domain: None,
            width: 1,
            height: 1,
            security: Security::Auto,
            resize: false,
        }
    }

    /// A manager over two fake targets whose engine spawns hand their channel
    /// ends to the test (which plays the engine role directly).
    fn manager_with_fake_engine() -> (Arc<SessionManager>, std_mpsc::Receiver<EngineEnds>) {
        let (hook_tx, hook_rx) = std_mpsc::channel();
        let spawner: EngineSpawner = Box::new(move |_target, input_rx, frame_tx| {
            hook_tx.send((input_rx, frame_tx)).unwrap();
        });
        let targets = vec![fake_target("fake"), fake_target("other")];
        (Arc::new(SessionManager::with_spawner(targets, spawner)), hook_rx)
    }

    async fn recv(events: &mut mpsc::Receiver<AttachEvent>) -> AttachEvent {
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for an attach event")
            .expect("event stream ended unexpectedly")
    }

    /// Assert the next event is the picker status.
    async fn expect_picker(events: &mut mpsc::Receiver<AttachEvent>) {
        assert!(
            matches!(recv(events).await, AttachEvent::Msg(ServerMsg::Picker)),
            "expected a picker status message"
        );
    }

    /// Assert the next event is the connected status for `name`.
    async fn expect_connected(events: &mut mpsc::Receiver<AttachEvent>, name: &str) {
        match recv(events).await {
            AttachEvent::Msg(ServerMsg::Connected { name: got }) => assert_eq!(got, name),
            other => panic!("expected connected({name}), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn claim_is_free_when_nothing_is_attached_and_refuses_a_live_slot() {
        let (mgr, _hooks) = manager_with_fake_engine();

        // Free slot: anyone can claim, and again (nothing attached yet).
        let first = mgr.claim(false, None).unwrap();
        let second = mgr.claim(false, None).unwrap();
        assert_ne!(first, second, "each claim mints a fresh token");

        // Attached slot: a plain claim is refused…
        let _att = mgr.attach(&second).unwrap();
        assert!(mgr.claim(false, None).is_err());
        // …but the holder reclaims with its token, and force takes over.
        mgr.claim(false, Some(&second)).unwrap();
        mgr.claim(true, None).unwrap();
    }

    #[tokio::test]
    async fn attach_requires_the_current_token() {
        let (mgr, _hooks) = manager_with_fake_engine();
        assert!(mgr.attach("nope").is_err(), "no claim yet");
        let token = mgr.claim(false, None).unwrap();
        assert!(mgr.attach("stale").is_err());
        assert!(mgr.attach(&token).is_ok());
    }

    #[tokio::test]
    async fn attach_announces_the_picker_and_connect_starts_the_engine() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();

        // No engine yet: attach lands the browser on the picker.
        expect_picker(&mut att.events).await;
        assert!(hooks.try_recv().is_err(), "attach must not spawn an engine");

        // Picking a target starts the engine and confirms with connected.
        mgr.connect(att.id, "fake").unwrap();
        expect_connected(&mut att.events, "fake").await;
        assert!(hooks.try_recv().is_ok(), "connect spawns the engine");

        // A second connect while one is live is refused.
        assert!(matches!(
            mgr.connect(att.id, "other"),
            Err(ConnectError::AlreadyConnected)
        ));
    }

    #[tokio::test]
    async fn connect_rejects_unknown_targets_and_stale_attachments() {
        let (mgr, _hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;

        assert!(matches!(
            mgr.connect(att.id, "nope"),
            Err(ConnectError::UnknownTarget(name)) if name == "nope"
        ));
        // An attachment that is no longer the current client can't connect.
        assert!(matches!(mgr.connect(att.id + 999, "fake"), Err(ConnectError::NotCurrent)));
    }

    #[tokio::test]
    async fn frames_reach_the_attached_client_and_are_dropped_while_detached() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "fake").unwrap();
        expect_connected(&mut att.events, "fake").await;
        let (_input_rx, frame_tx) = hooks.try_recv().expect("engine spawned on connect");

        frame_tx.send(ServerMsg::Resize { w: 10, h: 20 }).await.unwrap();
        assert!(matches!(
            recv(&mut att.events).await,
            AttachEvent::Msg(ServerMsg::Resize { w: 10, h: 20 })
        ));

        // Detached: frames are dropped, the engine keeps running.
        mgr.detach(att.id);
        frame_tx.send(ServerMsg::Resize { w: 1, h: 1 }).await.unwrap();
        // Wait for the pump to consume (and drop) the detached frame — a
        // reattach racing ahead of it would legitimately receive the frame.
        tokio::time::timeout(Duration::from_secs(5), async {
            while frame_tx.capacity() < frame_tx.max_capacity() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pump never drained the detached frame");

        // Reattach to the running engine: it announces connected, then only
        // frames sent after the reattach arrive.
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_connected(&mut att.events, "fake").await;
        assert!(hooks.try_recv().is_err(), "no second engine while one runs");
        frame_tx.send(ServerMsg::Resize { w: 30, h: 40 }).await.unwrap();
        assert!(matches!(
            recv(&mut att.events).await,
            AttachEvent::Msg(ServerMsg::Resize { w: 30, h: 40 })
        ));
    }

    #[tokio::test]
    async fn reattach_asks_the_running_engine_for_a_refresh() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "fake").unwrap();
        expect_connected(&mut att.events, "fake").await;
        let (mut input_rx, _frame_tx) = hooks.try_recv().unwrap();
        assert!(
            input_rx.try_recv().is_err(),
            "a fresh engine paints on connect; no refresh needed"
        );

        // Input is routed to the current engine through the manager.
        mgr.forward_input(att.id, ClientMsg::MouseMove { x: 1, y: 2 });
        assert!(matches!(input_rx.try_recv(), Ok(ClientMsg::MouseMove { x: 1, y: 2 })));

        mgr.detach(att.id);
        let token = mgr.claim(false, None).unwrap();
        let _att = mgr.attach(&token).unwrap();
        assert!(matches!(input_rx.try_recv(), Ok(ClientMsg::Refresh)));
    }

    #[tokio::test]
    async fn disconnect_returns_to_the_picker_and_reconnect_respawns() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "fake").unwrap();
        expect_connected(&mut att.events, "fake").await;
        let (input_rx, _frame_tx) = hooks.try_recv().unwrap();

        // Switch target: the engine is torn down (its input channel closes) and
        // the browser lands back on the picker without dropping the socket.
        mgr.disconnect(att.id);
        expect_picker(&mut att.events).await;
        assert!(input_rx.is_closed(), "disconnect closes the engine input channel");

        // Picking again spawns a fresh engine — a different target this time.
        mgr.connect(att.id, "other").unwrap();
        expect_connected(&mut att.events, "other").await;
        assert!(hooks.try_recv().is_ok(), "reconnect spawns a fresh engine");
    }

    #[tokio::test]
    async fn takeover_evicts_the_previous_client_but_keeps_the_engine() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token_a = mgr.claim(false, None).unwrap();
        let mut att_a = mgr.attach(&token_a).unwrap();
        expect_picker(&mut att_a.events).await;
        mgr.connect(att_a.id, "fake").unwrap();
        expect_connected(&mut att_a.events, "fake").await;
        let (mut input_rx, frame_tx) = hooks.try_recv().unwrap();

        let token_b = mgr.claim(true, None).unwrap();
        assert!(matches!(recv(&mut att_a.events).await, AttachEvent::Evicted));
        // The old token is superseded.
        assert!(mgr.attach(&token_a).is_err());

        // B inherits the live desktop: connected (not the picker) + a repaint.
        let mut att_b = mgr.attach(&token_b).unwrap();
        expect_connected(&mut att_b.events, "fake").await;
        assert!(hooks.try_recv().is_err(), "takeover reuses the running engine");
        assert!(matches!(input_rx.try_recv(), Ok(ClientMsg::Refresh)));
        frame_tx.send(ServerMsg::Resize { w: 5, h: 6 }).await.unwrap();
        assert!(matches!(
            recv(&mut att_b.events).await,
            AttachEvent::Msg(ServerMsg::Resize { w: 5, h: 6 })
        ));
    }

    #[tokio::test]
    async fn takeover_in_the_picker_lands_the_new_browser_on_the_picker() {
        let (mgr, _hooks) = manager_with_fake_engine();
        // A never connects — it just holds the slot on the picker.
        let token_a = mgr.claim(false, None).unwrap();
        let mut att_a = mgr.attach(&token_a).unwrap();
        expect_picker(&mut att_a.events).await;

        // B force-claims and attaches: it inherits the picker state.
        let token_b = mgr.claim(true, None).unwrap();
        assert!(matches!(recv(&mut att_a.events).await, AttachEvent::Evicted));
        let mut att_b = mgr.attach(&token_b).unwrap();
        expect_picker(&mut att_b.events).await;
    }

    #[tokio::test]
    async fn engine_death_returns_to_the_picker_and_reconnect_respawns() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "fake").unwrap();
        expect_connected(&mut att.events, "fake").await;
        let (_input_rx, frame_tx) = hooks.try_recv().unwrap();

        // The engine reports a final error and dies.
        frame_tx
            .send(ServerMsg::Error { message: "remote hung up".to_owned() })
            .await
            .unwrap();
        drop(frame_tx);
        // The browser sees the error, then lands back on the picker — the socket
        // stays open.
        assert!(matches!(
            recv(&mut att.events).await,
            AttachEvent::Msg(ServerMsg::Error { .. })
        ));
        expect_picker(&mut att.events).await;

        // Picking again (same socket) spawns a fresh engine.
        mgr.connect(att.id, "fake").unwrap();
        expect_connected(&mut att.events, "fake").await;
        tokio::task::spawn_blocking(move || {
            hooks
                .recv_timeout(Duration::from_secs(5))
                .expect("a fresh engine is spawned after the old one died")
        })
        .await
        .unwrap();
    }
}
