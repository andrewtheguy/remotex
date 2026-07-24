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
//! from the browser attachment (backend ↔ WebSocket):
//!
//! - **Claim** (`POST /api/session`): a browser obtains the slot token. If
//!   another browser's WebSocket is live, the claim needs `force` (takeover)
//!   or the current token (reclaim after a network drop).
//!   Claiming evicts the previous WebSocket but *keeps the engine running*.
//! - **Attach** (`/ws?session=<token>`): the WebSocket joins the slot. The
//!   engine is spawned on first attach and survives detach — closing the
//!   browser leaves the remote session alive. A reattach sends the engine
//!   [`ClientMsg::Refresh`], which re-announces the desktop size and repaints
//!   the whole framebuffer from the server-owned copy.
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

/// One WebSocket's live handle on the session slot, returned by
/// [`SessionManager::attach`].
pub struct Attachment {
    /// Identifies this attachment for [`SessionManager::detach`].
    pub id: u64,
    /// Engine output (and the eviction signal). Ends when the engine dies.
    pub events: mpsc::Receiver<AttachEvent>,
    /// Browser input into the engine.
    pub input_tx: mpsc::UnboundedSender<ClientMsg>,
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
    /// The running engine, if any. Survives detach; cleared when it dies.
    engine: Option<EngineSlot>,
    /// The attached WebSocket, if any.
    client: Option<ClientSlot>,
    next_attach_id: u64,
    next_generation: u64,
}

/// The single session slot: owns the engine lifecycle and routes its frames
/// to whichever browser currently holds the attachment.
pub struct SessionManager {
    target: TargetConfig,
    spawn_engine: EngineSpawner,
    // std Mutex: every critical section is short and never held across an await.
    state: Mutex<State>,
}

impl SessionManager {
    pub fn new(target: TargetConfig) -> Self {
        Self::with_spawner(target, Box::new(spawn_engine))
    }

    /// Test seam: run the manager against a scripted engine.
    fn with_spawner(target: TargetConfig, spawn_engine: EngineSpawner) -> Self {
        Self {
            target,
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

    /// Attach a WebSocket holding `token` to the slot. Spawns the engine on
    /// first attach; a running engine is asked to [`ClientMsg::Refresh`] so
    /// the new browser gets the desktop size and a full repaint.
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
        st.client = Some(ClientSlot { attach_id: id, event_tx });

        let input_tx = match &st.engine {
            Some(engine) => {
                info!("session: reattached to the running engine, requesting a repaint");
                let _ = engine.input_tx.send(ClientMsg::Refresh);
                engine.input_tx.clone()
            }
            None => {
                let (input_tx, input_rx) = mpsc::unbounded_channel();
                let (frame_tx, frame_rx) = mpsc::channel(FRAME_BUFFER);
                st.next_generation += 1;
                let generation = st.next_generation;
                st.engine = Some(EngineSlot { input_tx: input_tx.clone(), generation });
                (self.spawn_engine)(self.target.clone(), input_rx, frame_tx);
                tokio::spawn(Self::pump(Arc::clone(self), frame_rx, generation));
                input_tx
            }
        };

        Ok(Attachment { id, events, input_tx })
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
    /// them while detached. Ends when the engine dies, clearing the slot so
    /// the next attach spawns a fresh engine.
    async fn pump(mgr: Arc<Self>, mut frame_rx: mpsc::Receiver<ServerMsg>, generation: u64) {
        while let Some(msg) = frame_rx.recv().await {
            let event_tx = {
                let st = mgr.state.lock().unwrap();
                st.client.as_ref().map(|c| c.event_tx.clone())
            };
            let Some(event_tx) = event_tx else {
                continue; // detached: drop the frame, the engine owns the framebuffer
            };
            // A send error means that client is gone mid-frame; it will detach
            // itself, so just drop the frame like the detached case.
            let _ = event_tx.send(AttachEvent::Msg(msg)).await;
        }
        info!("session: engine ended");
        let mut st = mgr.state.lock().unwrap();
        if st.engine.as_ref().is_some_and(|e| e.generation == generation) {
            st.engine = None;
            // Dropping the client's sender ends its event stream, closing the
            // WebSocket after any buffered frames (e.g. the final error) drain.
            st.client = None;
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

    fn manager_with_fake_engine() -> (Arc<SessionManager>, std_mpsc::Receiver<EngineEnds>) {
        let (hook_tx, hook_rx) = std_mpsc::channel();
        let spawner: EngineSpawner = Box::new(move |_target, input_rx, frame_tx| {
            hook_tx.send((input_rx, frame_tx)).unwrap();
        });
        let target = TargetConfig {
            name: "fake".to_owned(),
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
        };
        (Arc::new(SessionManager::with_spawner(target, spawner)), hook_rx)
    }

    async fn recv(events: &mut mpsc::Receiver<AttachEvent>) -> AttachEvent {
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for an attach event")
            .expect("event stream ended unexpectedly")
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
    async fn frames_reach_the_attached_client_and_are_dropped_while_detached() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        let (_input_rx, frame_tx) = hooks.try_recv().expect("engine spawned on first attach");

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

        // Reattach: only frames sent after the reattach arrive.
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
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
        let att = mgr.attach(&token).unwrap();
        let (mut input_rx, _frame_tx) = hooks.try_recv().unwrap();
        assert!(
            input_rx.try_recv().is_err(),
            "a fresh engine paints on connect; no refresh needed"
        );

        // Input flows through the attachment to the engine.
        att.input_tx.send(ClientMsg::MouseMove { x: 1, y: 2 }).unwrap();
        assert!(matches!(input_rx.try_recv(), Ok(ClientMsg::MouseMove { x: 1, y: 2 })));

        mgr.detach(att.id);
        let token = mgr.claim(false, None).unwrap();
        let _att = mgr.attach(&token).unwrap();
        assert!(matches!(input_rx.try_recv(), Ok(ClientMsg::Refresh)));
    }

    #[tokio::test]
    async fn takeover_evicts_the_previous_client_but_keeps_the_engine() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token_a = mgr.claim(false, None).unwrap();
        let mut att_a = mgr.attach(&token_a).unwrap();
        let (mut input_rx, frame_tx) = hooks.try_recv().unwrap();

        let token_b = mgr.claim(true, None).unwrap();
        assert!(matches!(recv(&mut att_a.events).await, AttachEvent::Evicted));
        // The old token is superseded.
        assert!(mgr.attach(&token_a).is_err());

        let mut att_b = mgr.attach(&token_b).unwrap();
        assert!(hooks.try_recv().is_err(), "takeover reuses the running engine");
        assert!(matches!(input_rx.try_recv(), Ok(ClientMsg::Refresh)));
        frame_tx.send(ServerMsg::Resize { w: 5, h: 6 }).await.unwrap();
        assert!(matches!(
            recv(&mut att_b.events).await,
            AttachEvent::Msg(ServerMsg::Resize { w: 5, h: 6 })
        ));
    }

    #[tokio::test]
    async fn engine_death_ends_the_event_stream_and_the_next_attach_respawns() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        let (_input_rx, frame_tx) = hooks.try_recv().unwrap();

        // The engine reports a final error and dies.
        frame_tx
            .send(ServerMsg::Error { message: "remote hung up".to_owned() })
            .await
            .unwrap();
        drop(frame_tx);
        assert!(matches!(
            recv(&mut att.events).await,
            AttachEvent::Msg(ServerMsg::Error { .. })
        ));
        assert!(
            tokio::time::timeout(Duration::from_secs(5), att.events.recv())
                .await
                .expect("timed out waiting for the stream to end")
                .is_none(),
            "engine death ends the event stream"
        );

        // Reattaching spawns a fresh engine.
        let token = mgr.claim(false, None).unwrap();
        let _att = mgr.attach(&token).unwrap();
        tokio::task::spawn_blocking(move || {
            hooks
                .recv_timeout(Duration::from_secs(5))
                .expect("a fresh engine is spawned after the old one died")
        })
        .await
        .unwrap();
    }
}
