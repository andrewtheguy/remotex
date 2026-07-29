//! The session layer: the single session slot and the protocol-engine seam
//! (docs/architecture.md).
//!
//! ## The engine seam
//!
//! Every engine exposes the same contract: an async
//! `run(config, input_rx, frame_tx)` that connects to the target, consumes
//! browser input as [`ClientMsg`], emits the uniform [`ServerMsg`] stream
//! (resize, tiles, error), and returns when the session ends. That shared
//! signature *is* the seam — with three engines and no dynamic dispatch, a
//! `match` beats a trait object (which IronRDP's non-`Send` futures could not
//! implement cleanly anyway).
//!
//! One engine bends the "returns when the session ends" rule on purpose:
//! [`crate::rxa`] reconnects to its agent silently rather than ending, because
//! its keypair handshake needs no human. See that module.
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
//!   engine is spawned for it and survives a brief detach so the browser can
//!   reattach. Every protocol ends after the same browser-absence grace period.
//! - **Disconnect** ([`ClientMsg::Disconnect`], "switch target"): the engine is
//!   torn down and the slot returns to the picker, without dropping the
//!   WebSocket. An engine that ends on its own (remote hung up, connect
//!   failure) returns the slot to the picker the same way.
//! - **Detach**: the WebSocket went away. Frames keep flowing from a live
//!   engine and are dropped here during a short reattach grace period. If no
//!   browser returns, the session layer drops the engine input channel.
//!
//! One slot, permanently: takeover replaces the attached browser, never adds
//! one (see the tenet in docs/architecture.md).

use std::sync::{Arc, Mutex};

use log::{info, warn};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::audio::{AudioBridge, AudioListener};
use crate::config::{Protocol, TargetConfig};
use crate::protocol::{ClientMsg, ServerMsg};
use crate::{rdp, rxa, vnc};

/// Capacity of the engine→client frame channels. Bounded so a slow browser
/// link backpressures the engine instead of buffering unboundedly.
const FRAME_BUFFER: usize = 64;

/// How long an engine remains available for a browser to reattach after its
/// WebSocket disappears. Applies equally to RDP, VNC, and RXA.
pub const REATTACH_GRACE_PERIOD: std::time::Duration =
    std::time::Duration::from_secs(60);

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

/// A [`SessionManager::audio_listener`] was refused.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// The token is not the current claim. The audio response belongs to the
    /// claimed session and not merely to an authenticated login, so a stale
    /// token is refused here exactly as it is on `/ws`.
    #[error(transparent)]
    InvalidToken(#[from] InvalidToken),
    /// Nothing is streaming audio: the slot is on the picker, or the connected
    /// target did not opt in.
    #[error("this session has no audio")]
    NoSource,
}

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
///
/// The [`AudioBridge`] is `Some` only for a target that opted into audio, which
/// today means one RDP engine reads it and the other two never see it (see
/// [`spawn_engine`]).
type EngineSpawner = Box<
    dyn Fn(
            TargetConfig,
            mpsc::UnboundedReceiver<ClientMsg>,
            mpsc::Sender<ServerMsg>,
            Option<Arc<AudioBridge>>,
        ) + Send
        + Sync,
>;

struct EngineSlot {
    input_tx: mpsc::UnboundedSender<ClientMsg>,
    /// Guards the pump's cleanup against clearing a *newer* engine.
    generation: u64,
    /// Where this engine puts redirected audio, for an audio target. It lives on
    /// the engine slot because that is what the audio response's lifetime is: the
    /// endpoint finds it here, and every way an engine ends ends the response.
    ///
    /// Two things make that true, and neither is the `Arc` going out of scope —
    /// the engine holds the other reference and would keep the stream open until
    /// it noticed its input channel close. [`State::take_engine`] ends the
    /// listener on every path that stops an engine, and [`SessionManager::claim`]
    /// ends it for the one case where the engine *keeps running*: a takeover,
    /// where the desktop carries on for a browser that is not the one listening.
    audio: Option<Arc<AudioBridge>>,
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
    /// The running engine, if any. Remains available after detach until the
    /// reattach grace expires, a heartbeat expires, or an explicit disconnect.
    engine: Option<EngineSlot>,
    /// The attached WebSocket, if any.
    client: Option<ClientSlot>,
    /// Changes whenever the browser attachment changes. Detached-engine timers
    /// capture this value so a timer from an earlier detach cannot expire a
    /// session that reattached and later detached again.
    attachment_epoch: u64,
    next_attach_id: u64,
    next_generation: u64,
}

impl State {
    /// End the running engine, and with it any audio response, reporting whether
    /// there was one. Every path that stops an engine goes through here.
    ///
    /// Dropping the slot would get to the audio eventually — the engine notices
    /// its input channel close, unwinds, and takes the last reference to the
    /// bridge with it — but "eventually" is the wrong answer for a stream
    /// belonging to a desktop that has already gone. So the listener is ended
    /// here, before the engine has even noticed.
    fn take_engine(&mut self) -> bool {
        match self.engine.take() {
            Some(engine) => {
                if let Some(audio) = &engine.audio {
                    audio.stop_listener();
                }
                true
            }
            None => false,
        }
    }

    fn bump_epoch_for_detach(&mut self) -> Option<(u64, u64)> {
        self.attachment_epoch = self.attachment_epoch.wrapping_add(1);
        self.engine
            .as_ref()
            .map(|engine| (engine.generation, self.attachment_epoch))
    }
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

    #[cfg(test)]
    pub(crate) fn with_test_spawner(
        targets: Vec<TargetConfig>,
        spawn_engine: impl Fn(
            TargetConfig,
            mpsc::UnboundedReceiver<ClientMsg>,
            mpsc::Sender<ServerMsg>,
            Option<Arc<AudioBridge>>,
        ) + Send
        + Sync
        + 'static,
    ) -> Self {
        Self::with_spawner(targets, Box::new(spawn_engine))
    }

    /// Claim the session slot, returning the new token: a live attachment
    /// blocks the claim unless `force` (takeover) or `token` is the current
    /// claim (the same browser reclaiming after a drop). Both
    /// evict the previous WebSocket; the engine keeps running either way.
    pub fn claim(self: &Arc<Self>, force: bool, token: Option<&str>) -> Result<String, SessionBusy> {
        let (id, evicted, expiry) = {
            let mut st = self.state.lock().unwrap();
            let owns = token.is_some() && st.claim.as_deref() == token;
            if st.client.is_some() && !force && !owns {
                return Err(SessionBusy);
            }
            let id = Uuid::new_v4().to_string();
            st.claim = Some(id.clone());
            // The audio response belongs to the claim, and this claim has just
            // replaced it. The engine keeps running for whoever claimed — which
            // is exactly why this is here and not on the engine's own teardown
            // paths: nothing else about a takeover would end the previous
            // browser's stream, and it holds a token it no longer owns.
            if let Some(audio) = st.engine.as_ref().and_then(|e| e.audio.as_ref()) {
                audio.stop_listener();
            }
            let evicted = st.client.take();
            let expiry = if evicted.is_some() {
                st.bump_epoch_for_detach()
            } else {
                None
            };
            (id, evicted, expiry)
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
        if let Some((generation, attachment_epoch)) = expiry {
            self.schedule_detached_engine_expiry(generation, attachment_epoch);
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
        st.attachment_epoch = st.attachment_epoch.wrapping_add(1);

        // Tell the freshly attached browser which post-login state it is in. The
        // channel is empty, so try_send always lands.
        let status = match (&st.selected, &st.engine) {
            (Some(target), Some(engine)) => {
                info!("session: reattached to the running engine, requesting a repaint");
                let _ = engine.input_tx.send(ClientMsg::Refresh);
                ServerMsg::Connected {
                    name: target.name.clone(),
                    protocol: target.protocol.name(),
                    resize: target.resize,
                    clipboard: target.clipboard,
                    audio: target.audio,
                }
            }
            // No engine (idle, or an engine that ended): the picker.
            _ => ServerMsg::Picker,
        };
        let _ = event_tx.try_send(AttachEvent::Msg(status));

        st.client = Some(ClientSlot { attach_id: id, event_tx });
        Ok(Attachment { id, events })
    }

    /// Attach the session's one audio listener, for a client holding `token`.
    ///
    /// The audio counterpart of [`Self::attach`], and authorised the same way:
    /// the login cookie has already been checked by the time this is called, and
    /// the claim token is what proves the caller owns the *session* rather than
    /// merely holding a login. It takes no attachment id and does not require a
    /// live WebSocket — a listener is a second HTTP request, not a second
    /// browser.
    ///
    /// A second call replaces the first rather than adding to it: one session,
    /// one live audio consumer, and no shared stream (see CLAUDE.md).
    pub fn audio_listener(&self, token: &str) -> Result<AudioListener, AudioError> {
        let st = self.state.lock().unwrap();
        if st.claim.as_deref() != Some(token) {
            return Err(InvalidToken.into());
        }
        let audio = st
            .engine
            .as_ref()
            .and_then(|engine| engine.audio.as_ref())
            .ok_or(AudioError::NoSource)?;
        info!("session: audio listener attached");
        Ok(audio.take_listener())
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
        // Audio does not travel on `frame_tx`: that queue feeds the browser's
        // WebSocket through the tile encoder, and sound has no business waiting
        // behind pixels or costing an engine its frame rate. It gets a queue of
        // its own, which the HTTP endpoint reads (see [`crate::audio`]).
        let audio = target.audio.then(|| Arc::new(AudioBridge::new()));
        st.engine = Some(EngineSlot {
            input_tx,
            generation,
            audio: audio.clone(),
        });
        (self.spawn_engine)(target.clone(), input_rx, frame_tx, audio);
        tokio::spawn(Self::pump(Arc::clone(self), frame_rx, generation));

        let name = target.name.clone();
        let protocol = target.protocol.name();
        let resize = target.resize;
        let clipboard = target.clipboard;
        let audio = target.audio;
        st.selected = Some(target);
        // try_send is safe and ordered here: this runs under the state lock
        // before the just-spawned pump can acquire it, and with no engine until
        // now nothing else feeds this channel — so the buffer holds at most the
        // attach status, never 64 frames, and Connected lands before any tile.
        if let Some(client) = &st.client {
            let _ = client.event_tx.try_send(AttachEvent::Msg(ServerMsg::Connected {
                name,
                protocol,
                resize,
                clipboard,
                audio,
            }));
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
        let had_engine = st.take_engine();
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

    /// The WebSocket for attachment `id` went away. The engine remains
    /// available during [`REATTACH_GRACE_PERIOD`]; frames emitted while
    /// detached are dropped. A reattach invalidates this detach's timer.
    pub fn detach(self: &Arc<Self>, id: u64) {
        let expiry = {
            let mut st = self.state.lock().unwrap();
            if st.client.as_ref().is_none_or(|c| c.attach_id != id) {
                return;
            }
            st.client = None;
            st.bump_epoch_for_detach()
        };
        if let Some((generation, attachment_epoch)) = expiry {
            info!(
                "session: browser detached; engine available for {}s reattach grace",
                REATTACH_GRACE_PERIOD.as_secs()
            );
            self.schedule_detached_engine_expiry(generation, attachment_epoch);
        }
    }

    /// End everything the slot holds — engine, target selection, claim, and any
    /// attached browser. What logging out means.
    ///
    /// The third of three ways a session can end, and the distinction is the whole
    /// point of having it:
    ///
    /// - [`Self::disconnect`] ("switch target") ends the engine and keeps the claim,
    ///   because the same browser is about to pick again.
    /// - [`Self::detach`] keeps the *engine* for [`REATTACH_GRACE_PERIOD`], because a
    ///   browser whose socket closed may be coming back — a reload, a blip, a laptop
    ///   lid.
    /// - a log out is neither. The login that authorised the session is gone, so
    ///   nothing about it should outlive the request.
    ///
    /// Logging out used to take the `detach` path by default, because closing the
    /// socket is all the browser did and that is indistinguishable from a browser
    /// that crashed. So the remote stayed connected for the full grace period and a
    /// login inside that minute silently resumed the desktop instead of showing the
    /// picker — the session survived the credential that opened it.
    ///
    /// Takes no attachment id and checks nothing about who is calling. One login,
    /// one slot, one person (see CLAUDE.md — multi-session is out of scope, not
    /// deferred), so there is no other session this could end by mistake; and a log
    /// out that only worked from the tab holding the socket would miss the case
    /// where the socket is already down, which is exactly when the grace period is
    /// already counting.
    pub fn log_out(self: &Arc<Self>) {
        let evicted = {
            let mut st = self.state.lock().unwrap();
            st.claim = None;
            // Same reason as every other path that changes the attachment: a
            // detached-engine timer from an earlier close must not act on what is
            // left here.
            st.attachment_epoch = st.attachment_epoch.wrapping_add(1);
            let had_engine = st.take_engine();
            st.selected = None;
            if had_engine {
                info!("session: logged out; engine stopped");
            }
            st.client.take()
        };
        // Evicted rather than `Picker`: the claim this socket attached with is gone,
        // so leaving it attached would leave it holding a slot it no longer has. The
        // browser that logged out is unmounting anyway; another tab sharing the
        // cookie is logged out too, and its next request lands it on the login
        // screen.
        if let Some(client) = evicted {
            tokio::spawn(async move {
                let _ = client.event_tx.send(AttachEvent::Evicted).await;
            });
        }
    }

    /// End the current engine immediately when the WebSocket heartbeat expires.
    /// Unlike an orderly close, the heartbeat timeout has already consumed the
    /// reattach grace period while waiting for a pong.
    pub fn expire_attachment(&self, id: u64) {
        let mut st = self.state.lock().unwrap();
        if st.client.as_ref().is_none_or(|c| c.attach_id != id) {
            return;
        }
        st.client = None;
        st.attachment_epoch = st.attachment_epoch.wrapping_add(1);
        let had_engine = st.take_engine();
        st.selected = None;
        if had_engine {
            info!("session: browser heartbeat expired; engine stopped");
        }
    }

    fn schedule_detached_engine_expiry(
        self: &Arc<Self>,
        generation: u64,
        attachment_epoch: u64,
    ) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(REATTACH_GRACE_PERIOD).await;
            let expired = {
                let mut st = manager.state.lock().unwrap();
                if st.client.is_none()
                    && st.attachment_epoch == attachment_epoch
                    && st.engine.as_ref().is_some_and(|engine| engine.generation == generation)
                {
                    st.take_engine();
                    st.selected = None;
                    true
                } else {
                    false
                }
            };
            if expired {
                info!("session: reattach grace expired; engine stopped");
            }
        });
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
            if st.engine.as_ref().is_none_or(|e| e.generation != generation) {
                return;
            }
            st.take_engine();
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
/// `audio` reaches only the RDP engine, and only when the target opted in: MS-RDPEA
/// is the one audio channel any of these speak, which the config file has already
/// refused the other two protocols over.
fn spawn_engine(
    target: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
    audio: Option<Arc<AudioBridge>>,
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
            Protocol::Rdp => rt.block_on(rdp::run(target, input_rx, frame_tx, audio)),
            Protocol::Vnc => rt.block_on(vnc::run(target, input_rx, frame_tx)),
            Protocol::Rxa => rt.block_on(rxa::run(target, input_rx, frame_tx)),
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;

    use futures_util::{Stream, StreamExt as _};

    use super::*;
    use crate::audio::PCM_CD_QUALITY;
    use crate::config::Security;
    use crate::protocol::UNSCALED;

    /// A scripted engine: each spawn hands its channel ends — and the audio
    /// bridge the slot built for the target, if any — to the test, which plays
    /// the engine role directly (no task, no sockets).
    type EngineEnds = (
        mpsc::UnboundedReceiver<ClientMsg>,
        mpsc::Sender<ServerMsg>,
        Option<Arc<AudioBridge>>,
    );

    /// The per-target capabilities the connected status carries. One struct
    /// rather than four positional bools, and the same value builds the target
    /// and states the expectation — so a test cannot assert metadata the target
    /// it connected to never had.
    #[derive(Clone, Copy, Debug)]
    struct Meta {
        protocol: Protocol,
        resize: bool,
        clipboard: bool,
        audio: bool,
    }

    impl Meta {
        const fn of(protocol: Protocol) -> Self {
            Self {
                protocol,
                resize: false,
                clipboard: false,
                audio: false,
            }
        }

        const fn resize(mut self) -> Self {
            self.resize = true;
            self
        }

        const fn clipboard(mut self) -> Self {
            self.clipboard = true;
            self
        }

        const fn audio(mut self) -> Self {
            self.audio = true;
            self
        }
    }

    /// What the plain fake targets are: VNC with nothing switched on.
    const PLAIN: Meta = Meta::of(Protocol::Vnc);

    fn fake_target(name: &str) -> TargetConfig {
        fake_target_with(name, PLAIN)
    }

    fn fake_target_with(name: &str, meta: Meta) -> TargetConfig {
        TargetConfig {
            name: name.to_owned(),
            protocol: meta.protocol,
            host: "127.0.0.1".to_owned(),
            subtype: None,
            port: 1,
            username: String::new(),
            password: String::new(),
            vnc_password: String::new(),
            domain: None,
            width: 1,
            height: 1,
            security: Security::Auto,
            resize: meta.resize,
            clipboard: meta.clipboard,
            audio: meta.audio,
            agent_public_key: String::new(),
            gateway_private_key: String::new(),
        }
    }

    /// A manager over the fake targets, whose engine spawns hand their channel
    /// ends to the test (which plays the engine role directly).
    fn manager_with_fake_engine() -> (Arc<SessionManager>, std_mpsc::Receiver<EngineEnds>) {
        let (hook_tx, hook_rx) = std_mpsc::channel();
        let spawner: EngineSpawner = Box::new(move |_target, input_rx, frame_tx, audio| {
            hook_tx.send((input_rx, frame_tx, audio)).unwrap();
        });
        let targets = vec![
            fake_target("fake"),
            fake_target("other"),
            // Non-default metadata so the connected status can be checked to
            // carry each of the target's capability flags verbatim.
            fake_target_with("rdp-resize", Meta::of(Protocol::Rdp).resize()),
            fake_target_with("vnc-resize", Meta::of(Protocol::Vnc).resize()),
            fake_target_with("rxa", Meta::of(Protocol::Rxa).clipboard()),
            fake_target_with("rdp-audio", Meta::of(Protocol::Rdp).audio()),
        ];
        (Arc::new(SessionManager::with_spawner(targets, spawner)), hook_rx)
    }

    async fn recv(events: &mut mpsc::Receiver<AttachEvent>) -> AttachEvent {
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for an attach event")
            .expect("event stream ended unexpectedly")
    }

    /// The next chunk of an audio response, or `None` once it has ended.
    async fn next_chunk(
        stream: &mut (impl Stream<Item = Result<Vec<u8>, std::convert::Infallible>> + Unpin),
    ) -> Option<Vec<u8>> {
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("timed out waiting on the audio response")
            .map(|chunk| chunk.unwrap())
    }

    /// Ogg pages in a chunk of an audio response. What these tests care about is
    /// the lifecycle — whose response is open and when it ends — so a page is
    /// only ever used as proof that audio reached the stream at all. The bytes
    /// themselves are [`crate::opus_stream`]'s business.
    fn page_count(chunk: &[u8]) -> usize {
        chunk.windows(4).filter(|w| *w == b"OggS").count()
    }

    /// Enough PCM for one 20 ms Opus frame, since a smaller buffer is held by the
    /// encoder and would leave a `next_chunk` waiting for a page that never comes.
    fn one_frame_of_pcm() -> Vec<u8> {
        let frames = crate::opus_stream::FRAME_FRAMES * PCM_CD_QUALITY.sample_rate as usize
            / crate::opus_stream::OPUS_SAMPLE_RATE as usize;
        vec![0u8; frames * usize::from(PCM_CD_QUALITY.block_align())]
    }

    /// Assert the next event is the picker status.
    async fn expect_picker(events: &mut mpsc::Receiver<AttachEvent>) {
        assert!(
            matches!(recv(events).await, AttachEvent::Msg(ServerMsg::Picker)),
            "expected a picker status message"
        );
    }

    /// Assert the next event is the connected status for `name`, carrying the
    /// expected capability metadata.
    async fn expect_connected_meta(
        events: &mut mpsc::Receiver<AttachEvent>,
        name: &str,
        meta: Meta,
    ) {
        match recv(events).await {
            AttachEvent::Msg(ServerMsg::Connected {
                name: got,
                protocol: got_protocol,
                resize: got_resize,
                clipboard: got_clipboard,
                audio: got_audio,
            }) => {
                assert_eq!(got, name);
                assert_eq!(got_protocol, meta.protocol.name(), "protocol for {name}");
                assert_eq!(got_resize, meta.resize, "resize metadata for {name}");
                assert_eq!(got_clipboard, meta.clipboard, "clipboard metadata for {name}");
                assert_eq!(got_audio, meta.audio, "audio metadata for {name}");
            }
            other => panic!("expected connected({name}), got {other:?}"),
        }
    }

    /// Assert the next event is the connected status for `name`, one of the
    /// plain fake targets.
    async fn expect_connected(events: &mut mpsc::Receiver<AttachEvent>, name: &str) {
        expect_connected_meta(events, name, PLAIN).await;
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
    async fn connected_status_carries_the_targets_capability_metadata() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;

        // An RDP target with resize on: the connect status carries the
        // protocol and resize flag verbatim (the browser keys its manual-resize
        // UI off them). The VNC/no-resize case is covered by every other test's
        // expect_connected.
        let rdp_resize = Meta::of(Protocol::Rdp).resize();
        mgr.connect(att.id, "rdp-resize").unwrap();
        expect_connected_meta(&mut att.events, "rdp-resize", rdp_resize).await;
        // Keep the engine channels alive so the engine stays up across the
        // reattach below (dropping frame_tx would end it and flip to picker).
        let (_input_rx, _frame_tx, _audio) = hooks.try_recv().expect("engine spawned on connect");

        // Reattaching to the running engine reports the same metadata.
        mgr.detach(att.id);
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_connected_meta(&mut att.events, "rdp-resize", rdp_resize).await;

        // The clipboard flag travels the same way, and independently of resize:
        // the rxa fake target has clipboard on and resize off.
        let (mgr, _hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "rxa").unwrap();
        expect_connected_meta(&mut att.events, "rxa", Meta::of(Protocol::Rxa).clipboard()).await;

        // And so does audio, which is what tells the browser to offer the panel
        // that points at /api/session/audio.
        let (mgr, _hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "rdp-audio").unwrap();
        expect_connected_meta(&mut att.events, "rdp-audio", Meta::of(Protocol::Rdp).audio()).await;
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
        let (_input_rx, frame_tx, _audio) = hooks.try_recv().expect("engine spawned on connect");

        frame_tx
            .send(ServerMsg::Resize { w: 10, h: 20, scale: UNSCALED })
            .await
            .unwrap();
        assert!(matches!(
            recv(&mut att.events).await,
            AttachEvent::Msg(ServerMsg::Resize { w: 10, h: 20, scale: UNSCALED })
        ));

        // Detached: frames are dropped, the engine keeps running.
        mgr.detach(att.id);
        frame_tx
            .send(ServerMsg::Resize { w: 1, h: 1, scale: UNSCALED })
            .await
            .unwrap();
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
        frame_tx
            .send(ServerMsg::Resize { w: 30, h: 40, scale: UNSCALED })
            .await
            .unwrap();
        assert!(matches!(
            recv(&mut att.events).await,
            AttachEvent::Msg(ServerMsg::Resize { w: 30, h: 40, scale: UNSCALED })
        ));
    }

    #[tokio::test]
    async fn detached_engine_expires_after_the_grace_period_for_every_protocol() {
        tokio::time::pause();
        for (target, meta) in [
            ("rdp-resize", Meta::of(Protocol::Rdp).resize()),
            ("vnc-resize", Meta::of(Protocol::Vnc).resize()),
            ("rxa", Meta::of(Protocol::Rxa).clipboard()),
        ] {
            let protocol = meta.protocol.name();
            let (mgr, hooks) = manager_with_fake_engine();
            let token = mgr.claim(false, None).unwrap();
            let mut att = mgr.attach(&token).unwrap();
            expect_picker(&mut att.events).await;
            mgr.connect(att.id, target).unwrap();
            expect_connected_meta(&mut att.events, target, meta).await;
            let (input_rx, _frame_tx, _audio) = hooks.try_recv().unwrap();

            mgr.detach(att.id);
            tokio::task::yield_now().await;
            tokio::time::advance(REATTACH_GRACE_PERIOD - Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            assert!(!input_rx.is_closed(), "{protocol} expired before the grace period");

            tokio::time::advance(Duration::from_secs(1) + Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
            assert!(input_rx.is_closed(), "{protocol} survived the grace period");
        }
    }

    #[tokio::test]
    async fn reattach_invalidates_the_previous_detach_timer() {
        tokio::time::pause();
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "fake").unwrap();
        expect_connected(&mut att.events, "fake").await;
        let (mut input_rx, _frame_tx, _audio) = hooks.try_recv().unwrap();

        mgr.detach(att.id);
        tokio::task::yield_now().await;
        tokio::time::advance(REATTACH_GRACE_PERIOD / 2).await;
        let mut att = mgr.attach(&token).unwrap();
        expect_connected(&mut att.events, "fake").await;
        assert!(matches!(input_rx.try_recv(), Ok(ClientMsg::Refresh)));

        tokio::time::advance(REATTACH_GRACE_PERIOD).await;
        tokio::task::yield_now().await;
        assert!(!input_rx.is_closed(), "stale detach timer stopped the reattached engine");
    }

    #[tokio::test]
    async fn heartbeat_expiry_stops_the_engine_immediately() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "fake").unwrap();
        expect_connected(&mut att.events, "fake").await;
        let (input_rx, _frame_tx, _audio) = hooks.try_recv().unwrap();

        mgr.expire_attachment(att.id);
        assert!(input_rx.is_closed(), "heartbeat expiry left the engine running");
    }

    #[tokio::test]
    async fn reattach_asks_the_running_engine_for_a_refresh() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "fake").unwrap();
        expect_connected(&mut att.events, "fake").await;
        let (mut input_rx, _frame_tx, _audio) = hooks.try_recv().unwrap();
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
        let (input_rx, _frame_tx, _audio) = hooks.try_recv().unwrap();

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

    /// The bug this method exists for: logging out left the engine to the detach
    /// path, so the desktop was still there a moment later and the next login
    /// resumed it. A log out has to end the engine *and* the selection, or the
    /// attach after it reports `Connected` instead of `Picker`.
    #[tokio::test]
    async fn logging_out_stops_the_engine_and_the_next_login_lands_on_the_picker() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "fake").unwrap();
        expect_connected(&mut att.events, "fake").await;
        let (input_rx, _frame_tx, _audio) = hooks.try_recv().unwrap();

        mgr.log_out();

        assert!(input_rx.is_closed(), "logging out closes the engine input channel");
        // The attached socket does not stay attached to a slot whose claim is gone.
        assert!(matches!(recv(&mut att.events).await, AttachEvent::Evicted));
        // And the token it attached with is spent, so nothing can reattach on it.
        assert!(mgr.attach(&token).is_err(), "the claim is released");

        // The whole point: a fresh login gets the picker, not the desktop it just
        // logged out of.
        let next = mgr.claim(false, None).unwrap();
        let mut again = mgr.attach(&next).unwrap();
        expect_picker(&mut again.events).await;
        assert!(hooks.try_recv().is_err(), "no engine survived the log out");
    }

    /// A log out with nothing running must not panic or evict a socket that is not
    /// there — it is reachable from the picker, and from a browser whose socket has
    /// already closed.
    #[tokio::test]
    async fn logging_out_with_no_session_running_is_harmless() {
        let (mgr, hooks) = manager_with_fake_engine();
        mgr.log_out();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        // And again while attached but in the picker state.
        mgr.log_out();
        assert!(matches!(recv(&mut att.events).await, AttachEvent::Evicted));
        assert!(hooks.try_recv().is_err(), "no engine was ever spawned");
    }

    #[tokio::test]
    async fn takeover_evicts_the_previous_client_but_keeps_the_engine() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token_a = mgr.claim(false, None).unwrap();
        let mut att_a = mgr.attach(&token_a).unwrap();
        expect_picker(&mut att_a.events).await;
        mgr.connect(att_a.id, "fake").unwrap();
        expect_connected(&mut att_a.events, "fake").await;
        let (mut input_rx, frame_tx, _audio) = hooks.try_recv().unwrap();

        let token_b = mgr.claim(true, None).unwrap();
        assert!(matches!(recv(&mut att_a.events).await, AttachEvent::Evicted));
        // The old token is superseded.
        assert!(mgr.attach(&token_a).is_err());

        // B inherits the live desktop: connected (not the picker) + a repaint.
        let mut att_b = mgr.attach(&token_b).unwrap();
        expect_connected(&mut att_b.events, "fake").await;
        assert!(hooks.try_recv().is_err(), "takeover reuses the running engine");
        assert!(matches!(input_rx.try_recv(), Ok(ClientMsg::Refresh)));
        frame_tx
            .send(ServerMsg::Resize { w: 5, h: 6, scale: UNSCALED })
            .await
            .unwrap();
        assert!(matches!(
            recv(&mut att_b.events).await,
            AttachEvent::Msg(ServerMsg::Resize { w: 5, h: 6, scale: UNSCALED })
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
        let (_input_rx, frame_tx, _audio) = hooks.try_recv().unwrap();

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

    /// The bridge the endpoint hands out has to be the one the engine was given,
    /// or audio would arrive somewhere nobody is reading.
    #[tokio::test]
    async fn the_audio_listener_reads_what_the_engine_was_given() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "rdp-audio").unwrap();
        expect_connected_meta(&mut att.events, "rdp-audio", Meta::of(Protocol::Rdp).audio()).await;
        let (_input_rx, _frame_tx, audio) = hooks.try_recv().unwrap();
        let audio = audio.expect("an audio target's engine is given a bridge");

        let listener = mgr.audio_listener(&token).unwrap();
        let mut stream = Box::pin(listener.into_stream(PCM_CD_QUALITY));
        next_chunk(&mut stream).await.expect("the ogg header pages");
        audio.wave(one_frame_of_pcm());
        assert_eq!(page_count(&next_chunk(&mut stream).await.unwrap()), 1);
    }

    #[tokio::test]
    async fn the_audio_listener_needs_the_current_claim_and_an_audio_target() {
        let (mgr, _hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;

        // On the picker there is no engine, so nothing to listen to.
        assert!(matches!(
            mgr.audio_listener(&token),
            Err(AudioError::NoSource)
        ));

        // A target that did not opt in stays that way with a desktop up.
        mgr.connect(att.id, "fake").unwrap();
        expect_connected(&mut att.events, "fake").await;
        assert!(matches!(
            mgr.audio_listener(&token),
            Err(AudioError::NoSource)
        ));

        // And an audio target is refused a token that is not the current claim,
        // for the same reason /ws is: the stream belongs to the claimed session.
        mgr.disconnect(att.id);
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "rdp-audio").unwrap();
        expect_connected_meta(&mut att.events, "rdp-audio", Meta::of(Protocol::Rdp).audio()).await;
        assert!(matches!(
            mgr.audio_listener("not-the-claim"),
            Err(AudioError::InvalidToken(_))
        ));
        assert!(mgr.audio_listener(&token).is_ok());
    }

    /// The one lifecycle case the engine's own lifetime cannot express: a
    /// takeover keeps the desktop running for the new browser, so the previous
    /// browser's audio has to be ended on purpose.
    #[tokio::test]
    async fn a_takeover_ends_the_audio_response_while_the_engine_carries_on() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token_a = mgr.claim(false, None).unwrap();
        let mut att_a = mgr.attach(&token_a).unwrap();
        expect_picker(&mut att_a.events).await;
        mgr.connect(att_a.id, "rdp-audio").unwrap();
        expect_connected_meta(&mut att_a.events, "rdp-audio", Meta::of(Protocol::Rdp).audio())
            .await;
        let (input_rx, _frame_tx, audio) = hooks.try_recv().unwrap();
        let audio = audio.unwrap();

        let mut stream = Box::pin(mgr.audio_listener(&token_a).unwrap().into_stream(PCM_CD_QUALITY));
        next_chunk(&mut stream).await.expect("the ogg header pages");

        let token_b = mgr.claim(true, None).unwrap();
        assert!(
            next_chunk(&mut stream).await.is_none(),
            "the evicted browser's audio response should have ended"
        );
        assert!(!input_rx.is_closed(), "a takeover keeps the engine");

        // And the new holder gets its own stream off the same live engine.
        let mut stream = Box::pin(mgr.audio_listener(&token_b).unwrap().into_stream(PCM_CD_QUALITY));
        next_chunk(&mut stream).await.expect("the ogg header pages");
        audio.wave(one_frame_of_pcm());
        assert_eq!(page_count(&next_chunk(&mut stream).await.unwrap()), 1);
    }

    /// Every way an engine ends takes its audio with it, with nothing in those
    /// paths having to know about audio: the bridge lives on the engine slot, so
    /// dropping the slot is what ends the response.
    #[tokio::test]
    async fn ending_the_engine_ends_the_audio_response() {
        #[allow(clippy::type_complexity)]
        let ways: [(&str, Box<dyn Fn(&Arc<SessionManager>, u64)>); 3] = [
            ("switch target", Box::new(|mgr: &Arc<SessionManager>, id| mgr.disconnect(id))),
            ("log out", Box::new(|mgr: &Arc<SessionManager>, _| mgr.log_out())),
            (
                "heartbeat expiry",
                Box::new(|mgr: &Arc<SessionManager>, id| mgr.expire_attachment(id)),
            ),
        ];
        for (what, end_it) in ways {
            let (mgr, hooks) = manager_with_fake_engine();
            let token = mgr.claim(false, None).unwrap();
            let mut att = mgr.attach(&token).unwrap();
            expect_picker(&mut att.events).await;
            mgr.connect(att.id, "rdp-audio").unwrap();
            expect_connected_meta(&mut att.events, "rdp-audio", Meta::of(Protocol::Rdp).audio())
                .await;
            let (_input_rx, _frame_tx, _audio) = hooks.try_recv().unwrap();
            let mut stream =
                Box::pin(mgr.audio_listener(&token).unwrap().into_stream(PCM_CD_QUALITY));
            next_chunk(&mut stream).await.expect("the ogg header pages");

            end_it(&mgr, att.id);
            assert!(
                next_chunk(&mut stream).await.is_none(),
                "{what} left the audio response open"
            );
        }
    }
}
