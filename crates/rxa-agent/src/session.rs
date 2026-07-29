//! One gateway connection, from handshake to hangup.
//!
//! ## The pipeline
//!
//! ```text
//! SCStream callback ──▶ raw tile channel ──▶ encoder thread ──▶ out channel ──▶ pump ──▶ socket
//!   (dispatch queue)      (bounded, sync)     (std::thread)      (bounded)      (tokio)
//!                                              └─ ENCODE_WIDTH cells at once
//! ```
//!
//! Three deliberate choices in that chain:
//!
//! - **The capture callback never encodes and never blocks.** It extracts the
//!   dirty pixels and hands them on. Blocking ScreenCaptureKit's dispatch queue
//!   stalls capture itself.
//! - **Both channels are bounded, and a full raw channel coalesces.** Rather
//!   than queueing frames the link cannot carry, the sink drops the frame and
//!   sets the full-repaint flag, so falling behind becomes one later, coarser
//!   repaint. An unbounded queue would grow for as long as the browser is slow
//!   and then deliver a flood of stale tiles.
//! - **Cells of one frame encode in parallel; a frame is never split across two.**
//!   Order is correctness here rather than tidiness: the same region *is* commonly
//!   dirty in consecutive frames, tiles overwrite their rectangles with no delta
//!   state, and an older tile landing on top of a newer one leaves stale pixels on
//!   screen until something else redraws them.
//!
//!   What makes that free is the *shape of the channel*: the callback hands over a
//!   whole frame at once, so [`encode_batch`] can compress a batch's cells
//!   concurrently and still collect them by index — output order is input order by
//!   construction, and two frames cannot interleave because a batch is finished
//!   before the next is received. The gateway needs a FIFO of `JoinHandle`s for the
//!   same guarantee (`src/encode.rs`) only because its bands trickle out of a
//!   protocol-read loop one at a time.
//!
//!   The parallelism is worth having because a *full* repaint is not the dozen cells
//!   typical damage is: a Retina 1600x1000 desktop is 3200x2000 captured, which is
//!   320 cells of the 320x64 grid and over a tenth of a second of encoding. Every
//!   attach, Refresh, display switch and capture restart pays it — and so does a
//!   coalesced frame, which is the shape that can feed itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use log::{debug, info, warn};
use rxa_proto::frame::{FrameReader, FrameWriter};
use rxa_proto::msg::{
    AgentMsg, DisplayEntry, GatewayMsg, MAX_CLIPBOARD_BYTES, SCALE_ONE, clipboard_fits,
};
use rxa_proto::next_clipboard_time;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval};

use crate::capture::{self, Capture, FrameSink, RawTile};
use crate::cursor;
use crate::encode;
use crate::input::{Injector, PointerHome};
use crate::pasteboard;

/// Frames of raw tiles buffered between the capture callback and the encoder.
/// Small on purpose: see the coalescing note in the module docs.
const RAW_BACKLOG: usize = 2;

/// Encoded tiles buffered between the encoder and the socket.
const OUT_BACKLOG: usize = 64;

/// Cells of one frame compressed at once.
///
/// The only parallelism dial, and it bounds in-flight work rather than describing a
/// pool: [`encode_batch`] keeps this many encodes running, collects the oldest, and
/// forwards it before starting another. So a 320-cell repaint never spawns 320 tasks,
/// and the first tile still reaches the pump as soon as it is encoded rather than
/// after the whole frame — which is what keeps back-pressure and the coalescing above
/// behaving as they did when this was one cell at a time.
///
/// **8 is measured, not guessed**, on a 12-core (8 performance) arm64 Mac. First
/// `encode_width_sweep` below, over a 3200x2000 full repaint's 320 cells, medians of
/// three runs:
///
/// | width | flat UI wall | its CPU | photographic wall | its CPU |
/// |---|---|---|---|---|
/// | 1 | 35.5 ms | 33.7 ms | 380 ms | 377 ms |
/// | 2 | 18.8 ms | 35.7 ms | 198 ms | 390 ms |
/// | 4 | 10.1 ms | 36.5 ms | 99 ms | 390 ms |
/// | 8 | **6.3 ms** | 41.3 ms | **51 ms** | 398 ms |
/// | 12 | 6.0 ms | 42–49 ms | 44 ms | 448 ms |
///
/// The CPU column is why this stops at 8 rather than going wider, and it is also what
/// answers the objection that the agent should be modest because it shares its cores
/// with the desktop it is capturing. Up to 8, total CPU barely moves — the same work
/// finishes in a shorter, wider burst, which is *better* for that desktop than pinning
/// one core for 380 ms while the frame pipeline backs up behind it. At 12 the wall clock
/// stops improving and the CPU does not: 19% more of it for 15% less time in the
/// photographic case, and for nothing at all in the flat one.
///
/// Then live, against the agent on the test VM's 3200x2000 display under
/// `tests/rxa_repaint_probe.rs` — which is where the number that mattered was, because a
/// synthetic sweep cannot say whether the *link* was the constraint all along. 26s of
/// streaming under a refresh every 250 ms; width 1 once, width 8 twice and identical:
///
/// | width | frames | frames/s | tiles/frame | encode ÷ waiting | stalled |
/// |---|---|---|---|---|---|
/// | 1 | 191 | 7.3 | 200 | 0.97 | 0.7% |
/// | 8 | 749 | 28.8 | 63 | 6.05 | 0.6% |
///
/// `stalled` is time handing tiles to the socket, so at well under 1% the encoder was
/// the whole cost and no part of widening it could be wasted — which is the thing that
/// had to be checked first and could not be checked synthetically.
///
/// **The frame rate roughly quadrupled and the tile count barely moved**, and that pair
/// is the whole result. `tiles/frame` at 200 is the loop this fixes: a 320-cell repaint
/// outlived the two-frame raw channel, so capture dropped a frame, set `full_repaint`,
/// and asked for another 320-cell repaint — an agent that could not keep up was spending
/// nearly all of its encoder on pixels nobody had changed. At 63 it is reporting real
/// damage again. So the 6x in the encoder does not show up as 6x the tiles; it shows up
/// as four times the frames carrying a third of the redundancy.
///
/// A plain constant rather than `available_parallelism`, for the reason
/// `encode::ENCODE_DEPTH` gives on the gateway: a measurement should be reproducible
/// from the source, and a build that behaves differently on two Macs makes it not be.
/// A Mac with fewer cores does not want a smaller number either — over-committing costs
/// interleaving, not correctness, and the collector waits on the oldest cell regardless.
const ENCODE_WIDTH: usize = 8;

/// How often the pointer shape is compared against what this session last sent.
const CURSOR_POLL: Duration = Duration::from_millis(100);

/// How often the set of attached displays is re-listed, so a screen plugged in
/// mid-session reaches the client's menu without a reconnect.
///
/// Far slower than [`CURSOR_POLL`], which shares the same tick: listing means
/// `SCShareableContent::get`, a round trip to a system service, where the cursor
/// poll is a local read. Plugging a monitor in is not a thing that needs
/// answering within 100 ms.
const DISPLAY_POLL: Duration = Duration::from_secs(2);

/// Waits before each attempt to restart a capture stream that died, and with
/// their length, the number of attempts.
///
/// A display being reconfigured is briefly absent from the shareable-content
/// list altogether, so the first attempt is expected to fail and the total has
/// to cover a mode switch settling — about a second on the VMs this was measured
/// on. Beyond that the display really is gone and the session says so.
const CAPTURE_RESTART_BACKOFF: &[Duration] = &[
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(1000),
    Duration::from_millis(2000),
];

/// Drop the connection if the gateway says nothing at all for this long. It
/// pings every 5s, so this is a wide margin — it exists to reap a half-open TCP
/// connection, not to police latency.
const GATEWAY_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

// The plan also calls for the capture stream to *linger* a few seconds after a
// disconnect, so a brief blip doesn't cost a teardown/restart cycle. That is not
// implemented here: the stream is owned by the session task and stops with it.
// Keeping it alive across sessions means hoisting it into agent-level shared
// state, and it only pays off for outages shorter than the gateway's own 1s
// minimum reconnect backoff — restarting the stream costs about as much. Left as
// a measured optimisation rather than a guess; see docs/roadmap.md.

/// A measured backing scale as the wire carries it: hundredths, so a Retina
/// display's 2.0 travels as 200.
///
/// The gateway refuses anything outside 1x..4x (`scale_ratio`), so this only has
/// to keep a measurement from wrapping on the way there; `capture` already falls
/// back to 1.0 for a display whose mode it cannot read.
fn wire_scale(scale: f64) -> u16 {
    let hundredths = (scale * f64::from(SCALE_ONE)).round();
    if hundredths.is_finite() {
        hundredths.clamp(0.0, f64::from(u16::MAX)) as u16
    } else {
        SCALE_ONE
    }
}

/// What the encoder sends to the pump, in capture order.
enum Out {
    Tile {
        format: u8,
        rect: capture::Rect,
        data: Vec<u8>,
    },
    Resized(u16, u16),
    Failed(String),
}

/// What the capture callback sends to the encoder, in capture order.
///
/// Resizes travel the same channel as tiles rather than a side channel, so a
/// resize can never overtake the tiles it invalidates — the browser must learn
/// the new size *before* a tile with new coordinates arrives.
enum Captured {
    Tiles(Vec<RawTile>),
    Resized(u16, u16),
    Failed(String),
}

/// The [`FrameSink`] the capture stream writes into.
struct Sink {
    tx: std::sync::mpsc::SyncSender<Captured>,
    full_repaint: Arc<AtomicBool>,
}

impl FrameSink for Sink {
    fn tiles(&self, tiles: Vec<RawTile>) {
        // Never block the dispatch queue: if the encoder is behind, throw this
        // frame away and ask for a full repaint once it catches up.
        if self.tx.try_send(Captured::Tiles(tiles)).is_err() {
            self.full_repaint.store(true, Ordering::Relaxed);
        }
    }

    fn resized(&self, w: u16, h: u16) {
        // A resize must not be dropped — the browser would then paint new
        // coordinates into an old canvas — so this one blocks, briefly.
        let _ = self.tx.send(Captured::Resized(w, h));
    }

    fn failed(&self, message: String) {
        let _ = self.tx.send(Captured::Failed(message));
    }
}

/// The display the agent made, behind the mutex that keeps two reconfigures from
/// racing each other.
type DisplayHandle = Arc<std::sync::Mutex<crate::virtualdisplay::VirtualDisplay>>;

/// The display the agent made, in the two forms a session needs it.
///
/// One parameter rather than two because they are never meaningfully apart: a
/// session either has a display of its own or it does not. `target` is what
/// resolves a client's display id to a capture target; `handle` is the object
/// whose density [`rxa_proto::msg::GatewayMsg::HostScale`] and size
/// [`rxa_proto::msg::GatewayMsg::ResizeDisplay`] can set.
#[derive(Clone, Default)]
pub struct Owned {
    pub target: Option<capture::Target>,
    pub handle: Option<DisplayHandle>,
}

/// The display to reconfigure for a client's request, or `None` if there is not
/// one to reconfigure.
///
/// Both gates at once, and they are separate questions: this agent has a display
/// of its own *and* that display is the one this session is sharing. A Mac's own
/// panel is set on the Mac, and a display nobody is looking at has nothing to
/// match — so a request naming either is dropped rather than answered.
///
/// Shared by [`rxa_proto::msg::GatewayMsg::HostScale`] and
/// [`rxa_proto::msg::GatewayMsg::ResizeDisplay`], which differ in what they do
/// with the handle rather than in who may have one. Returns a clone because
/// every caller hands it to a task that outlives the message.
fn shared_owned_display(
    display: Option<&DisplayHandle>,
    target: capture::Target,
    owned: Option<capture::Target>,
) -> Option<DisplayHandle> {
    match (display, target) {
        (Some(display), capture::Target::Owned { id, .. })
            if Some(id) == owned.map(capture::Target::id) =>
        {
            Some(Arc::clone(display))
        }
        _ => None,
    }
}

/// A gateway that has completed the Noise handshake, and is therefore *the*
/// gateway this Mac is paired with.
///
/// Split out from [`serve`] so the caller can hold the two apart: authenticating
/// is what earns a connection the single session slot, and anything short of it
/// must not disturb the session already in it (see [`crate::serve`]).
pub struct Authenticated {
    reader: FrameReader<OwnedReadHalf>,
    writer: FrameWriter<OwnedWriteHalf>,
}

/// Answer a dial, proving to it and about it that both ends hold the right keys.
///
/// A peer whose public key is not the configured one fails here, before the
/// agent has revealed anything at all — including whether anyone is connected.
pub async fn handshake(
    mut stream: TcpStream,
    private_key: [u8; 32],
    gateway_public_key: [u8; 32],
) -> anyhow::Result<Authenticated> {
    stream.set_nodelay(true).ok();
    let transport = rxa_proto::noise::respond(&mut stream, &private_key, &gateway_public_key)
        .await
        .map_err(|e| anyhow::anyhow!("handshake: {e}"))?;
    let (read_half, write_half) = stream.into_split();
    let (reader, writer) = rxa_proto::frame::split(read_half, write_half, transport);
    Ok(Authenticated { reader, writer })
}

/// Serve one authenticated gateway connection until it hangs up or fails.
pub async fn serve(
    authenticated: Authenticated,
    owned: Owned,
    cursor_tracker: Arc<cursor::Tracker>,
) -> anyhow::Result<()> {
    let Authenticated { reader, mut writer } = authenticated;
    let display = owned.handle.clone();
    let owned = owned.target;

    // Every session starts on the Mac's main display, whichever that is. It is
    // *not* reliably the Mac's own screen: an earlier note here said a display of
    // ours joins as an extended one and never takes that role, and measurement
    // says otherwise — on the test VM the agent's display came up at the global
    // origin, which is what being the main display means, with the Mac's own screen
    // pushed to its left. macOS decides that placement and the arrangement is the
    // user's to set in System Settings, so this reads the answer rather than
    // assuming one.
    //
    // A selection is session state, not agent state: the person at the far end
    // picks a screen for as long as they are looking at it, and the next connection
    // starts from the same place rather than from wherever the last one wandered
    // off to.
    //
    // Through `resolve` so that a Mac whose *only* display is ours still measures
    // it as ours — its backing scale cannot be read back from the system.
    let target = resolve(capture::main_display(), owned);

    // The size has to be known before `Attach`, so it is probed without starting
    // a stream. This is also where a missing Screen Recording grant surfaces.
    let geometry = match capture::probe(target) {
        Ok(geometry) => geometry,
        Err(e) => {
            // Report it to the browser rather than dying quietly in a log — the
            // fix is two clicks in System Settings and the user has to be told.
            let message = format!(
                "cannot capture the screen ({e}). Grant Screen Recording to \
                 remotex-agent in System Settings > Privacy & Security."
            );
            warn!("session: {message}");
            let _ = writer.send(&AgentMsg::Error { message }.encode()).await;
            return Ok(());
        }
    };

    writer
        .send(
            &AgentMsg::Hello {
                version: rxa_proto::VERSION,
                agent_version: env!("CARGO_PKG_VERSION").to_owned(),
                w: geometry.width,
                h: geometry.height,
                scale: wire_scale(geometry.scale),
            }
            .encode(),
        )
        .await?;

    // Next to `Hello`, so a client has the menu before it has a picture.
    let displays = display_list(owned).await;
    writer
        .send(
            &AgentMsg::Displays {
                active: geometry.id,
                displays: displays.clone(),
            }
            .encode(),
        )
        .await?;

    pump(
        reader,
        writer,
        geometry,
        target,
        Owned {
            target: owned,
            handle: display,
        },
        displays,
        cursor_tracker,
    )
    .await
}

/// The target for a display id, given the display this agent made (if any).
///
/// The one place that mapping happens, because getting it wrong is quiet:
/// [`capture::Target::Owned`] is what carries the created point size, and
/// without it the backing scale of our own display is read through
/// `CGDisplayCopyDisplayMode`, which returns nothing for it and so reads 1x. The
/// list would then advertise a 2x display while capture took half the pixels.
fn resolve(id: u32, owned: Option<capture::Target>) -> capture::Target {
    match owned {
        Some(owned) if owned.id() == id => owned,
        _ => capture::Target::Real(id),
    }
}

/// Build the display list to report, off the runtime's worker.
///
/// `capture::displays` is a synchronous round trip to a system service, measured
/// at a **68 ms median** on the test VM (min 59, max 71, twelve samples). That is
/// far too long to run on a worker that is also forwarding tiles, so every caller
/// goes through `spawn_blocking`.
///
/// The callers here are the one-off ones — a `Hello`, an `Attach`, a switch — where
/// the answer is part of the reply and waiting for it is unavoidable. The periodic
/// poll must *not* await it inline, or the pump stops forwarding tiles for those
/// 68 ms; it hands the work to [`spawn_display_probe`] and collects the result on
/// its own `select!` branch.
async fn display_list(owned: Option<capture::Target>) -> Vec<DisplayEntry> {
    match tokio::task::spawn_blocking(move || display_list_blocking(owned)).await {
        Ok(displays) => displays,
        // The blocking pool is only gone if the runtime is shutting down, and a
        // panic in there is a bug. Either way an empty list is the same honest
        // answer the error case below gives.
        Err(e) => {
            warn!("session: display enumeration did not finish: {e}");
            Vec::new()
        }
    }
}

/// Enumerate and label the displays. Synchronous, and slow — see [`display_list`],
/// which is how the async side reaches it.
fn display_list_blocking(owned: Option<capture::Target>) -> Vec<DisplayEntry> {
    match capture::displays(owned) {
        Ok(displays) => displays
            .into_iter()
            .map(|display| DisplayEntry {
                id: display.geometry.id,
                label: display.label,
                detail: display.detail,
                w: display.geometry.width,
                h: display.geometry.height,
                scale: wire_scale(display.geometry.scale),
                flags: (u8::from(display.is_main) * DisplayEntry::MAIN)
                    | (u8::from(display.is_owned) * DisplayEntry::OWNED),
            })
            .collect(),
        // An empty list is the honest answer for a client: it hides the picker
        // rather than offering a display that cannot be named. Anything that
        // would actually stop a session — a missing grant above all — has
        // already been reported by `probe`.
        Err(e) => {
            warn!("session: cannot list displays: {e:#}");
            Vec::new()
        }
    }
}

/// Start a display enumeration on the blocking pool, to be collected on the
/// pump's `displays_rx` branch.
///
/// Fire and forget on purpose. The periodic poll exists so a monitor plugged in
/// mid-session reaches the menu, which is worth 68 ms of somebody's time but not
/// 68 ms of the tile path's — so nothing here is awaited, and the pump keeps
/// forwarding frames while the WindowServer is asked.
fn spawn_display_probe(owned: Option<capture::Target>, tx: mpsc::Sender<Vec<DisplayEntry>>) {
    tokio::task::spawn_blocking(move || {
        // A full channel means a probe's result is still unread, which cannot
        // happen while the pump starts at most one at a time — and if it ever
        // did, dropping this list is right: the next poll produces a fresher one.
        let _ = tx.blocking_send(display_list_blocking(owned));
    });
}

/// Tear down whatever is streaming and start again on `target`.
///
/// The teardown order is the same one the session's own exit path uses and is
/// not incidental: dropping the receiver is what lets an encoder parked on a
/// full output channel finish, so joining before that would deadlock exactly
/// when it matters most.
///
/// On failure nothing is left running, and the caller decides whether to fall
/// back to the display it came from or give up.
fn switch_capture(
    target: capture::Target,
    full_repaint: &Arc<AtomicBool>,
    capture: &mut Option<Capture>,
    out_rx: &mut Option<mpsc::Receiver<Out>>,
    encoder_thread: &mut Option<std::thread::JoinHandle<()>>,
) -> anyhow::Result<capture::Geometry> {
    *capture = None;
    *out_rx = None;
    if let Some(thread) = encoder_thread.take() {
        let _ = thread.join();
    }
    let (started, rx, thread) = start_pipeline(target, Arc::clone(full_repaint))?;
    let live = started.geometry;
    *capture = Some(started);
    *out_rx = Some(rx);
    *encoder_thread = Some(thread);
    Ok(live)
}

async fn pump(
    reader: FrameReader<OwnedReadHalf>,
    mut writer: FrameWriter<OwnedWriteHalf>,
    geometry: capture::Geometry,
    target: capture::Target,
    owned: Owned,
    // What `serve` has already reported, so the poll below starts quiet.
    displays: Vec<DisplayEntry>,
    cursor_tracker: Arc<cursor::Tracker>,
) -> anyhow::Result<()> {
    let display = owned.handle;
    let owned = owned.target;
    let mut target = target;
    // `FrameReader::recv` is not cancel-safe, so it gets its own task.
    let (gateway_tx, mut gateway_rx) = mpsc::channel(32);
    let read_task = tokio::spawn(read_loop(reader, gateway_tx));
    let _abort = AbortOnDrop(read_task);

    let mut injector = Injector::new(geometry.scale, geometry.origin);
    // Before the first injected event, which is the only moment this is worth
    // asking: from here on the pointer is wherever the client last put it.
    let pointer_home = PointerHome::note(owned.map(capture::Target::id));
    let full_repaint = Arc::new(AtomicBool::new(true));
    // All three appear together when `Attach` starts the pipeline, and are torn
    // down together when the session ends.
    let mut capture: Option<Capture> = None;
    let mut out_rx: Option<mpsc::Receiver<Out>> = None;
    let mut encoder_thread: Option<std::thread::JoinHandle<()>> = None;

    let mut cursor_seen = cursor::UNSEEN;
    let mut cursor_tick = interval(CURSOR_POLL);
    cursor_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_heard = Instant::now();

    // What the client was last told about the displays, so the poll below can
    // stay quiet while nothing has changed.
    let mut displays_sent = displays;
    let mut displays_polled = Instant::now();
    // Results from the periodic enumeration, which runs on the blocking pool
    // rather than on this loop — see `spawn_display_probe`. Capacity one, and at
    // most one probe is ever outstanding.
    let (displays_tx, mut displays_rx) = mpsc::channel::<Vec<DisplayEntry>>(1);
    let mut probing_displays = false;

    // Pasteboard watch state. `None` means not watching, and the gateway only
    // turns it on for a target that opted in — so the default costs nothing and
    // reads nothing. `Some(count)` is the last change counter this session has
    // accounted for; contents are read only when the live counter differs.
    let mut clipboard_seen: Option<isize> = None;
    // Wall-clock time of the last pasteboard counter change this agent
    // observed. A Fetch never advances it; `None` means the current contents
    // predate this watched session and macOS exposes no timestamp for them.
    let mut clipboard_changed_at_ms: Option<u64> = None;

    let result = loop {
        // `out_rx` only exists once the stream is attached; before that, park
        // that branch on a future that never completes.
        let tile_ready = async {
            match out_rx.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            out = tile_ready => {
                let Some(out) = out else {
                    // The encoder thread ended, which means the capture stream
                    // is gone. Restarting is the session's job; report it.
                    break Err(anyhow::anyhow!("the capture pipeline stopped"));
                };
                match out {
                    Out::Tile { format, rect, data } => {
                        writer.send(&AgentMsg::Tile {
                            format,
                            x: rect.x,
                            y: rect.y,
                            w: rect.w,
                            h: rect.h,
                            data,
                        }.encode()).await?;
                    }
                    Out::Resized(w, h) => {
                        // The scale comes from the display rather than the frame:
                        // a surface size arrives with none attached, and
                        // `follow_display` has already re-measured the display by
                        // the time frames at the new size do. The size cannot
                        // stand in for it either — 1920x1080 HiDPI and 3840x2160
                        // at 1x capture the same pixel count at different scales.
                        let scale = capture
                            .as_ref()
                            .map_or(geometry.scale, |live| live.geometry.scale);
                        info!("session: display reconfigured to {w}x{h} at {scale}x");
                        writer
                            .send(
                                &AgentMsg::DisplaySize {
                                    w,
                                    h,
                                    scale: wire_scale(scale),
                                }
                                .encode(),
                            )
                            .await?;
                    }
                    Out::Failed(message) => {
                        // Most often a host-driven display reconfigure: dragging
                        // a VM's window with dynamic resolution on does not
                        // resize the stream, it *kills* it — ScreenCaptureKit
                        // loses the display the filter names and reports "no
                        // capture source provided". The display is back a moment
                        // later in a new mode, so this is a pause rather than
                        // the end of the session. Reporting it as fatal would
                        // bounce the browser to the picker every time someone
                        // resized the VM window.
                        warn!("session: capture failed: {message}");
                        // The dead pipeline first: dropping the receiver is what
                        // lets the encoder thread finish (see the teardown at
                        // the end of this function, which does the same dance).
                        capture = None;
                        out_rx = None;
                        if let Some(thread) = encoder_thread.take() {
                            let _ = thread.join();
                        }

                        let mut restarted = None;
                        for delay in CAPTURE_RESTART_BACKOFF {
                            tokio::time::sleep(*delay).await;
                            match start_pipeline(target, Arc::clone(&full_repaint)) {
                                Ok(started) => {
                                    restarted = Some(started);
                                    break;
                                }
                                // Expected while the display is mid-reconfigure:
                                // it briefly is not in the shareable list at all.
                                Err(e) => debug!("session: capture restart: {e:#}"),
                            }
                        }
                        let Some((started, rx, thread)) = restarted else {
                            let message = format!("{message} (and it did not come back)");
                            warn!("session: {message}");
                            writer.send(&AgentMsg::Error { message }.encode()).await?;
                            break Ok(());
                        };

                        let live = started.geometry;
                        info!("session: capture restarted at {}x{}", live.width, live.height);
                        // The display it came back on, which is not always the
                        // one it went down on: a screen that was unplugged
                        // rather than reconfigured falls back to the main one.
                        target = target.resolved(live.id);
                        injector = Injector::new(live.scale, live.origin);
                        cursor_tracker.set_scale(live.scale);
                        capture = Some(started);
                        out_rx = Some(rx);
                        encoder_thread = Some(thread);
                        // Unconditionally, unlike `Attach`: the browser's canvas
                        // is stale whatever the size came back as, and a full
                        // repaint is coming regardless.
                        writer.send(&AgentMsg::DisplaySize {
                            w: live.width,
                            h: live.height,
                            scale: wire_scale(live.scale),
                        }.encode()).await?;
                        cursor_seen = cursor::UNSEEN;
                    }
                }
            }

            msg = gateway_rx.recv() => {
                let Some(msg) = msg else {
                    break Ok(()); // the gateway hung up
                };
                last_heard = Instant::now();
                match msg {
                    GatewayMsg::Attach => {
                        if capture.is_none() {
                            match start_pipeline(target, Arc::clone(&full_repaint)) {
                                Ok((started, rx, thread)) => {
                                    // The running stream is authoritative: the
                                    // display can have changed mode between the
                                    // probe that fed `Hello` and this Attach, and
                                    // painting into a stale canvas size — or
                                    // dividing input by a stale scale — is worse
                                    // than a redundant DisplaySize.
                                    let live = started.geometry;
                                    target = target.resolved(live.id);
                                    // The scale counts as a change of its own: a
                                    // mode switch can keep the pixel count and
                                    // change how those pixels should be presented,
                                    // and the gateway would otherwise still be
                                    // holding the one `Hello` carried.
                                    let announced = (geometry.width, geometry.height, geometry.scale);
                                    if (live.width, live.height, live.scale) != announced {
                                        info!(
                                            "session: display changed since Hello, now {}x{} at {}x",
                                            live.width, live.height, live.scale
                                        );
                                        writer.send(&AgentMsg::DisplaySize {
                                            w: live.width,
                                            h: live.height,
                                            scale: wire_scale(live.scale),
                                        }.encode()).await?;
                                    }
                                    injector = Injector::new(live.scale, live.origin);
                                    // The pointer is drawn onto a canvas in
                                    // captured pixels, so it must be sized by
                                    // the capture's scale, not the main
                                    // display's.
                                    cursor_tracker.set_scale(live.scale);
                                    capture = Some(started);
                                    out_rx = Some(rx);
                                    encoder_thread = Some(thread);
                                }
                                Err(e) => {
                                    let message = format!("cannot start screen capture: {e}");
                                    warn!("session: {message}");
                                    writer.send(&AgentMsg::Error { message }.encode()).await?;
                                    break Ok(());
                                }
                            }
                        }
                        // Re-send the cached pointer shape: a browser attaching
                        // now would otherwise have none until it changed.
                        cursor_seen = cursor::UNSEEN;
                        // And the display list, for the same reason: a browser
                        // reattaching to a running session missed the one sent
                        // beside `Hello`, and would show no picker at all.
                        displays_sent = display_list(owned).await;
                        writer.send(&AgentMsg::Displays {
                            active: target.id(),
                            displays: displays_sent.clone(),
                        }.encode()).await?;
                    }
                    // The client's screen density. Acted on only for a display
                    // the agent made, and only while that is the one being
                    // shared: a Mac's own panel does not change because someone
                    // connected, and a display nobody is looking at has nothing
                    // to match. `set_scale` returns early when the density already
                    // agrees, which is the common case.
                    //
                    // The reconfigure itself is a WindowServer round trip that
                    // takes hundreds of milliseconds, so it is neither run on this
                    // thread nor waited for on it: awaiting the blocking task here
                    // would hold the select! loop, and with it tiles, cursor
                    // updates and input injection, for the whole of it. Nothing in
                    // the loop depends on the outcome — the size that follows a
                    // successful change arrives through the display poll like any
                    // other reconfigure — so the task only has to outlive the
                    // message, and reports for itself.
                    GatewayMsg::HostScale { scale } => {
                        let wanted = rxa_proto::msg::scale_ratio(scale) >= 1.5;
                        // `None` is a client reporting something true that this
                        // session cannot use.
                        if let Some(display) =
                            shared_owned_display(display.as_ref(), target, owned)
                        {
                            tokio::spawn(async move {
                                let done = tokio::task::spawn_blocking(move || {
                                    display
                                        .lock()
                                        .map_err(|_| anyhow::anyhow!("the display lock is poisoned"))
                                        .and_then(|display| display.set_scale(wanted))
                                })
                                .await;
                                match done {
                                    Ok(Ok(_)) => {}
                                    Ok(Err(e)) => warn!("session: cannot set the display density: {e:#}"),
                                    Err(e) => warn!("session: the density change did not run: {e}"),
                                }
                            });
                        }
                    }
                    // The size of the client's window, gated exactly as the
                    // density above is and for the same two reasons, and detached
                    // for the same reason too — this reconfigure is the slower of
                    // the pair, since it waits for the WindowServer to settle
                    // before releasing the lock.
                    //
                    // The one deliberate difference is `try_lock`. A held lock
                    // means a reconfigure is already running, and dropping this
                    // request is the right answer: the display cannot be two sizes
                    // at once, and whoever pressed the button can press it again
                    // once the desktop has settled. Waiting instead would let a
                    // person mashing the button queue one WindowServer round trip
                    // per press — which is exactly the shape that wedges a guest's
                    // display stack until it is rebooted, and would park a
                    // blocking-pool thread per press if it did.
                    GatewayMsg::ResizeDisplay { w, h } => {
                        // `None` is a resize asked of a Mac's own screen, or of a
                        // display this session is not sharing. Ignored rather than
                        // answered with an `Error`, which the gateway treats as
                        // fatal: a button that did nothing must never be what ends
                        // a session.
                        if let Some(display) =
                            shared_owned_display(display.as_ref(), target, owned)
                        {
                            let points = (u32::from(w), u32::from(h));
                            tokio::spawn(async move {
                                let done = tokio::task::spawn_blocking(move || {
                                    match display.try_lock() {
                                        Ok(display) => display.set_size(points).map(Some),
                                        Err(std::sync::TryLockError::WouldBlock) => Ok(None),
                                        Err(std::sync::TryLockError::Poisoned(_)) => {
                                            Err(anyhow::anyhow!("the display lock is poisoned"))
                                        }
                                    }
                                })
                                .await;
                                match done {
                                    Ok(Ok(Some(_))) => {}
                                    Ok(Ok(None)) => debug!(
                                        "session: a display reconfigure is already running; dropping this resize"
                                    ),
                                    Ok(Err(e)) => warn!("session: cannot resize the display: {e:#}"),
                                    Err(e) => warn!("session: the resize did not run: {e}"),
                                }
                            });
                        }
                    }
                    // Which display to look at is the client's to choose, and
                    // separately from how large to make it — see the messages'
                    // own documentation.
                    GatewayMsg::SelectDisplay { id } => {
                        let next = if displays_sent.iter().any(|display| display.id == id) {
                            Some(resolve(id, owned))
                        } else {
                            // An id from a list this session has since replaced,
                            // or one that was never on it. Not an `AgentMsg::Error`
                            // — the gateway treats those as fatal, and a menu one
                            // tick out of date is not worth ending a session over.
                            warn!("session: display {id} is not one of ours; ignoring");
                            None
                        };
                        match next {
                            Some(next) if next != target => {
                                info!("session: switching to display {id}");
                                if capture.is_none() {
                                    // Nothing is streaming yet, so there is
                                    // nothing to restart: `Attach` will start on
                                    // this target and announce its size itself.
                                    target = next;
                                } else {
                                    let previous = target;
                                    let live = match switch_capture(
                                        next,
                                        &full_repaint,
                                        &mut capture,
                                        &mut out_rx,
                                        &mut encoder_thread,
                                    ) {
                                        Ok(live) => {
                                            target = next.resolved(live.id);
                                            live
                                        }
                                        Err(e) => {
                                            warn!(
                                                "session: cannot capture display {id}: {e:#}; \
                                                 staying on {}",
                                                previous.id()
                                            );
                                            // Putting the old stream back is not
                                            // optional: it was torn down to make
                                            // room, and leaving it down would
                                            // freeze the desktop with no way back
                                            // short of a reconnect.
                                            match switch_capture(
                                                previous,
                                                &full_repaint,
                                                &mut capture,
                                                &mut out_rx,
                                                &mut encoder_thread,
                                            ) {
                                                Ok(live) => live,
                                                Err(e) => {
                                                    let message = format!(
                                                        "lost the capture stream while switching \
                                                         display: {e}"
                                                    );
                                                    warn!("session: {message}");
                                                    writer
                                                        .send(&AgentMsg::Error { message }.encode())
                                                        .await?;
                                                    break Ok(());
                                                }
                                            }
                                        }
                                    };
                                    // Ordered exactly as a capture restart is: the
                                    // size before any tile drawn at it, and input
                                    // conversion re-derived before the first click
                                    // on the new display arrives.
                                    injector = Injector::new(live.scale, live.origin);
                                    cursor_tracker.set_scale(live.scale);
                                    cursor_seen = cursor::UNSEEN;
                                    full_repaint.store(true, Ordering::Relaxed);
                                    writer.send(&AgentMsg::DisplaySize {
                                        w: live.width,
                                        h: live.height,
                                        scale: wire_scale(live.scale),
                                    }.encode()).await?;
                                }
                                displays_sent = display_list(owned).await;
                            }
                            // Already there, or refused above. Either way the list
                            // still goes back, so a checkmark that moved
                            // optimistically lands on what is actually active.
                            _ => {}
                        }
                        writer.send(&AgentMsg::Displays {
                            active: target.id(),
                            displays: displays_sent.clone(),
                        }.encode()).await?;
                    }
                    GatewayMsg::Refresh => {
                        full_repaint.store(true, Ordering::Relaxed);
                        if let Some(capture) = &capture {
                            capture.request_full_repaint();
                        }
                        cursor_seen = cursor::UNSEEN;
                    }
                    GatewayMsg::PointerMove { x, y } => injector.pointer_move(x, y),
                    GatewayMsg::PointerButton { button, pressed } => {
                        injector.pointer_button(button, pressed);
                    }
                    GatewayMsg::Wheel { dx, dy } => injector.wheel(dx, dy),
                    GatewayMsg::Key { code, pressed, caps } => {
                        injector.key(&code, pressed, caps);
                    }
                    GatewayMsg::Ping { nonce } => {
                        writer.send(&AgentMsg::Pong { nonce }.encode()).await?;
                    }
                    // The gateway only asks when the browser presses Fetch, so
                    // this is one read per click — see [`crate::pasteboard`].
                    // An empty reply covers both "nothing copied" and "the
                    // pasteboard holds an image": the browser wants text.
                    GatewayMsg::ClipboardRequest => {
                        let text = pasteboard::read().unwrap_or_default();
                        writer
                            .send(&clipboard_msg(text, clipboard_changed_at_ms, true).encode())
                            .await?;
                    }
                    GatewayMsg::Clipboard { text } => {
                        // A refused write still cleared the pasteboard, so the
                        // counter moved either way — but only re-baseline when
                        // the text actually landed. Leaving the stale baseline
                        // after a refusal lets the watcher notice on its next
                        // tick and tell the browser the pasteboard is now
                        // empty, which beats silently pretending the paste
                        // worked. `pasteboard::write` logs the refusal; there
                        // is no negative acknowledgement on this wire, and
                        // AgentMsg::Error is fatal at the gateway — far too
                        // blunt for one lost paste.
                        // Oversized text is refused outright rather than written
                        // in part: a partial paste on the Mac would look like the
                        // whole thing. Every layer above refuses it too, so this
                        // is the last line rather than the expected one.
                        let wrote = if clipboard_fits(&text) {
                            pasteboard::write(&text)
                        } else {
                            warn!(
                                "session: refusing {} bytes to the pasteboard, over the {} byte limit",
                                text.len(),
                                MAX_CLIPBOARD_BYTES
                            );
                            false
                        };
                        // Our own write bumps the counter. Without this the
                        // watcher would read it straight back and push it to
                        // the browser that just sent it.
                        if wrote && clipboard_seen.is_some() {
                            clipboard_seen = Some(pasteboard::change_count());
                            clipboard_changed_at_ms =
                                Some(next_clipboard_time(clipboard_changed_at_ms));
                        }
                    }
                    // Baseline the counter without reading anything: the first
                    // push should be the user's next copy, not whatever
                    // happened to be on the pasteboard when the browser
                    // connected. That also matches VNC, where ServerCutText
                    // only arrives on a change.
                    GatewayMsg::ClipboardWatch { enabled } => {
                        clipboard_seen = enabled.then(pasteboard::change_count);
                        clipboard_changed_at_ms = None;
                        info!(
                            "session: pasteboard watch {}",
                            if enabled { "on" } else { "off" }
                        );
                        if enabled && let Some(warning) = pasteboard::access_warning() {
                            warn!("session: {warning}");
                        }
                    }
                }
            }

            Some(live) = displays_rx.recv() => {
                probing_displays = false;
                // Timed from the answer rather than from the question, so a slow
                // WindowServer spaces the probes out instead of queueing them.
                displays_polled = Instant::now();
                if live != displays_sent {
                    info!("session: the display list changed ({} attached)", live.len());
                    displays_sent = live;
                    writer.send(&AgentMsg::Displays {
                        active: target.id(),
                        displays: displays_sent.clone(),
                    }.encode()).await?;
                }
            }

            _ = cursor_tick.tick() => {
                // A display that changed mode does not resize the capture
                // surface on its own — see `Capture::follow_display`, which
                // explains why this is a poll rather than an event. Riding the
                // cursor tick like the pasteboard check does: comparing two
                // integers against the display's current mode is far cheaper
                // than the cursor poll already on this tick, and a resize the
                // browser asked for should not wait on a slower timer.
                if let Some(capture) = capture.as_mut() {
                    match capture.follow_display() {
                        Ok(Some(live)) => {
                            // Input is converted with these, so they have to
                            // move with the surface — a stale scale or origin
                            // puts clicks in the wrong place. The browser is
                            // told by the frame that arrives next, which the
                            // resized surface is what produces.
                            injector = Injector::new(live.scale, live.origin);
                            cursor_tracker.set_scale(live.scale);
                            cursor_seen = cursor::UNSEEN;
                        }
                        Ok(None) => {}
                        Err(e) => warn!("session: {e:#}"),
                    }
                }
                if last_heard.elapsed() > GATEWAY_IDLE_TIMEOUT {
                    break Err(anyhow::anyhow!(
                        "no traffic from the gateway for {}s",
                        last_heard.elapsed().as_secs()
                    ));
                }
                // Displays come and go while a session is up — a monitor plugged
                // in, a lid closed, the agent's own display appearing a moment
                // after it was created. On its own slower schedule, because
                // listing them is a round trip to a system service where
                // everything else on this tick is a local read — and started
                // rather than awaited, so those 68 ms are not taken out of the
                // tile path. The answer arrives on the branch below.
                if !probing_displays && displays_polled.elapsed() >= DISPLAY_POLL {
                    probing_displays = true;
                    spawn_display_probe(owned, displays_tx.clone());
                }
                if let Some((generation, shape)) = cursor_tracker.changed_since(cursor_seen) {
                    cursor_seen = generation;
                    writer.send(&AgentMsg::Cursor(shape).encode()).await?;
                }
                // Riding the cursor tick rather than a timer of its own: both
                // want the same "has anything changed" cadence, and a counter
                // compare is far cheaper than the cursor poll already here.
                if let Some(seen) = clipboard_seen {
                    let now = pasteboard::change_count();
                    if now != seen {
                        clipboard_seen = Some(now);
                        let changed_at_ms = next_clipboard_time(clipboard_changed_at_ms);
                        clipboard_changed_at_ms = Some(changed_at_ms);
                        // The one content read, and only because the counter
                        // moved. An empty string covers a pasteboard holding
                        // an image or a file — the browser wants text.
                        let text = pasteboard::read().unwrap_or_default();
                        writer
                            .send(
                                &clipboard_msg(text, Some(changed_at_ms), false).encode(),
                            )
                            .await?;
                    }
                }
            }
        }
    };

    // A browser that vanished mid-chord must not leave Command or a mouse button
    // stuck down on the Mac — that is baffling and hard to clear from the
    // keyboard.
    injector.release_all();
    // The same obligation for the pointer, and for the same reason: a session that
    // shared the agent's own display has left the Mac's one pointer on a screen
    // nobody there can see. Releases first — the buttons belong to the position they
    // were pressed at, and a warp before them would let go somewhere else.
    if let Some(pointer_home) = pointer_home {
        pointer_home.restore();
    }
    // Stop capturing: the stream costs CPU and battery with nobody watching.
    drop(capture);
    // Before the join, and load-bearing: an encoder parked in `blocking_send` on
    // a full output channel only wakes when the receiver is gone. Joining first
    // would deadlock exactly in the case this teardown matters most — a browser
    // that vanished while behind on tiles. An encoder parked collecting a worker
    // instead is not a second hazard: an encode is bounded work and completes.
    drop(out_rx);
    if let Some(thread) = encoder_thread {
        // The encoder exits once its channel closes with the sink, or once this
        // dropped receiver fails its send.
        let _ = thread.join();
    }
    result
}

/// One pasteboard read as a message for the gateway.
///
/// Text over [`MAX_CLIPBOARD_BYTES`] is reported as its size instead of being
/// truncated to fit: the browser can then say the Mac's clipboard is too large,
/// where a truncated paste would arrive looking like all of it.
fn clipboard_msg(text: String, changed_at_ms: Option<u64>, requested: bool) -> AgentMsg {
    if clipboard_fits(&text) {
        debug!("session: pasteboard read, {} bytes", text.len());
        return AgentMsg::Clipboard {
            text,
            changed_at_ms,
            requested,
            oversized_bytes: None,
        };
    }
    debug!(
        "session: pasteboard holds {} bytes, over the {MAX_CLIPBOARD_BYTES} byte limit",
        text.len()
    );
    AgentMsg::Clipboard {
        oversized_bytes: Some(text.len() as u64),
        text: String::new(),
        changed_at_ms,
        requested,
    }
}

/// Wire up capture → encoder → pump for an attached session.
type Pipeline = (Capture, mpsc::Receiver<Out>, std::thread::JoinHandle<()>);

fn start_pipeline(
    target: capture::Target,
    full_repaint: Arc<AtomicBool>,
) -> anyhow::Result<Pipeline> {
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel(RAW_BACKLOG);
    let (out_tx, out_rx) = mpsc::channel(OUT_BACKLOG);

    let sink = Arc::new(Sink {
        tx: raw_tx,
        full_repaint: Arc::clone(&full_repaint),
    });
    let capture = Capture::start(target, sink, full_repaint)?;

    // The encoder is a plain thread but reaches the runtime for its workers, so it
    // needs a handle to the one this session is running on. Safe to take here
    // because every caller — `Attach`, a capture restart, `switch_capture` — is
    // inside `pump`; `Handle::current` panics loudly rather than quietly if that
    // ever stops being true.
    let handle = tokio::runtime::Handle::current();

    let thread = std::thread::Builder::new()
        .name("rxa-encoder".to_owned())
        .spawn(move || encode_loop(&handle, raw_rx, out_tx))?;

    Ok((capture, out_rx, thread))
}

/// What one pipeline's encoder cost, logged as it ends.
///
/// The repo has no benchmark harness, and until this existed nothing in the agent
/// measured the encoder at all. Like `wire::Totals` on the gateway's browser link,
/// this line *is* the measurement. Each number answers something the others cannot:
///
/// - `frames` against `tiles` is the batch size [`ENCODE_WIDTH`] has to cover. A
///   width above a frame's cell count buys nothing.
/// - `encode` (summed across workers) against `waiting` (wall clock) is the
///   parallelism actually achieved: equal means serial, `encode` well above
///   `waiting` means cells really did overlap.
/// - **`stalled` is the one that decides whether any of this helps.** It is time the
///   encoder spent blocked handing tiles to the pump, so if it dominates then the
///   socket is the constraint and widening the encoder cannot make a repaint faster.
/// - `lossy` is the classifier's split, which nothing downstream can observe because
///   both branches are WebP. It was a per-frame `debug!` and is now also a total.
/// - `dropped` counts tiles a failed encode discarded. Each one was already a
///   `warn!`, but a count is what says whether it happened once or constantly.
/// - `bytes` cross-checks against the gateway's own `ws: outbound totals`.
#[derive(Default)]
struct EncodeTotals {
    frames: u64,
    tiles: u64,
    lossy: u64,
    dropped: u64,
    bytes: u64,
    encode_micros: u64,
    waited_micros: u64,
    stalled_micros: u64,
}

impl std::fmt::Display for EncodeTotals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} frame(s) / {} tile(s) ({} photographic, {} dropped) / {} bytes, \
             {}µs encoding across workers in {}µs of waiting, stalled {}µs",
            self.frames,
            self.tiles,
            self.lossy,
            self.dropped,
            self.bytes,
            self.encode_micros,
            self.waited_micros,
            self.stalled_micros
        )
    }
}

fn micros(since: std::time::Instant) -> u64 {
    since.elapsed().as_micros() as u64
}

/// Encode raw tiles and forward them, until either end of the pipeline closes.
///
/// One exit, so the totals cannot be lost down the path where the pump has gone —
/// which is the interesting one, because that is where `stalled` is largest.
fn encode_loop(
    handle: &tokio::runtime::Handle,
    rx: std::sync::mpsc::Receiver<Captured>,
    tx: mpsc::Sender<Out>,
) {
    let mut totals = EncodeTotals::default();
    let mut ended = "capture stream closed";

    while let Ok(msg) = rx.recv() {
        let alive = match msg {
            Captured::Resized(w, h) => send(&tx, Out::Resized(w, h), &mut totals),
            Captured::Failed(message) => send(&tx, Out::Failed(message), &mut totals),
            Captured::Tiles(tiles) => encode_batch(handle, tiles, &tx, &mut totals),
        };
        if !alive {
            ended = "the pump is gone";
            break;
        }
    }

    debug!("encoder: {ended}");
    if totals.tiles > 0 || totals.dropped > 0 {
        info!("encoder: encode totals: {totals}");
    }
}

/// Compress one frame's cells and forward them in the order capture produced them.
///
/// Returns `false` once the pump has gone, which is this loop's only reason to stop.
///
/// [`ENCODE_WIDTH`] cells are kept in flight and the oldest is always the one
/// collected, so the order out is the order in *whatever order the encodes finish*.
/// That is the whole ordering argument: it is a property of the queue rather than
/// something the code has to be careful about.
fn encode_batch(
    handle: &tokio::runtime::Handle,
    tiles: Vec<RawTile>,
    tx: &mpsc::Sender<Out>,
    totals: &mut EncodeTotals,
) -> bool {
    encode_batch_at(handle, ENCODE_WIDTH, tiles, tx, totals)
}

/// [`encode_batch`] with the width given rather than taken from the constant, which
/// is how `encode_width_sweep` measures what the constant should be.
fn encode_batch_at(
    handle: &tokio::runtime::Handle,
    width: usize,
    tiles: Vec<RawTile>,
    tx: &mpsc::Sender<Out>,
    totals: &mut EncodeTotals,
) -> bool {
    totals.frames += 1;
    // This frame's share of the running totals, for the per-frame line at the end.
    let before = (totals.tiles, totals.lossy);

    let mut cells = tiles.into_iter();
    let mut inflight = std::collections::VecDeque::with_capacity(width.max(1));
    loop {
        while inflight.len() < width.max(1) {
            let Some(cell) = cells.next() else { break };
            // The rect travels with the job so a failed encode can still be named,
            // and the encode's own cost comes back with it: summed across workers it
            // is the CPU this frame took, which is not the wall clock any more.
            inflight.push_back(handle.spawn_blocking(move || {
                let started = std::time::Instant::now();
                let encoded = encode::encode_tile(cell.rect.w, cell.rect.h, &cell.rgb);
                (cell.rect, encoded, micros(started))
            }));
        }
        let Some(job) = inflight.pop_front() else { break };

        let started = std::time::Instant::now();
        let joined = handle.block_on(job);
        totals.waited_micros += micros(started);

        let (rect, encoded, encode_micros) = match joined {
            Ok(finished) => finished,
            // Only reachable by cancellation, and only if the runtime is going away
            // under us — in which case the session is ending anyway.
            Err(e) => {
                warn!("encoder: a tile encode did not finish: {e}");
                totals.dropped += 1;
                continue;
            }
        };
        totals.encode_micros += encode_micros;

        let encoded = match encoded {
            Ok(encoded) => encoded,
            // One bad tile is a dropped rectangle, not a dead session; the next
            // repaint covers it. (The gateway cannot afford this — its shadow has
            // already recorded those pixels as sent.)
            Err(e) => {
                warn!("encoder: dropping a tile: {e:#}");
                totals.dropped += 1;
                continue;
            }
        };
        totals.tiles += 1;
        totals.lossy += u64::from(!encoded.lossless);
        totals.bytes += encoded.data.len() as u64;

        if !send(
            tx,
            Out::Tile {
                format: encoded.format,
                rect,
                data: encoded.data,
            },
            totals,
        ) {
            return false;
        }
    }

    // Counted per frame as well as in the totals: the classifier's split is the only
    // thing about it that can be judged from outside, and one line a frame says
    // whether it is finding what it should. Every payload is a WebP either way, so
    // nothing downstream can tell.
    let (tiles, lossy) = (totals.tiles - before.0, totals.lossy - before.1);
    if lossy > 0 {
        debug!("encoder: {tiles} tile(s), {lossy} photographic");
    }
    true
}

/// Hand one item to the pump, timing the wait. `false` means the pump has gone.
///
/// Blocking is the point: back-pressure from a slow browser reaches the raw channel,
/// which then coalesces frames rather than queueing them.
fn send(tx: &mpsc::Sender<Out>, item: Out, totals: &mut EncodeTotals) -> bool {
    let started = std::time::Instant::now();
    let delivered = tx.blocking_send(item).is_ok();
    // Sub-microsecond sends truncate to zero, so this counts only real waiting.
    totals.stalled_micros += micros(started);
    delivered
}

async fn read_loop(mut reader: FrameReader<OwnedReadHalf>, tx: mpsc::Sender<GatewayMsg>) {
    loop {
        let frame = match reader.recv().await {
            Ok(frame) => frame,
            Err(e) => {
                debug!("session: read ended: {e}");
                return;
            }
        };
        match GatewayMsg::decode(&frame) {
            Ok(msg) => {
                if tx.send(msg).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                warn!("session: undecodable message from the gateway: {e}");
                return;
            }
        }
    }
}

/// Aborts the reader task on drop, so no exit path leaks it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point size the owned target in these tests was created at.
    const BASE: (u32, u32) = (1600, 1000);
    const OURS: u32 = 6;

    fn owned() -> capture::Target {
        capture::Target::Owned {
            id: OURS,
            base_points: BASE,
        }
    }

    // The bug this function exists to prevent, and it was a live one: an id that
    // is *ours* has to come back as `Owned`, or its backing scale is read through
    // `CGDisplayCopyDisplayMode` — which returns nothing for a display we made,
    // and so reads 1x. The list then advertises a 2x display while capture takes
    // half the pixels.
    //
    // It bit on the display a session *starts* on rather than one it switches to,
    // because macOS made ours the main display and the start went straight to
    // `Target::Real(main_display())`. Both paths go through here now.
    #[test]
    fn our_own_display_resolves_to_the_owned_target_that_carries_its_size() {
        assert_eq!(resolve(OURS, Some(owned())), owned());
        // Every other id is one of the Mac's own, whether or not we made one.
        assert_eq!(resolve(1, Some(owned())), capture::Target::Real(1));
        assert_eq!(resolve(OURS, None), capture::Target::Real(OURS));
        assert_eq!(resolve(1, None), capture::Target::Real(1));
    }

    // `resolved` keeps `active` honest after a pipeline start: a real display
    // that has been unplugged falls back to the main one, so the id captured can
    // differ from the id asked for. An owned target never falls back.
    #[test]
    fn a_real_target_follows_the_display_capture_landed_on() {
        assert_eq!(
            capture::Target::Real(3).resolved(1),
            capture::Target::Real(1)
        );
        assert_eq!(owned().resolved(1), owned(), "ours does not fall back");
    }

    #[test]
    fn a_targets_id_is_the_display_it_names() {
        assert_eq!(capture::Target::Real(4).id(), 4);
        assert_eq!(owned().id(), OURS);
    }

    // ---- the encoder pipeline ----------------------------------------------
    //
    // These are plain `#[test]`s with a runtime built by hand rather than
    // `#[tokio::test]`, and that is not a style choice: `encode_batch` collects its
    // workers with `Handle::block_on`, which *panics* inside an asynchronous
    // context. The encoder is a `std::thread` in production for the same reason, so
    // a test has to stand outside the runtime too — which also lets it drain the
    // output with `blocking_recv`.

    /// A runtime to spawn encodes onto, from a thread that is not running on it.
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime for the encoder's workers")
    }

    fn cell(x: u16, y: u16, w: u16, h: u16) -> RawTile {
        RawTile {
            rect: capture::Rect { x, y, w, h },
            // Content is irrelevant to ordering; only the length has to match.
            rgb: vec![0x40; usize::from(w) * usize::from(h) * 3],
        }
    }

    /// Every rect that reached the pump, in the order it arrived.
    fn drain_rects(rx: &mut mpsc::Receiver<Out>) -> Vec<capture::Rect> {
        let mut rects = Vec::new();
        while let Ok(out) = rx.try_recv() {
            match out {
                Out::Tile { rect, .. } => rects.push(rect),
                other => panic!("expected a tile, got {}", label(&other)),
            }
        }
        rects
    }

    fn label(out: &Out) -> &'static str {
        match out {
            Out::Tile { .. } => "a tile",
            Out::Resized(..) => "a resize",
            Out::Failed(_) => "a failure",
        }
    }

    /// Cell widths descending, so they cost visibly different amounts to encode and a
    /// loop that forwarded whatever finished first would almost certainly interleave
    /// them. The assertion is on order alone, which holds however fast the machine is.
    ///
    /// Through `encode_batch_at` with the widths named here rather than through
    /// [`ENCODE_WIDTH`], and deliberately: at width 1 nothing overlaps, so a test
    /// that read the constant would silently stop proving anything the day it was
    /// tuned back down.
    #[test]
    fn a_frames_cells_are_forwarded_in_the_order_capture_produced_them() {
        let runtime = runtime();
        for width in [1usize, 2, 8, 64] {
            let (tx, mut rx) = mpsc::channel(256);
            let mut totals = EncodeTotals::default();

            let cells: Vec<RawTile> = (0..32u16)
                .map(|i| cell(0, i * 64, 320 - i * 8, 64))
                .collect();
            let expected: Vec<capture::Rect> = cells.iter().map(|cell| cell.rect).collect();

            assert!(encode_batch_at(runtime.handle(), width, cells, &tx, &mut totals));
            assert_eq!(
                drain_rects(&mut rx),
                expected,
                "cells left the encoder reordered at width {width}"
            );
            assert_eq!((totals.frames, totals.tiles, totals.dropped), (1, 32, 0));
        }
    }

    /// The hazard a side channel for control messages would create, and the one
    /// `tests/rxa_resize_e2e.rs` asserts end to end: the browser must learn a new
    /// size *before* a tile drawn in the new coordinate space arrives. Through the
    /// real loop and the real channel, since that ordering is the loop's to keep.
    #[test]
    fn a_resize_cannot_overtake_the_tiles_before_it() {
        let runtime = runtime();
        let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel(RAW_BACKLOG);
        let (out_tx, mut out_rx) = mpsc::channel(256);

        let handle = runtime.handle().clone();
        let encoder = std::thread::spawn(move || encode_loop(&handle, raw_rx, out_tx));

        raw_tx
            .send(Captured::Tiles((0..8u16).map(|i| cell(0, i * 64, 320, 64)).collect()))
            .unwrap();
        raw_tx.send(Captured::Resized(640, 480)).unwrap();
        raw_tx.send(Captured::Tiles(vec![cell(0, 0, 64, 64)])).unwrap();
        drop(raw_tx);
        encoder.join().unwrap();

        let mut out = Vec::new();
        while let Ok(item) = out_rx.try_recv() {
            out.push(item);
        }
        assert_eq!(out.len(), 10, "{:?}", out.iter().map(label).collect::<Vec<_>>());
        for (i, item) in out.iter().take(8).enumerate() {
            match item {
                Out::Tile { rect, .. } => assert_eq!(rect.y, i as u16 * 64),
                other => panic!("expected a tile at {i}, got {}", label(other)),
            }
        }
        assert!(
            matches!(out[8], Out::Resized(640, 480)),
            "the resize did not keep its place"
        );
        assert!(matches!(out[9], Out::Tile { .. }));
    }

    /// One tile the encoder cannot compress is a dropped rectangle, not a dead
    /// session — the next repaint covers it. The opposite of the gateway, whose
    /// shadow has already recorded those pixels as sent by then.
    #[test]
    fn one_unencodable_cell_is_dropped_and_the_rest_keep_their_order() {
        let runtime = runtime();
        let (tx, mut rx) = mpsc::channel(64);
        let mut totals = EncodeTotals::default();

        let mut cells = vec![cell(0, 0, 320, 64), cell(0, 64, 320, 64), cell(0, 128, 320, 64)];
        // A payload one byte short of its geometry, which `encode_tile` rejects
        // rather than letting libwebp read past the buffer.
        cells[1].rgb.pop();

        // Width 4, so the good cells really are in flight beside the bad one.
        assert!(encode_batch_at(runtime.handle(), 4, cells, &tx, &mut totals));
        assert_eq!(
            drain_rects(&mut rx)
                .into_iter()
                .map(|rect| rect.y)
                .collect::<Vec<_>>(),
            vec![0, 128]
        );
        assert_eq!((totals.tiles, totals.dropped), (2, 1));
    }

    /// What picks [`ENCODE_WIDTH`]: how well the fan-out scales over a full
    /// repaint's worth of cells. Synthetic on purpose — the quantity is CPU-bound,
    /// so it needs no VM, no deploy and no real screen, and a measurement that can
    /// be re-run from the source beats one nobody can reproduce.
    ///
    /// Run it in **release**: an encode is several times slower in a debug build,
    /// which would flatter the fan-out by making the fixed costs disappear.
    ///
    /// ```sh
    /// cargo test -p rxa-agent --release --lib -- --ignored --nocapture encode_width
    /// ```
    #[test]
    #[ignore = "prints timings; run explicitly in release"]
    fn encode_width_sweep() {
        // A Retina 1600x1000 desktop: 3200x2000 captured, cut by `split_cells` into
        // ten columns of 320 and 32 rows of 64 with the last row clipped to 16.
        let (cols, rows) = (10u16, 32u16);
        let runtime = runtime();

        for (label, fill) in [("flat  ", flat_cell as fn(u16, u16) -> Vec<u8>), ("photo ", photo_cell)] {
            println!("\n{label} {cols}x{rows} cells of a 3200x2000 full repaint");
            let mut serial = None;
            for width in [1usize, 2, 4, 8, 12] {
                let cells: Vec<RawTile> = (0..rows)
                    .flat_map(|row| {
                        (0..cols).map(move |col| {
                            let h = if row == rows - 1 { 16 } else { 64 };
                            RawTile {
                                rect: capture::Rect {
                                    x: col * 320,
                                    y: row * 64,
                                    w: 320,
                                    h,
                                },
                                rgb: fill(320, h),
                            }
                        })
                    })
                    .collect();
                let count = cells.len();

                // Wide enough that the drain never blocks: this measures encoding,
                // not the pump, and `stalled` in a live session measures the pump.
                let (tx, mut rx) = mpsc::channel(count + 1);
                let mut totals = EncodeTotals::default();
                let started = std::time::Instant::now();
                assert!(encode_batch_at(runtime.handle(), width, cells, &tx, &mut totals));
                let wall = started.elapsed();
                assert_eq!(drain_rects(&mut rx).len(), count);

                let speedup = serial.map_or(1.0, |first: std::time::Duration| {
                    first.as_secs_f64() / wall.as_secs_f64()
                });
                if serial.is_none() {
                    serial = Some(wall);
                }
                println!(
                    "  width {width:>2}: {count} cells in {wall:>9.2?} \
                     ({:>7.2?} encoding across workers, {speedup:.2}x)",
                    std::time::Duration::from_micros(totals.encode_micros)
                );
            }
        }
    }

    /// Flat UI: a couple of colours and hard edges, the lossless branch's content.
    fn flat_cell(w: u16, h: u16) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for y in 0..h {
            for x in 0..w {
                if (y / 16) % 2 == 0 && (x / 7) % 3 == 0 {
                    rgb.extend_from_slice(&[20, 20, 24]);
                } else {
                    rgb.extend_from_slice(&[246, 246, 248]);
                }
            }
        }
        rgb
    }

    /// Continuous tone with real noise in it, so it reaches the lossy branch and
    /// prediction cannot win — the same fixture shape `encode.rs`'s tests use, and
    /// for the reason recorded there.
    fn photo_cell(w: u16, h: u16) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for y in 0..h {
            for x in 0..w {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let jitter = ((state >> 56) as i32 - 128) / 5;
                for channel in [
                    u32::from(x) * 7 + u32::from(y) * 3,
                    u32::from(x) * 3 + u32::from(y) * 11 + 40,
                    u32::from(x) * 13 + u32::from(y) * 5 + 90,
                ] {
                    rgb.push(((channel as i32 % 256 + jitter).clamp(0, 255)) as u8);
                }
            }
        }
        rgb
    }
}
