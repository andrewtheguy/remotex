//! The seam between an engine's protocol-read loop and the WebP encoder.
//!
//! Every tile the RDP and VNC engines produce used to be encoded *inside* the loop
//! that reads the remote's protocol: while a frame compressed, no PDU was read and
//! no protocol response was written. At the encoder's measured cost (about 60µs
//! fixed plus ~17ns a pixel — see [`crate::protocol`]) a desktop fully in motion
//! spent tens of milliseconds a frame there, which was both a frame-rate ceiling
//! and an input-latency floor. This module moves that work off the loop.
//!
//! ```text
//! read loop:  pack → Shadow::accept → bands()
//!                                       │  one owned Vec<u8> per band
//!                                       ▼
//!             TileSink::tile() ── spawn_blocking(encode) ─┐
//!                                                         │ handle pushed, in order
//!             TileSink::msg()  ── ServerMsg ──────────────┤  mpsc, cap ENCODE_DEPTH
//!                                                         ▼
//!                                order task: await handles FIFO → frame_tx
//! ```
//!
//! What the shape is answering:
//!
//! - **Ordering is structural, not sequence-numbered.** The order task awaits
//!   `JoinHandle`s out of a FIFO, so what reaches `frame_tx` is in push order
//!   however the encodes finish. That is not a nicety: tiles overwrite their
//!   rectangles with no delta state, [`crate::wire`] drops a pending tile a later
//!   one covers and names slots in placement order, and both clients apply a
//!   frame's records strictly in arrival order. A reordering bug shows up as stale
//!   pixels, never as an error — so it is worth having by construction rather than
//!   by care.
//! - **Control messages share the queue.** A `Resize` travels the same channel as
//!   tiles rather than going straight to `frame_tx`, so it can never overtake the
//!   tiles it invalidates — the client must learn the new size *before* a tile in
//!   the new coordinate space arrives. The macOS agent's pipeline avoids the same
//!   hazard the same way (`crates/rxa-agent/src/session.rs`). This is why the
//!   engines no longer hold a `frame_tx` at all.
//! - **Back-pressure, not coalescing.** The agent drops a whole frame and asks for
//!   a coarser repaint when its encoder falls behind. The gateway cannot copy that:
//!   `Shadow::accept` has *already* recorded those pixels as sent by the time a
//!   band reaches here, so dropping one would need a `forget()` and a full repaint
//!   — worse than waiting, under exactly the sustained-motion case this is for. So
//!   a full queue blocks the engine, which lets the remote's own flow control slow
//!   down, precisely as a full `frame_tx` already did.
//! - **`spawn_blocking`, not a pool crate.** An engine runs on its own
//!   current-thread runtime (see [`crate::session`]) which still has a
//!   multi-threaded blocking pool, in-flight work is bounded by [`ENCODE_DEPTH`] so
//!   it cannot grow into that pool's 512-thread ceiling, and a `Tile` is already
//!   `Send` because `encode_webp` copies out of libwebp's non-`Send` buffer.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use log::{debug, info, warn};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::protocol::{ServerMsg, Tile};

/// How many encodes may be queued ahead of the one being collected.
///
/// This is the channel's capacity, so encodes in flight are at most this many plus
/// the one the order task is awaiting. It is the only parallelism dial: raising it
/// lets more bands of one frame compress at once, and a full queue is what stalls
/// the engine.
///
/// **16 is measured, not guessed** — `tests/rdp_repaint_probe.rs`, against a real
/// Windows desktop at 1280x800, forty full repaints of 13 bands each, two runs per
/// depth on a 12-core (8 performance) arm64 Mac:
///
/// | depth | repaint median | p90 | engine stalled, per repaint |
/// |---|---|---|---|
/// | 1 | 15.9, 12.3 ms | 19.1, 15.5 ms | 11.0, 9.3 ms |
/// | 8 | 7.6, 8.7 ms | 12.4, 11.3 ms | 2.1, 1.9 ms |
/// | 16 | 8.1, 8.1 ms | 11.2, 11.5 ms | 0.004, 0.008 ms |
/// | 32 | 7.4, 6.5 ms | 14.0, 8.5 ms | 0.005, 0.003 ms |
///
/// The dial does two separate things, and the second is the one this module is for.
/// It roughly halves a repaint, which is the frame rate. But it takes *all* of what
/// the engine's own read loop waits for the encoder, which is input latency and
/// protocol throughput — the thing that was actually wrong.
///
/// **The number that matters is one frame's worth of bands.** At 13 bands a depth of
/// 16 means a whole repaint is in flight and the engine never waits at all, and that
/// is exactly where the stall column falls off a cliff — 8 still leaves 2ms a repaint
/// because five bands have to queue behind the rest. Wall clock stops improving at the
/// same point, so 32 buys nothing but memory (one band buffer each, ~250KB at this
/// width) and, in one of its two runs, a worse tail.
///
/// It does not cover every desktop: 1080p is 17 bands and 1512p is 24, so a taller
/// screen keeps a little stall. That degrades gracefully, which is why this is a plain
/// constant — a measurement should be reproducible from the source, and
/// `available_parallelism` would make one build behave differently on the dev Mac and
/// the deploy host. A host with fewer cores does not want a smaller number either:
/// over-committing costs interleaving, not correctness, and the order task waits on
/// the first band regardless.
///
/// Byte counts are *not* comparable between runs of that probe — a live Windows
/// desktop draws its own clock — but the 520 records of the repaints themselves are,
/// and were identical at every depth. Earlier 20-repaint runs threw the occasional
/// 200ms+ repaint at several depths; at 40 they stopped appearing, so they were the
/// desktop and not the encoder.
const ENCODE_DEPTH: usize = 16;

/// One item in the ordered queue.
///
/// A tile arrives as the *handle* to work already running, which is what buys the
/// parallelism: the engine pushed it and moved on. Everything else is already
/// finished and only needs its place in the order kept.
enum Pending {
    /// An encode in flight, yielding the tile and the microseconds it cost.
    Tile(JoinHandle<anyhow::Result<(Tile, u64)>>),
    Msg(ServerMsg),
    /// A caller waiting for everything pushed before it to have reached `frame_tx`.
    Flush(oneshot::Sender<()>),
}

/// State the sink and its order task both touch.
///
/// The counters live here rather than in the order task because the engine is what
/// reports them: an engine's `run` returning drops the thread's whole runtime, so a
/// line the task logged on its own way out would be cancelled before it printed.
#[derive(Default)]
struct Shared {
    /// Why the order task gave up, so the engine's next push can report it rather
    /// than a bare closed channel. See [`TileSink::closed`].
    failure: Mutex<Option<String>>,
    tiles: AtomicU64,
    encoded_bytes: AtomicU64,
    /// Summed across workers, so it may exceed the wall clock.
    encode_micros: AtomicU64,
    /// Wall time the order task spent waiting for encodes to finish.
    waited_micros: AtomicU64,
    /// Time the engine spent blocked pushing into a full queue.
    stalled_micros: AtomicU64,
}

/// The engine's handle on the encoder.
///
/// `Clone` because the VNC engine drives its read loop as a separate task while
/// its input side keeps sending control messages; both push into the same queue,
/// and only the read loop pushes tiles.
#[derive(Clone)]
pub struct TileSink {
    engine: &'static str,
    tx: mpsc::Sender<Pending>,
    shared: Arc<Shared>,
}

impl TileSink {
    /// Start an encoder for one engine. `engine` prefixes its log lines.
    pub fn new(engine: &'static str, frame_tx: mpsc::Sender<ServerMsg>) -> Self {
        let (tx, rx) = mpsc::channel(ENCODE_DEPTH);
        let shared = Arc::new(Shared::default());
        tokio::spawn(order_loop(engine, rx, frame_tx, Arc::clone(&shared)));
        Self { engine, tx, shared }
    }

    /// Queue a band of packed RGB888 for encoding.
    ///
    /// `rgb` is owned rather than borrowed because the engine's framebuffer is
    /// gone by the time a worker reads it — the read loop has moved on to the next
    /// PDU. The encode starts immediately; only its *place in the order* is queued.
    pub async fn tile(&self, x: u16, y: u16, w: u16, h: u16, rgb: Vec<u8>) -> anyhow::Result<()> {
        let handle = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let tile = Tile::from_rgb(x, y, w, h, &rgb)?;
            Ok((tile, micros(started)))
        });
        self.push(Pending::Tile(handle)).await
    }

    /// Queue anything that is not a tile, keeping it behind the tiles it follows.
    pub async fn msg(&self, msg: ServerMsg) -> anyhow::Result<()> {
        self.push(Pending::Msg(msg)).await
    }

    /// Shut the encoder down: deliver what is still in flight, then log what it cost.
    ///
    /// The one thing an engine's `run` must do before returning, and one call rather
    /// than two because the order cannot be got wrong this way. Both halves matter and
    /// for different reasons: the runtime is dropped when `run` returns, so anything
    /// the order task still held would be cancelled — including the `ServerMsg::Error`
    /// that explains why the session ended, leaving the browser on the picker with
    /// nothing to show — and the totals are only complete once it has stopped adding
    /// to them.
    pub async fn finish(&self) {
        self.flush().await;
        self.report();
    }

    /// Wait until everything pushed so far has reached the frame channel.
    ///
    /// Returns early if the order task is already gone; there is then nothing left to
    /// wait for. Shutdown wants [`Self::finish`] instead; this is for a caller that
    /// needs to *read* what it pushed, which in practice means a test.
    pub async fn flush(&self) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(Pending::Flush(ack_tx)).await.is_ok() {
            let _ = ack_rx.await;
        }
    }

    async fn push(&self, item: Pending) -> anyhow::Result<()> {
        let started = Instant::now();
        let result = self.tx.send(item).await;
        // Sub-microsecond sends truncate to zero, so this counts only real waiting.
        self.shared
            .stalled_micros
            .fetch_add(micros(started), Ordering::Relaxed);
        result.map_err(|_| self.closed())
    }

    /// Log what this engine's encoder cost. Private: [`Self::finish`] is the only
    /// caller, so it cannot be run before the flush that completes the numbers.
    ///
    /// Explicit rather than emitted by the order task on its way out, for the reason
    /// [`Shared`] gives. Silent for an engine that never encoded anything: `rxa`
    /// relays tiles the agent already compressed, and a line of zeroes says nothing.
    fn report(&self) {
        let totals = Totals::of(&self.shared);
        if totals.tiles > 0 {
            info!("{}: encode totals: {totals}", self.engine);
        }
    }

    /// The error a push reports once the order task has stopped.
    ///
    /// An encode failure used to `?` straight out of the engine's own loop and end
    /// the session, which is what stops the shadow from believing the client holds
    /// pixels that were never sent. Deferred, the failure lands in the order task
    /// instead, so it is recorded there and surfaces here on the next push — one
    /// message later than before, with the same outcome.
    fn closed(&self) -> anyhow::Error {
        match self.shared.failure.lock().unwrap().take() {
            Some(message) => anyhow::anyhow!(message),
            None => anyhow::anyhow!("frame channel closed"),
        }
    }
}

/// Collect finished encodes in push order and forward them.
async fn order_loop(
    engine: &'static str,
    mut rx: mpsc::Receiver<Pending>,
    frame_tx: mpsc::Sender<ServerMsg>,
    shared: Arc<Shared>,
) {
    while let Some(item) = rx.recv().await {
        let msg = match item {
            Pending::Msg(msg) => msg,
            Pending::Flush(ack) => {
                // Everything before this is already through, which is the whole
                // claim; the ack costs nothing and asks for no ordering of its own.
                let _ = ack.send(());
                continue;
            }
            Pending::Tile(handle) => {
                let started = Instant::now();
                let joined = handle.await;
                shared.waited_micros.fetch_add(micros(started), Ordering::Relaxed);
                match joined {
                    Ok(Ok((tile, encode_micros))) => {
                        shared.tiles.fetch_add(1, Ordering::Relaxed);
                        shared
                            .encoded_bytes
                            .fetch_add(tile.data.len() as u64, Ordering::Relaxed);
                        shared.encode_micros.fetch_add(encode_micros, Ordering::Relaxed);
                        debug!(
                            "{engine}: tile {}x{} at ({},{}): {} -> {} bytes",
                            tile.w,
                            tile.h,
                            tile.x,
                            tile.y,
                            usize::from(tile.w) * usize::from(tile.h) * 3,
                            tile.data.len()
                        );
                        ServerMsg::Tile(tile)
                    }
                    // Ends the session, as encoding on the read loop did: the
                    // shadow already counts those pixels as delivered.
                    Ok(Err(e)) => {
                        give_up(engine, &shared, format!("tile encode failed: {e}"));
                        break;
                    }
                    // Only reachable by cancellation — `panic = "abort"` in release
                    // means a panicking worker never gets this far.
                    Err(e) => {
                        give_up(engine, &shared, format!("tile encoder stopped: {e}"));
                        break;
                    }
                }
            }
        };
        if frame_tx.send(msg).await.is_err() {
            break; // browser gone; the engine learns it from its own next push
        }
    }
}

/// Record why the queue stopped, for the engine's next push to report.
fn give_up(engine: &str, shared: &Shared, message: String) {
    warn!("{engine}: {message}");
    *shared.failure.lock().unwrap() = Some(message);
}

fn micros(since: Instant) -> u64 {
    since.elapsed().as_micros() as u64
}

/// A snapshot of what one engine's encoder cost, for [`TileSink::report`].
///
/// The repo has no benchmark harness, so — like `wire::Totals` for the browser
/// link — this line is the only measurement of the encoder that exists in
/// production. Each number earns its place by answering something the others
/// cannot:
///
/// - `encode` against `waiting` says whether bands overlapped, and it is **not** the
///   parallelism achieved — read as that it flatters itself. `waiting` accrues only
///   while the order task finds a handle *unfinished*, so a band already encoded when
///   its turn came adds encode time and no waiting at all. The ratio is an upper bound
///   on the concurrency and can exceed [`ENCODE_DEPTH`] outright. Its low end is the
///   sound part: 1.0 means every collect blocked for the whole of its band, so nothing
///   overlapped.
/// - `stalled` is what the read loop still pays. It is the number that says whether
///   [`ENCODE_DEPTH`] is the binding constraint: zero means the engine never waited
///   for the encoder at all, which is the point of the whole module.
/// - `bytes` cross-checks against the `ws: outbound totals` line, and must not move
///   when the depth does — the same pixels are encoded either way.
struct Totals {
    tiles: u64,
    encoded_bytes: u64,
    encode_micros: u64,
    waited_micros: u64,
    stalled_micros: u64,
}

impl Totals {
    fn of(shared: &Shared) -> Self {
        // Relaxed throughout, and read while the order task may still be running:
        // this is a log line, not a decision, and the counters only ever grow.
        Self {
            tiles: shared.tiles.load(Ordering::Relaxed),
            encoded_bytes: shared.encoded_bytes.load(Ordering::Relaxed),
            encode_micros: shared.encode_micros.load(Ordering::Relaxed),
            waited_micros: shared.waited_micros.load(Ordering::Relaxed),
            stalled_micros: shared.stalled_micros.load(Ordering::Relaxed),
        }
    }
}

impl fmt::Display for Totals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} tile(s) / {} bytes, {}µs encoding across workers in {}µs of waiting, \
             engine stalled {}µs",
            self.tiles,
            self.encoded_bytes,
            self.encode_micros,
            self.waited_micros,
            self.stalled_micros
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::UNSCALED;

    /// Packed RGB888 for a `w`x`h` band, filled so no two bands share bytes.
    fn rgb(w: u16, h: u16, seed: u8) -> Vec<u8> {
        (0..usize::from(w) * usize::from(h) * 3)
            .map(|i| seed.wrapping_add((i % 251) as u8))
            .collect()
    }

    /// The assertion every test here makes: what came out, in the order it came.
    async fn drain(rx: &mut mpsc::Receiver<ServerMsg>, count: usize) -> Vec<ServerMsg> {
        let mut out = Vec::new();
        for _ in 0..count {
            out.push(rx.recv().await.expect("frame channel closed early"));
        }
        out
    }

    /// Sizes deliberately unequal and descending, so a sink that forwarded
    /// whatever finished first would almost certainly interleave them. The
    /// assertion is on order alone, which holds however fast the machine is.
    #[tokio::test]
    async fn tiles_reach_the_frame_channel_in_push_order() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx);

        for i in 0..64u16 {
            let (w, h) = (320 - i * 4, 64);
            sink.tile(0, i * 64, w, h, rgb(w, h, i as u8)).await.unwrap();
        }
        sink.flush().await;

        for (i, msg) in drain(&mut frame_rx, 64).await.into_iter().enumerate() {
            let ServerMsg::Tile(tile) = msg else {
                panic!("expected a tile at {i}");
            };
            assert_eq!(tile.y, i as u16 * 64, "tiles left the sink out of order");
            assert_eq!(tile.format, Tile::FORMAT_WEBP);
        }
    }

    /// The hazard a side channel for control messages would create: the client
    /// must learn a new size before a tile in the new coordinate space arrives.
    #[tokio::test]
    async fn a_control_message_cannot_overtake_the_tiles_before_it() {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx);

        for i in 0..8u16 {
            sink.tile(0, i * 64, 320, 64, rgb(320, 64, i as u8)).await.unwrap();
        }
        sink.msg(ServerMsg::Resize { w: 640, h: 480, scale: UNSCALED }).await.unwrap();
        sink.tile(0, 0, 16, 16, rgb(16, 16, 9)).await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 10).await;
        for (i, msg) in out.iter().take(8).enumerate() {
            let ServerMsg::Tile(tile) = msg else {
                panic!("expected a tile at {i}");
            };
            assert_eq!(tile.y, i as u16 * 64);
        }
        assert!(
            matches!(out[8], ServerMsg::Resize { w: 640, .. }),
            "the resize did not keep its place"
        );
        assert!(matches!(out[9], ServerMsg::Tile(_)));
    }

    #[tokio::test]
    async fn flush_waits_for_everything_pushed_before_it() {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx);

        for i in 0..16u16 {
            sink.tile(0, i * 64, 320, 64, rgb(320, 64, i as u8)).await.unwrap();
        }
        sink.flush().await;

        // Everything is already queued on the frame channel, so this is a
        // synchronous drain rather than a wait — which is the claim.
        for i in 0..16u16 {
            match frame_rx.try_recv().expect("flush returned with tiles still in flight") {
                ServerMsg::Tile(tile) => assert_eq!(tile.y, i * 64),
                other => panic!("expected a tile, got {other:?}"),
            }
        }
    }

    /// A failing encode ends the session, as it did when it `?`-ed out of the
    /// engine's own loop — and it says why, rather than reading as a closed channel.
    #[tokio::test]
    async fn an_encode_failure_stops_the_sink_and_reports_itself() {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx);

        // A payload one byte short of the geometry: `Tile::from_rgb` rejects it
        // rather than letting libwebp panic on a buffer it would read past.
        let short = rgb(32, 32, 0)[1..].to_vec();
        // Accepted: the queue takes work without running it, so the engine's own
        // push cannot be what reports this.
        sink.tile(0, 0, 32, 32, short).await.unwrap();

        // A flush is how a test reaches the failure at any `ENCODE_DEPTH`: it makes
        // the order task get as far as the bad tile, and returns when the task drops
        // the ack rather than answering it.
        sink.flush().await;
        let error = sink
            .msg(ServerMsg::RemoteOs { macos: false })
            .await
            .expect_err("the sink kept accepting work after a failed encode");
        let text = format!("{error:#}");
        assert!(text.contains("tile encode failed"), "{text}");
        assert!(text.contains("expected 3072"), "the cause is lost: {text}");
        assert!(frame_rx.recv().await.is_none(), "nothing follows a failed encode");
    }

    /// Not an error: a browser that leaves mid-frame ends the sink the same way a
    /// full engine teardown does, and the engine hears about it on its next push.
    #[tokio::test]
    async fn a_dropped_frame_channel_is_reported_as_a_closed_channel() {
        let (frame_tx, frame_rx) = mpsc::channel(1);
        let sink = TileSink::new("test", frame_tx);
        drop(frame_rx);

        sink.tile(0, 0, 320, 64, rgb(320, 64, 0)).await.unwrap();
        sink.flush().await; // the order task discovers it has nowhere to forward to
        let error = sink
            .msg(ServerMsg::RemoteOs { macos: false })
            .await
            .expect_err("the sink accepted work with nowhere to put it");
        // No encode failed, so this is the plain closed-channel message the engines
        // reported before the sink existed.
        assert_eq!(format!("{error}"), "frame channel closed");
    }
}
