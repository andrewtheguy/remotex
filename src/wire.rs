//! Turning a run of [`ServerMsg`] into the frames one client's socket sends.
//!
//! Everything stateful about the server -> client transport lives here, and it is
//! per *attachment* rather than per session or per engine. That is not tidiness:
//! [`crate::session`]'s pump drops frames while no browser is attached, so
//! anything remembering "the client already has this" must sit **downstream** of
//! that drop or it will withhold pixels from a client that never received them.
//! One outbound task owns one of these, and its lifetime is exactly one socket
//! ([`crate::ws`]).
//!
//! Deliberately a plain owned struct with a pure `encode` — no sockets, no async,
//! no channels. There is no benchmark harness in this repo, so a unit test over a
//! synthetic run of messages is the only way the byte cost of the transport can be
//! checked in CI at all, and that requires being able to call it without a client.
//!
//! Three rules the tests pin, each of which is a correctness matter rather than a
//! tuning knob:
//!
//! - **A control message flushes the pending batch.** Tiles and control messages
//!   share one ordered socket, and `resize` reallocates the client's canvas. A
//!   tile that arrived before a resize has to be *sent* before it, or it lands on
//!   a canvas that has already been cleared and thrown away.
//! - **A batch is bounded in bytes.** Not for tidiness: exceeding a client's
//!   WebSocket message ceiling fails the whole socket rather than dropping one
//!   frame, and a full repaint of a Retina desktop is megabytes of payload. The
//!   cap also keeps a slow link from waiting on one enormous write.
//! - **A partial batch never outlives the run that built it.** `encode` returns
//!   every frame for the messages it was given, so nothing is held back waiting
//!   for work that may not arrive.

use crate::protocol::{ServerMsg, WireFrame, batch};

/// How many bytes of records a single batch frame may carry before it is flushed
/// and a new one started.
///
/// The ceiling that matters is the client's: `URLSessionWebSocketTask` defaults to
/// 1 MiB and the viewer raises it to 16 MiB, and going past it kills the socket
/// instead of dropping a frame. This sits far below either, because the reason to
/// batch is to stop paying per-frame costs on a burst of small tiles — not to send
/// a whole repaint as one write, which would only move the latency somewhere else.
const MAX_BATCH_BYTES: usize = 256 * 1024;

/// Per-attachment encoder for the server -> client direction.
#[derive(Default)]
pub struct Wire {
    /// Records accumulated for the batch currently being built.
    records: Vec<u8>,
    /// How many records `records` holds.
    count: u16,
    pub totals: Totals,
}

impl Wire {
    /// Encode a run of messages, in order, into the frames to write.
    ///
    /// "Run" means everything the caller had available at once. Whatever is
    /// returned is complete: no records are retained for a later call, so a batch
    /// can never sit waiting for traffic that never comes.
    pub fn encode(&mut self, run: impl IntoIterator<Item = ServerMsg>) -> Vec<WireFrame> {
        let mut frames = Vec::new();
        for msg in run {
            match msg.text_frame() {
                // A control message: flush what is pending so the client applies
                // the tiles that preceded it before the state change.
                Some(json) => {
                    self.flush(&mut frames);
                    self.totals.text(json.len());
                    frames.push(WireFrame::Text(json));
                }
                None => {
                    let ServerMsg::Tile(tile) = msg else {
                        // `text_frame` returns None for tiles alone.
                        unreachable!("only a tile has no text encoding");
                    };
                    if !self.records.is_empty()
                        && self.records.len() + tile.record_len() > MAX_BATCH_BYTES
                    {
                        self.flush(&mut frames);
                    }
                    tile.write_record(batch::NO_SLOT, &mut self.records);
                    self.count += 1;
                    self.totals.tile(tile.record_len());
                }
            }
        }
        self.flush(&mut frames);
        frames
    }

    /// Emit the pending batch, if there is one.
    fn flush(&mut self, frames: &mut Vec<WireFrame>) {
        if self.count == 0 {
            return;
        }
        let mut frame = Vec::with_capacity(batch::HEADER_LEN + self.records.len());
        frame.push(batch::FRAME_KIND);
        frame.push(0); // flags
        frame.extend_from_slice(&self.count.to_le_bytes());
        frame.extend_from_slice(&self.records);
        self.records.clear();
        self.count = 0;
        self.totals.frame(frame.len());
        frames.push(WireFrame::Binary(frame));
    }
}

/// What one attachment cost on the wire, logged when it ends.
///
/// The repo has no benchmark harness, so this line is the only measurement of the
/// browser link that exists in production. Three things it reports that a plain
/// byte total cannot:
///
/// - **records separately from frames**, which is the whole point of batching: the
///   two moved apart, and only seeing both says by how much;
/// - **payload bytes separately from frame bytes**, so envelope overhead is
///   visible rather than assumed;
/// - **the largest single frame**, which is the number a client's WebSocket
///   message ceiling is measured against.
#[derive(Default)]
pub struct Totals {
    pub binary_frames: u64,
    pub binary_bytes: u64,
    pub tiles: u64,
    pub tile_bytes: u64,
    pub text_frames: u64,
    pub text_bytes: u64,
    pub largest_binary: u64,
}

impl Totals {
    fn frame(&mut self, len: usize) {
        self.binary_frames += 1;
        self.binary_bytes += len as u64;
        self.largest_binary = self.largest_binary.max(len as u64);
    }

    fn tile(&mut self, len: usize) {
        self.tiles += 1;
        self.tile_bytes += len as u64;
    }

    fn text(&mut self, len: usize) {
        self.text_frames += 1;
        self.text_bytes += len as u64;
    }
}

impl std::fmt::Display for Totals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} binary frames / {} bytes carrying {} tile records / {} bytes, \
             {} text frames / {} bytes, largest binary {} bytes",
            self.binary_frames,
            self.binary_bytes,
            self.tiles,
            self.tile_bytes,
            self.text_frames,
            self.text_bytes,
            self.largest_binary,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Tile, UNSCALED};

    fn tile(y: u16, bytes: usize) -> ServerMsg {
        ServerMsg::Tile(Tile {
            format: Tile::FORMAT_PNG,
            x: 0,
            y,
            w: 320,
            h: 64,
            data: vec![7u8; bytes],
        })
    }

    fn resize() -> ServerMsg {
        ServerMsg::Resize {
            w: 1600,
            h: 1000,
            scale: UNSCALED,
        }
    }

    /// Records of a batch frame as `(op, slot, x, y, w, h, payload_len)`.
    fn records(frame: &[u8]) -> Vec<(u8, u16, u16, u16, u16, u16, usize)> {
        assert_eq!(frame[0], batch::FRAME_KIND);
        assert_eq!(frame[1], 0, "flags must be zero");
        let count = u16::from_le_bytes([frame[2], frame[3]]);
        let mut at = batch::HEADER_LEN;
        let mut out = Vec::new();
        while at < frame.len() {
            let op = frame[at];
            assert_eq!(op, batch::OP_TILE, "only tiles are emitted yet");
            let le = |o: usize| u16::from_le_bytes([frame[at + o], frame[at + o + 1]]);
            let len = u32::from_le_bytes([
                frame[at + 12],
                frame[at + 13],
                frame[at + 14],
                frame[at + 15],
            ]) as usize;
            out.push((op, le(2), le(4), le(6), le(8), le(10), len));
            at += batch::TILE_HEADER_LEN + len;
        }
        assert_eq!(at, frame.len(), "records must exactly fill the frame");
        assert_eq!(
            out.len(),
            usize::from(count),
            "the header's count must match the records present"
        );
        out
    }

    fn binary(frames: &[WireFrame]) -> Vec<&Vec<u8>> {
        frames
            .iter()
            .filter_map(|f| match f {
                WireFrame::Binary(bytes) => Some(bytes),
                WireFrame::Text(_) => None,
            })
            .collect()
    }

    // The point of the whole module: many tiles, one frame.
    #[test]
    fn a_run_of_tiles_becomes_one_frame() {
        let mut wire = Wire::default();
        let frames = wire.encode((0..8).map(|i| tile(i * 64, 100)));
        assert_eq!(frames.len(), 1, "eight tiles must not cost eight frames");
        let records = records(binary(&frames)[0]);
        assert_eq!(records.len(), 8);
        // In order, and every one marked uncacheable until the cache exists.
        for (i, record) in records.iter().enumerate() {
            assert_eq!(record.1, batch::NO_SLOT);
            assert_eq!(record.3, i as u16 * 64, "records keep their arrival order");
            assert_eq!(record.6, 100);
        }
        assert_eq!(wire.totals.binary_frames, 1);
        assert_eq!(wire.totals.tiles, 8);
    }

    // Ordering across the two frame types is load-bearing: a resize reallocates
    // the client's canvas, so tiles from before it must be sent before it.
    #[test]
    fn a_control_message_flushes_the_tiles_that_preceded_it() {
        let mut wire = Wire::default();
        let frames = wire.encode(vec![tile(0, 50), tile(64, 50), resize(), tile(0, 50)]);
        assert!(
            matches!(frames[0], WireFrame::Binary(_)),
            "the first two tiles go out before the resize"
        );
        assert!(matches!(frames[1], WireFrame::Text(_)));
        assert!(matches!(frames[2], WireFrame::Binary(_)));
        assert_eq!(frames.len(), 3);
        assert_eq!(records(binary(&frames)[0]).len(), 2);
        assert_eq!(records(binary(&frames)[1]).len(), 1);
    }

    // Exceeding a client's message ceiling kills the socket rather than dropping a
    // frame, so the cap is a hard one.
    #[test]
    fn a_batch_is_split_before_it_exceeds_the_byte_cap() {
        let mut wire = Wire::default();
        let each = 100 * 1024;
        let frames = wire.encode((0..6).map(|i| tile(i * 64, each)));
        assert!(frames.len() > 1, "600 KB of tiles cannot be one frame");
        for frame in binary(&frames) {
            assert!(
                frame.len() <= MAX_BATCH_BYTES + batch::TILE_HEADER_LEN + batch::HEADER_LEN,
                "frame of {} bytes exceeds the cap",
                frame.len()
            );
        }
        // Split, not dropped: every tile is still there, in order.
        let seen: Vec<u16> = binary(&frames)
            .iter()
            .flat_map(|f| records(f))
            .map(|r| r.3)
            .collect();
        assert_eq!(seen, (0..6).map(|i| i * 64).collect::<Vec<_>>());
    }

    // A tile larger than the cap on its own still has to be sent: a cap that
    // silently dropped it would leave a permanent hole in the picture.
    #[test]
    fn a_single_oversized_tile_is_still_sent() {
        let mut wire = Wire::default();
        let frames = wire.encode(vec![tile(0, MAX_BATCH_BYTES * 2)]);
        assert_eq!(frames.len(), 1);
        assert_eq!(records(binary(&frames)[0])[0].6, MAX_BATCH_BYTES * 2);
    }

    // Nothing may be retained past the run that produced it, or a batch could sit
    // in the encoder waiting for traffic that never comes.
    #[test]
    fn nothing_is_held_back_between_runs() {
        let mut wire = Wire::default();
        assert_eq!(wire.encode(vec![tile(0, 10)]).len(), 1);
        assert!(wire.records.is_empty());
        assert_eq!(wire.count, 0);
        // A run with nothing in it produces nothing, rather than an empty frame.
        assert!(wire.encode(Vec::new()).is_empty());
        assert_eq!(wire.encode(vec![resize()]).len(), 1);
    }

    // The transport's reason to exist, as a byte comparison against v2's one
    // frame per tile. Same spirit as `tile_frame_beats_old_base64_json_baseline`
    // in protocol.rs: the change has to pay for itself measurably, not plausibly.
    #[test]
    fn batching_costs_less_than_one_frame_per_tile() {
        // A 1600x1000 repaint at 320x64 is 5 x 16 = 80 cells.
        let cells = 80;
        let payload = 900;
        let mut wire = Wire::default();
        let frames = wire.encode((0..cells).map(|i| tile(i as u16 * 64, payload)));

        // v2: one WebSocket frame per tile, each with a 10-byte header. The frame
        // count is the real cost — 80 client events and 80 scheduled decodes.
        assert_eq!(frames.len(), 1, "one repaint, one frame");
        assert!(
            wire.totals.binary_bytes < (payload + 10) as u64 * cells as u64 + 4096,
            "the envelope must not cost more than the per-frame headers it replaced"
        );
    }
}
