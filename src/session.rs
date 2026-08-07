//! The single session slot and common engine boundary. A claim owns the slot,
//! an attachment supplies its WebSocket, and the selected engine may outlive a
//! brief detach. Takeover replaces the attachment without adding a session.

use std::sync::{Arc, Mutex};

use log::{debug, info, warn};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::audio::AudioBridge;
use crate::config::{Protocol, Subtype, TargetConfig};
use crate::feedback::LinkFeedback;
use crate::protocol::{ClientMsg, HostDisplay, ServerMsg};
use crate::{rdp, vnc};

/// Capacity of the engine→client frame channels. Bounded so a slow browser
/// link backpressures the engine instead of buffering unboundedly.
const FRAME_BUFFER: usize = 64;

/// The same capacity for a target on `render_type = "video"`, where a message is not
/// the same size of thing.
///
/// Sixty-four was chosen for *tiles*: [`crate::tiles::Rect::bands`] cuts damage by
/// height alone, so a full 1080p repaint is around seventeen records and sixty-four
/// of them is a few repaints' worth of slack. Under video one message is an entire
/// frame, so the same number — and there are two of these queues in series — would be
/// seconds of buffered picture before an engine felt anything at all. A session that
/// far behind its remote is unusable however good the picture is, and the delay would
/// also make the congestion signal in [`crate::encode`] arrive long after the fact.
///
/// Four rather than one: a little slack absorbs an ordinary encode landing while the
/// socket is mid-write, without letting a backlog become latency.
const VIDEO_FRAME_BUFFER: usize = 4;

/// How deep this target's outbound queue should be. See [`VIDEO_FRAME_BUFFER`].
///
/// `render_motion_subtype = "stream"` keeps the tile depth, deliberately, even though
/// it produces access units too: the same queue carries its still tiles, and a
/// repaint is dozens of them, so four would stall the engine on the ordinary path to
/// sharpen a signal about the streaming one. Its regions are also a fraction of a
/// desktop apiece, so sixty-four records is nowhere near sixty-four frames. The
/// congestion loop is correspondingly less sharp there, which is a thing to know when
/// reading `coarsened` in the `encode totals` line.
///
/// Matches the config axis rather than asking for a [`crate::config::RenderPlan`],
/// because this is called from [`SessionManager::attach`] over targets nobody has
/// connected to. The two say the same thing: `render_type = "video"` is the only
/// value that produces a plan with no tiles in it.
fn frame_buffer(target: &TargetConfig) -> usize {
    match target.render_type {
        crate::config::RenderType::Video => VIDEO_FRAME_BUFFER,
        _ => FRAME_BUFFER,
    }
}

/// How deep the audio socket's outbound queue is, in wave buffers.
///
/// One message here is one of the remote's wave buffers — ~32 KB, or ~186 ms, from a
/// Windows host at 44.1 kHz.
///
/// Two, because this queue's one job is to keep an ordinary socket write in flight
/// from stalling the pump — one message being written and one waiting is that, and
/// nothing more. This queue is FIFO, so every slot past that is stale sound
/// faithfully delivered on a link that is already behind, spent against the very
/// pictures that put it behind, and then thrown away by the client's own ceiling.
/// The bridge behind the pump drops its *oldest* and keeps sound that is still
/// live — losses on a slow link belong there
/// ([`crate::audio::AUDIO_QUEUE_DEPTH`]), and a stalled pump is what sends them
/// there.
const AUDIO_SOCKET_BUFFER: usize = 2;

/// How long an engine remains available for a browser to reattach after its
/// WebSocket disappears. Applies equally to RDP and VNC.
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
    /// Where this attachment's paint tracker publishes the link's lag for the
    /// encoders. The slot's one handle, freshly [`LinkFeedback::reset`]; the ws
    /// bridge writes through it for as long as the attachment lives.
    pub feedback: Arc<LinkFeedback>,
}

/// One audio WebSocket's live handle on the session, returned by
/// [`SessionManager::attach_audio`].
///
/// Audio has its own socket because it had no business sharing the other one: on a
/// `render_type = "video"` target that queue is four deep, and an audio pump waiting
/// behind a video backlog stops draining the bridge, which then drops wave buffers.
/// Every reference client this was checked against — Myrtille, FreeRDP, Guacamole —
/// keeps sound off the path that carries pictures.
pub struct AudioAttachment {
    /// Identifies this socket for [`SessionManager::detach_audio`], so a socket
    /// closing *after* it was superseded cannot clear its replacement.
    pub id: u64,
    /// [`ServerMsg::AudioFormat`], then [`ServerMsg::Audio`]. Nothing else is ever
    /// sent here, and the format arrives again whenever a new engine is armed.
    pub packets: mpsc::Receiver<ServerMsg>,
    /// Resolves when the session drops this socket — a takeover, a log out, or a
    /// newer audio socket. Both outcomes mean the same thing, so the caller does not
    /// inspect it.
    pub evicted: oneshot::Receiver<()>,
}

/// Spawns a protocol engine. Injectable so the manager's unit tests can run
/// against a scripted fake instead of a real RDP/VNC connect.
///
/// The [`AudioBridge`] is `Some` only for a target that opted into audio, which
/// today means one RDP engine reads it and the other never sees it (see
/// [`spawn_engine`]).
///
type EngineSpawner = Box<
    dyn Fn(
            TargetConfig,
            Option<HostDisplay>,
            mpsc::UnboundedReceiver<ClientMsg>,
            mpsc::Sender<ServerMsg>,
            Option<Arc<AudioBridge>>,
            Arc<LinkFeedback>,
        ) + Send
        + Sync,
>;

struct EngineSlot {
    input_tx: mpsc::UnboundedSender<ClientMsg>,
    /// Guards the pump's cleanup against clearing a *newer* engine.
    generation: u64,
    /// The render dial this engine resolved to, as one line for the client's session card.
    ///
    /// Held rather than recomputed: it is resolved when
    /// the engine is built and cannot change while the engine runs, so a reattach reports
    /// what is *running* rather than what today's config file says.
    render: String,
    /// Where this engine puts redirected audio, for an audio target. It lives on
    /// the engine slot because that is the lifetime audio has: a subscription
    /// ([`SessionManager::arm_audio`]) finds it here, and every way an engine ends
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

/// The dedicated audio WebSocket, while one is open.
///
/// Bound to the *claim*, not to a [`ClientSlot`]: a main-socket reattach, a target
/// switch and an engine death all leave it alone, and only a change of claim ends it.
/// That one sentence is its whole lifetime rule, and every teardown path below is that
/// sentence applied. It is a field of [`State`] rather than of `ClientSlot` for the same
/// reason — `ClientSlot` is destroyed by exactly the events audio now has to survive,
/// and audio playing with no main socket attached (mid-blip, inside the reattach grace)
/// is a state that has to be representable.
struct AudioSlot {
    id: u64,
    packets: mpsc::Sender<ServerMsg>,
    /// Held, never sent on: dropping this slot resolves the socket's receiver, which
    /// is what closes it. A signal rather than closing `packets`, because a browser
    /// that has been taken over must stop hearing the session *now* rather than after
    /// however much sound is still queued for it.
    _close: oneshot::Sender<()>,
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
    /// The attached *audio* WebSocket, if any. See [`AudioSlot`].
    audio: Option<AudioSlot>,
    next_audio_id: u64,
    /// Changes whenever the browser attachment changes. Detached-engine timers
    /// capture this value so a timer from an earlier detach cannot expire a
    /// session that reattached and later detached again.
    attachment_epoch: u64,
    next_attach_id: u64,
    next_generation: u64,
    /// The task forwarding audio to [`Self::audio`], while there is both a socket to
    /// send to and an engine producing any.
    ///
    /// One handle, not a set: one session, one audio socket, one subscription (see
    /// CLAUDE.md). Holding it is what makes stopping audio *immediate* rather than
    /// eventual — the pump would otherwise sit in `recv()` until the remote's next
    /// wave buffer, which on a quiet desktop is never.
    audio_pump: Option<tokio::task::JoinHandle<()>>,
    /// Bumped by every change to the audio subscription, so a pump built while the
    /// state lock was released can tell whether it is still the one wanted.
    ///
    /// The same device as [`Self::attachment_epoch`], and needed for the same reason:
    /// [`SessionManager::arm_audio`] does its expensive work — an Opus encoder and an
    /// FFT plan — outside the lock. Comparing the socket's id afterwards is not enough
    /// on its own, because a target switch re-arms the *same* socket.
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

    /// End the audio socket outright: the claim it attached to is no longer its claim.
    ///
    /// Stopping the pump is *not* enough on its own. The slot would survive, and the
    /// next [`SessionManager::connect`] would re-arm it — so a browser that had been
    /// shown a takeover screen would start hearing the new holder's desktop. Closing
    /// the socket is the only thing that ends that.
    fn evict_audio(&mut self) {
        self.stop_audio();
        if self.audio.take().is_some() {
            info!("session: closing the audio socket; the claim changed under it");
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
    /// The slot's one link-feedback handle, shared between whichever ws bridge is
    /// attached (writer) and whichever engine is running (reader). One rather than
    /// one per attachment because the engine outlives attachments: the handle's
    /// identity has to survive a reattach for the running [`crate::encode::TileSink`]
    /// to keep reading it, and [`LinkFeedback::reset`] on every attachment change is
    /// what keeps its *contents* from outliving the browser they measured.
    feedback: Arc<LinkFeedback>,
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
            feedback: Arc::new(LinkFeedback::new()),
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
        // The scripted engines play the browser-facing role directly and never
        // read the link or a client screen, so the seam hides both from them.
        Self::with_spawner(
            targets,
            Box::new(move |target, _display, input_rx, frame_tx, audio, _feedback| {
                spawn_engine(target, input_rx, frame_tx, audio);
            }),
        )
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
            // Audio belongs to the claim, and this claim has just replaced it — unless
            // it is the same browser reclaiming, which is what `owns` is for and what
            // lets sound survive a reconnect. The condition is `!owns` rather than
            // `force` on purpose: a plain claim succeeds whenever no socket is attached,
            // so a second browser arriving during a detach gets the slot with no
            // takeover prompt anywhere — and must not inherit somebody else's ears.
            if !owns {
                st.evict_audio();
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
        // Audio is deliberately *not* touched here. It belongs to the claim, not to
        // this socket, so a browser that dropped and came back is still listening —
        // which is the whole point of giving it a socket of its own.

        // Sized for whatever is selected, because both queues are in series between
        // the engine and the socket: leaving this one deep would put the buffering
        // back that the engine's own shallow queue was meant to remove.
        //
        // With nothing selected, sized for the shallowest thing that *could* be picked
        // next — which is the ordinary case, not an edge one: the first attach always
        // lands on the picker, and `connect` builds a new engine channel but not a new
        // attachment. Sizing this for tiles and then connecting to a video target
        // would leave 64 whole frames of slack in front of the engine's 4, which is
        // the backpressure `Congestion` reads to notice a link falling behind. A
        // gateway with no video target keeps 64 exactly as before.
        let depth = st.selected.as_ref().map_or_else(
            || self.targets.iter().map(frame_buffer).min().unwrap_or(FRAME_BUFFER),
            frame_buffer,
        );
        let (event_tx, events) = mpsc::channel(depth);
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
                    subtype: target.subtype.map(Subtype::name),
                    resize: target.resize,
                    clipboard: target.clipboard,
                    audio: target.audio,
                    render: engine.render.clone(),
                }
            }
            // No engine (idle, or an engine that ended): the picker.
            _ => ServerMsg::Picker,
        };
        let _ = event_tx.try_send(AttachEvent::Msg(status));

        st.client = Some(ClientSlot { attach_id: id, event_tx });
        // A fresh browser starts unmeasured: whatever the last one's link looked
        // like, this one has not shown its own yet.
        self.feedback.reset();
        Ok(Attachment { id, events, feedback: Arc::clone(&self.feedback) })
    }

    /// Attach the audio WebSocket holding `token`. Opening the socket *is* the
    /// subscription, and closing it is the only way to stop.
    ///
    /// Accepted in every session state, including the picker and a target that carries
    /// no sound: the socket is bound to the claim rather than to a source, so a session
    /// with nothing to send is silent, not refused. That is what lets [`Self::connect`]
    /// re-arm the same socket when a target is picked.
    pub fn attach_audio(self: &Arc<Self>, token: &str) -> Result<AudioAttachment, InvalidToken> {
        let attachment = {
            let mut st = self.state.lock().unwrap();
            if st.claim.as_deref() != Some(token) {
                return Err(InvalidToken);
            }
            // Supersede is evict-then-install, so there is one code path for both: a
            // second socket on the same claim closes the first.
            st.evict_audio();
            let (packets_tx, packets) = mpsc::channel(AUDIO_SOCKET_BUFFER);
            let (close_tx, evicted) = oneshot::channel();
            st.next_audio_id += 1;
            let id = st.next_audio_id;
            st.audio = Some(AudioSlot { id, packets: packets_tx, _close: close_tx });
            info!("session: an audio socket attached");
            AudioAttachment { id, packets, evicted }
        };
        // Outside the lock, and unconditional: an engine may already be running, in
        // which case this socket has sound waiting for it.
        self.arm_audio();
        Ok(attachment)
    }

    /// The audio socket `id` went away. No grace period — unlike an engine there is
    /// nothing here worth preserving, and a pump writing into a socket that has gone is
    /// the exact stall this endpoint exists to remove.
    pub fn detach_audio(&self, id: u64) {
        let mut st = self.state.lock().unwrap();
        if st.audio.as_ref().is_none_or(|slot| slot.id != id) {
            return;
        }
        st.stop_audio();
        st.audio = None;
        info!("session: the audio socket went away");
    }

    /// Install the audio pump if — and only if — there is both a socket to send to and
    /// an engine producing sound.
    ///
    /// Called from exactly two places, because only two things can make that conjunction
    /// become true: [`Self::attach_audio`], where the socket appears, and
    /// [`Self::connect`], where the engine does. The second of those is the target
    /// switch: the socket outlives the engine, so picking a new desktop has to hand it
    /// the new engine's queue.
    fn arm_audio(&self) {
        // What the construction below needs, taken under the lock and used without it.
        // Building an encoder means allocating a codec *and* planning an FFT for the
        // resampler, which is real work — and this mutex is the one every mouse move
        // goes through (`forward_input`), so the rule this module states for itself is
        // that critical sections stay short. `audio_epoch` is what makes letting go
        // safe.
        let (bridge, out, audio_id, epoch, plan) = {
            let mut st = self.state.lock().unwrap();
            // Unconditional, and first: this is also how "replace the previous pump" is
            // expressed, and it is what tells a build already in flight to stand down.
            st.stop_audio();
            let Some((audio_id, out)) =
                st.audio.as_ref().map(|slot| (slot.id, slot.packets.clone()))
            else {
                // Nobody is listening. Not a fault: the ordinary state of a session
                // whose browser never asked for sound.
                return;
            };
            let Some(bridge) = st.engine.as_ref().and_then(|engine| engine.audio.clone()) else {
                debug!("session: an audio socket is open, but this session has no audio source");
                return;
            };
            // The target's, not a session setting: the codec and its rate are a
            // property of the link to this desktop, which is what the operator
            // configured them from.
            let plan = st
                .selected
                .as_ref()
                .map(|target| target.audio_plan())
                .unwrap_or_default();
            (bridge, out, audio_id, st.audio_epoch, plan)
        };

        // The negotiated format when the remote's channel is up, and otherwise the
        // only format this gateway ever advertises — which is not a guess: with one
        // advertised format that is the only format a wave buffer can be in (see
        // the RDP audio channel), so the decoder can be configured before any
        // negotiation has happened.
        //
        // Whether it *has* is worth a line, because it is the only place the
        // difference between a quiet remote and one that will never redirect is
        // visible at all — nothing branches on it.
        let negotiated = bridge.negotiated_format();
        info!(
            "session: arming audio, the remote's audio channel is {}",
            if negotiated.is_some() { "up" } else { "not up yet" }
        );
        let format = negotiated.unwrap_or(crate::audio::PCM_CD_QUALITY);

        let encoded = match bridge.take_listener().into_packets(format, plan) {
            Ok(encoded) => encoded,
            Err(e) => {
                warn!("session: no audio will be sent: {e:#}");
                return;
            }
        };
        // The adaptive walk, when the plan asked for one. It lives in the pump —
        // the side whose sends block — and publishes through the signals the
        // encoder reads; see [`crate::audio::AudioCongestion`].
        let mut congestion = match (encoded.signals.clone(), plan.adaptive_floor_bps) {
            (Some(signals), Some(floor)) => {
                Some(crate::audio::AudioCongestion::new(plan.bitrate_bps, floor, signals))
            }
            _ => None,
        };

        let mut st = self.state.lock().unwrap();
        // Anything that touched audio while the lock was down wins, and the epoch is
        // what says so. The socket id alone would not be enough: a target switch re-arms
        // the *same* socket, so two `arm_audio` calls racing over one id resolve by
        // whichever bumped the epoch last. Dropping the stream here unsubscribes the
        // listener that was just taken, so nothing is left reading the queue.
        if st.audio_epoch != epoch || st.audio.as_ref().map(|slot| slot.id) != Some(audio_id) {
            debug!("session: discarding an audio subscription that was superseded while it was set up");
            return;
        }
        st.audio_pump = Some(tokio::spawn(async move {
            let announce = ServerMsg::AudioFormat {
                codec: encoded.codec,
                sample_rate: encoded.sample_rate,
                channels: encoded.channels,
                packet_frames: encoded.packet_frames,
                head: encoded.head,
            };
            // Sent before any packet, and awaited rather than tried: a decoder
            // configured *after* the audio it was meant to decode has already thrown
            // that audio away.
            if out.send(announce).await.is_err() {
                return;
            }
            let mut packets = std::pin::pin!(encoded.packets);
            while let Some(packets) = futures_util::StreamExt::next(&mut packets).await {
                // Awaiting here is the backpressure, and it is the right shape: a
                // browser that cannot keep up stops this task reading the queue, and
                // the queue then drops its *oldest* buffers rather than growing a
                // delay (see [`crate::audio`]). What it no longer waits behind is a
                // video frame.
                //
                // How long the await took is also the adaptive walk's whole
                // signal: the queue is two deep, so blocking at all means the
                // socket is not draining sound as fast as the remote produces it.
                let queued = tokio::time::Instant::now();
                if out.send(ServerMsg::Audio(packets)).await.is_err() {
                    break;
                }
                if let Some(congestion) = &mut congestion {
                    let now = tokio::time::Instant::now();
                    if let Some(bps) = congestion.observe(now - queued, now) {
                        debug!("session: the audio walk asks for {} kbit/s", bps / 1000);
                    }
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
    /// attachment is no longer the current client, the name is unknown, a
    /// session is already connected — each refusal (except a stale attachment, which isn't
    /// the current browser) tells the browser with a [`ServerMsg::Error`] so a rejected pick
    /// never hangs the picker.
    ///
    /// Nothing here asks the browser what it can decode. A client that cannot decode what a
    /// streaming target sends says so from its own `VideoDecoder` rather than being refused
    /// here on the strength of a probe.
    /// `display` is the client's screen from [`ClientMsg::Connect`], handed to
    /// the engine at spawn so a High Performance session can open its virtual
    /// display at that screen's full resolution.
    pub fn connect(
        self: &Arc<Self>,
        attach_id: u64,
        target_name: &str,
        display: Option<HostDisplay>,
    ) -> Result<(), ConnectError> {
        // Scoped so the lock is released before the re-arm below: every refusal returns
        // from inside it, so only the success path falls through.
        {
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

        let render = target.render_plan().describe();

        info!("session: connecting to target {:?} ({render})", target.name);
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (frame_tx, frame_rx) = mpsc::channel(frame_buffer(&target));
        st.next_generation += 1;
        let generation = st.next_generation;
        // Audio travels on neither `frame_tx` nor the socket it feeds: that queue is
        // the engine's, and its capacity is what backpressures an engine drawing faster
        // than a browser can paint. Sound belongs to a listener rather than to the
        // engine — it is discarded outright while nobody is subscribed — so it gets a
        // queue of its own, which [`Self::arm_audio`] reads (see [`crate::audio`]), and
        // a socket of its own beyond that.
        let audio = target.audio.then(|| Arc::new(AudioBridge::new()));
        st.engine = Some(EngineSlot {
            input_tx,
            generation,
            render: render.clone(),
            audio: audio.clone(),
        });
        (self.spawn_engine)(
            target.clone(),
            display,
            input_rx,
            frame_tx,
            audio,
            Arc::clone(&self.feedback),
        );
        tokio::spawn(Self::pump(Arc::clone(self), frame_rx, generation));

        let name = target.name.clone();
        let protocol = target.protocol.name();
        let subtype = target.subtype.map(Subtype::name);
        let resize = target.resize;
        let clipboard = target.clipboard;
        let audio = target.audio;
        st.selected = Some(target);
        // try_send is safe and ordered here: this runs under the state lock
        // before the just-spawned pump can acquire it, and with no engine until
        // now nothing else feeds this channel — so the buffer holds at most the
        // attach status rather than a queue's worth of frames, whatever depth it
        // was given, and Connected lands before any tile.
        if let Some(client) = &st.client {
            let _ = client.event_tx.try_send(AttachEvent::Msg(ServerMsg::Connected {
                name,
                protocol,
                subtype,
                resize,
                clipboard,
                audio,
                render,
            }));
        }
        }
        // The engine that just appeared is the half an already-open audio socket was
        // missing. This is the target switch: the socket outlived the previous desktop,
        // so it is handed this one without the browser asking again.
        self.arm_audio();
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
        // No browser, no lag: an engine surviving the grace period must not spend
        // it coarsening quality against the measurements of a socket that is gone.
        self.feedback.reset();
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
            // The claim is gone, so the socket bound to it goes with it — the same
            // reasoning as the main socket's eviction below.
            st.evict_audio();
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
/// The engine runs on a dedicated thread with a current-thread runtime. The
/// reason has changed and the arrangement has not: it used to be that IronRDP's
/// `read_pdu` future was not `Send`-general, so it could not live on the shared
/// multi-thread runtime. The RDP engine is FreeRDP now, which owns *its own* OS
/// thread and a blocking event loop — so what this isolates is a session's whole
/// lifetime from the runtime serving HTTP, which matters more rather than less
/// now that a C library is in there. The VNC engine doesn't need either
/// property, but sharing the one spawn path keeps the seam uniform. The engine
/// ends when the remote host disconnects (the session outlives any one browser —
/// see [`SessionManager`]).
///
/// Scalability: this costs one OS thread + one current-thread runtime per
/// engine — fine here, since multi session is permanently out of scope
/// (single user, one active session at a time; see CLAUDE.md).
/// `audio` reaches only the RDP engine, and only when the target opted in: MS-RDPEA
/// is the one audio channel either of these speaks, which the config file has
/// already refused the VNC protocol over.
fn spawn_engine(
    target: TargetConfig,
    display: Option<HostDisplay>,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
    audio: Option<Arc<AudioBridge>>,
    feedback: Arc<LinkFeedback>,
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
            Protocol::Rdp => {
                rt.block_on(rdp::run(target, display, input_rx, frame_tx, audio, feedback))
            }
            Protocol::Vnc => rt.block_on(vnc::run(target, display, input_rx, frame_tx, feedback)),
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

    /// A scripted engine: each spawn hands its channel ends — and the audio bridge the
    /// slot built for the target, if any — to the test, which plays the engine role
    /// directly (no task, no sockets).
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
        /// `None` is the target saying nothing, which is Opus.
        audio_codec: Option<crate::config::AudioCodec>,
    }

    impl Meta {
        const fn of(protocol: Protocol) -> Self {
            Self {
                protocol,
                resize: false,
                clipboard: false,
                audio: false,
                audio_codec: None,
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

        const fn audio_codec(mut self, codec: crate::config::AudioCodec) -> Self {
            self.audio = true;
            self.audio_codec = Some(codec);
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
            width: Some(1),
            height: Some(1),
            security: Security::Auto,
            egfx: true,
            resize: meta.resize,
            clipboard: meta.clipboard,
            audio: meta.audio,
            audio_codec: meta.audio_codec,
            render_type: crate::config::RenderType::Full,
            render_subtype: crate::config::RenderSubtype::Png,
            render_quality: None,
            render_motion_subtype: None,
            render_motion_quality: None,
            render_motion_debug: false,
            render_adaptive: false,
            render_adaptive_min: None,
            audio_bitrate: None,
            audio_adaptive: false,
            audio_bitrate_min: None,
        }
    }

    /// A target that streams the whole desktop as video.
    fn video_target(name: &str) -> TargetConfig {
        TargetConfig {
            render_type: crate::config::RenderType::Video,
            render_quality: Some(60),
            ..fake_target(name)
        }
    }

    /// A manager over the fake targets, whose engine spawns hand their channel
    /// ends to the test (which plays the engine role directly).
    fn manager_with_fake_engine() -> (Arc<SessionManager>, std_mpsc::Receiver<EngineEnds>) {
        let (hook_tx, hook_rx) = std_mpsc::channel();
        let spawner: EngineSpawner =
            Box::new(
                move |_target: TargetConfig, _display, input_rx, frame_tx, audio, _feedback| {
                    hook_tx.send((input_rx, frame_tx, audio)).unwrap();
                },
            );
        let targets = vec![
            fake_target("fake"),
            fake_target("other"),
            // Non-default metadata so the connected status can be checked to
            // carry each of the target's capability flags verbatim.
            fake_target_with("rdp-resize", Meta::of(Protocol::Rdp).resize()),
            fake_target_with("vnc-resize", Meta::of(Protocol::Vnc).resize()),
            fake_target_with("vnc-clip", Meta::of(Protocol::Vnc).clipboard()),
            fake_target_with("rdp-audio", Meta::of(Protocol::Rdp).audio()),
            fake_target_with(
                "rdp-pcm",
                Meta::of(Protocol::Rdp).audio_codec(crate::config::AudioCodec::Pcm),
            ),
            // A target that streams. Nothing refuses it: the browser capability probe
            // that could is gone, and what a client can decode is answered by its own
            // decoder rather than by this connect.
            video_target("video"),
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
        let frames = crate::pcm48::group_frames_in(PCM_CD_QUALITY.sample_rate)
            .expect("the negotiated rate makes whole groups");
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
                // The fake targets are all plain RDP and plain VNC, so there is no
                // subtype to report either. `config.rs` owns what the Apple ones mean.
                subtype: None,
                resize: got_resize,
                clipboard: got_clipboard,
                audio: got_audio,
                render: _,
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
        mgr.connect(att.id, "fake", None).unwrap();
        expect_connected(&mut att.events, "fake").await;
        assert!(hooks.try_recv().is_ok(), "connect spawns the engine");

        // A second connect while one is live is refused.
        assert!(matches!(
            mgr.connect(att.id, "other", None),
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
        mgr.connect(att.id, "rdp-resize", None).unwrap();
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
        // the vnc-clip fake target has clipboard on and resize off.
        let (mgr, _hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "vnc-clip", None).unwrap();
        expect_connected_meta(&mut att.events, "vnc-clip", Meta::of(Protocol::Vnc).clipboard()).await;

        // And so does audio, which is what tells the browser it may offer the toggle
        // that opens the audio socket.
        let (mgr, _hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "rdp-audio", None).unwrap();
        expect_connected_meta(&mut att.events, "rdp-audio", Meta::of(Protocol::Rdp).audio()).await;
    }

    /// One resize switch on the wire, and it is the operator's config bit:
    /// every engine with it follows the window, none of them carries a second
    /// mode of its own.
    #[tokio::test]
    async fn resize_targets_state_the_one_switch() {
        for (name, protocol) in [("vnc-resize", "vnc"), ("rdp-resize", "rdp")] {
            let (mgr, _hooks) = manager_with_fake_engine();
            let token = mgr.claim(false, None).unwrap();
            let mut att = mgr.attach(&token).unwrap();
            expect_picker(&mut att.events).await;
            mgr.connect(att.id, name, None).unwrap();
            match recv(&mut att.events).await {
                AttachEvent::Msg(ServerMsg::Connected {
                    protocol: got_protocol,
                    resize,
                    ..
                }) => {
                    assert_eq!(got_protocol, protocol, "protocol for {name}");
                    assert!(resize, "{name} is configured resize = true");
                }
                other => panic!("expected connected({name}), got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn connect_rejects_unknown_targets_and_stale_attachments() {
        let (mgr, _hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;

        assert!(matches!(
            mgr.connect(att.id, "nope", None),
            Err(ConnectError::UnknownTarget(name)) if name == "nope"
        ));
        // An attachment that is no longer the current client can't connect.
        assert!(matches!(mgr.connect(att.id + 999, "fake", None), Err(ConnectError::NotCurrent)));
    }

    // ---- the video render dial ------------------------------------------------

    /// A video target connects like any other — nothing asks the browser what it can
    /// decode — and its connected status carries the resolved render line, which is
    /// the one place a client learns this session streams.
    #[tokio::test]
    async fn a_video_target_connects_and_names_its_render_plan() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;

        mgr.connect(att.id, "video", None).unwrap();
        assert!(hooks.try_recv().is_ok(), "engine spawned on connect");
        match recv(&mut att.events).await {
            AttachEvent::Msg(ServerMsg::Connected { render, .. }) => {
                assert_eq!(render, "video q60")
            }
            other => panic!("expected connected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn frames_reach_the_attached_client_and_are_dropped_while_detached() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "fake", None).unwrap();
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
            ("vnc-clip", Meta::of(Protocol::Vnc).clipboard()),
        ] {
            let protocol = meta.protocol.name();
            let (mgr, hooks) = manager_with_fake_engine();
            let token = mgr.claim(false, None).unwrap();
            let mut att = mgr.attach(&token).unwrap();
            expect_picker(&mut att.events).await;
            mgr.connect(att.id, target, None).unwrap();
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
        mgr.connect(att.id, "fake", None).unwrap();
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
        mgr.connect(att.id, "fake", None).unwrap();
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
        mgr.connect(att.id, "fake", None).unwrap();
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
        mgr.connect(att.id, "fake", None).unwrap();
        expect_connected(&mut att.events, "fake").await;
        let (input_rx, _frame_tx, _audio) = hooks.try_recv().unwrap();

        // Switch target: the engine is torn down (its input channel closes) and
        // the browser lands back on the picker without dropping the socket.
        mgr.disconnect(att.id);
        expect_picker(&mut att.events).await;
        assert!(input_rx.is_closed(), "disconnect closes the engine input channel");

        // Picking again spawns a fresh engine — a different target this time.
        mgr.connect(att.id, "other", None).unwrap();
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
        mgr.connect(att.id, "fake", None).unwrap();
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
        mgr.connect(att_a.id, "fake", None).unwrap();
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
        mgr.connect(att.id, "fake", None).unwrap();
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
        mgr.connect(att.id, "fake", None).unwrap();
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
    ) -> (String, Attachment, Arc<AudioBridge>, EngineEnds) {
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "rdp-audio", None).unwrap();
        expect_connected_meta(&mut att.events, "rdp-audio", Meta::of(Protocol::Rdp).audio()).await;
        let ends = hooks.try_recv().unwrap();
        let audio = ends
            .2
            .clone()
            .expect("an audio target's engine is given a bridge");
        (token, att, audio, ends)
    }

    /// The next message on an audio socket, or a failure naming the wait.
    async fn recv_audio(packets: &mut mpsc::Receiver<ServerMsg>) -> ServerMsg {
        tokio::time::timeout(Duration::from_secs(5), packets.recv())
            .await
            .expect("timed out waiting for audio")
            .expect("the audio socket ended unexpectedly")
    }

    /// Assert the next event configures a decoder, and that it says what a decoder
    /// needs. Returns the codec, for the one test that cares which one it got.
    ///
    /// Encoded targets only — passthrough announces none of this, which is what
    /// `a_pcm_target_is_announced_as_the_remotes_own_bytes` is for.
    async fn expect_audio_format(packets: &mut mpsc::Receiver<ServerMsg>) -> &'static str {
        match recv_audio(packets).await {
            ServerMsg::AudioFormat {
                codec,
                sample_rate,
                channels,
                packet_frames,
                head,
            } => {
                // The *stream's* rate, not the 44100 the remote negotiated: an
                // encoded stream is resampled to 48 kHz on the way in.
                assert_eq!(sample_rate, 48_000);
                assert_eq!(channels, 2);
                assert!(packet_frames > 0, "a packet with no samples in it is not a packet");
                assert!(!head.is_empty(), "a decoder cannot be configured from nothing");
                codec
            }
            other => panic!("expected the audio format, got {other:?}"),
        }
    }

    /// The same, for the default target, which is Opus.
    async fn expect_opus_format(packets: &mut mpsc::Receiver<ServerMsg>) {
        assert_eq!(expect_audio_format(packets).await, "opus");
    }

    /// Assert the next message is audio, returning how many packets it carried.
    async fn expect_audio(packets: &mut mpsc::Receiver<ServerMsg>) -> usize {
        match recv_audio(packets).await {
            ServerMsg::Audio(packets) => packets.len(),
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
    async fn opening_the_audio_socket_forwards_the_engines_queue_behind_its_format() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (token, _att, audio, _engine) = connected_audio_session(&mgr, &hooks).await;

        let mut sound = mgr.attach_audio(&token).unwrap();
        audio.wave(one_frame_of_pcm());

        expect_opus_format(&mut sound.packets).await;
        assert_eq!(expect_audio(&mut sound.packets).await, 1, "one frame in, one packet out");

        audio.wave(one_frame_of_pcm());
        assert_eq!(expect_audio(&mut sound.packets).await, 1, "and it keeps going");
    }

    /// The target's `audio_codec` is what the client is told, and the whole
    /// difference reaches the socket.
    ///
    /// Asserted here rather than in `pcm_stream` because this is the seam that could
    /// go wrong without either stream being at fault: a session that read the key and
    /// then announced Opus anyway would send raw samples described as an encoded
    /// stream, which is noise with no error anywhere.
    ///
    /// Passthrough end to end, which is the only place the claim can actually be
    /// checked: the bytes the engine put on the queue are the bytes that reach the
    /// socket, byte for byte, and the announcement tells the client to play them
    /// rather than decode them.
    ///
    /// [`crate::pcm_stream`]'s own tests prove the stream does not alter a buffer.
    /// This proves nothing between it and the client does either — a resample or a
    /// re-frame quietly reintroduced anywhere on this path fails here and nowhere
    /// else.
    #[tokio::test]
    async fn a_pcm_target_is_announced_as_the_remotes_own_bytes() {
        let (mgr, hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;
        mgr.connect(att.id, "rdp-pcm", None).unwrap();
        expect_connected_meta(
            &mut att.events,
            "rdp-pcm",
            Meta::of(Protocol::Rdp).audio_codec(crate::config::AudioCodec::Pcm),
        )
        .await;
        // Held, not dropped: letting the engine's channel ends go ends the engine,
        // and the session falls back to the picker before any audio is asked for.
        let ends = hooks.try_recv().unwrap();
        let audio = ends
            .2
            .clone()
            .expect("an audio target's engine is given a bridge");

        let mut sound = mgr.attach_audio(&token).unwrap();
        match recv_audio(&mut sound.packets).await {
            ServerMsg::AudioFormat {
                codec,
                sample_rate,
                packet_frames,
                head,
                ..
            } => {
                assert_eq!(codec, "pcm-s16le");
                assert_eq!(sample_rate, 44_100, "the remote's rate, because nothing resampled");
                assert_eq!(packet_frames, 0, "each packet's length is its own");
                assert!(head.is_empty(), "there is no decoder to configure");
            }
            other => panic!("expected the audio format, got {other:?}"),
        }

        // Not silence: a buffer whose bytes can be told apart from any other.
        let wave: Vec<u8> = (0..2_048u32).map(|byte| byte as u8).collect();
        audio.wave(wave.clone());
        match recv_audio(&mut sound.packets).await {
            ServerMsg::Audio(packets) => {
                assert_eq!(packets, vec![wave], "the wave buffer, unaltered");
            }
            other => panic!("expected audio packets, got {other:?}"),
        }
    }

    /// A second socket on one claim must not leave two pumps on one queue: every
    /// packet would arrive twice, which is a decoder fault rather than a duplicate.
    ///
    /// Counted on the queue's subscribers rather than on the socket, because that is
    /// where the invariant lives — and because two pumps racing to send would not
    /// reliably show up as two frames in a row.
    #[tokio::test]
    async fn a_second_audio_socket_supersedes_the_first() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (token, _att, audio, _engine) = connected_audio_session(&mgr, &hooks).await;

        let first = mgr.attach_audio(&token).unwrap();
        expect_listeners(&audio, 1).await;
        let mut second = mgr.attach_audio(&token).unwrap();
        expect_listeners(&audio, 1).await;

        // The first is told, rather than left holding a socket nothing will ever
        // write to.
        assert!(
            tokio::time::timeout(Duration::from_secs(5), first.evicted)
                .await
                .is_ok(),
            "the superseded socket should have been closed"
        );

        // And the survivor works: the replacement is a live subscription, not a handle
        // to a task that was aborted with it.
        audio.wave(one_frame_of_pcm());
        expect_opus_format(&mut second.packets).await;
        assert_eq!(expect_audio(&mut second.packets).await, 1);
    }

    /// Closing the socket means it: the desktop carries on and the sound stops, which
    /// is the whole of each client's toggle now that there is no message for it.
    #[tokio::test]
    async fn closing_the_audio_socket_stops_it_with_the_engine_still_running() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (token, _att, audio, _engine) = connected_audio_session(&mgr, &hooks).await;

        let mut sound = mgr.attach_audio(&token).unwrap();
        audio.wave(one_frame_of_pcm());
        expect_opus_format(&mut sound.packets).await;
        assert_eq!(expect_audio(&mut sound.packets).await, 1);

        mgr.detach_audio(sound.id);
        expect_listeners(&audio, 0).await;

        // Nothing sent while there is no socket reaches the next one, and this says so
        // without waiting on an absence: a fresh socket's channel starts empty, so a
        // buffer that had been forwarded would arrive *before* its format.
        audio.wave(one_frame_of_pcm());
        let mut again = mgr.attach_audio(&token).unwrap();
        expect_opus_format(&mut again.packets).await;

        // The engine never noticed any of this.
        assert!(mgr.state.lock().unwrap().engine.is_some());
    }

    /// Nothing to listen to is accepted and silent rather than refused, on both of its
    /// paths: the picker has no engine, and a target that did not opt in has no queue.
    ///
    /// Accepting is the decision being pinned. The socket is bound to the claim, not to
    /// a source, which is exactly what lets `connect` hand it an engine later — so
    /// refusing here would make a client that opened audio before picking a target have
    /// to notice and try again.
    #[tokio::test]
    async fn an_audio_socket_with_no_source_is_accepted_and_silent() {
        let (mgr, _hooks) = manager_with_fake_engine();
        let token = mgr.claim(false, None).unwrap();
        let mut att = mgr.attach(&token).unwrap();
        expect_picker(&mut att.events).await;

        let sound = mgr.attach_audio(&token).unwrap();
        {
            let st = mgr.state.lock().unwrap();
            assert!(st.audio.is_some(), "the socket is held even with nothing to send");
            assert!(st.audio_pump.is_none(), "the picker has no audio to subscribe to");
        }

        mgr.connect(att.id, "fake", None).unwrap();
        expect_connected(&mut att.events, "fake").await;
        assert!(
            mgr.state.lock().unwrap().audio_pump.is_none(),
            "a target that did not opt into audio has none to send"
        );
        drop(sound);
    }

    /// A superseded claim cannot open one, for the same reason a superseded attachment
    /// cannot inject input: the token is no longer this session's.
    #[tokio::test]
    async fn an_audio_socket_opened_with_a_superseded_token_is_refused() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (token_a, mut old, _audio, _engine) = connected_audio_session(&mgr, &hooks).await;

        let _token_b = mgr.claim(true, None).unwrap();
        assert!(matches!(recv(&mut old.events).await, AttachEvent::Evicted));

        assert!(
            mgr.attach_audio(&token_a).is_err(),
            "the old claim should not be able to open an audio socket"
        );
    }

    /// The headline behaviour of giving audio its own socket: it outlives the engine,
    /// so picking a new target hands the same socket the new desktop's sound without
    /// the browser asking again.
    ///
    /// Both bridges are checked, because the failure that matters is not silence but
    /// *staleness* — a socket left subscribed to the desktop that was switched away
    /// from would keep playing it.
    #[tokio::test]
    async fn a_target_switch_rearms_the_open_audio_socket() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (token, mut att, first_bridge, _engine) = connected_audio_session(&mgr, &hooks).await;

        let mut sound = mgr.attach_audio(&token).unwrap();
        first_bridge.wave(one_frame_of_pcm());
        expect_opus_format(&mut sound.packets).await;
        assert_eq!(expect_audio(&mut sound.packets).await, 1);

        mgr.disconnect(att.id);
        expect_picker(&mut att.events).await;
        expect_listeners(&first_bridge, 0).await;

        mgr.connect(att.id, "rdp-audio", None).unwrap();
        expect_connected_meta(&mut att.events, "rdp-audio", Meta::of(Protocol::Rdp).audio()).await;
        // Held, not dropped: letting the ends go is how a fake engine dies, and this
        // one has to outlive the assertions below.
        let second_ends = hooks.try_recv().unwrap();
        let second_bridge = second_ends
            .2
            .clone()
            .expect("the second engine is given a bridge too");

        // The same socket, a fresh format, and sound off the *new* engine's queue.
        second_bridge.wave(one_frame_of_pcm());
        expect_opus_format(&mut sound.packets).await;
        assert_eq!(expect_audio(&mut sound.packets).await, 1);
        expect_listeners(&first_bridge, 0).await;
        expect_listeners(&second_bridge, 1).await;
    }

    /// The other half of that lifetime: the audio socket belongs to the claim, so a
    /// browser whose session socket dropped and came back is still listening.
    ///
    /// This is what `attach` no longer calling `stop_audio` buys, and it is why a
    /// reclaim with the browser's own token must not evict.
    #[tokio::test]
    async fn audio_survives_a_main_socket_reattach() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (token, att, audio, _engine) = connected_audio_session(&mgr, &hooks).await;

        let mut sound = mgr.attach_audio(&token).unwrap();
        audio.wave(one_frame_of_pcm());
        expect_opus_format(&mut sound.packets).await;
        assert_eq!(expect_audio(&mut sound.packets).await, 1);

        // The browser's session socket goes and comes back on its own token.
        mgr.detach(att.id);
        audio.wave(one_frame_of_pcm());
        let token_again = mgr.claim(false, Some(&token)).unwrap();
        let mut back = mgr.attach(&token_again).unwrap();
        expect_connected_meta(&mut back.events, "rdp-audio", Meta::of(Protocol::Rdp).audio()).await;

        // Never interrupted: one listener throughout, and the buffer sent while the
        // session socket was down still arrives.
        expect_listeners(&audio, 1).await;
        assert_eq!(expect_audio(&mut sound.packets).await, 1);
    }

    /// The sneaky one. A plain claim succeeds whenever no session socket is attached,
    /// so a *different* browser can take the slot with no takeover prompt anywhere —
    /// and it must not inherit the previous one's ears.
    ///
    /// Stopping the pump would not be enough: the slot would survive and the next
    /// `connect` would re-arm it, so the browser that lost the session would start
    /// hearing the new holder's desktop.
    #[tokio::test]
    async fn a_claim_by_a_different_browser_takes_the_audio_socket_with_the_slot() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (token_a, att_a, audio, _engine) = connected_audio_session(&mgr, &hooks).await;

        let sound_a = mgr.attach_audio(&token_a).unwrap();
        audio.wave(one_frame_of_pcm());
        expect_listeners(&audio, 1).await;

        // No force and no token: legal only because the session socket is down.
        mgr.detach(att_a.id);
        let token_b = mgr.claim(false, None).unwrap();

        assert!(
            tokio::time::timeout(Duration::from_secs(5), sound_a.evicted)
                .await
                .is_ok(),
            "the previous claim's audio socket should have been closed"
        );
        expect_listeners(&audio, 0).await;
        assert!(mgr.state.lock().unwrap().audio.is_none());

        // And it stays gone across a reconnect, which is where a surviving slot would
        // have shown itself.
        let mut att_b = mgr.attach(&token_b).unwrap();
        expect_connected_meta(&mut att_b.events, "rdp-audio", Meta::of(Protocol::Rdp).audio())
            .await;
        mgr.disconnect(att_b.id);
        expect_picker(&mut att_b.events).await;
        mgr.connect(att_b.id, "rdp-audio", None).unwrap();
        expect_connected_meta(&mut att_b.events, "rdp-audio", Meta::of(Protocol::Rdp).audio())
            .await;
        assert!(
            mgr.state.lock().unwrap().audio_pump.is_none(),
            "a closed socket must not be re-armed by the next connect"
        );
    }

    /// A takeover is the same rule with the prompt: the desktop keeps running for the
    /// new browser, and the previous one stops hearing it.
    #[tokio::test]
    async fn a_takeover_ends_the_audio_while_the_engine_carries_on() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (token_a, mut att_a, audio, _engine) = connected_audio_session(&mgr, &hooks).await;

        let mut sound_a = mgr.attach_audio(&token_a).unwrap();
        audio.wave(one_frame_of_pcm());
        expect_opus_format(&mut sound_a.packets).await;
        assert_eq!(
            expect_audio(&mut sound_a.packets).await,
            1,
            "this browser's audio should be live before the takeover"
        );

        let token_b = mgr.claim(true, None).unwrap();
        assert!(matches!(recv(&mut att_a.events).await, AttachEvent::Evicted));
        assert!(
            tokio::time::timeout(Duration::from_secs(5), sound_a.evicted)
                .await
                .is_ok(),
            "the taken-over browser's audio socket should have been closed"
        );
        expect_listeners(&audio, 0).await;
        assert!(
            mgr.state.lock().unwrap().engine.is_some(),
            "a takeover keeps the engine"
        );

        // The new holder inherits the desktop, and gets audio off the same live queue
        // once it opens a socket of its own — which it must, on its own claim.
        let mut att_b = mgr.attach(&token_b).unwrap();
        expect_connected_meta(&mut att_b.events, "rdp-audio", Meta::of(Protocol::Rdp).audio())
            .await;
        let mut sound_b = mgr.attach_audio(&token_b).unwrap();
        audio.wave(one_frame_of_pcm());
        expect_opus_format(&mut sound_b.packets).await;
        assert_eq!(expect_audio(&mut sound_b.packets).await, 1);
    }

    /// Every way an engine ends takes its audio with it, with nothing in those paths
    /// having to know about audio: they all go through `take_engine`, which stops the
    /// pump. The *socket* stays, because the claim did — that is what
    /// `a_target_switch_rearms_the_open_audio_socket` then goes on to use.
    ///
    /// The subscriber count is the assertion rather than a silent socket, and for a
    /// reason worth keeping: the queue outlives the slot by however long the engine
    /// takes to notice its input channel closed, so "the bridge was dropped" would be
    /// testing something that has not happened yet.
    #[tokio::test]
    async fn ending_the_engine_ends_the_audio_but_not_the_socket() {
        #[allow(clippy::type_complexity)]
        let ways: [(&str, Box<dyn Fn(&Arc<SessionManager>, u64)>); 2] = [
            ("switch target", Box::new(|mgr: &Arc<SessionManager>, id| mgr.disconnect(id))),
            (
                "heartbeat expiry",
                Box::new(|mgr: &Arc<SessionManager>, id| mgr.expire_attachment(id)),
            ),
        ];
        for (what, end_it) in ways {
            let (mgr, hooks) = manager_with_fake_engine();
            let (token, att, audio, _engine) = connected_audio_session(&mgr, &hooks).await;
            let mut sound = mgr.attach_audio(&token).unwrap();
            audio.wave(one_frame_of_pcm());
            expect_opus_format(&mut sound.packets).await;
            assert_eq!(
                expect_audio(&mut sound.packets).await,
                1,
                "{what}: the audio should be live before the engine ends"
            );

            end_it(&mgr, att.id);
            expect_listeners(&audio, 0).await;
            let st = mgr.state.lock().unwrap();
            assert!(st.audio_pump.is_none(), "{what} left the audio pump behind");
            assert!(
                st.audio.is_some(),
                "{what} closed the audio socket, which belongs to the claim and not the engine"
            );
        }
    }

    /// The one way that *does* take the socket, because it is the one that ends the
    /// claim the socket attached to.
    #[tokio::test]
    async fn logging_out_closes_the_audio_socket() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (token, _att, audio, _engine) = connected_audio_session(&mgr, &hooks).await;
        let mut sound = mgr.attach_audio(&token).unwrap();
        audio.wave(one_frame_of_pcm());
        expect_opus_format(&mut sound.packets).await;
        assert_eq!(expect_audio(&mut sound.packets).await, 1);

        mgr.log_out();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), sound.evicted)
                .await
                .is_ok(),
            "a log out closes the audio socket"
        );
        expect_listeners(&audio, 0).await;
        let st = mgr.state.lock().unwrap();
        assert!(st.audio_pump.is_none());
        assert!(st.audio.is_none());
    }

    /// An engine that died on its own leaves the socket armable: the browser is still
    /// there, still holds the claim, and picking another target resumes its sound.
    #[tokio::test]
    async fn engine_death_leaves_the_audio_socket_armable() {
        let (mgr, hooks) = manager_with_fake_engine();
        let (token, mut att, audio, engine) = connected_audio_session(&mgr, &hooks).await;
        let mut sound = mgr.attach_audio(&token).unwrap();
        audio.wave(one_frame_of_pcm());
        expect_opus_format(&mut sound.packets).await;
        assert_eq!(expect_audio(&mut sound.packets).await, 1);

        // The engine ends of its own accord: dropping its channel ends is how the fake
        // one dies, and the pump notices and returns the slot to the picker.
        drop(engine);
        expect_picker(&mut att.events).await;
        expect_listeners(&audio, 0).await;

        mgr.connect(att.id, "rdp-audio", None).unwrap();
        expect_connected_meta(&mut att.events, "rdp-audio", Meta::of(Protocol::Rdp).audio()).await;
        let revived_ends = hooks.try_recv().unwrap();
        let revived = revived_ends
            .2
            .clone()
            .expect("the replacement engine is given a bridge");
        revived.wave(one_frame_of_pcm());
        expect_opus_format(&mut sound.packets).await;
        assert_eq!(expect_audio(&mut sound.packets).await, 1);
    }
}
