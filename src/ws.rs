//! WebSocket endpoint bridging a browser to the server-side remote-desktop
//! session.
//!
//! Two endpoints, both presenting the claim token from `POST /api/session`.
//!
//! `/ws?session=<token>` is the session: it attaches to the single slot
//! ([`crate::session::SessionManager`]). The URL also names the client's screen
//! (`w`/`h`/`scale`, the same numbers `connect` carries), so an attach that finds
//! a target whose engine a claim change ended can reconnect it for *this*
//! browser's screen. Inbound `ClientMsg` split two ways —
//! session-control messages (`connect` to pick a target from the post-login picker,
//! `disconnect` to switch back to it) act on the slot; everything else is engine
//! input, routed to the current engine (or dropped in the picker state). Outbound
//! `ServerMsg` go to the browser as screen tiles in binary frames and control messages
//! (resize/error, the picker/connected status) as JSON text (see [`crate::protocol`]).
//!
//! `/ws/audio?session=<token>` is sound, and **opening it is the subscription** —
//! there is no message that turns audio on. It carries exactly two things, the format
//! and then packets, and it exists so that neither ever waits behind a picture: on a
//! `render_type = "video"` target the session socket's queue is four frames deep, and
//! an audio pump stuck behind it stops draining the bridge, which then drops wave
//! buffers outright. It is bound to the *claim* rather than to an attachment, so it
//! survives a reattach and a target switch and ends only when the claim does.
//!
//! `/ws/camera?session=<token>` is the browser's camera going the other way, and
//! **opening it is the enable** — per session, explicit, never a remembered
//! preference. Inbound: one `cameraFormat` text message that plugs the virtual device
//! into the remote, then binary H.264 samples. Outbound: the remote's streaming
//! decisions (`cameraStart` / `cameraStop` / `cameraKeyframe`). Unlike audio it is
//! bound to the claim **and the engine**: a target switch or engine end closes it, so
//! the next session starts with the camera off, and closing it unplugs the device
//! from the remote.
//!
//! Close codes tell the browser why any socket ended:
//! - `4000` — the token is missing or superseded; claim again.
//! - `4001` — evicted: another browser claimed the slot, or a newer audio/camera
//!   socket replaced this one.
//! - `4002` — the running target does not carry this socket's medium (camera on a
//!   target without `camera = true`, or no engine running).
//!
//! Any other close on the session socket detaches the browser. The owner reattaching
//! within the grace period restores the picker or live engine; a different claim's
//! attach reconnects the selected target instead; otherwise the engine ends for every
//! protocol. A closed audio socket ends nothing but the sound; a closed camera socket
//! ends nothing but the camera.

use axum::{
    extract::{
        Query, State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use futures_util::{SinkExt as _, StreamExt as _};
use log::{info, warn};
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::{Instant, MissedTickBehavior, interval};

use crate::{
    camera::{CameraFormat, CameraSignal},
    feedback::LinkFeedback,
    mic::MicSignal,
    protocol::{self, ClientMsg, ServerMsg, WireFrame},
    server::AppState,
    session::{AttachEvent, CameraRefused, MicRefused, REATTACH_GRACE_PERIOD, SessionManager},
    wire::Wire,
};

/// Close code: the session token is missing, invalid, or superseded.
const CLOSE_INVALID_TOKEN: u16 = 4000;
/// Close code: another browser took over the session slot.
const CLOSE_EVICTED: u16 = 4001;
/// Close code: the running target does not carry what this socket carries —
/// the camera socket on a target without `camera = true`, the microphone socket
/// on one without `microphone = true`, or either with no engine running at all.
const CLOSE_UNSUPPORTED: u16 = 4002;
/// Standard internal-error close. The browser treats it as reconnectable, so a
/// fresh attachment gets a fresh sequence space rather than reusing one.
const CLOSE_SEQUENCE_EXHAUSTED: u16 = 1011;

/// WebSocket keepalive interval. Browsers answer protocol pings in the network
/// stack, so background-tab JavaScript timer throttling cannot suppress it.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct HeartbeatTimings {
    interval: Duration,
    timeout: Duration,
}

const HEARTBEAT_TIMINGS: HeartbeatTimings = HeartbeatTimings {
    interval: HEARTBEAT_INTERVAL,
    timeout: REATTACH_GRACE_PERIOD,
};

/// Sent-batch timestamps retained while the painter owes an acknowledgment.
/// Bounded independently of the backpressure window below: a broken or raw test
/// client must not turn missing feedback into unbounded gateway memory.
const MAX_TRACKED_PAINTS: usize = 4096;

/// Screen batches allowed past the WebSocket without an acknowledgment.
///
/// The classic WebSocket API has no receive backpressure: the page takes every
/// binary frame the moment it arrives and the painter worker appends it to a
/// promise chain, so without this the only bound on browser-side backlog is how
/// fast the gateway can write. The queues *before* the socket are already bounded
/// ([`crate::session::FRAME_BUFFER`]); this is the same discipline applied to the
/// one hop that had none.
///
/// Twenty-four is what the UAT profiles measured, not a guess. Attached to live
/// RDP, generic VNC and Apple Standard desktops across idle, continuous motion
/// and interactive input, the deepest a *working* attachment ran was 22 — a
/// 1280x800 RDP desktop playing video. Generic VNC on the same LAN and the same
/// video ran 461 deep with the browser 49ms behind on average and half a second
/// behind at worst, which is not a working depth but the backlog this bounds:
/// RFB has no pacing of its own (see `src/rdp.rs` `DAMAGE_INTERVAL`, deliberately
/// not mirrored in VNC), so nothing between that engine and the canvas was
/// telling it to stop. Windowed, the same run held 24 in flight at 2/43ms
/// end-to-end and carried the same picture in half as many tile records.
///
/// Above the deepest working depth on purpose: a window that a healthy
/// attachment hits is a throughput tax, not backpressure. Eight was measured too
/// and is worse in the way that matters — the gateway coalesces harder while a
/// batch is parked, so batches get fatter (largest 258KB against 219KB) and one
/// fat batch takes longer to decode and draw than the queue it saved (draw max
/// 110ms against 19ms, end-to-end max 124ms against 43ms). The floor on latency
/// is a batch, so squeezing the window past that trades a queue for a stall.
///
/// One number rather than one per plan, because a second one would bind on
/// nothing: video never came near this. An access unit is a whole frame and
/// [`crate::session::VIDEO_FRAME_BUFFER`] already holds that path to four, which
/// is why VP9 under the same video ran 1 deep. What paces video is
/// [`PAINT_LAG_LIMIT`], which counts time instead of messages.
const PAINT_WINDOW: usize = 24;

/// How far behind the painter may fall before nothing more is added to its queue.
///
/// A depth window alone cannot pace video, which the UAT showed rather than
/// argued: with the renderer throttled twenty times, a VP9 attachment ran 222ms
/// behind while never exceeding 7 batches in flight. Nothing parked, so nothing
/// filled the queues behind the socket, so the congestion loop in
/// [`crate::encode`] — which reads exactly that blocking — coarsened not one
/// round while the client fell a fifth of a second behind. Depth is the wrong
/// unit for a path whose messages are whole frames.
///
/// So the window has two rules and a batch waits on either: too many owed, or
/// the oldest owed for too long. Past this limit the wait is until the painter
/// catches up completely, which is lock-step — one batch per paint — and is what
/// pacing to a client's real presentation rate means.
///
/// 150ms because it is above every working attachment measured and below every
/// failing one: the worst healthy end-to-end across RDP, generic VNC and Apple
/// Standard was 115ms (Apple Standard under motion, at 97 batches a second), and
/// the throttled painters ran 222ms and 336ms behind. Nothing is dropped to
/// achieve it — an access unit's dependency order is untouched, and a decoder
/// that has every frame in order needs no keyframe to recover.
const PAINT_LAG_LIMIT: Duration = Duration::from_millis(150);

/// How long a batch waits for the window before it is sent anyway.
///
/// The window is pacing, not a protocol requirement: a client that acknowledges
/// nothing — a raw socket in an e2e test, a painter that died — must not be able
/// to wedge the session by staying silent. Past this the batch goes out and the
/// attachment is counted as having run past its window, which is the thing to
/// look for in the totals line when a session felt slow.
const PAINT_WINDOW_GRACE: Duration = Duration::from_millis(500);

struct PendingPaint {
    sequence: u32,
    sent: Instant,
}

/// What waiting for the window cost one batch — reported by
/// [`wait_for_paint_window`] and recorded only once that batch is on the socket,
/// because a batch the socket refused waited for nothing and went past nothing.
///
/// An enum rather than two flags: running past the window implies having waited
/// on it, and there is no fourth state to represent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Admission {
    /// The window had room; nothing waited.
    Immediate,
    /// Parked until an acknowledgment opened the window.
    Waited,
    /// Parked until [`PAINT_WINDOW_GRACE`] gave up on one arriving.
    PastWindow,
}

/// Acknowledgments whose end-to-end times the baseline is the minimum of.
///
/// The baseline is what [`LinkFeedback::lag`] subtracts so distance does not read
/// as queueing. A window rather than an all-time minimum so a route change is
/// eventually believed; at the paint window's ordinary cadence this is a few
/// seconds of history, the same order as RustDesk's 60-sample RTT window.
const BASELINE_WINDOW: usize = 32;

#[derive(Default)]
struct PaintTracker {
    pending: VecDeque<PendingPaint>,
    /// Where this attachment's verdict about the link is published for the
    /// encoders — see [`LinkFeedback`]. `None` in tests that only assert the
    /// tracker's own arithmetic.
    feedback: Option<Arc<LinkFeedback>>,
    /// End-to-end times (ms) of the last [`BASELINE_WINDOW`] acknowledgments,
    /// whose minimum is the published baseline.
    recent_end_to_end: VecDeque<u32>,
    sent: u64,
    acknowledgments: u64,
    completed: u64,
    forgotten: u64,
    stale: u64,
    max_in_flight: u64,
    queued_ms: u64,
    max_queued_ms: u32,
    draw_ms: u64,
    max_draw_ms: u32,
    end_to_end_ms: u64,
    max_end_to_end_ms: u64,
    past_window: u64,
    window_waits: u64,
}

impl PaintTracker {
    /// A tracker that publishes what it learns through `feedback`.
    fn publishing(feedback: Arc<LinkFeedback>) -> Self {
        Self { feedback: Some(feedback), ..Self::default() }
    }

    /// Tell the encoders how long the oldest owed batch has been owed — called
    /// after every change to the front of `pending`.
    fn publish_owed(&self) {
        if let Some(feedback) = &self.feedback {
            feedback.owed_since(self.pending.front().map(|paint| paint.sent));
        }
    }
    /// Batches the painter has not acknowledged yet — what [`PAINT_WINDOW`]
    /// bounds.
    fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// How long the painter has owed its oldest unacknowledged batch — what
    /// [`PAINT_LAG_LIMIT`] bounds.
    ///
    /// The oldest rather than the newest acknowledgment's round trip, because a
    /// painter that stops answering has to become visible *while* it is silent:
    /// this grows the moment it stalls, where the last completed round trip only
    /// says how things went before it did.
    fn behind(&self) -> Duration {
        self.pending
            .front()
            .map_or(Duration::ZERO, |paint| paint.sent.elapsed())
    }

    /// Whether another batch may go out, which is both rules of the window: not
    /// too many owed, and the oldest not owed for too long. An attachment owing
    /// nothing always may — that is what keeps the lag rule from being a deadlock
    /// rather than a pacing.
    fn admits_a_batch(&self) -> bool {
        self.in_flight() < PAINT_WINDOW && self.behind() <= PAINT_LAG_LIMIT
    }

    fn sent(&mut self, sequence: u32) {
        if self.pending.len() == MAX_TRACKED_PAINTS {
            self.pending.pop_front();
            self.forgotten += 1;
        }
        self.pending.push_back(PendingPaint {
            sequence,
            sent: Instant::now(),
        });
        self.sent += 1;
        self.max_in_flight = self.max_in_flight.max(self.pending.len() as u64);
        self.publish_owed();
    }

    /// Record what a batch's admission cost, once that batch is on the socket.
    /// Separate from [`Self::sent`] because the two become true at different
    /// moments: the timestamp before the write, this after it.
    fn admitted(&mut self, admission: Admission) {
        match admission {
            Admission::Immediate => {}
            Admission::Waited => self.window_waits += 1,
            Admission::PastWindow => {
                self.window_waits += 1;
                self.past_window += 1;
            }
        }
    }

    /// Remove a batch timestamp installed immediately before a socket write
    /// that then failed. It is always the newest entry; keeping the operation
    /// explicit avoids counting bytes the browser could never acknowledge.
    fn unsent(&mut self, sequence: u32) {
        if self.pending.back().is_some_and(|paint| paint.sequence == sequence) {
            self.pending.pop_back();
            self.sent -= 1;
            self.publish_owed();
        }
    }

    fn acknowledge(&mut self, sequence: u32, queued_ms: u32, draw_ms: u32) {
        let Some(position) = self.pending.iter().position(|paint| paint.sequence == sequence) else {
            self.stale += 1;
            return;
        };
        let elapsed = self.pending[position].sent.elapsed().as_millis();
        let elapsed = u64::try_from(elapsed).unwrap_or(u64::MAX);
        // The worker is strictly ordered, so completing this sequence also
        // proves every older retained batch completed. Treat the ack as
        // cumulative so a later coalescing change needs no protocol change.
        for _ in 0..=position {
            self.pending.pop_front();
            self.completed += 1;
        }
        self.acknowledgments += 1;
        self.queued_ms = self.queued_ms.saturating_add(u64::from(queued_ms));
        self.max_queued_ms = self.max_queued_ms.max(queued_ms);
        self.draw_ms = self.draw_ms.saturating_add(u64::from(draw_ms));
        self.max_draw_ms = self.max_draw_ms.max(draw_ms);
        self.end_to_end_ms = self.end_to_end_ms.saturating_add(elapsed);
        self.max_end_to_end_ms = self.max_end_to_end_ms.max(elapsed);
        if self.recent_end_to_end.len() == BASELINE_WINDOW {
            self.recent_end_to_end.pop_front();
        }
        self.recent_end_to_end.push_back(u32::try_from(elapsed).unwrap_or(u32::MAX));
        if let Some(feedback) = &self.feedback
            && let Some(baseline) = self.recent_end_to_end.iter().min()
        {
            feedback.baseline(*baseline);
        }
        self.publish_owed();
    }
}

impl std::fmt::Display for PaintTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let average = |total: u64| total.checked_div(self.acknowledgments).unwrap_or(0);
        write!(
            f,
            "{} batch(es) sent, {} ack(s) completing {}, {} still in flight, max {} in flight, \
             worker queue avg/max {}/{}ms, draw avg/max {}/{}ms, end-to-end avg/max {}/{}ms, \
             {} timestamp(s) forgotten, {} stale ack(s), {} batch(es) waited on the window, \
             {} sent past it",
            self.sent,
            self.acknowledgments,
            self.completed,
            self.pending.len(),
            self.max_in_flight,
            average(self.queued_ms),
            self.max_queued_ms,
            average(self.draw_ms),
            self.max_draw_ms,
            average(self.end_to_end_ms),
            self.max_end_to_end_ms,
            self.forgotten,
            self.stale,
            self.window_waits,
            self.past_window,
        )
    }
}

/// Hold a batch until the painter admits another one — under [`PAINT_WINDOW`]
/// owed and no older than [`PAINT_LAG_LIMIT`] behind — or until
/// [`PAINT_WINDOW_GRACE`] passes without an acknowledgment arriving.
///
/// Only an acknowledgment can open either rule, so acknowledgments are the only
/// wakeup this waits for. Nothing here polls the lag: it shrinks when a batch is
/// completed and at no other time.
///
/// Reports what the wait cost rather than recording it: the totals are about
/// batches that reached the browser, and whether this one does is not known until
/// the write after this returns. See [`send_batch`].
///
/// Called with the frame already encoded and about to be written, so a control
/// message queued behind a batch cannot overtake it: this delays the whole write
/// sequence in wire order rather than letting anything past. The inbound half is
/// a separate task reading the same socket, so a browser's input and its
/// acknowledgments keep arriving throughout — as does the audio socket, which
/// shares nothing with this one.
///
/// Heartbeats continue while parked. A window wait is short, but "short" is a
/// property of a working client, and a session must not look dead to the timeout
/// on the other side of this socket because it was busy being paced.
async fn wait_for_paint_window<S>(
    paint: &Mutex<PaintTracker>,
    room: &tokio::sync::Notify,
    heartbeat: &mut tokio::time::Interval,
    ws_tx: &mut S,
) -> Result<Admission, S::Error>
where
    S: futures_util::Sink<Message> + Unpin,
{
    let deadline = Instant::now() + PAINT_WINDOW_GRACE;
    let mut waited = false;
    loop {
        // Registered before the check, so an acknowledgment landing in between
        // is a wakeup this loop still sees rather than one it slept through.
        let room = room.notified();
        if paint.lock().unwrap().admits_a_batch() {
            return Ok(if waited {
                Admission::Waited
            } else {
                Admission::Immediate
            });
        }
        waited = true;
        tokio::select! {
            () = room => {}
            _ = heartbeat.tick() => ws_tx.send(Message::Ping(Vec::new().into())).await?,
            () = tokio::time::sleep_until(deadline) => return Ok(Admission::PastWindow),
        }
    }
}

/// Write one screen batch, in the order its two facts become true: the timestamp
/// goes in before the write, so an acknowledgment racing the write still finds
/// the batch to complete, and the window counters go in after it, so a batch the
/// socket refused is counted nowhere. A failed write leaves the tracker as though
/// the batch had never been admitted.
async fn send_batch<S>(
    paint: &Mutex<PaintTracker>,
    ws_tx: &mut S,
    sequence: u32,
    batch: Message,
    admission: Admission,
) -> Result<(), S::Error>
where
    S: futures_util::Sink<Message> + Unpin,
{
    paint.lock().unwrap().sent(sequence);
    if let Err(e) = ws_tx.send(batch).await {
        paint.lock().unwrap().unsent(sequence);
        return Err(e);
    }
    paint.lock().unwrap().admitted(admission);
    Ok(())
}

#[derive(Deserialize)]
pub struct WsParams {
    session: Option<String>,
    /// The client's screen, as [`crate::protocol::HostDisplay`] fields. Only the
    /// session socket reads them, and only a claim-change reconnect acts on them
    /// ([`SessionManager::attach`]); absent params read as no report, like a
    /// degenerate one.
    w: Option<u16>,
    h: Option<u16>,
    scale: Option<u16>,
}

pub async fn handler(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(state): State<AppState>,
) -> Response {
    let display = match (params.w, params.h, params.scale) {
        (Some(w), Some(h), Some(scale)) => Some(protocol::HostDisplay { w, h, scale }),
        _ => None,
    };
    ws.on_upgrade(move |socket| {
        session(socket, state.sessions, params.session, display, HEARTBEAT_TIMINGS)
    })
}

pub async fn audio_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| audio(socket, state.sessions, params.session, HEARTBEAT_TIMINGS))
}

/// The audio socket: one task, because there is nothing inbound to do.
///
/// It still reads `ws_rx`, because that is the only way pongs and the browser's close
/// frame arrive, but it acts on neither beyond keeping the heartbeat alive and noticing
/// the end. Everything else is a one-way stream of the format and its packets.
async fn audio(
    mut socket: WebSocket,
    sessions: Arc<SessionManager>,
    token: Option<String>,
    heartbeat_timings: HeartbeatTimings,
) {
    let attachment = token.and_then(|t| sessions.attach_audio(&t).ok());
    let Some(attachment) = attachment else {
        warn!("ws: rejected an audio connection without a valid session token");
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: CLOSE_INVALID_TOKEN,
                reason: "invalid session token".into(),
            })))
            .await;
        return;
    };

    info!("ws: an audio socket attached");

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (audio_id, mut packets, mut evicted) =
        (attachment.id, attachment.packets, attachment.evicted);
    // The same encoder the session socket uses, so the two cannot disagree about a
    // frame layout, and so `Totals` keeps reporting audio bytes where the field
    // measurement already reads them. Its tile machinery simply never sees a tile.
    let mut wire = Wire::default();
    let mut heartbeat = interval(heartbeat_timings.interval);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_heartbeat = Instant::now();

    loop {
        tokio::select! {
            // Biased, and eviction first: a takeover has to stop this browser hearing
            // the session *now*, not after however much sound is still queued for it.
            biased;
            _ = &mut evicted => {
                info!("ws: audio socket evicted");
                let _ = ws_tx
                    .send(Message::Close(Some(CloseFrame {
                        code: CLOSE_EVICTED,
                        reason: "session taken over".into(),
                    })))
                    .await;
                break;
            }
            msg = packets.recv() => {
                // `None` cannot happen while the slot holds the sender, so it means the
                // session dropped this socket without evicting it — end quietly.
                let Some(msg) = msg else { break };
                // No run collection: every audio message is its own frame regardless,
                // so batching would only add latency to the one thing that cannot
                // absorb any.
                let frames = match wire.encode(vec![msg]) {
                    Ok(frames) => frames,
                    Err(e) => {
                        warn!("ws: audio wire failed: {e}");
                        break;
                    }
                };
                for frame in frames {
                    let frame = match frame {
                        WireFrame::Audio(bytes) => Message::Binary(bytes.into()),
                        WireFrame::Text(json) => Message::Text(json.into()),
                        WireFrame::Batch { .. } => {
                            warn!("ws: screen batch reached the audio socket");
                            continue;
                        }
                    };
                    if ws_tx.send(frame).await.is_err() {
                        return finish(&sessions, audio_id, &wire);
                    }
                }
            }
            frame = ws_rx.next() => {
                match frame {
                    Some(Ok(Message::Pong(_))) => last_heartbeat = Instant::now(),
                    Some(Ok(_)) => {}
                    // A close frame or a dead socket. Nothing here is inbound, so
                    // anything else the browser sends is ignored rather than refused.
                    Some(Err(_)) | None => break,
                }
            }
            _ = heartbeat.tick() => {
                // A browser that vanished without a FIN would otherwise leave its slot
                // registered, and the next `connect` would arm a pump into a socket
                // nobody reads — the exact stall this endpoint exists to remove, for a
                // listener who is not there.
                //
                // Detaching audio is *all* this does. It must never reach
                // `expire_attachment`: the session socket is the authority on whether
                // the browser is alive, and a desktop has to survive its sound dying.
                if last_heartbeat.elapsed() >= heartbeat_timings.timeout {
                    warn!(
                        "ws: audio socket heartbeat timed out after {}s",
                        heartbeat_timings.timeout.as_secs()
                    );
                    break;
                }
                if ws_tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    finish(&sessions, audio_id, &wire);
}

fn finish(sessions: &Arc<SessionManager>, audio_id: u64, wire: &Wire) {
    sessions.detach_audio(audio_id);
    info!("ws: audio totals: {}", wire.totals);
}

pub async fn camera_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| camera(socket, state.sessions, params.session, HEARTBEAT_TIMINGS))
}

/// The camera socket: the enable, the frames, and the remote's decisions.
///
/// Opening it is the enable — per session, explicit, never remembered — and closing it
/// is the disable, which also unplugs the device from the remote. Inbound it carries
/// one `cameraFormat` text message (which plugs the device) and then binary H.264
/// samples; outbound go the remote's streaming decisions as `cameraStart`,
/// `cameraStop` and `cameraKeyframe` text frames. Unlike the audio socket it is
/// refused outright — close `4002` — when the running target carries no camera.
async fn camera(
    mut socket: WebSocket,
    sessions: Arc<SessionManager>,
    token: Option<String>,
    heartbeat_timings: HeartbeatTimings,
) {
    let attachment = match token {
        Some(token) => sessions.attach_camera(&token),
        None => Err(CameraRefused::InvalidToken),
    };
    let attachment = match attachment {
        Ok(attachment) => attachment,
        Err(refused) => {
            let (code, reason) = match refused {
                CameraRefused::InvalidToken => (CLOSE_INVALID_TOKEN, "invalid session token"),
                CameraRefused::Unsupported => {
                    (CLOSE_UNSUPPORTED, "the target carries no camera")
                }
            };
            warn!("ws: rejected a camera connection: {reason}");
            let _ = socket
                .send(Message::Close(Some(CloseFrame { code, reason: reason.into() })))
                .await;
            return;
        }
    };

    info!("ws: a camera socket attached");

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (camera_id, mut signals, mut evicted) =
        (attachment.id, attachment.signals, attachment.evicted);
    let mut heartbeat = interval(heartbeat_timings.interval);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_heartbeat = Instant::now();

    // Diagnostics for the half no unit test reaches — a real browser feeding a real
    // host. `samples_seen` answers the first question every camera bug asks ("is the
    // browser sending anything at all?") and is logged at close. `REMOTEX_CAMERA_DUMP`
    // additionally appends every access unit's raw bytes to that path: Annex B
    // concatenates into a replayable elementary stream, so the exact stream a host
    // rejected can be inspected with ffprobe or replayed with tmp/camera_send_probe.py.
    let mut samples_seen: u64 = 0;
    let mut dump = std::env::var_os("REMOTEX_CAMERA_DUMP")
        .and_then(|path| std::fs::File::create(path).ok());
    if dump.is_some() {
        info!("ws: dumping camera samples (REMOTEX_CAMERA_DUMP)");
    }

    loop {
        tokio::select! {
            // Biased, eviction first, for the audio socket's reason turned around: a
            // taken-over browser must stop *feeding* the session now — its camera is
            // pointed at a person who no longer holds the desktop it goes to.
            biased;
            _ = &mut evicted => {
                info!("ws: camera socket evicted");
                let _ = ws_tx
                    .send(Message::Close(Some(CloseFrame {
                        code: CLOSE_EVICTED,
                        reason: "session taken over".into(),
                    })))
                    .await;
                break;
            }
            signal = signals.recv() => {
                // `None` means the session dropped this socket without evicting it —
                // the engine ended — so the enable it represents is over.
                let Some(signal) = signal else { break };
                let msg = match signal {
                    CameraSignal::Start(format) => ServerMsg::CameraStart {
                        width: format.width,
                        height: format.height,
                        fps_numerator: format.fps_numerator,
                        fps_denominator: format.fps_denominator,
                    },
                    CameraSignal::Stop => ServerMsg::CameraStop,
                    CameraSignal::Keyframe => ServerMsg::CameraKeyframe,
                };
                let Some(json) = msg.text_frame() else { continue };
                if ws_tx.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
            frame = ws_rx.next() => {
                match frame {
                    Some(Ok(Message::Binary(bytes))) => {
                        // A malformed frame is dropped whole — see
                        // [`protocol::camera::parse`] — and quietly: one bad frame at
                        // 30 fps must not become a log at 30 lines a second.
                        if let Some((keyframe, unit)) = protocol::camera::parse(&bytes) {
                            samples_seen += 1;
                            if let Some(file) = dump.as_mut() {
                                use std::io::Write as _;
                                let _ = file.write_all(unit);
                            }
                            sessions.camera_sample(camera_id, unit, keyframe);
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        // The one inbound text message this socket has. Anything else
                        // parseable is a client bug worth a line, not a close.
                        match serde_json::from_str::<ClientMsg>(&text) {
                            Ok(ClientMsg::CameraFormat {
                                width,
                                height,
                                fps_numerator,
                                fps_denominator,
                            }) => sessions.camera_plug(
                                camera_id,
                                CameraFormat { width, height, fps_numerator, fps_denominator },
                            ),
                            // Named without its payload: a stray message here can
                            // be clipboard text or keystrokes, which belong on no
                            // log line.
                            Ok(_) => {
                                warn!("ws: the camera socket ignores non-camera client messages");
                            }
                            Err(e) => {
                                warn!("ws: unparseable text on the camera socket: {e}");
                            }
                        }
                    }
                    Some(Ok(Message::Pong(_))) => last_heartbeat = Instant::now(),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            _ = heartbeat.tick() => {
                // Detaching the camera is *all* a timeout does, exactly as for audio:
                // the session socket is the authority on whether the browser is alive,
                // and a desktop has to survive its camera dying.
                if last_heartbeat.elapsed() >= heartbeat_timings.timeout {
                    warn!(
                        "ws: camera socket heartbeat timed out after {}s",
                        heartbeat_timings.timeout.as_secs()
                    );
                    break;
                }
                if ws_tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    sessions.detach_camera(camera_id);
    info!("ws: the camera socket closed after {samples_seen} sample(s) from the browser");
}

pub async fn mic_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| mic(socket, state.sessions, params.session, HEARTBEAT_TIMINGS))
}

/// The microphone socket: the enable, the PCM, and the host's decisions.
///
/// The camera's twin, one direction over. Opening it is the enable — per session,
/// explicit, never remembered — and closing it is the disable. Inbound it carries
/// binary buffers of raw signed-16-bit PCM in the host's chosen format and nothing
/// else (the browser is purely reactive here — unlike the camera it announces no
/// format, because MS-RDPEAI lets the *host* pick one). Outbound go the host's
/// decisions as `micOpen` and `micClose` text frames. Like the camera it is refused
/// outright — close `4002` — when the running target carries no microphone.
async fn mic(
    mut socket: WebSocket,
    sessions: Arc<SessionManager>,
    token: Option<String>,
    heartbeat_timings: HeartbeatTimings,
) {
    let attachment = match token {
        Some(token) => sessions.attach_mic(&token),
        None => Err(MicRefused::InvalidToken),
    };
    let attachment = match attachment {
        Ok(attachment) => attachment,
        Err(refused) => {
            let (code, reason) = match refused {
                MicRefused::InvalidToken => (CLOSE_INVALID_TOKEN, "invalid session token"),
                MicRefused::Unsupported => (CLOSE_UNSUPPORTED, "the target carries no microphone"),
            };
            warn!("ws: rejected a microphone connection: {reason}");
            let _ = socket
                .send(Message::Close(Some(CloseFrame { code, reason: reason.into() })))
                .await;
            return;
        }
    };

    info!("ws: a microphone socket attached");

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (mic_id, mut signals, mut evicted) =
        (attachment.id, attachment.signals, attachment.evicted);
    let mut heartbeat = interval(heartbeat_timings.interval);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_heartbeat = Instant::now();

    // The camera socket's diagnostic, for the same reason: this half — a real browser
    // feeding a real host — no unit test reaches. `buffers_seen` answers "is the browser
    // sending anything?" and is logged at close; `REMOTEX_MIC_DUMP` appends every PCM
    // buffer to that path, a headerless little-endian S16 stream ffmpeg can play with
    // `-f s16le -ar <rate> -ac <channels>`.
    let mut buffers_seen: u64 = 0;
    let mut dump =
        std::env::var_os("REMOTEX_MIC_DUMP").and_then(|path| std::fs::File::create(path).ok());
    if dump.is_some() {
        info!("ws: dumping microphone buffers (REMOTEX_MIC_DUMP)");
    }

    loop {
        tokio::select! {
            // Biased, eviction first, for the camera socket's reason: a taken-over
            // browser must stop feeding the session now.
            biased;
            _ = &mut evicted => {
                info!("ws: microphone socket evicted");
                let _ = ws_tx
                    .send(Message::Close(Some(CloseFrame {
                        code: CLOSE_EVICTED,
                        reason: "session taken over".into(),
                    })))
                    .await;
                break;
            }
            signal = signals.recv() => {
                // `None` means the session dropped this socket without evicting it —
                // the engine ended — so the enable it represents is over.
                let Some(signal) = signal else { break };
                let msg = match signal {
                    MicSignal::Open(format) => ServerMsg::MicOpen {
                        sample_rate: format.sample_rate,
                        channels: format.channels,
                    },
                    MicSignal::Close => ServerMsg::MicClose,
                };
                let Some(json) = msg.text_frame() else { continue };
                if ws_tx.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
            frame = ws_rx.next() => {
                match frame {
                    Some(Ok(Message::Binary(bytes))) => {
                        buffers_seen += 1;
                        if let Some(file) = dump.as_mut() {
                            use std::io::Write as _;
                            let _ = file.write_all(&bytes);
                        }
                        sessions.mic_sample(mic_id, &bytes);
                    }
                    Some(Ok(Message::Text(_))) => {
                        // The microphone socket has no inbound text message: the browser
                        // learns its format from `micOpen` and only ever sends PCM.
                        warn!("ws: the microphone socket ignores client text messages");
                    }
                    Some(Ok(Message::Pong(_))) => last_heartbeat = Instant::now(),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            _ = heartbeat.tick() => {
                // Detaching the microphone is all a timeout does, exactly as for the
                // camera: the session socket is the authority on browser liveness.
                if last_heartbeat.elapsed() >= heartbeat_timings.timeout {
                    warn!(
                        "ws: microphone socket heartbeat timed out after {}s",
                        heartbeat_timings.timeout.as_secs()
                    );
                    break;
                }
                if ws_tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    sessions.detach_mic(mic_id);
    info!("ws: the microphone socket closed after {buffers_seen} buffer(s) from the browser");
}

async fn session(
    mut socket: WebSocket,
    sessions: Arc<SessionManager>,
    token: Option<String>,
    display: Option<protocol::HostDisplay>,
    heartbeat_timings: HeartbeatTimings,
) {
    let attachment = token.and_then(|t| sessions.attach(&t, display).ok());
    let Some(attachment) = attachment else {
        warn!("ws: rejected connection without a valid session token");
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: CLOSE_INVALID_TOKEN,
                reason: "invalid session token".into(),
            })))
            .await;
        return;
    };

    info!("ws: client attached to the session slot");

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (attach_id, mut events) = (attachment.id, attachment.events);

    // How many times the client has said it lost its tile cache. The inbound half
    // bumps it; the outbound half, which owns the cache, notices before its next
    // batch. A counter and not a flag or a channel: the outbound task must not have
    // to *wait* for this, an extra clear is always harmless, and comparing two
    // numbers needs no lock and cannot deadlock against the send path.
    let cache_epoch = Arc::new(AtomicU64::new(0));
    let inbound_epoch = Arc::clone(&cache_epoch);
    let paint = Arc::new(Mutex::new(PaintTracker::publishing(attachment.feedback)));
    let outbound_paint = Arc::clone(&paint);
    // Woken by the inbound half whenever an acknowledgment advances the window,
    // so a parked batch leaves as soon as there is room rather than on a timer.
    let room = Arc::new(tokio::sync::Notify::new());
    let outbound_room = Arc::clone(&room);

    // Outbound: session events -> browser, batched through [`Wire`], whose
    // counters are logged when the attachment ends so the transport can be
    // measured in the field. Ends on eviction (explicit close) or engine death.
    let mut outbound = tokio::spawn(async move {
        let mut wire = Wire::default();
        let mut seen_epoch = 0u64;
        let mut heartbeat = interval(heartbeat_timings.interval);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        'outbound: loop {
            let event = tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    event
                }
                _ = heartbeat.tick() => {
                    if ws_tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                    continue;
                }
            };

            // Before anything is encoded: if the client has said it lost its
            // cache, stop believing it holds anything. Checked here rather than on
            // arrival because this is the task that knows, and one iteration of
            // latency costs at most a batch of references the client would answer
            // with another reset.
            let epoch = cache_epoch.load(Ordering::Relaxed);
            if epoch != seen_epoch {
                seen_epoch = epoch;
                wire.reset_cache();
            }

            // Take everything already queued behind the first message, so a burst
            // of tiles is batched instead of costing a frame each. `try_recv` and
            // not another `await`: a batch must never *wait* for more work, only
            // collect what is already there. Under a slow link the channel fills
            // and the batches grow, which is the adaptation wanted — bigger writes
            // exactly when per-frame overhead hurts most.
            let mut run = Vec::new();
            let mut evicted = false;
            for event in std::iter::once(event).chain(std::iter::from_fn(|| events.try_recv().ok()))
            {
                match event {
                    AttachEvent::Msg(msg) => run.push(msg),
                    AttachEvent::Evicted => {
                        evicted = true;
                        break;
                    }
                }
            }

            // Whatever was already encoded still goes out first: eviction is not a
            // reason to drop paint the client was owed.
            let frames = match wire.encode(run) {
                Ok(frames) => frames,
                Err(e) => {
                    warn!("ws: closing exhausted attachment: {e}");
                    let _ = ws_tx
                        .send(Message::Close(Some(CloseFrame {
                            code: CLOSE_SEQUENCE_EXHAUSTED,
                            reason: e.to_string().into(),
                        })))
                        .await;
                    break 'outbound;
                }
            };
            for frame in frames {
                match frame {
                    WireFrame::Batch { sequence, bytes } => {
                        // The one hop with no backpressure of its own. Waiting
                        // here — before the write, after the encode — is what
                        // makes the browser's paint queue as bounded as every
                        // queue behind it: the events channel fills while this
                        // is parked, then the pump's, and the engine feels it.
                        let Ok(admission) = wait_for_paint_window(
                            &outbound_paint,
                            &outbound_room,
                            &mut heartbeat,
                            &mut ws_tx,
                        )
                        .await
                        else {
                            break 'outbound; // browser gone
                        };
                        if send_batch(
                            &outbound_paint,
                            &mut ws_tx,
                            sequence,
                            Message::Binary(bytes.into()),
                            admission,
                        )
                        .await
                        .is_err()
                        {
                            break 'outbound; // browser gone
                        }
                    }
                    WireFrame::Text(json) => {
                        if ws_tx.send(Message::Text(json.into())).await.is_err() {
                            break 'outbound; // browser gone
                        }
                    }
                    WireFrame::Audio(_) => {
                        warn!("ws: audio frame reached the session socket");
                    }
                }
            }

            if evicted {
                info!("ws: evicted by a session takeover");
                let _ = ws_tx
                    .send(Message::Close(Some(CloseFrame {
                        code: CLOSE_EVICTED,
                        reason: "session taken over".into(),
                    })))
                    .await;
                break;
            }
        }
        info!("ws: outbound totals: {}", wire.totals);
    });

    // Inbound: browser input -> protocol engine. Also ends when the outbound
    // side finishes (eviction / engine death), so a socket that lingers after
    // eviction can't keep injecting input into the session.
    let mut outbound_done = false;
    let mut heartbeat_check = interval(heartbeat_timings.interval);
    heartbeat_check.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_heartbeat = Instant::now();
    loop {
        let msg = tokio::select! {
            res = &mut outbound => {
                if let Err(e) = res {
                    warn!("ws: outbound task failed: {e}");
                }
                outbound_done = true;
                break;
            }
            msg = ws_rx.next() => msg,
            _ = heartbeat_check.tick() => {
                if last_heartbeat.elapsed() >= heartbeat_timings.timeout {
                    warn!(
                        "ws: browser heartbeat timed out after {}s",
                        last_heartbeat.elapsed().as_secs()
                    );
                    // Waiting for the missing pong already consumed the
                    // reattach grace period, so end the engine immediately.
                    sessions.expire_attachment(attach_id);
                    break;
                }
                continue;
            }
        };
        match msg {
            Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientMsg>(&text) {
                // Session-control messages act on the slot, not an engine: pick a
                // target from the picker, or tear the session down and go back to
                // it ("switch target").
                Ok(ClientMsg::Connect { target, display }) => {
                    if let Err(e) = sessions.connect(attach_id, &target, display) {
                        warn!("ws: connect to {target:?} refused: {e}");
                    }
                }
                Ok(ClientMsg::Disconnect) => sessions.disconnect(attach_id),
                // The client lost the tiles it was told to remember. Both halves
                // have to act: the outbound task forgets the slots (through the
                // epoch), and the engine repaints — a repaint alone would come back
                // as the same references and miss again.
                Ok(ClientMsg::CacheReset) => {
                    inbound_epoch.fetch_add(1, Ordering::Relaxed);
                    sessions.forward_input(attach_id, ClientMsg::Refresh);
                }
                // Attachment transport feedback, not remote input. Consuming
                // it here is what keeps an RDP/VNC engine from learning that a
                // browser or a paint worker exists.
                Ok(ClientMsg::PaintAck {
                    sequence,
                    queued_ms,
                    draw_ms,
                }) => {
                    paint
                        .lock()
                        .unwrap()
                        .acknowledge(sequence, queued_ms, draw_ms);
                    // Unconditional: a stale acknowledgment frees nothing, and
                    // the woken sender re-checks the window anyway.
                    room.notify_one();
                }
                // Everything else is engine input, routed to the current engine
                // (dropped in the picker state). Routing through the manager —
                // rather than a captured engine sender — means it always reaches
                // the engine that is live *now*, across connect/disconnect.
                Ok(input) => sessions.forward_input(attach_id, input),
                Err(e) => warn!("ws: bad client message: {e} (raw: {text})"),
            },
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(Message::Pong(_))) => {
                last_heartbeat = Instant::now();
            }
            Some(Ok(_)) => {} // Binary/Ping: nothing to do
            Some(Err(e)) => {
                warn!("ws: receive error: {e}");
                break;
            }
        }
    }

    // Give the slot back and start the shared reattach grace period. If the
    // slot already moved on (takeover or heartbeat expiry) this is a no-op.
    sessions.detach(attach_id);

    // Let the outbound task drain (its totals line should still be logged),
    // but don't wait on a hung socket forever.
    if !outbound_done
        && tokio::time::timeout(std::time::Duration::from_secs(5), &mut outbound)
            .await
            .is_err()
    {
        outbound.abort();
    }
    info!("ws: paint totals: {}", paint.lock().unwrap());
    info!("ws: client detached");
}

#[cfg(test)]
mod tests {
    use axum::{Router, routing::any};
    use futures_util::SinkExt as _;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message as ClientFrame;

    use super::*;
    use crate::config::{Protocol, Security, TargetConfig};
    use crate::protocol::ServerMsg;
    use crate::session::SessionManager;

    #[test]
    fn paint_acknowledgments_are_ordered_and_cumulative() {
        let mut paint = PaintTracker::default();
        paint.sent(1);
        paint.sent(2);
        paint.sent(3);

        paint.acknowledge(2, 7, 11);
        assert_eq!(paint.sent, 3);
        assert_eq!(paint.acknowledgments, 1);
        assert_eq!(paint.completed, 2);
        assert_eq!(paint.pending.len(), 1);
        assert_eq!(paint.pending.front().unwrap().sequence, 3);
        assert_eq!(paint.max_in_flight, 3);
        assert_eq!(paint.queued_ms, 7);
        assert_eq!(paint.max_queued_ms, 7);
        assert_eq!(paint.draw_ms, 11);
        assert_eq!(paint.max_draw_ms, 11);

        paint.acknowledge(1, 100, 100);
        assert_eq!(paint.stale, 1);
        assert_eq!(paint.acknowledgments, 1);
        assert_eq!(paint.pending.len(), 1);
    }

    /// What the tracker learns reaches the encoders: sending marks the link
    /// owed, the first acknowledgment measures the baseline, and settling the
    /// debt settles the lag. Asked about *later* instants rather than waited
    /// for, so nothing here depends on the machine's speed.
    #[test]
    fn the_tracker_publishes_owed_age_and_baseline_through_the_feedback() {
        let feedback = Arc::new(LinkFeedback::new());
        let mut paint = PaintTracker::publishing(Arc::clone(&feedback));
        let later = |ms: u64| Instant::now() + Duration::from_millis(ms);

        // Nothing owed: no lag, however much later it is asked.
        assert_eq!(feedback.lag(later(500)), Duration::ZERO);

        // A batch owed but never acknowledged: still none — with no baseline,
        // queueing cannot be told from distance, and the safe answer is clear.
        paint.sent(1);
        assert_eq!(feedback.lag(later(500)), Duration::ZERO);

        // The first acknowledgment measures the floor; the age of the next owed
        // batch beyond that floor is lag.
        paint.acknowledge(1, 0, 0);
        paint.sent(2);
        let lag = feedback.lag(later(500));
        assert!(lag > Duration::from_millis(400), "expected ~500ms of lag, got {lag:?}");

        // Settling the debt settles the lag.
        paint.acknowledge(2, 0, 0);
        assert_eq!(feedback.lag(later(500)), Duration::ZERO);
    }

    #[test]
    fn paint_tracking_is_bounded_and_a_failed_write_is_not_counted() {
        let mut paint = PaintTracker::default();
        for sequence in 1..=u32::try_from(MAX_TRACKED_PAINTS + 1).unwrap() {
            paint.sent(sequence);
        }
        assert_eq!(paint.pending.len(), MAX_TRACKED_PAINTS);
        assert_eq!(paint.forgotten, 1);
        assert_eq!(paint.pending.front().unwrap().sequence, 2);

        let last = u32::try_from(MAX_TRACKED_PAINTS + 1).unwrap();
        paint.unsent(last);
        assert_eq!(paint.sent, u64::from(last - 1));
        assert_eq!(paint.pending.len(), MAX_TRACKED_PAINTS - 1);
        assert_eq!(paint.pending.back().unwrap().sequence, last - 1);
    }

    /// A tracker owing exactly [`PAINT_WINDOW`] batches: the state in which the
    /// next one has to wait.
    fn full_window() -> Arc<Mutex<PaintTracker>> {
        let mut paint = PaintTracker::default();
        for sequence in 1..=u32::try_from(PAINT_WINDOW).unwrap() {
            paint.sent(sequence);
        }
        Arc::new(Mutex::new(paint))
    }

    fn window_heartbeat() -> tokio::time::Interval {
        let mut heartbeat = interval(HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        heartbeat
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_window_holds_the_next_batch_until_an_acknowledgment() {
        let paint = full_window();
        let room = Arc::new(tokio::sync::Notify::new());
        let waiter = tokio::spawn({
            let (paint, room) = (Arc::clone(&paint), Arc::clone(&room));
            async move {
                let mut heartbeat = window_heartbeat();
                wait_for_paint_window(
                    &paint,
                    &room,
                    &mut heartbeat,
                    &mut futures_util::sink::drain(),
                )
                .await
                .unwrap()
            }
        });
        // Yields rather than a sleep: the clock is paused and this task stays
        // ready, so the waiter parks without the grace deadline moving closer.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(!waiter.is_finished(), "a full window let a batch through");

        paint.lock().unwrap().acknowledge(1, 3, 5);
        room.notify_one();
        assert_eq!(
            waiter.await.unwrap(),
            Admission::Waited,
            "the window opened, so nothing ran past it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_acknowledges_nothing_is_paced_not_wedged() {
        let paint = full_window();
        let room = Arc::new(tokio::sync::Notify::new());
        let mut heartbeat = window_heartbeat();
        let started = Instant::now();
        // Nothing acknowledges anything: with the clock paused this returns only
        // by the grace deadline, which is the point — a silent client costs the
        // session pacing, never progress.
        let admission = wait_for_paint_window(
            &paint,
            &room,
            &mut heartbeat,
            &mut futures_util::sink::drain(),
        )
        .await
        .unwrap();

        assert_eq!(admission, Admission::PastWindow);
        assert!(started.elapsed() >= PAINT_WINDOW_GRACE);
        let paint = paint.lock().unwrap();
        assert_eq!(paint.in_flight(), PAINT_WINDOW, "nothing was acknowledged");
    }

    #[tokio::test(start_paused = true)]
    async fn a_painter_that_is_behind_holds_a_batch_the_depth_window_would_admit() {
        let paint = Arc::new(Mutex::new(PaintTracker::default()));
        let room = Arc::new(tokio::sync::Notify::new());
        // One batch owed — nowhere near the depth window, which is the whole
        // point: this is the shape a video attachment falls behind in.
        paint.lock().unwrap().sent(1);
        tokio::time::advance(PAINT_LAG_LIMIT + Duration::from_millis(1)).await;
        assert!(paint.lock().unwrap().in_flight() < PAINT_WINDOW);
        assert!(!paint.lock().unwrap().admits_a_batch());

        let waiter = tokio::spawn({
            let (paint, room) = (Arc::clone(&paint), Arc::clone(&room));
            async move {
                let mut heartbeat = window_heartbeat();
                wait_for_paint_window(
                    &paint,
                    &room,
                    &mut heartbeat,
                    &mut futures_util::sink::drain(),
                )
                .await
                .unwrap()
            }
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(!waiter.is_finished(), "a painter 150ms behind was fed more");

        // Catching up is what opens it: the acknowledgment empties the queue, so
        // there is no oldest batch to be behind on any more.
        paint.lock().unwrap().acknowledge(1, 3, 5);
        room.notify_one();
        assert_eq!(waiter.await.unwrap(), Admission::Waited);
    }

    /// A sink that refuses every write, which is what a browser that went away
    /// looks like from the outbound half.
    struct DeadSocket;

    impl futures_util::Sink<Message> for DeadSocket {
        type Error = ();

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), ()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(self: std::pin::Pin<&mut Self>, _: Message) -> Result<(), ()> {
            Err(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), ()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), ()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// The totals count batches the browser was actually given. A write that
    /// fails takes its batch out of every one of them — including the two the
    /// window keeps, which is what the wait reporting its cost rather than
    /// recording it is for.
    #[tokio::test]
    async fn a_batch_the_socket_refuses_is_counted_nowhere() {
        let paint = Arc::new(Mutex::new(PaintTracker::default()));
        let refused = send_batch(
            &paint,
            &mut DeadSocket,
            1,
            Message::Binary(Vec::new().into()),
            // The admission that used to be recorded before the write, so a
            // failed write reported a batch as having run past the window while
            // the same batch was rolled out of `sent`.
            Admission::PastWindow,
        )
        .await;

        assert!(refused.is_err());
        let paint = paint.lock().unwrap();
        assert_eq!(paint.sent, 0);
        assert_eq!(paint.in_flight(), 0);
        assert_eq!(paint.window_waits, 0);
        assert_eq!(paint.past_window, 0);
        let totals = paint.to_string();
        assert!(totals.starts_with("0 batch(es) sent"), "{totals}");
        assert!(
            totals.ends_with("0 batch(es) waited on the window, 0 sent past it"),
            "{totals}"
        );
    }

    /// The other half of the same rule: a batch that does reach the socket
    /// carries its wait into the totals, so the counters are not simply dead.
    #[tokio::test]
    async fn a_batch_that_reaches_the_socket_carries_its_wait_into_the_totals() {
        let paint = Arc::new(Mutex::new(PaintTracker::default()));
        send_batch(
            &paint,
            &mut futures_util::sink::drain(),
            1,
            Message::Binary(Vec::new().into()),
            Admission::PastWindow,
        )
        .await
        .unwrap();

        let paint = paint.lock().unwrap();
        assert_eq!(paint.sent, 1);
        assert_eq!(paint.in_flight(), 1);
        assert_eq!(paint.window_waits, 1);
        assert_eq!(paint.past_window, 1);
    }

    fn fake_target(audio: bool) -> TargetConfig {
        TargetConfig {
            name: "fake".to_owned(),
            protocol: Protocol::Vnc,
            subtype: None,
            host: "127.0.0.1".to_owned(),
            port: 1,
            username: String::new(),
            password: String::new(),
            vnc_password: String::new(),
            domain: None,
            width: Some(1),
            height: Some(1),
            security: Security::Auto,
            egfx: None,
            resize: false,
            clipboard: false,
            audio,
            audio_codec: None,
            render_type: crate::config::RenderType::Tiles,
            render_subtype: crate::config::RenderSubtype::Png,
            render_quality: None,
            render_motion_subtype: None,
            render_motion_quality: None,
            render_motion_debug: false,
            render_classify_debug: false,
            render_adaptive: false,
            render_adaptive_min: None,
            audio_bitrate: None,
            audio_adaptive: false,
            audio_bitrate_min: None,
            camera: false,
            microphone: false,
        }
    }

    #[tokio::test]
    async fn paint_feedback_stops_at_the_websocket_bridge() {
        let (engine_tx, mut engine_rx) = mpsc::unbounded_channel();
        let sessions = Arc::new(SessionManager::with_test_spawner(
            vec![fake_target(false)],
            move |_target, input_rx, frame_tx, _audio| {
                engine_tx.send((input_rx, frame_tx)).unwrap();
            },
        ));
        let token = sessions.claim(false, None).unwrap();
        let app = Router::new().route(
            "/ws",
            any(move |ws: WebSocketUpgrade| {
                let sessions = Arc::clone(&sessions);
                let token = token.clone();
                async move {
                    ws.on_upgrade(move |socket| {
                        session(socket, sessions, Some(token), None, HEARTBEAT_TIMINGS)
                    })
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        client
            .send(ClientFrame::text(r#"{"type":"connect","target":"fake"}"#))
            .await
            .unwrap();
        let (mut input_rx, _frame_tx) = engine_rx.recv().await.unwrap();
        // A following engine input is the ordering fence: when it arrives, the
        // bridge has already parsed the paint acknowledgment before it. If the
        // acknowledgment leaked through, it would be the first message here.
        client
            .send(ClientFrame::text(
                r#"{"type":"paintAck","sequence":1,"queuedMs":7,"drawMs":11}"#,
            ))
            .await
            .unwrap();
        client
            .send(ClientFrame::text(r#"{"type":"refresh"}"#))
            .await
            .unwrap();
        assert!(matches!(input_rx.recv().await, Some(ClientMsg::Refresh)));

        drop(client);
        server.abort();
    }

    #[tokio::test]
    async fn unanswered_websocket_pings_expire_the_session_engine() {
        let target = fake_target(false);
        let (engine_tx, mut engine_rx) = mpsc::unbounded_channel();
        let sessions = Arc::new(SessionManager::with_test_spawner(
            vec![target],
            move |_target, input_rx, frame_tx, _audio| {
                engine_tx.send((input_rx, frame_tx)).unwrap();
            },
        ));
        let token = sessions.claim(false, None).unwrap();
        let assertions = Arc::clone(&sessions);
        let timings = HeartbeatTimings {
            interval: Duration::from_secs(1),
            timeout: REATTACH_GRACE_PERIOD,
        };
        let app = Router::new().route(
            "/ws",
            any(move |ws: WebSocketUpgrade| {
                let sessions = Arc::clone(&sessions);
                let token = token.clone();
                async move {
                    ws.on_upgrade(move |socket| session(socket, sessions, Some(token), None, timings))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        // Establish the OS-backed WebSocket under real time. Pausing before the
        // handshake lets Tokio auto-advance idle time through heartbeat ticks
        // while the runtime waits for socket I/O, which can expire the session
        // before this test reaches its first assertion under a busy test suite.
        client
            .send(ClientFrame::Pong(Vec::new().into()))
            .await
            .unwrap();
        client
            .send(ClientFrame::text(r#"{"type":"connect","target":"fake"}"#))
            .await
            .unwrap();
        let (input_rx, _frame_tx) = engine_rx.recv().await.unwrap();
        // WebSocket frames are processed in order, so engine creation proves
        // the preceding Pong reset the heartbeat baseline. Freeze only the
        // timeout window that the assertions below control.
        tokio::time::pause();

        // Never poll `client`: its protocol stack cannot read Ping frames and
        // therefore cannot enqueue automatic Pong replies.
        tokio::time::advance(REATTACH_GRACE_PERIOD - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(!input_rx.is_closed(), "engine expired before the heartbeat timeout");

        tokio::time::advance(Duration::from_secs(1) + Duration::from_millis(1)).await;
        for _ in 0..10 {
            if input_rx.is_closed() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(input_rx.is_closed(), "heartbeat timeout did not stop the engine");
        let replacement_token = assertions
            .claim(false, None)
            .expect("heartbeat timeout did not release the browser attachment");
        let mut replacement = assertions.attach(&replacement_token, None).unwrap();
        assert!(matches!(
            replacement.events.recv().await,
            Some(AttachEvent::Msg(ServerMsg::Picker))
        ));

        drop(client);
        server.abort();
    }

    /// The mirror of the test above, and the one that pins the difference between the
    /// two endpoints: an audio socket that stops answering reaps *itself* and nothing
    /// else. The session socket is the authority on whether the browser is alive, so a
    /// desktop has to survive its sound dying.
    #[tokio::test]
    async fn unanswered_audio_pings_close_the_audio_socket_and_leave_the_engine_alone() {
        let (engine_tx, mut engine_rx) = mpsc::unbounded_channel();
        let sessions = Arc::new(SessionManager::with_test_spawner(
            vec![fake_target(true)],
            move |_target, input_rx, frame_tx, audio| {
                engine_tx.send((input_rx, frame_tx, audio)).unwrap();
            },
        ));
        let token = sessions.claim(false, None).unwrap();
        // A live desktop, driven in process: this test is about the audio socket, and
        // the session socket only has to exist for `connect` to be legal.
        let mut att = sessions.attach(&token, None).unwrap();
        assert!(matches!(
            att.events.recv().await,
            Some(AttachEvent::Msg(ServerMsg::Picker))
        ));
        sessions.connect(att.id, "fake", None).unwrap();
        let (input_rx, _frame_tx, bridge) = engine_rx.recv().await.unwrap();
        let bridge = bridge.expect("an audio target's engine is given a bridge");

        let timings = HeartbeatTimings {
            interval: Duration::from_secs(1),
            timeout: REATTACH_GRACE_PERIOD,
        };
        let served = Arc::clone(&sessions);
        let app = Router::new().route(
            "/ws/audio",
            any(move |ws: WebSocketUpgrade| {
                let sessions = Arc::clone(&served);
                let token = token.clone();
                async move {
                    ws.on_upgrade(move |socket| audio(socket, sessions, Some(token), timings))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/audio"))
            .await
            .unwrap();
        // Same reasoning as above: establish the socket under real time, and prove the
        // baseline was reset before freezing the window the assertions control.
        // No opening Pong, unlike the session test above: this socket has no inbound
        // message to prove one was processed in order, and a Pong still in flight when
        // the clock froze would reset the baseline *after* the advance below and hold
        // the socket open forever. The baseline is the task's own start instead, which
        // is established under real time by the wait here.
        //
        // The subscriber count is the observable, as in `session`'s own audio tests: it
        // says the socket attached *and* armed a pump, and the session answers it
        // rather than socket I/O, which a paused clock cannot drive.
        expect_listeners(&bridge, 1).await;
        tokio::time::pause();

        // Never poll `client`: its protocol stack cannot read Ping frames, and
        // therefore cannot answer them, unless it is polled.
        tokio::time::advance(REATTACH_GRACE_PERIOD + Duration::from_secs(1)).await;
        expect_listeners(&bridge, 0).await;
        assert!(
            !input_rx.is_closed(),
            "an audio socket timing out must not take the desktop with it"
        );

        drop(client);
        server.abort();
    }

    /// Wait for the bridge's subscriber count to settle at `want`, bounded so a count
    /// that never settles fails rather than hangs. The twin of `session`'s helper of
    /// the same name, and polled for the same reason: stopping audio aborts a task, so
    /// the listener goes when the runtime drops it — prompt, but not synchronous.
    async fn expect_listeners(bridge: &crate::audio::AudioBridge, want: usize) {
        for _ in 0..1000 {
            if bridge.listener_count() == want {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "expected {want} audio listener(s), found {}",
            bridge.listener_count()
        );
    }
}
