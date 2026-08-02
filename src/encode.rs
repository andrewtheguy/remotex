//! Ordered tile encoding outside the RDP and VNC protocol-read loops.
//!
//! ```text
//! read loop:  pack → Shadow::accept → TileSink::damage()
//!                                       │  bands, cut at the cell grid where it matters
//!                                       ▼
//!                        spawn_blocking(encode) ─────────┐
//!                                                        │ handle pushed, in order
//!             TileSink::msg()  ── ServerMsg ─────────────┤  mpsc, cap ENCODE_DEPTH
//!                                                        ▼
//!                                order task: await handles FIFO → frame_tx
//!                                            ⤷ cleanup tick → base re-encode
//! ```
//!
//! FIFO collection preserves source order even when encodes finish out of order.
//! Control messages share the queue so a resize cannot overtake related tiles.
//! A full queue backpressures the engine because its shadow has already recorded
//! submitted pixels.
//!
//! This is also where the render dial's `motion` strategy lives, because it is the
//! one place both engines already funnel their damage through. See [`Motion`].

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::config::{RenderPlan, TileCodec};
use crate::protocol::{ServerMsg, Tile};
use crate::tiles::Rect;

/// Maximum queued encodes ahead of the handle currently collected. This covers
/// roughly one 1280×800 repaint while bounding memory and worker pressure.
const ENCODE_DEPTH: usize = 16;

/// The granularity churn is counted at. Several changes inside one slot count
/// once, so churn measures how much of a stretch of *time* a cell was busy for.
///
/// Time rather than frames, because neither engine has a frame worth counting.
/// RDP's outer loop turns once per PDU received, most of which redraw nothing, so
/// a counter driven by it races ahead of the repaints and a cell's history ages
/// out between its own changes. VNC's turns once per `FramebufferUpdate`, which is
/// damage-driven and so much closer, but its rate is set by the update-request
/// loop rather than by the remote: a cell changing in every update looks identical
/// whether that is sixty times a second or twice. A slot means the same thing on
/// both, and "in motion" stays one statement about the remote rather than two
/// about the transports.
const CHURN_SLOT: Duration = Duration::from_millis(100);

/// Slots of change history each cell keeps — the width of the `u8` shift register
/// in [`ChurnCell`], so it may not exceed 8. With [`CHURN_SLOT`] that is a window
/// of 800ms.
const CHURN_WINDOW: u64 = 8;

/// Churn — how many of a cell's last [`CHURN_WINDOW`] slots changed it — at which
/// the cell is taken to be in motion and switches to the motion encode.
///
/// A hard switch rather than a ramp between the two encodes. Which of those is
/// right is a question for measurement, and the switch is the one worth measuring
/// first: it makes the detection legible in the totals, where a ramp would smear
/// the answer across every quality in between.
///
/// Half the window, so one isolated change is never motion — a popup, an image
/// that just finished loading, a cursor — and a cell has to keep changing for
/// 400ms of the last 800ms before its quality drops. Anything less would only draw
/// a pointless cleanup a moment later.
const CHURN_MOVING: u32 = 4;

/// How long a cell sent at the motion encode must sit unchanged before it is
/// re-sent at the base one. Long enough that a brief pause in motion is not chased
/// with a redundant re-encode, short enough that a settled region sharpens while
/// the eye is still on it.
const CLEANUP_IDLE: Duration = Duration::from_millis(500);

/// How often the order task wakes to look for settled cells. It has to be its own
/// timer rather than something the next frame does, because a screen that stops
/// changing produces no next frame — which is exactly the case a cleanup is for.
const CLEANUP_TICK: Duration = Duration::from_millis(250);

/// Cleanups per tick, so a whole stopped video settles over a few ticks rather
/// than in one burst competing with live motion for the socket.
const MAX_CLEANUPS_PER_TICK: usize = 8;

/// Cap on the source pixels held for cleanups at once. Past it a cell keeps the
/// motion encode until it next changes — safe, just not as crisp — rather than the
/// stash growing without bound under a full-screen video.
const MAX_STASH_BYTES: usize = 8 * 1024 * 1024;

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

/// One cell's recent history: bit 0 is the slot it was last seen changing,
/// shifted left one place for every slot since. Its churn is the number of set
/// bits — how many of the last [`CHURN_WINDOW`] slots it changed in.
#[derive(Clone, Copy)]
struct ChurnCell {
    history: u8,
    last_slot: u64,
}

/// A piece sent at the motion encode, held so it can be re-sent at the base one
/// once its cell stops moving.
///
/// It keeps the *source* pixels, so the re-encode needs no fresh frame from the
/// remote — which is the whole point, since a still screen produces none. `rgb` is
/// shared with the encode worker rather than copied for it: the two want the same
/// bytes and neither writes them.
struct Stashed {
    rect: Rect,
    rgb: Arc<Vec<u8>>,
    /// Tokio's clock rather than the standard library's, so that it is the same
    /// clock [`CLEANUP_TICK`] runs on. That keeps the two from disagreeing under a
    /// test that pauses time, which is the only way to assert the cleanup path
    /// without asserting how fast the machine is.
    sent_at: tokio::time::Instant,
}

/// Which cells are changing fast enough to take the cheap encode, and which of
/// them are still owed a re-send at the base one.
///
/// Keyed by [`Rect::cell_key`], which is the reason the grid exists: RDP and VNC
/// describe the same moving region with different rectangles from one frame to the
/// next, and a key that moved with them would count no churn at all.
///
/// Guarded by a mutex on [`Shared`] because [`TileSink`] is `Clone` and VNC pushes
/// from two tasks. Every critical section here is a hash lookup and some
/// arithmetic; nothing holds the lock across an await.
#[derive(Default)]
struct Motion {
    /// Where slot numbering starts, taken from the first observation and dropped
    /// by [`Self::clear`].
    ///
    /// Slots have to be numbered against one origin every cell shares rather than
    /// measured as a delta per cell: a cell changing faster than [`CHURN_SLOT`]
    /// would otherwise carry its own last-seen instant forward with every change,
    /// never advance its register, and so never be in motion — which is the case
    /// this exists to catch.
    origin: Option<tokio::time::Instant>,
    churn: HashMap<(u16, u16), ChurnCell>,
    stash: HashMap<(u16, u16), Stashed>,
    stash_bytes: usize,
}

impl Motion {
    /// Record that `key` changed at `now`, and return its churn.
    ///
    /// Aging is lazy: a cell that did not change is not touched, and its history is
    /// shifted by the whole gap when it is next seen. That is also what keeps the
    /// shift from overflowing — a gap of a full window empties the history instead.
    fn observe(&mut self, key: (u16, u16), now: tokio::time::Instant) -> u32 {
        let origin = *self.origin.get_or_insert(now);
        let slot = u64::try_from(
            now.saturating_duration_since(origin).as_millis() / CHURN_SLOT.as_millis(),
        )
        .unwrap_or(u64::MAX);
        let cell = self
            .churn
            .entry(key)
            .or_insert(ChurnCell { history: 0, last_slot: slot });
        let elapsed = slot.saturating_sub(cell.last_slot);
        cell.history = if elapsed >= CHURN_WINDOW {
            0
        } else {
            cell.history << elapsed
        };
        cell.history |= 1;
        cell.last_slot = slot;
        cell.history.count_ones()
    }

    /// Remember a piece about to be sent at the motion encode, so a later tick can
    /// settle it. Reports whether the debt was recorded.
    ///
    /// A `false` return has to be honoured by the caller, which is why this is
    /// `#[must_use]`: `Shadow` records the *source* pixels as delivered the moment a
    /// rectangle is accepted, so a cell sent at the motion encode with no debt
    /// recorded is one the client holds a lossy copy of while the gateway believes
    /// it holds the exact pixels — and nothing will ever re-send it, because nothing
    /// knows it is owed. That is permanent, not merely coarse. Sending such a cell
    /// at the base encode instead is the only safe reading of a full stash.
    ///
    /// Over [`MAX_STASH_BYTES`] the *existing* entry is kept and the new one
    /// dropped. Keeping the older one is the point: it is the one closer to coming
    /// due, and a cell whose debt was dropped instead would never clean up at all
    /// until it next moved.
    #[must_use]
    fn stash(
        &mut self,
        key: (u16, u16),
        rect: Rect,
        rgb: Arc<Vec<u8>>,
        now: tokio::time::Instant,
    ) -> bool {
        let replaced = self.stash.get(&key).map_or(0, |old| old.rgb.len());
        let after = self.stash_bytes - replaced + rgb.len();
        if after > MAX_STASH_BYTES {
            return false;
        }
        self.stash_bytes = after;
        self.stash.insert(key, Stashed { rect, rgb, sent_at: now });
        true
    }

    /// Forget the cleanup owed for `key` if `sent` — a rectangle just sent at the
    /// base encode — covers all of what is owed.
    ///
    /// The containment test is the whole of it. Damage is sent as it is reported,
    /// clipped to the cell rather than snapped out to it, so a cell can owe a
    /// cleanup for a region wider than the piece that just went out crisp. Settling
    /// on that would leave a sliver at the motion encode with nothing left to
    /// remember it.
    fn settle(&mut self, key: (u16, u16), sent: Rect) {
        if self.stash.get(&key).is_some_and(|owed| sent.contains(&owed.rect)) {
            let old = self.stash.remove(&key).expect("just found it");
            self.stash_bytes -= old.rgb.len();
        }
    }

    /// Take up to `max` of the cells that have sat unchanged for [`CLEANUP_IDLE`],
    /// oldest first, for the caller to re-encode at the base quality.
    ///
    /// Oldest first so a backlog larger than one tick drains in the order it
    /// accrued; picking arbitrarily lets a long-settled cell be starved by newer
    /// ones for as long as the motion lasts.
    fn take_due(&mut self, now: tokio::time::Instant, max: usize) -> Vec<Stashed> {
        let mut due: Vec<((u16, u16), tokio::time::Instant)> = self
            .stash
            .iter()
            .filter(|(_, s)| now.saturating_duration_since(s.sent_at) >= CLEANUP_IDLE)
            .map(|(key, s)| (*key, s.sent_at))
            .collect();
        due.sort_unstable_by_key(|(_, sent_at)| *sent_at);
        due.truncate(max);
        due.into_iter()
            .map(|(key, _)| {
                let stashed = self.stash.remove(&key).expect("key came from the map");
                self.stash_bytes -= stashed.rgb.len();
                stashed
            })
            .collect()
    }

    /// Drop everything. A resize changes what every key means, and a repaint has
    /// already re-sent every pixel the stash was holding.
    fn clear(&mut self) {
        self.origin = None;
        self.churn.clear();
        self.stash.clear();
        self.stash_bytes = 0;
    }
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
    motion: Mutex<Motion>,
    tiles: AtomicU64,
    encoded_bytes: AtomicU64,
    /// Of [`Self::tiles`], those sent at the motion encode rather than the base.
    motion_tiles: AtomicU64,
    /// Of [`Self::tiles`], those that are a settled cell being re-sent crisp.
    cleanups: AtomicU64,
    cleanup_bytes: AtomicU64,
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
    /// The resolved render dial, read once from the target in `rdp::run` /
    /// `vnc::run` (see [`crate::config::TargetConfig::render_plan`]).
    plan: RenderPlan,
}

impl TileSink {
    /// Start an encoder for one engine. `engine` prefixes its log lines. `plan` is
    /// the resolved render dial — a base codec, and for the `motion` strategy the
    /// cheaper one a cell in motion takes instead.
    pub fn new(engine: &'static str, frame_tx: mpsc::Sender<ServerMsg>, plan: RenderPlan) -> Self {
        let (tx, rx) = mpsc::channel(ENCODE_DEPTH);
        let shared = Arc::new(Shared::default());
        tokio::spawn(order_loop(engine, rx, frame_tx, Arc::clone(&shared), plan));
        Self {
            engine,
            tx,
            shared,
            plan,
        }
    }

    /// Queue everything a changed rectangle owes the client.
    ///
    /// `pack` copies a sub-rectangle of the engine's framebuffer out as packed
    /// RGB888. It is a callback rather than one buffer because the two engines hold
    /// those pixels differently — RDP repacks out of the decoded image, VNC crops
    /// out of the rectangle it just read — and because how a rectangle is *cut* is
    /// the one thing both should agree on, which is what this method is.
    ///
    /// Without a motion plan the cut is [`Rect::bands`] and nothing else, which is
    /// what the gateway has always sent. With one, a band whose cells are all quiet
    /// is still sent whole and at the base encode — a target with nothing moving is
    /// byte-for-byte what the same target sends today — and only a band containing a
    /// moving cell is cut at the grid, so a video in a window costs its own cells
    /// their quality and costs the text beside it nothing.
    pub async fn damage<F>(&self, changed: Rect, pack: F) -> anyhow::Result<()>
    where
        F: Fn(Rect) -> Vec<u8>,
    {
        let Some(motion_codec) = self.plan.motion else {
            for band in changed.bands() {
                self.encode(band, Arc::new(pack(band)), self.plan.base).await?;
            }
            return Ok(());
        };

        // One reading for the whole rectangle: the pieces of one report of damage
        // arrived together and belong in the same slot, however long the cutting
        // and encoding below take.
        let now = tokio::time::Instant::now();
        for band in changed.bands() {
            // Every cell this band touches is a cell that just changed, so this is
            // where its churn is recorded — whether or not the band is then cut.
            let cells: Vec<(Rect, bool)> = {
                let mut motion = self.shared.motion.lock().unwrap();
                band.cells()
                    .map(|cell| {
                        let churn = motion.observe(cell.cell_key(), now);
                        (cell, churn >= CHURN_MOVING)
                    })
                    .collect()
            };

            if !cells.iter().any(|(_, moving)| *moving) {
                let rgb = Arc::new(pack(band));
                {
                    let mut motion = self.shared.motion.lock().unwrap();
                    for (cell, _) in &cells {
                        motion.settle(cell.cell_key(), band);
                    }
                }
                self.encode(band, rgb, self.plan.base).await?;
                continue;
            }

            for (cell, moving) in cells {
                let rgb = Arc::new(pack(cell));
                // A cell takes the motion encode only if its cleanup was recorded.
                // Past the stash cap it stays crisp instead: the alternative is a
                // client left holding a lossy copy that nothing is owed and so
                // nothing will ever replace. Costing bytes is recoverable; that is
                // not.
                let took_the_discount = moving && {
                    // Timed at dispatch rather than when the encode lands, which is
                    // what keeps a cleanup from overtaking fresher pixels: a cell
                    // with a tile still in the queue has just been touched, so it
                    // cannot also be idle.
                    self.shared
                        .motion
                        .lock()
                        .unwrap()
                        .stash(cell.cell_key(), cell, Arc::clone(&rgb), now)
                };
                let codec = if took_the_discount {
                    self.shared.motion_tiles.fetch_add(1, Ordering::Relaxed);
                    motion_codec
                } else {
                    // Crisp pixels discharge whatever this cell was owed, whether it
                    // is quiet or only crisp because the stash is full.
                    self.shared.motion.lock().unwrap().settle(cell.cell_key(), cell);
                    self.plan.base
                };
                self.encode(cell, rgb, codec).await?;
            }
        }
        Ok(())
    }

    /// Forget every cell's history and every cleanup owed.
    ///
    /// For a resize, where the keys no longer name the same pixels, and for a
    /// repaint, where every pixel is about to be re-sent at the base encode anyway
    /// — carrying churn across either would put cells in motion that are only
    /// being redrawn once.
    pub fn reset_motion(&self) {
        self.shared.motion.lock().unwrap().clear();
    }

    /// Queue one rectangle of packed RGB888 at the base encode.
    ///
    /// The primitive under [`Self::damage`], and what a caller with a rectangle
    /// already cut to its liking wants.
    pub async fn tile(&self, x: u16, y: u16, w: u16, h: u16, rgb: Vec<u8>) -> anyhow::Result<()> {
        let Some(rect) = Rect::from_size(x, y, w, h) else {
            return Ok(());
        };
        self.encode(rect, Arc::new(rgb), self.plan.base).await
    }

    /// Start an encode and queue its place in the order.
    ///
    /// `rgb` is owned rather than borrowed because the engine's framebuffer is
    /// gone by the time a worker reads it — the read loop has moved on to the next
    /// PDU. The encode starts immediately; only its *place in the order* is queued.
    async fn encode(
        &self,
        rect: Rect,
        rgb: Arc<Vec<u8>>,
        codec: TileCodec,
    ) -> anyhow::Result<()> {
        let handle = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let tile = encode_tile(rect, &rgb, codec)?;
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
    /// [`Shared`] gives. Silent when an engine encoded nothing, since a line of
    /// zeroes says nothing — though both current engines, RDP and VNC, do encode.
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

/// Encode one rectangle of packed RGB888 with the given codec.
fn encode_tile(rect: Rect, rgb: &[u8], codec: TileCodec) -> anyhow::Result<Tile> {
    let (x, y, w, h) = (rect.left, rect.top, rect.w(), rect.h());
    match codec {
        TileCodec::Png => Tile::from_rgb(x, y, w, h, rgb),
        TileCodec::Jpeg(q) => Tile::from_rgb_jpeg(x, y, w, h, rgb, q),
        TileCodec::Webp(q) => Tile::from_rgb_webp(x, y, w, h, rgb, q),
    }
}

/// Re-send cells that have stopped moving at the base encode, so a paused screen
/// sharpens on its own. `false` means the browser is gone.
///
/// A cleanup that fails to encode is put back rather than ending the session, which
/// is the one place this path differs from an ordinary tile: a tile that never
/// arrives leaves the shadow claiming the client has pixels it never got, while a
/// cleanup that never arrives leaves the client with pixels that are correct and
/// merely coarser.
///
/// Put back rather than dropped, though. The shadow recorded the source pixels as
/// delivered when the cell was first sent, so a discarded debt is a region the
/// client holds a lossy copy of that nothing will ever re-send. Keeping it means the
/// next tick tries again.
async fn flush_cleanups(
    engine: &'static str,
    shared: &Arc<Shared>,
    base: TileCodec,
    frame_tx: &mpsc::Sender<ServerMsg>,
) -> bool {
    let due = shared
        .motion
        .lock()
        .unwrap()
        .take_due(tokio::time::Instant::now(), MAX_CLEANUPS_PER_TICK);
    for stashed in due {
        let Stashed { rect, rgb, sent_at } = stashed;
        // On a worker like any other encode: a cleanup arrives when the screen is
        // quiet, but the order task is not the thread to find that out on.
        let owed = Arc::clone(&rgb);
        let joined = tokio::task::spawn_blocking(move || encode_tile(rect, &rgb, base)).await;
        let tile = match joined {
            Ok(Ok(tile)) => tile,
            Ok(Err(e)) => {
                warn!("{engine}: re-queueing a cleanup tile that would not encode: {e:#}");
                // Back into the stash at its original age, so it is due again on the
                // next tick rather than waiting out another idle period.
                let _ = shared
                    .motion
                    .lock()
                    .unwrap()
                    .stash(rect.cell_key(), rect, owed, sent_at);
                continue;
            }
            Err(e) => {
                give_up(engine, shared, format!("tile encoder stopped: {e}"));
                return false;
            }
        };
        debug!(
            "{engine}: cleanup {}x{} at ({},{}): {} bytes",
            tile.w,
            tile.h,
            tile.x,
            tile.y,
            tile.data.len()
        );
        let bytes = tile.data.len() as u64;
        shared.tiles.fetch_add(1, Ordering::Relaxed);
        shared.cleanups.fetch_add(1, Ordering::Relaxed);
        shared.encoded_bytes.fetch_add(bytes, Ordering::Relaxed);
        shared.cleanup_bytes.fetch_add(bytes, Ordering::Relaxed);
        if frame_tx.send(ServerMsg::Tile(tile)).await.is_err() {
            return false;
        }
    }
    true
}

/// Collect finished encodes in push order and forward them, and — for a target on
/// the `motion` strategy — settle cells that have stopped moving.
///
/// The cleanup timer belongs here rather than anywhere the frames arrive, because
/// the case it exists for is a screen that has stopped producing them.
async fn order_loop(
    engine: &'static str,
    mut rx: mpsc::Receiver<Pending>,
    frame_tx: mpsc::Sender<ServerMsg>,
    shared: Arc<Shared>,
    plan: RenderPlan,
) {
    let mut cleanup = tokio::time::interval(CLEANUP_TICK);
    // Delay rather than Burst: a tick missed while the queue was busy is a tick
    // whose cells are still there to settle, and firing the backlog at once would
    // only stack re-encodes behind whatever made it late.
    cleanup.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let item = tokio::select! {
            item = rx.recv() => match item {
                Some(item) => item,
                None => break,
            },
            _ = cleanup.tick(), if plan.motion.is_some() => {
                if flush_cleanups(engine, &shared, plan.base, &frame_tx).await {
                    continue;
                }
                break;
            }
        };
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
/// - `motion` and `cleanup` are the whole measurement of the `motion` strategy, and
///   they are read together. `motion` at zero on a target that has one configured
///   is the claim that a still screen is untouched, and it is the number to check
///   before believing any saving. `cleanup` against it is what the discount cost:
///   every cleanup is a tile sent twice, so a scheme that pays more in re-sends than
///   it saves in motion shows up here as a `cleanup` byte count rivalling the
///   saving — and both are already inside `tiles` and `bytes`, which stay the
///   totals for the link.
struct Totals {
    tiles: u64,
    encoded_bytes: u64,
    motion_tiles: u64,
    cleanups: u64,
    cleanup_bytes: u64,
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
            motion_tiles: shared.motion_tiles.load(Ordering::Relaxed),
            cleanups: shared.cleanups.load(Ordering::Relaxed),
            cleanup_bytes: shared.cleanup_bytes.load(Ordering::Relaxed),
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
            "{} tile(s) / {} bytes ({} in motion, {} cleanup / {} bytes), \
             {}µs encoding across workers in {}µs of waiting, engine stalled {}µs",
            self.tiles,
            self.encoded_bytes,
            self.motion_tiles,
            self.cleanups,
            self.cleanup_bytes,
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

    fn plan(base: TileCodec, motion: Option<TileCodec>) -> RenderPlan {
        RenderPlan { base, motion }
    }

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a non-empty rectangle")
    }

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
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Png, None));

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
            assert_eq!(tile.format, Tile::FORMAT_PNG);
        }
    }

    /// A sink built with a JPEG quality encodes its tiles as JPEG; the default
    /// (`None`, asserted above) stays PNG. The one bit the render dial threads
    /// all the way to the wire.
    #[tokio::test]
    async fn a_jpeg_quality_makes_tiles_jpeg() {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Jpeg(60), None));

        sink.tile(0, 0, 320, 64, rgb(320, 64, 1)).await.unwrap();
        sink.flush().await;

        let ServerMsg::Tile(tile) = &drain(&mut frame_rx, 1).await[0] else {
            panic!("expected a tile");
        };
        assert_eq!(tile.format, Tile::FORMAT_JPEG);
    }

    /// Same for the other lossy codec: a WebP-configured sink emits WebP tiles.
    #[tokio::test]
    async fn a_webp_quality_makes_tiles_webp() {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Webp(60), None));

        sink.tile(0, 0, 320, 64, rgb(320, 64, 1)).await.unwrap();
        sink.flush().await;

        let ServerMsg::Tile(tile) = &drain(&mut frame_rx, 1).await[0] else {
            panic!("expected a tile");
        };
        assert_eq!(tile.format, Tile::FORMAT_WEBP);
    }

    /// The hazard a side channel for control messages would create: the client
    /// must learn a new size before a tile in the new coordinate space arrives.
    #[tokio::test]
    async fn a_control_message_cannot_overtake_the_tiles_before_it() {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Png, None));

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
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Png, None));

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
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Png, None));

        // A payload one byte short of the geometry: `Tile::from_rgb` rejects it on
        // its length check rather than handing a short buffer to the PNG encoder.
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
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Png, None));
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

    // ---- motion ----
    //
    // Churn is counted in wall-clock slots, so every test below either drives
    // `Motion` directly with instants it made up or runs under `start_paused` and
    // moves the clock itself. Nothing sleeps for real and nothing counts events, so
    // no assertion here changes if the machine is twice as slow.

    const MOTION: RenderPlan = RenderPlan {
        base: TileCodec::Png,
        motion: Some(TileCodec::Jpeg(10)),
    };

    /// Damage given to `sink` once per churn slot for `slots` slots — what a video
    /// playing in a window looks like from here. Requires a paused clock.
    async fn drive(sink: &TileSink, area: Rect, slots: u64) {
        for _ in 0..slots {
            sink.damage(area, |piece| rgb(piece.w(), piece.h(), 7)).await.unwrap();
            tokio::time::advance(CHURN_SLOT).await;
        }
    }

    fn formats(msgs: &[ServerMsg]) -> Vec<u8> {
        msgs.iter()
            .map(|m| match m {
                ServerMsg::Tile(tile) => tile.format,
                other => panic!("expected a tile, got {other:?}"),
            })
            .collect()
    }

    /// A slot's worth of instants, for driving `Motion` by hand.
    fn slots(base: tokio::time::Instant, n: u64) -> tokio::time::Instant {
        base + CHURN_SLOT * u32::try_from(n).unwrap()
    }

    #[test]
    fn churn_counts_recent_slots_and_ages_out() {
        let mut motion = Motion::default();
        let key = (0, 0);
        let base = tokio::time::Instant::now();
        // Changing every slot ramps churn up one per slot, to the window width.
        for slot in 0..CHURN_WINDOW {
            assert_eq!(
                u64::from(motion.observe(key, slots(base, slot))),
                slot + 1,
                "slot {slot}"
            );
        }
        // It saturates at the window rather than overflowing the register.
        assert_eq!(
            u64::from(motion.observe(key, slots(base, CHURN_WINDOW))),
            CHURN_WINDOW,
            "churn should cap at the window"
        );
        // A gap longer than the window empties the history: one recent change only.
        assert_eq!(
            motion.observe(key, slots(base, CHURN_WINDOW * 3)),
            1,
            "a long-idle cell reset"
        );
    }

    /// Churn is how much of a stretch of time a cell was busy for, not how many
    /// rectangles described it. This is the property that decides the whole scheme:
    /// a burst of damage in one instant is not motion, and an engine that reports
    /// the same change as ten rectangles must not read as ten times as busy.
    #[test]
    fn changes_inside_one_slot_count_once() {
        let mut motion = Motion::default();
        let base = tokio::time::Instant::now();
        for i in 0..40 {
            let churn = motion.observe((0, 0), base + Duration::from_millis(i));
            assert_eq!(churn, 1, "a flurry inside one slot counted as motion");
        }
        // And the next slot advances it by exactly one.
        assert_eq!(motion.observe((0, 0), slots(base, 1)), 2);
    }

    /// The claim the whole design rests on: a target with nothing moving sends
    /// exactly what it would send without a motion plan at all — same rectangles,
    /// same codec, same bytes. Asserted against a second sink rather than against a
    /// written-down expectation, so it cannot drift.
    #[tokio::test(start_paused = true)]
    async fn a_still_screen_is_byte_identical_to_its_base_configuration() {
        let area = rect(37, 41, 900, 200);
        let mut out = Vec::new();
        for plan in [plan(TileCodec::Png, None), MOTION] {
            let (frame_tx, mut frame_rx) = mpsc::channel(256);
            let sink = TileSink::new("test", frame_tx, plan);
            // A screen that changes now and then rather than continuously: the same
            // region redrawn four times, but with a full churn window of quiet
            // between each, so no cell is ever in motion. Four redraws rather than
            // one, so a plan that simply never split would not pass by never being
            // asked to.
            for _ in 0..4 {
                sink.damage(area, |piece| rgb(piece.w(), piece.h(), 7)).await.unwrap();
                tokio::time::advance(CHURN_SLOT * u32::try_from(CHURN_WINDOW).unwrap()).await;
            }
            sink.flush().await;

            let mut tiles = Vec::new();
            while let Ok(ServerMsg::Tile(tile)) = frame_rx.try_recv() {
                tiles.push((tile.format, tile.x, tile.y, tile.w, tile.h, tile.data));
            }
            out.push(tiles);
        }
        assert_eq!(out[0], out[1], "a motion plan changed a still screen's output");
        assert!(!out[0].is_empty(), "the test sent nothing");
    }

    /// A cell changing every slot switches to the motion encode once its churn
    /// reaches the threshold, and not one slot before.
    #[tokio::test(start_paused = true)]
    async fn a_cell_changing_every_slot_switches_at_the_threshold() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx, MOTION);

        // One cell, so one tile per slot and the tile index is the slot index.
        let cell = rect(0, 0, 320, 64);
        drive(&sink, cell, u64::from(CHURN_MOVING) + 2).await;
        sink.flush().await;

        let sent = formats(&drain(&mut frame_rx, usize::try_from(CHURN_MOVING).unwrap() + 2).await);
        let (before, after) = sent.split_at(usize::try_from(CHURN_MOVING).unwrap() - 1);
        assert!(
            before.iter().all(|&f| f == Tile::FORMAT_PNG),
            "a cell went to the motion encode before it had the churn for it: {sent:?}"
        );
        assert!(
            after.iter().all(|&f| f == Tile::FORMAT_JPEG),
            "a cell at the threshold stayed on the base encode: {sent:?}"
        );
    }

    /// The payoff the grid buys: a video in a window costs its own cells their
    /// quality and costs the text beside it nothing. The band spans four cells and
    /// only the first keeps changing.
    #[tokio::test(start_paused = true)]
    async fn only_the_cells_in_motion_lose_their_quality() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx, MOTION);

        let moving = rect(0, 0, 320, 64);
        let band = rect(0, 0, 1280, 64);
        // Enough slots of the small region alone to put its cell in motion.
        drive(&sink, moving, u64::from(CHURN_MOVING)).await;
        // Then one report of the whole band, which now contains one moving cell and
        // three quiet ones.
        sink.damage(band, |piece| rgb(piece.w(), piece.h(), 7)).await.unwrap();
        sink.flush().await;

        let all = drain(&mut frame_rx, usize::try_from(CHURN_MOVING).unwrap() + 4).await;
        let split = &all[usize::try_from(CHURN_MOVING).unwrap()..];
        assert_eq!(
            formats(split),
            vec![
                Tile::FORMAT_JPEG,
                Tile::FORMAT_PNG,
                Tile::FORMAT_PNG,
                Tile::FORMAT_PNG
            ],
            "the band was not cut at the cell the motion is in"
        );
        let ServerMsg::Tile(first) = &split[0] else { unreachable!() };
        assert_eq!((first.x, first.w), (0, 320), "the moving piece is one cell wide");
        let ServerMsg::Tile(last) = &split[3] else { unreachable!() };
        assert_eq!((last.x, last.w), (960, 320), "the quiet pieces cover the rest");
    }

    #[test]
    fn a_settled_cell_is_cleaned_up_once_then_forgotten() {
        let mut motion = Motion::default();
        let cell = rect(0, 0, 320, 64);
        let base = tokio::time::Instant::now();
        assert!(motion.stash((0, 0), cell, Arc::new(rgb(320, 64, 1)), base));

        assert!(motion.take_due(base, 8).is_empty(), "settled before it was idle");
        assert!(
            motion.take_due(base + CLEANUP_IDLE - Duration::from_millis(1), 8).is_empty(),
            "settled a millisecond early"
        );
        let due = motion.take_due(base + CLEANUP_IDLE, 8);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].rect, cell);
        assert_eq!(motion.stash_bytes, 0, "the debt was not refunded");
        assert!(
            motion.take_due(base + CLEANUP_IDLE * 4, 8).is_empty(),
            "a cell was cleaned up twice"
        );
    }

    /// Damage is sent as reported, clipped to the cell rather than snapped out to
    /// it, so a piece that goes out crisp may cover less than the cell owes. Only
    /// full cover settles the debt; anything less would strand a sliver at the
    /// motion encode with nothing left to remember it.
    #[test]
    fn only_full_cover_cancels_a_pending_cleanup() {
        let mut motion = Motion::default();
        let owed = rect(0, 0, 320, 64);
        let stash = |m: &mut Motion| {
            assert!(m.stash((0, 0), owed, Arc::new(rgb(320, 64, 1)), tokio::time::Instant::now()))
        };

        stash(&mut motion);
        motion.settle((0, 0), rect(0, 0, 160, 64));
        assert_eq!(motion.stash.len(), 1, "a partial cover cancelled the whole debt");

        motion.settle((0, 0), owed);
        assert!(motion.stash.is_empty(), "an exact cover left the debt standing");
        assert_eq!(motion.stash_bytes, 0);

        stash(&mut motion);
        motion.settle((0, 0), rect(0, 0, 1280, 64));
        assert!(motion.stash.is_empty(), "a band covering the cell left the debt standing");
    }

    /// A whole stopped video settles over a few ticks rather than in one burst, and
    /// in the order the debts accrued — otherwise a long-settled cell can be
    /// starved by newer ones for as long as the motion lasts.
    #[test]
    fn cleanups_are_bounded_per_tick_and_drain_oldest_first() {
        let mut motion = Motion::default();
        let base = tokio::time::Instant::now();
        let count = MAX_CLEANUPS_PER_TICK + 3;
        for i in 0..count {
            let key = (i as u16, 0);
            let cell = rect(i as u16 * 320, 0, 320, 64);
            // Staggered, so "oldest" is a fact about the data rather than about
            // whichever order the map happens to iterate in.
            let sent_at = base + Duration::from_millis(i as u64);
            assert!(motion.stash(key, cell, Arc::new(rgb(320, 64, 1)), sent_at));
        }

        let now = base + Duration::from_millis(count as u64) + CLEANUP_IDLE;
        let first = motion.take_due(now, MAX_CLEANUPS_PER_TICK);
        assert_eq!(first.len(), MAX_CLEANUPS_PER_TICK);
        assert_eq!(
            first.iter().map(|s| s.rect.left).collect::<Vec<_>>(),
            (0..MAX_CLEANUPS_PER_TICK).map(|i| i as u16 * 320).collect::<Vec<_>>(),
            "the backlog did not drain oldest first"
        );
        let second = motion.take_due(now, MAX_CLEANUPS_PER_TICK);
        assert_eq!(second.len(), 3, "the rest did not follow on the next tick");
        assert_eq!(motion.stash_bytes, 0);
    }

    /// Past the cap a cell keeps the motion encode until it next changes — safe,
    /// just not as crisp. The *existing* entry is what survives: it is the one
    /// closer to coming due, and dropping it would leave that cell owed nothing.
    #[test]
    fn a_full_stash_keeps_the_debt_it_already_has() {
        let mut motion = Motion::default();
        let now = tokio::time::Instant::now();
        let cell = |i: u16| rect(i * 320, 0, 320, 64);
        // 320x64 RGB is 61,440 bytes, so this fills 8 MiB in 136 cells and change.
        let per_cell = 320 * 64 * 3;
        let fits = MAX_STASH_BYTES / per_cell;
        for i in 0..fits {
            assert!(motion.stash((i as u16, 0), cell(i as u16), Arc::new(rgb(320, 64, 1)), now));
        }
        let held = motion.stash_bytes;
        assert!(held + per_cell > MAX_STASH_BYTES, "the stash did not fill");

        // A new cell past the cap is dropped rather than admitted.
        assert!(
            !motion.stash((9000, 0), cell(0), Arc::new(rgb(320, 64, 1)), now),
            "a stash past the cap must report that it recorded nothing"
        );
        assert_eq!(motion.stash.len(), fits, "the cap did not hold");
        assert_eq!(motion.stash_bytes, held);

        // A cell that already owes one is still admitted at the cap, because what it
        // replaces is its own entry: the bytes come back before the new ones are
        // counted, so a same-sized redraw of a cell already in the stash always
        // fits. It has to be admitted, too — a refusal here would send that cell
        // crisp for no reason, since the debt is unchanged either way.
        let older = now - Duration::from_secs(1);
        assert!(motion.stash((0, 0), cell(0), Arc::new(rgb(320, 64, 2)), older));
        assert_eq!(motion.stash.len(), fits);
        assert_eq!(
            motion.take_due(now + CLEANUP_IDLE, 1).len(),
            1,
            "the cell that was already owed a cleanup lost it"
        );
    }

    /// Past the stash cap a cell stays on the base encode rather than taking a
    /// discount nothing is owed for.
    ///
    /// `Shadow` records the *source* pixels as delivered the moment a rectangle is
    /// accepted, so a cell sent lossy with no debt recorded is one the client holds
    /// a worse copy of than the gateway believes, with nothing left that would ever
    /// re-send it. An HP virtual display is big enough to reach this: 8 MiB holds
    /// 136 cells, and 2560×1600 is 200 of them.
    #[tokio::test(start_paused = true)]
    async fn a_cell_past_the_stash_cap_stays_on_the_base_encode() {
        let (frame_tx, mut frame_rx) = mpsc::channel(4096);
        let sink = TileSink::new("test", frame_tx, MOTION);

        // 61,440 bytes a cell, so the stash holds 136 and this asks for 140.
        let (across, down) = (10u16, 14u16);
        let fits = MAX_STASH_BYTES / (320 * 64 * 3);
        let total = usize::from(across) * usize::from(down);
        assert!(total > fits, "the area has to outgrow the stash for this to test it");

        let area = rect(0, 0, across * 320, down * 64);
        drive(&sink, area, u64::from(CHURN_MOVING)).await;
        sink.flush().await;

        // Every slot before the last is under the threshold and goes out as whole
        // bands; the last one splits into cells.
        let bands = usize::from(down) * (usize::try_from(CHURN_MOVING).unwrap() - 1);
        let sent = formats(&drain(&mut frame_rx, bands + total).await);
        let cells = &sent[bands..];
        assert_eq!(
            cells.iter().filter(|&&f| f == Tile::FORMAT_JPEG).count(),
            fits,
            "the discount was handed out to more cells than the stash could record"
        );
        assert!(
            cells[fits..].iter().all(|&f| f == Tile::FORMAT_PNG),
            "a cell went out lossy with no cleanup recorded for it"
        );
    }

    /// The cleanup path end to end, through the order task's own timer: a cell
    /// that stops moving is re-sent crisp with nothing to prompt it, because the
    /// case it exists for is a remote that has stopped sending frames entirely.
    ///
    /// Time is paused, so the sleep costs nothing and the assertion is about
    /// [`CLEANUP_IDLE`] rather than about how fast the machine is.
    #[tokio::test(start_paused = true)]
    async fn a_screen_that_stops_moving_sharpens_on_its_own() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx, MOTION);

        let cell = rect(0, 0, 320, 64);
        drive(&sink, cell, u64::from(CHURN_MOVING)).await;
        sink.flush().await;
        let moving = drain(&mut frame_rx, usize::try_from(CHURN_MOVING).unwrap()).await;
        assert_eq!(*formats(&moving).last().unwrap(), Tile::FORMAT_JPEG);

        // Nothing is pushed from here on: the remote has gone quiet, and the tick
        // is the only thing left that can act.
        tokio::time::sleep(CLEANUP_IDLE + CLEANUP_TICK * 2).await;

        let ServerMsg::Tile(settled) = &drain(&mut frame_rx, 1).await[0] else {
            panic!("a settled cell was never re-sent");
        };
        assert_eq!(settled.format, Tile::FORMAT_PNG, "it settled to the wrong encode");
        assert_eq!(
            (settled.x, settled.y, settled.w, settled.h),
            (cell.left, cell.top, cell.w(), cell.h()),
            "the cleanup covered something other than the cell that settled"
        );

        // Once, and only once: the debt is discharged, not standing.
        tokio::time::sleep(CLEANUP_IDLE * 4).await;
        assert!(frame_rx.try_recv().is_err(), "a settled cell was cleaned up twice");
    }

    /// A resize makes every key name somewhere else, and a repaint re-sends every
    /// pixel at the base encode. Either way nothing carries across.
    #[tokio::test(start_paused = true)]
    async fn a_reset_drops_every_history_and_every_debt() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx, MOTION);

        let cell = rect(0, 0, 320, 64);
        drive(&sink, cell, u64::from(CHURN_MOVING)).await;
        sink.flush().await;
        drain(&mut frame_rx, usize::try_from(CHURN_MOVING).unwrap()).await;
        assert!(!sink.shared.motion.lock().unwrap().stash.is_empty(), "nothing was owed");

        sink.reset_motion();
        {
            let motion = sink.shared.motion.lock().unwrap();
            assert!(motion.churn.is_empty() && motion.stash.is_empty());
            assert_eq!(motion.stash_bytes, 0);
        }

        // The next change is a first change, not the continuation of a motion.
        sink.damage(cell, |piece| rgb(piece.w(), piece.h(), 7)).await.unwrap();
        sink.flush().await;
        assert_eq!(formats(&drain(&mut frame_rx, 1).await), vec![Tile::FORMAT_PNG]);
    }
}
