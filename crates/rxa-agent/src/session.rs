//! One gateway connection, from handshake to hangup.
//!
//! ## The pipeline
//!
//! ```text
//! SCStream callback ──▶ raw tile channel ──▶ encoder thread ──▶ out channel ──▶ pump ──▶ socket
//!   (dispatch queue)      (bounded, sync)     (std::thread)      (bounded)      (tokio)
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
//! - **One encoder thread, not a pool.** A pool would let two frames' tiles
//!   finish out of order, and the same region *is* commonly dirty in consecutive
//!   frames — so an older tile could land on top of a newer one and leave stale
//!   pixels on screen until something else redraws them. Ordering is worth more
//!   than the parallelism until measurement says otherwise; the fallback ladder
//!   in docs/roadmap.md starts with downscaling instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use log::{debug, info, warn};
use rxa_proto::frame::{FrameReader, FrameWriter};
use rxa_proto::msg::{AgentMsg, GatewayMsg, MAX_CLIPBOARD_BYTES, clipboard_fits};
use rxa_proto::next_clipboard_time;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval};

use crate::capture::{self, Capture, FrameSink, RawTile};
use crate::cursor;
use crate::displaymode;
use crate::encode;
use crate::input::Injector;
use crate::pasteboard;

/// Frames of raw tiles buffered between the capture callback and the encoder.
/// Small on purpose: see the coalescing note in the module docs.
const RAW_BACKLOG: usize = 2;

/// Encoded tiles buffered between the encoder and the socket.
const OUT_BACKLOG: usize = 64;

/// How often the pointer shape is compared against what this session last sent.
const CURSOR_POLL: Duration = Duration::from_millis(100);

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

/// How long a mode switch may run before it is written off as wedged. Generous:
/// a healthy switch settles in about a second, and the only thing this bound
/// buys is a log line — the work itself cannot be cancelled.
const RESIZE_TIMEOUT_SECS: u64 = 10;

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

/// Serve one gateway connection until it hangs up or fails.
pub async fn serve(
    stream: TcpStream,
    psk: [u8; 32],
    display: usize,
    cursor_tracker: Arc<cursor::Tracker>,
) -> anyhow::Result<()> {
    let mut stream = stream;
    stream.set_nodelay(true).ok();

    // A wrong PSK fails here, before the agent has revealed anything at all.
    let transport = rxa_proto::noise::respond(&mut stream, &psk)
        .await
        .map_err(|e| anyhow::anyhow!("handshake: {e}"))?;
    let (read_half, write_half) = stream.into_split();
    let (reader, mut writer) = rxa_proto::frame::split(read_half, write_half, transport);

    // The size has to be known before `Attach`, so it is probed without starting
    // a stream. This is also where a missing Screen Recording grant surfaces.
    let geometry = match capture::probe(display) {
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

    // What `Hello` claims is decided once, from the display the session is about
    // to share. The *guard* inside the pump is re-decided whenever the display
    // being captured turns out to be a different one (see there): this value
    // only has to be right about the display the gateway was told about.
    let resizable = displaymode::resizable(geometry.id);

    writer
        .send(
            &AgentMsg::Hello {
                version: rxa_proto::VERSION,
                agent_version: env!("CARGO_PKG_VERSION").to_owned(),
                w: geometry.width,
                h: geometry.height,
                resizable,
            }
            .encode(),
        )
        .await?;

    pump(reader, writer, geometry, display, resizable, cursor_tracker).await
}

async fn pump(
    reader: FrameReader<OwnedReadHalf>,
    mut writer: FrameWriter<OwnedWriteHalf>,
    geometry: capture::Geometry,
    display: usize,
    mut resizable: bool,
    cursor_tracker: Arc<cursor::Tracker>,
) -> anyhow::Result<()> {
    // `FrameReader::recv` is not cancel-safe, so it gets its own task.
    let (gateway_tx, mut gateway_rx) = mpsc::channel(32);
    let read_task = tokio::spawn(read_loop(reader, gateway_tx));
    let _abort = AbortOnDrop(read_task);

    let mut injector = Injector::new(geometry.scale, geometry.origin);
    // The display the stream is on, re-read from the live geometry at `Attach`:
    // the config's index is resolved inside `capture`, and a mode switch must
    // address the display being captured rather than re-resolving the index.
    let mut display_id = geometry.id;
    // A resize the browser asks for is already in flight, if any. One at a time:
    // two concurrent CoreGraphics display configurations on the same display is
    // not something to find out about the hard way, and a wedged one would
    // otherwise let a click-happy browser pile up blocked threads.
    let mut resize_task: Option<tokio::task::JoinHandle<()>> = None;
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
                        info!("session: display reconfigured to {w}x{h}");
                        writer.send(&AgentMsg::DisplaySize { w, h }.encode()).await?;
                        // The mode list is regenerated around whatever size the
                        // host just pushed, so the browser's menu is stale from
                        // this moment — including after a resize this session
                        // asked for, which can move the list's ceiling.
                        if resizable {
                            send_modes(&mut writer, display_id).await?;
                        }
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
                            match start_pipeline(display, Arc::clone(&full_repaint)) {
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
                        injector = Injector::new(live.scale, live.origin);
                        cursor_tracker.set_scale(live.scale);
                        // A restart re-resolves the config's display index, which
                        // can land on a *different* display — the one that just
                        // died may be the one that went away. Re-decide the guard
                        // for whatever is being captured now: `displaymode::apply`
                        // trusts it, and it is the only thing between a browser
                        // and a physical monitor's mode.
                        if live.id != display_id {
                            resizable = displaymode::resizable(live.id);
                        }
                        display_id = live.id;
                        capture = Some(started);
                        out_rx = Some(rx);
                        encoder_thread = Some(thread);
                        // Unconditionally, unlike `Attach`: the browser's canvas
                        // is stale whatever the size came back as, and a full
                        // repaint is coming regardless.
                        writer.send(&AgentMsg::DisplaySize {
                            w: live.width,
                            h: live.height,
                        }.encode()).await?;
                        if resizable {
                            send_modes(&mut writer, display_id).await?;
                        }
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
                            match start_pipeline(display, Arc::clone(&full_repaint)) {
                                Ok((started, rx, thread)) => {
                                    // The running stream is authoritative: the
                                    // display can have changed mode between the
                                    // probe that fed `Hello` and this Attach, and
                                    // painting into a stale canvas size — or
                                    // dividing input by a stale scale — is worse
                                    // than a redundant DisplaySize.
                                    let live = started.geometry;
                                    if (live.width, live.height) != (geometry.width, geometry.height) {
                                        info!(
                                            "session: display changed since Hello, now {}x{}",
                                            live.width, live.height
                                        );
                                        writer.send(&AgentMsg::DisplaySize {
                                            w: live.width,
                                            h: live.height,
                                        }.encode()).await?;
                                    }
                                    injector = Injector::new(live.scale, live.origin);
                                    // Same reason as the restart path: the index
                                    // resolves at stream start, so the display
                                    // captured here need not be the one `Hello`
                                    // measured, and the guard has to follow it.
                                    if live.id != display_id {
                                        resizable = displaymode::resizable(live.id);
                                    }
                                    display_id = live.id;
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
                        // Sent on every Attach, not just the first: a browser
                        // that reattaches to a running session has no menu
                        // otherwise.
                        if resizable {
                            send_modes(&mut writer, display_id).await?;
                        }
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
                    // Never fatal when refused: AgentMsg::Error tears the
                    // session down at the gateway, which is far too blunt for a
                    // resize the agent simply will not do. A physical display
                    // lands here only if the gateway ignored `Hello`.
                    GatewayMsg::SetDisplaySize { w, h } => {
                        if !resizable {
                            warn!(
                                "session: refusing to resize display {display_id} to {w}x{h} — \
                                 not a virtual display"
                            );
                        } else if resize_task.as_ref().is_some_and(|t| !t.is_finished()) {
                            // One at a time. Picking three sizes in three
                            // seconds is an ordinary thing to do in a menu, and
                            // each switch takes about a second — overlapping
                            // them would have two display configurations open on
                            // one display. Dropping the newer request is right
                            // rather than queueing it: by the time the running
                            // one lands, the queued size is what the user has
                            // already changed their mind about, and the menu is
                            // still there to click again.
                            warn!(
                                "session: ignoring a resize to {w}x{h} — one is still in flight"
                            );
                        } else {
                            // On a blocking thread with a deadline:
                            // CGCompleteDisplayConfiguration can hang forever
                            // once a VM's display stack wedges, and this task
                            // also carries tiles, input and the keepalive.
                            // Nothing here awaits the answer — the capture
                            // stream reports the new size on its own — so a
                            // hung switch costs one thread, not the session.
                            let deadline = Duration::from_secs(RESIZE_TIMEOUT_SECS);
                            let id = display_id;
                            resize_task = Some(tokio::spawn(async move {
                                let switch = tokio::task::spawn_blocking(move || {
                                    displaymode::apply(id, w, h)
                                });
                                match tokio::time::timeout(deadline, switch).await {
                                    Ok(Ok(Ok(_))) => {}
                                    Ok(Ok(Err(e))) => {
                                        warn!("session: resize to {w}x{h} failed: {e:#}");
                                    }
                                    Ok(Err(e)) => warn!("session: resize task died: {e}"),
                                    // The task ends here even though the blocking
                                    // one behind it cannot be cancelled, so a
                                    // wedged switch blocks further requests for
                                    // the timeout and no longer.
                                    Err(_) => warn!(
                                        "session: resize to {w}x{h} is still running after \
                                         {RESIZE_TIMEOUT_SECS}s; the display stack may be wedged"
                                    ),
                                }
                            }));
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
    // Stop capturing: the stream costs CPU and battery with nobody watching.
    drop(capture);
    // Before the join, and load-bearing: an encoder parked in `blocking_send` on
    // a full output channel only wakes when the receiver is gone. Joining first
    // would deadlock exactly in the case this teardown matters most — a browser
    // that vanished while behind on tiles.
    drop(out_rx);
    if let Some(thread) = encoder_thread {
        // The encoder exits once its channel closes with the sink, or once this
        // dropped receiver fails its send.
        let _ = thread.join();
    }
    result
}

/// Send the display's current resolution menu.
///
/// Read fresh every time rather than cached: the list is regenerated whenever
/// the host resizes a virtual display, so the only correct list is the one read
/// at the moment it is sent.
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

async fn send_modes(
    writer: &mut FrameWriter<OwnedWriteHalf>,
    display_id: u32,
) -> anyhow::Result<()> {
    let modes: Vec<(u16, u16)> = displaymode::modes(display_id)
        .into_iter()
        .map(|m| (m.width, m.height))
        .collect();
    debug!("session: display {display_id} offers {} resolutions", modes.len());
    writer.send(&AgentMsg::DisplayModes { modes }.encode()).await?;
    Ok(())
}

/// Wire up capture → encoder → pump for an attached session.
type Pipeline = (Capture, mpsc::Receiver<Out>, std::thread::JoinHandle<()>);

fn start_pipeline(display: usize, full_repaint: Arc<AtomicBool>) -> anyhow::Result<Pipeline> {
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel(RAW_BACKLOG);
    let (out_tx, out_rx) = mpsc::channel(OUT_BACKLOG);

    let sink = Arc::new(Sink {
        tx: raw_tx,
        full_repaint: Arc::clone(&full_repaint),
    });
    let capture = Capture::start(display, sink, full_repaint)?;

    let thread = std::thread::Builder::new()
        .name("rxa-encoder".to_owned())
        .spawn(move || encode_loop(raw_rx, out_tx))?;

    Ok((capture, out_rx, thread))
}

/// Encode raw tiles and forward them, until either end of the pipeline closes.
fn encode_loop(rx: std::sync::mpsc::Receiver<Captured>, tx: mpsc::Sender<Out>) {
    while let Ok(msg) = rx.recv() {
        let out = match msg {
            Captured::Resized(w, h) => vec![Out::Resized(w, h)],
            Captured::Failed(message) => vec![Out::Failed(message)],
            Captured::Tiles(tiles) => tiles
                .into_iter()
                .filter_map(|tile| {
                    match encode::encode_tile(tile.rect.w, tile.rect.h, &tile.rgb) {
                        Ok(encoded) => Some(Out::Tile {
                            format: encoded.format,
                            rect: tile.rect,
                            data: encoded.data,
                        }),
                        Err(e) => {
                            // One bad tile is a dropped rectangle, not a dead
                            // session; the next repaint covers it.
                            warn!("encoder: dropping a tile: {e:#}");
                            None
                        }
                    }
                })
                .collect(),
        };
        for item in out {
            // Blocking is the point: back-pressure from a slow browser reaches
            // the raw channel, which then coalesces frames rather than queueing
            // them. `blocking_send` fails only once the pump is gone.
            if tx.blocking_send(item).is_err() {
                return;
            }
        }
    }
    debug!("encoder: capture stream closed");
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
