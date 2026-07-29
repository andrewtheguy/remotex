//! The single session slot and common engine boundary. A claim owns the slot,
//! an attachment supplies its WebSocket, and the selected engine may outlive a
//! brief detach. Takeover replaces the attachment without adding a session.

use std::sync::{Arc, Mutex};

use log::{debug, info, warn};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::audio::AudioBridge;
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
    /// the engine slot because that is the lifetime audio has: a subscription
    /// ([`SessionManager::set_audio`]) finds it here, and every way an engine ends
    /// takes it with it.
    ///
    /// Dropping this is not what stops the sound, and it cannot be — the engine
    /// holds the other `Arc` and would keep the queue alive until it noticed its
    /// input channel close. [`State::stop_audio`] ends the pump instead, on every
    /// path that stops an engine *and* on the one path where the engine keeps
    /// running: a takeover, where the desktop carries on for a browser that is not
    /// the one listening.
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
    /// The task forwarding this attachment's audio, while it has asked for any.
    ///
    /// One handle, not a set: one session, one attachment, one subscription (see
    /// CLAUDE.md). Holding it is what makes stopping audio *immediate* rather than
    /// eventual — the pump would otherwise sit in `recv()` until the remote's next
    /// wave buffer, which on a quiet desktop is never.
    audio_pump: Option<tokio::task::JoinHandle<()>>,
    /// Bumped by every change to the audio subscription, so a pump built while the
    /// state lock was released can tell whether it is still the one wanted.
    ///
    /// The same device as [`Self::attachment_epoch`], and needed for the same reason:
    /// [`SessionManager::set_audio`] does its expensive work — an Opus encoder and an
    /// FFT plan — outside the lock, and "is this still the current attachment" is not
    /// enough to check afterwards, because a *disable* leaves the attachment exactly as
    /// it was.
    audio_epoch: u64,
}

impl State {
    /// End the running engine, and with it any audio, reporting whether there was
    /// one. Every path that stops an engine goes through here.
    fn take_engine(&mut self) -> bool {
        self.stop_audio();
        self.engine.take().is_some()
    }

    /// Stop forwarding audio, if this attachment was.
    ///
    /// Aborting the task rather than dropping something it holds: the queue it reads
    /// belongs to the engine, which may well still be running (a takeover), and even
    /// when it is not the engine keeps the last `Arc` until it unwinds. "Eventually"
    /// is the wrong answer for sound belonging to a desktop or a browser that has
    /// already gone.
    fn stop_audio(&mut self) {
        // Bumped even when there is no pump to stop: a subscription may be *being
        // built* right now, outside the lock, and this is what tells it not to install
        // itself. Nothing depends on the value, only on it changing.
        self.audio_epoch = self.audio_epoch.wrapping_add(1);
        if let Some(pump) = self.audio_pump.take() {
            debug!("session: stopping this attachment's audio");
            pump.abort();
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
            // Audio belongs to the claim, and this claim has just replaced it. The
            // engine keeps running for whoever claimed — which is exactly why this is
            // here and not on the engine's own teardown paths: nothing else about a
            // takeover would end the previous browser's sound, and it is listening on
            // a claim it no longer owns.
            st.stop_audio();
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
        // Audio is per attachment and starts disabled until the client asks.
        st.stop_audio();

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

    /// Start or stop audio for the current authenticated attachment. Enabling
    /// replaces any existing pump; sessions without an audio source ignore it.
    pub fn set_audio(&self, attach_id: u64, enabled: bool) {
        // What the construction below needs, taken under the lock and used without it.
        // Building an encoder means allocating an Opus encoder *and* planning an FFT
        // for the resampler, which is real work — and this mutex is the one every
        // mouse move goes through (`forward_input`), so the rule this module states
        // for itself is that critical sections stay short. `audio_epoch` is what makes
        // letting go safe.
        let (bridge, events, epoch) = {
            let mut st = self.state.lock().unwrap();
            if st.client.as_ref().map(|c| c.attach_id) != Some(attach_id) {
                return;
            }
            // Unconditional, and before the enable path: this is also how "replace the
            // previous subscription" is expressed.
            st.stop_audio();
            if !enabled {
                info!("session: audio disabled by the browser");
                return;
            }
            let Some(bridge) = st.engine.as_ref().and_then(|engine| engine.audio.clone()) else {
                warn!("session: audio was asked for, but this session has no audio source");
                return;
            };
            let Some(events) = st.client.as_ref().map(|client| client.event_tx.clone()) else {
                return;
            };
            (bridge, events, st.audio_epoch)
        };

        // The negotiated format when the remote's channel is up, and otherwise the
        // only format this gateway ever advertises — which is not a guess: with one
        // advertised format that is the only format a wave buffer can be in (see
        // [`crate::rdp_audio`]), so the decoder can be configured before any
        // negotiation has happened.
        //
        // Whether it *has* is worth a line, because it is the only place the
        // difference between a quiet remote and one that will never redirect is
        // visible at all — nothing branches on it.
        let negotiated = bridge.negotiated_format();
        info!(
            "session: audio enabled, the remote's audio channel is {}",
            if negotiated.is_some() { "up" } else { "not up yet" }
        );
        let format = negotiated.unwrap_or(crate::audio::PCM_CD_QUALITY);

        let (head, packets) = match bridge.take_listener().into_packets(format) {
            Ok(pair) => pair,
            Err(e) => {
                warn!("session: no audio will be sent: {e:#}");
                return;
            }
        };

        let mut st = self.state.lock().unwrap();
        // Anything that touched audio while the lock was down wins, and the epoch is
        // what says so: a disable, a takeover, a reattach and a second enable all bump
        // it. Checking the attachment alone would miss the first of those, which keeps
        // the same `attach_id`. Dropping the stream here unsubscribes the listener that
        // was just taken, so nothing is left reading the queue.
        if st.audio_epoch != epoch
            || st.client.as_ref().map(|c| c.attach_id) != Some(attach_id)
        {
            debug!("session: discarding an audio subscription that was superseded while it was set up");
            return;
        }
        st.audio_pump = Some(tokio::spawn(async move {
            let format = ServerMsg::AudioFormat {
                codec: "opus",
                sample_rate: crate::opus_stream::OPUS_SAMPLE_RATE,
                channels: format.channels,
                head,
            };
            // Sent before any packet, and awaited rather than tried: a decoder
            // configured *after* the audio it was meant to decode has already thrown
            // that audio away.
            if events.send(AttachEvent::Msg(format)).await.is_err() {
                return;
            }
            let mut packets = std::pin::pin!(packets);
            while let Some(packets) = futures_util::StreamExt::next(&mut packets).await {
                // Awaiting here is the backpressure, and it is the right shape: a
                // browser that cannot keep up stops this task reading the queue, and
                // the queue then drops its *oldest* buffers rather than growing a
                // delay (see [`crate::audio`]).
                if events.send(AttachEvent::Msg(ServerMsg::Audio(packets))).await.is_err() {
                    break;
                }
            }
        }));
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
        // Audio does not travel on `frame_tx`, even though it ends up on the same
        // socket: that queue is the engine's, and its capacity is what backpressures
        // an engine drawing faster than a browser can paint. Sound belongs to a
        // listener rather than to the engine — it is discarded outright while nobody
        // is subscribed — so it gets a queue of its own, which [`Self::set_audio`]
        // reads (see [`crate::audio`]).
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

    /// End engine, target, claim, and attachment on logout. Unlike disconnect or
    /// detach, no state survives after the authorizing login ends.
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

    /// Enough PCM for one 20 ms Opus frame, since a smaller buffer is held by the
    /// encoder and never becomes a packet at all.
    ///
    /// What these tests care about is the lifecycle — whose audio is live and when it
    /// ends — so a packet is only ever proof that sound reached the socket. The bytes
    /// themselves are [`crate::opus_stream`]'s business.
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

    /// A connected `rdp-audio` session, with the queue the engine was handed.
    ///
    /// The engine's own channel ends come back too, and they have to: dropping them
    /// is how a fake engine dies, so a helper that kept them to itself would return an
    /// attachment that is already on its way back to the picker.
    async fn connected_audio_session(
        mgr: &Arc<SessionManager>,
        hooks: &std_mpsc::Receiver<EngineEnds>,
    ) -> (Attachment, Arc<AudioBridge>, EngineEnds) {
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "rdp-audio").unwrap();
        expect_connected_meta(&mut att.events, "rdp-audio", Meta::of(Protocol::Rdp).audio()).await;
        let ends = hooks.try_recv().unwrap();
        let audio = ends
            .2
            .clone()
            .expect("an audio target's engine is given a bridge");
        (att, audio, ends)
    }

    /// Assert the next event configures a decoder, and that it says what a decoder
    /// needs.
    async fn expect_audio_format(events: &mut mpsc::Receiver<AttachEvent>) {
        match recv(events).await {
            AttachEvent::Msg(ServerMsg::AudioFormat {
                codec,
                sample_rate,
                channels,
                head,
            }) => {
                assert_eq!(codec, "opus");
                // The *stream's* rate, not the 44100 the remote negotiated: libopus
                // encodes at 48 kHz and nothing else.
                assert_eq!(sample_rate, 48_000);
                assert_eq!(channels, 2);
                assert_eq!(&head[0..8], b"OpusHead");
            }
            other => panic!("expected the audio format, got {other:?}"),
        }
    }

    /// Assert the next event is audio, returning how many packets it carried.
    async fn expect_audio(events: &mut mpsc::Receiver<AttachEvent>) -> usize {
        match recv(events).await {
            AttachEvent::Msg(ServerMsg::Audio(packets)) => packets.len(),
            other => panic!("expected audio packets, got {other:?}"),
        }
    }

    /// Wait for the queue's subscriber count to settle at `want`.
    ///
    /// Polled rather than asserted outright because `stop_audio` aborts a task: the
    /// listener goes when the runtime drops it, which is prompt but not synchronous.
    /// Bounded, so a count that never settles fails rather than hangs.
    async fn expect_listeners(audio: &AudioBridge, want: usize) {
        for _ in 0..1000 {
            if audio.listener_count() == want {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "expected {want} audio listener(s), found {}",
            audio.listener_count()
        );
    }

    /// Everything the audio subscription has to get right in one sequence, because
    /// the pieces are only meaningful together: the pump reads the queue *the engine
    /// was given*, the format lands **before** any packet, and packets keep coming.
    ///
    /// The format's position is the part worth asserting rather than commenting: a
    /// decoder configured after the audio it was meant to decode has already thrown
    /// that audio away, and nothing downstream could recover it.
    #[tokio::test]
    async fn enabling_audio_forwards_the_engines_queue_behind_its_format() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (mut att, audio, _engine) = connected_audio_session(&mgr, &hooks).await;

        mgr.set_audio(att.id, true);
        audio.wave(one_frame_of_pcm());

        expect_audio_format(&mut att.events).await;
        assert_eq!(expect_audio(&mut att.events).await, 1, "one frame in, one packet out");

        audio.wave(one_frame_of_pcm());
        assert_eq!(expect_audio(&mut att.events).await, 1, "and it keeps going");
    }

    /// A repeated request must not leave two pumps on one queue: every packet would
    /// arrive twice, which is a decoder fault rather than a duplicate.
    ///
    /// Counted on the queue's subscribers rather than on the socket, because that is
    /// where the invariant lives — and because two pumps racing to send would not
    /// reliably show up as two frames in a row.
    #[tokio::test]
    async fn enabling_audio_twice_leaves_one_pump() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (mut att, audio, _engine) = connected_audio_session(&mgr, &hooks).await;

        mgr.set_audio(att.id, true);
        expect_listeners(&audio, 1).await;
        mgr.set_audio(att.id, true);
        expect_listeners(&audio, 1).await;

        // And the surviving one works: the replacement is a live subscription, not a
        // handle to a task that was aborted with it.
        audio.wave(one_frame_of_pcm());
        loop {
            match recv(&mut att.events).await {
                // The first pump's format, if it got one out before being aborted.
                AttachEvent::Msg(ServerMsg::AudioFormat { .. }) => continue,
                AttachEvent::Msg(ServerMsg::Audio(packets)) => {
                    assert_eq!(packets.len(), 1);
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// Disabling means it: the desktop carries on and the sound stops, which is the
    /// whole of the FAB's toggle.
    #[tokio::test]
    async fn disabling_audio_stops_it_with_the_engine_still_running() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (mut att, audio, _engine) = connected_audio_session(&mgr, &hooks).await;

        mgr.set_audio(att.id, true);
        audio.wave(one_frame_of_pcm());
        expect_audio_format(&mut att.events).await;
        assert_eq!(expect_audio(&mut att.events).await, 1);

        mgr.set_audio(att.id, false);
        expect_listeners(&audio, 0).await;

        // Nothing sent while off reaches the browser, and this says so without
        // waiting on an absence: the channel is ordered, so if that buffer had been
        // forwarded it would arrive *before* the next subscription's format.
        audio.wave(one_frame_of_pcm());
        mgr.set_audio(att.id, true);
        expect_audio_format(&mut att.events).await;

        // The engine never noticed any of this.
        assert!(mgr.state.lock().unwrap().engine.is_some());
    }

    /// Nothing to listen to is a no-op rather than an error, on both of its paths:
    /// the picker has no engine, and a target that did not opt in has no queue. The
    /// SPA and the viewer both only offer the control when `connected` said `audio`,
    /// so getting here is a client bug — and one that must not cost the socket anything.
    #[tokio::test]
    async fn asking_for_audio_without_a_source_changes_nothing() {
        let (mgr, _hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;

        mgr.set_audio(att.id, true);
        assert!(
            mgr.state.lock().unwrap().audio_pump.is_none(),
            "the picker has no audio to subscribe to"
        );

        mgr.connect(att.id, "fake").unwrap();
        expect_connected(&mut att.events, "fake").await;
        mgr.set_audio(att.id, true);
        assert!(
            mgr.state.lock().unwrap().audio_pump.is_none(),
            "a target that did not opt into audio has none to send"
        );
    }

    /// A stale attachment cannot subscribe, for the same reason it cannot inject
    /// input: an evicted-but-lingering socket must not be able to act on the slot.
    #[tokio::test]
    async fn a_superseded_attachment_cannot_enable_audio() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (mut old, audio, _engine) = connected_audio_session(&mgr, &hooks).await;

        // A takeover, then the new browser attaching in its place.
        let token_b = mgr.claim(true, None).unwrap();
        assert!(matches!(recv(&mut old.events).await, AttachEvent::Evicted));
        let mut new = mgr.attach(&token_b).unwrap();
        expect_connected_meta(&mut new.events, "rdp-audio", Meta::of(Protocol::Rdp).audio()).await;

        mgr.set_audio(old.id, true);
        assert!(
            mgr.state.lock().unwrap().audio_pump.is_none(),
            "the superseded attachment should not have subscribed anything"
        );
        expect_listeners(&audio, 0).await;
    }

    /// The one lifecycle case the engine's own lifetime cannot express: a takeover
    /// keeps the desktop running for the new browser, so the previous browser's audio
    /// has to be ended on purpose.
    #[tokio::test]
    async fn a_takeover_ends_the_audio_while_the_engine_carries_on() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (mut att_a, audio, _engine) = connected_audio_session(&mgr, &hooks).await;

        mgr.set_audio(att_a.id, true);
        audio.wave(one_frame_of_pcm());
        expect_audio_format(&mut att_a.events).await;
        assert_eq!(
            expect_audio(&mut att_a.events).await,
            1,
            "this browser's audio should be live before the takeover"
        );

        let token_b = mgr.claim(true, None).unwrap();
        assert!(matches!(recv(&mut att_a.events).await, AttachEvent::Evicted));
        expect_listeners(&audio, 0).await;
        assert!(
            mgr.state.lock().unwrap().engine.is_some(),
            "a takeover keeps the engine"
        );

        // The new holder inherits the desktop, and gets audio off the same live queue
        // once it asks — which it must do for itself: a subscription belongs to an
        // attachment, and this is a different browser.
        let mut att_b = mgr.attach(&token_b).unwrap();
        expect_connected_meta(&mut att_b.events, "rdp-audio", Meta::of(Protocol::Rdp).audio())
            .await;
        mgr.set_audio(att_b.id, true);
        audio.wave(one_frame_of_pcm());
        expect_audio_format(&mut att_b.events).await;
        assert_eq!(expect_audio(&mut att_b.events).await, 1);
    }

    /// Every way an engine ends takes its audio with it, with nothing in those paths
    /// having to know about audio: they all go through `take_engine`, which stops the
    /// pump.
    ///
    /// The subscriber count is the assertion rather than a silent socket, and for a
    /// reason worth keeping: the queue outlives the slot by however long the engine
    /// takes to notice its input channel closed, so "the bridge was dropped" would be
    /// testing something that has not happened yet.
    #[tokio::test]
    async fn ending_the_engine_ends_the_audio() {
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
            let (mut att, audio, _engine) = connected_audio_session(&mgr, &hooks).await;
            mgr.set_audio(att.id, true);
            audio.wave(one_frame_of_pcm());
            expect_audio_format(&mut att.events).await;
            assert_eq!(
                expect_audio(&mut att.events).await,
                1,
                "{what}: the audio should be live before the engine ends"
            );

            end_it(&mgr, att.id);
            expect_listeners(&audio, 0).await;
            assert!(
                mgr.state.lock().unwrap().audio_pump.is_none(),
                "{what} left the audio pump behind"
            );
        }
    }
}
