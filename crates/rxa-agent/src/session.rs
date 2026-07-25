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
use rxa_proto::msg::{AgentMsg, GatewayMsg};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval};

use crate::capture::{self, Capture, FrameSink, RawTile};
use crate::cursor;
use crate::encode;
use crate::input::Injector;

/// Frames of raw tiles buffered between the capture callback and the encoder.
/// Small on purpose: see the coalescing note in the module docs.
const RAW_BACKLOG: usize = 2;

/// Encoded tiles buffered between the encoder and the socket.
const OUT_BACKLOG: usize = 64;

/// How often the pointer shape is compared against what this session last sent.
const CURSOR_POLL: Duration = Duration::from_millis(100);

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

    writer
        .send(
            &AgentMsg::Hello {
                version: rxa_proto::VERSION,
                agent_version: env!("CARGO_PKG_VERSION").to_owned(),
                w: geometry.width,
                h: geometry.height,
            }
            .encode(),
        )
        .await?;

    pump(reader, writer, geometry, display, cursor_tracker).await
}

async fn pump(
    reader: FrameReader<OwnedReadHalf>,
    mut writer: FrameWriter<OwnedWriteHalf>,
    geometry: capture::Geometry,
    display: usize,
    cursor_tracker: Arc<cursor::Tracker>,
) -> anyhow::Result<()> {
    // `FrameReader::recv` is not cancel-safe, so it gets its own task.
    let (gateway_tx, mut gateway_rx) = mpsc::channel(32);
    let read_task = tokio::spawn(read_loop(reader, gateway_tx));
    let _abort = AbortOnDrop(read_task);

    let mut injector = Injector::new(geometry.scale, geometry.origin);
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
                    }
                    Out::Failed(message) => {
                        warn!("session: capture failed: {message}");
                        writer.send(&AgentMsg::Error { message }.encode()).await?;
                        break Ok(());
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
                }
            }

            _ = cursor_tick.tick() => {
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
