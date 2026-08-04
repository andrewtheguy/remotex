//! Per-attachment conversion from [`ServerMsg`] to WebSocket frames. Control
//! messages flush tile batches to preserve coordinate-space ordering, batches
//! are bounded, and a tile covered later in the same batch may be dropped.
//! Cache state lives here because frames are discarded before attachment.

use crate::protocol::{self, ServerMsg, Tile, VideoUnit, WireFrame, batch};

/// Record bytes per batch, below client WebSocket limits and large enough to
/// amortize per-frame overhead without making a full repaint one work unit.
const MAX_BATCH_BYTES: usize = 256 * 1024;

/// Records per batch, bounded below the `u16` wire limit and client work limit.
const MAX_BATCH_RECORDS: usize = 4096;

/// One client cache entry. Payload comparison makes hash collisions harmless.
struct Cached {
    digest: u64,
    format: u8,
    w: u16,
    h: u16,
    data: Vec<u8>,
}

/// Server-owned content lookup and round-robin client slot assignment.
struct Slots {
    /// `SLOT_COUNT` entries, `None` until first used.
    entries: Vec<Option<Cached>>,
    /// Content digest -> slot, for the lookup. Only ever holds digests that are
    /// live in `entries`.
    index: std::collections::HashMap<u64, u16>,
    /// The next slot to claim. Round robin: the oldest *assignment* is evicted,
    /// which needs no per-hit bookkeeping and cannot develop a pathology worse
    /// than sending a payload that would have been a reference.
    next: u16,
}

impl Default for Slots {
    fn default() -> Self {
        Self {
            entries: (0..batch::SLOT_COUNT).map(|_| None).collect(),
            index: std::collections::HashMap::new(),
            next: 0,
        }
    }
}

/// How a tile should be written.
enum Placed {
    /// The client already holds these bytes here: seven bytes instead of a payload.
    Ref(u16),
    /// Send the payload and have the client remember it in this slot.
    Store(u16),
    /// Send the payload; not worth a slot.
    Plain,
}

impl Slots {
    fn place(&mut self, tile: &Tile) -> Placed {
        if tile.data.len() > batch::MAX_CACHED_BYTES {
            return Placed::Plain;
        }
        // The dimensions and format are hashed with the payload: a reference
        // carries only a position, so the client redraws the remembered tile at
        // its remembered size, and two tiles must never share a slot unless they
        // agree on all three.
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        hasher.update(&[tile.format]);
        hasher.update(&tile.w.to_le_bytes());
        hasher.update(&tile.h.to_le_bytes());
        hasher.update(&tile.data);
        let digest = hasher.digest();

        if let Some(&slot) = self.index.get(&digest) {
            let held = self.entries[usize::from(slot)].as_ref();
            if held.is_some_and(|c| {
                c.format == tile.format && c.w == tile.w && c.h == tile.h && c.data == tile.data
            }) {
                return Placed::Ref(slot);
            }
        }

        let slot = self.next;
        self.next = (self.next + 1) % batch::SLOT_COUNT;
        if let Some(evicted) = self.entries[usize::from(slot)].take() {
            self.index.remove(&evicted.digest);
        }
        self.index.insert(digest, slot);
        self.entries[usize::from(slot)] = Some(Cached {
            digest,
            format: tile.format,
            w: tile.w,
            h: tile.h,
            data: tile.data.clone(),
        });
        Placed::Store(slot)
    }

    /// Forget everything, because the client has.
    fn clear(&mut self) {
        self.entries.iter_mut().for_each(|slot| *slot = None);
        self.index.clear();
        self.next = 0;
    }
}

/// One record of the batch being built.
///
/// Two kinds rather than one with a flag, because the whole difference is that the
/// rules below apply to one of them and not the other. A tile may be cached,
/// referenced, and dropped when something covers it — all three sound reasoning
/// about *pixels*, and all three wrong about a frame whose meaning is "what changed
/// since the last one".
enum Record {
    Tile(Tile),
    Video(VideoUnit),
}

impl Record {
    fn record_len(&self) -> usize {
        match self {
            Record::Tile(tile) => tile.record_len(),
            Record::Video(unit) => unit.record_len(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireError {
    #[error("screen batch sequence exhausted; reconnect the attachment")]
    SequenceExhausted,
}

/// Per-attachment encoder for the server -> client direction.
pub struct Wire {
    /// Records accumulated for the batch currently being built.
    ///
    /// Held as records rather than serialized on arrival so a later tile can still
    /// displace an earlier one it covers. Serializing happens once, at flush, so
    /// this costs no extra copy — only the ability to change one's mind.
    ///
    /// Access units sit in the same list as the tiles rather than beside them,
    /// because their order *against* the tiles is load-bearing: a settled region is
    /// restored by a still tile that has to land after the last access unit covering
    /// it, not before.
    pending: Vec<Record>,
    /// What `pending` will serialize to, so the byte cap can be checked without
    /// serializing to find out.
    ///
    /// An upper bound rather than exact: a tile that turns out to be a reference at
    /// flush costs seven bytes instead of its payload, so a batch can come out
    /// smaller than this predicted. Bounding high is the safe direction for a cap.
    pending_bytes: usize,
    slots: Slots,
    /// Sequence written into the next screen batch. Per attachment because a
    /// `Wire` belongs to one attachment; starts at one so zero can remain the
    /// conspicuous value in malformed or hand-built frames.
    next_sequence: u32,
    pub totals: Totals,
}

impl Default for Wire {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            pending_bytes: 0,
            slots: Slots::default(),
            next_sequence: 1,
            totals: Totals::default(),
        }
    }
}

impl Wire {
    /// Encode a run of messages, in order, into the frames to write.
    ///
    /// "Run" means everything the caller had available at once. Whatever is
    /// returned is complete: no records are retained for a later call, so a batch
    /// can never sit waiting for traffic that never comes.
    ///
    /// Sequence exhaustion is terminal for this attachment. The caller must
    /// close it so a reattachment creates a fresh `Wire`; this encoder never
    /// wraps or reuses a sequence.
    pub fn encode(
        &mut self,
        run: impl IntoIterator<Item = ServerMsg>,
    ) -> Result<Vec<WireFrame>, WireError> {
        let mut frames = Vec::new();
        for msg in run {
            match msg.text_frame() {
                // A control message: flush what is pending so the client applies
                // the tiles that preceded it before the state change.
                Some(json) => {
                    self.flush(&mut frames)?;
                    self.totals.text(json.len());
                    frames.push(WireFrame::Text(json));
                }
                // Tiles and audio are the two without a text encoding. Matched
                // rather than assumed: this runs on the socket's own task, so a
                // variant added later without a `text_frame` arm should cost that
                // one message, not the whole attachment.
                None => match msg {
                    ServerMsg::Tile(tile) => self.push_tile(tile, &mut frames)?,
                    ServerMsg::Video(unit) => self.push(Record::Video(unit), &mut frames)?,
                    // Audio has no pixel-order dependency, so do not delay it
                    // behind the current tile batch.
                    ServerMsg::Audio(packets) => {
                        let frame = protocol::audio::frame(&packets);
                        self.totals.audio(frame.len(), packets.len());
                        frames.push(WireFrame::Audio(frame));
                    }
                    other => log::warn!("wire: dropping {other:?}, which has no encoding"),
                },
            }
        }
        self.flush(&mut frames)?;
        Ok(frames)
    }

    fn push_tile(&mut self, tile: Tile, frames: &mut Vec<WireFrame>) -> Result<(), WireError> {
        // Before the cap is consulted, because dropping covered tiles is what
        // makes room and a flush that was not needed costs a frame.
        self.supersede(&tile);
        self.push(Record::Tile(tile), frames)
    }

    fn push(&mut self, record: Record, frames: &mut Vec<WireFrame>) -> Result<(), WireError> {
        let len = record.record_len();
        if !self.pending.is_empty()
            && (self.pending_bytes + len > MAX_BATCH_BYTES
                || self.pending.len() >= MAX_BATCH_RECORDS)
        {
            self.flush(frames)?;
        }
        self.pending_bytes += len;
        self.pending.push(record);
        Ok(())
    }

    /// Drop pending tiles that `tile` completely covers.
    ///
    /// Sound only because every record in a frame is applied in order before
    /// anything is presented, so a covered paint is one nothing could have seen.
    /// A partial overlap is left alone: the uncovered part is still owed.
    ///
    /// An access unit is outside the coverage relation entirely — it neither
    /// supersedes nor is superseded — because "nothing could have seen it" is true of
    /// pixels and false of an inter-frame stream. A dropped access unit is one the
    /// decoder needed, and everything after it decodes wrongly until the next
    /// keyframe. That case is not hypothetical: under `render_type = "video"` every
    /// unit covers the whole framebuffer, so each one covers its predecessor exactly.
    /// It follows from the record kinds here, which is the point of them: the
    /// question is not asked rather than answered carefully.
    fn supersede(&mut self, tile: &Tile) {
        let (right, bottom) = (
            u32::from(tile.x) + u32::from(tile.w),
            u32::from(tile.y) + u32::from(tile.h),
        );
        let mut dropped_bytes = 0usize;
        let mut dropped = 0u64;
        self.pending.retain(|old| {
            let Record::Tile(old) = old else {
                return true;
            };
            let covered = old.x >= tile.x
                && old.y >= tile.y
                && u32::from(old.x) + u32::from(old.w) <= right
                && u32::from(old.y) + u32::from(old.h) <= bottom;
            if covered {
                dropped += 1;
                dropped_bytes += old.record_len();
            }
            !covered
        });
        self.pending_bytes -= dropped_bytes;
        self.totals.superseded += dropped;
        self.totals.superseded_bytes += dropped_bytes as u64;
    }

    /// Emit the pending batch, if there is one.
    fn flush(&mut self, frames: &mut Vec<WireFrame>) -> Result<(), WireError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let sequence = self.next_sequence;
        self.next_sequence = sequence
            .checked_add(1)
            .ok_or(WireError::SequenceExhausted)?;
        let mut frame = Vec::with_capacity(batch::HEADER_LEN + self.pending_bytes);
        frame.push(batch::FRAME_KIND);
        frame.push(0); // flags
        frame.extend_from_slice(&(self.pending.len() as u16).to_le_bytes());
        frame.extend_from_slice(&sequence.to_le_bytes());
        for record in self.pending.drain(..) {
            let tile = match record {
                Record::Video(unit) => {
                    self.totals.video(unit.record_len());
                    unit.write_record(&mut frame);
                    continue;
                }
                Record::Tile(tile) => tile,
            };
            match self.slots.place(&tile) {
                Placed::Ref(slot) => {
                    self.totals.tile_ref(tile.record_len());
                    protocol::write_tile_ref(slot, tile.x, tile.y, &mut frame);
                }
                Placed::Store(slot) => {
                    self.totals.tile(tile.record_len());
                    tile.write_record(slot, &mut frame);
                }
                Placed::Plain => {
                    self.totals.tile(tile.record_len());
                    tile.write_record(batch::NO_SLOT, &mut frame);
                }
            }
        }
        self.pending_bytes = 0;
        self.totals.frame(frame.len());
        frames.push(WireFrame::Batch { sequence, bytes: frame });
        Ok(())
    }

    /// Forget every slot, because the client says it lost them.
    ///
    /// The only way back from a client that could not decode a tile it was told to
    /// remember: the server believes the slot is full, the client knows it is
    /// empty, and nothing else would ever correct that. Spurious calls are free —
    /// the next tiles are simply sent whole.
    pub fn reset_cache(&mut self) {
        self.slots.clear();
        self.totals.cache_resets += 1;
    }
}

/// Per-attachment wire counters logged on teardown.
#[derive(Default)]
pub struct Totals {
    pub binary_frames: u64,
    pub binary_bytes: u64,
    pub tiles: u64,
    pub tile_bytes: u64,
    pub text_frames: u64,
    pub text_bytes: u64,
    pub largest_binary: u64,
    pub superseded: u64,
    pub superseded_bytes: u64,
    /// References sent, and what their payloads would have cost.
    pub refs: u64,
    pub refs_saved_bytes: u64,
    pub cache_resets: u64,
    /// Access units and their record bytes, counted apart from tiles because none of
    /// the numbers beside them can apply: a `VIDEO` record is never a reference,
    /// never superseded, and never in a slot. Folding them into `tiles` would flatter
    /// every ratio the cache is judged by.
    pub video: u64,
    pub video_bytes: u64,
    /// Audio packets and their binary-frame bytes. Separate from tile traffic, and now
    /// on a separate socket too — so on any one `Wire` these and the tile counters are
    /// mutually exclusive.
    pub audio_frames: u64,
    pub audio_packets: u64,
    pub audio_bytes: u64,
}

impl Totals {
    fn frame(&mut self, len: usize) {
        self.binary_frames += 1;
        self.binary_bytes += len as u64;
        self.largest_binary = self.largest_binary.max(len as u64);
    }

    fn audio(&mut self, len: usize, packets: usize) {
        self.audio_frames += 1;
        self.audio_packets += packets as u64;
        self.audio_bytes += len as u64;
        // Audio frames are binary frames too: the ceiling a client's WebSocket
        // message limit is measured against has to include them.
        self.binary_frames += 1;
        self.binary_bytes += len as u64;
        self.largest_binary = self.largest_binary.max(len as u64);
    }

    fn tile(&mut self, len: usize) {
        self.tiles += 1;
        self.tile_bytes += len as u64;
    }

    fn video(&mut self, len: usize) {
        self.video += 1;
        self.video_bytes += len as u64;
    }

    fn tile_ref(&mut self, would_have_been: usize) {
        self.refs += 1;
        self.tile_bytes += batch::TILE_REF_LEN as u64;
        self.refs_saved_bytes += (would_have_been - batch::TILE_REF_LEN) as u64;
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
            "{} binary frames / {} bytes carrying {} tile records / {} bytes \
             and {} video records / {} bytes, \
             {} text frames / {} bytes, largest binary {} bytes, \
             {} superseded / {} bytes, \
             {} cache refs saving {} bytes, {} cache resets, \
             {} audio frames / {} bytes carrying {} opus packets",
            self.binary_frames,
            self.binary_bytes,
            self.tiles,
            self.tile_bytes,
            self.video,
            self.video_bytes,
            self.text_frames,
            self.text_bytes,
            self.largest_binary,
            self.superseded,
            self.superseded_bytes,
            self.refs,
            self.refs_saved_bytes,
            self.cache_resets,
            self.audio_frames,
            self.audio_bytes,
            self.audio_packets,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Tile, UNSCALED, VideoUnit};

    fn tile(y: u16, bytes: usize) -> ServerMsg {
        rect(0, y, 320, 64, bytes)
    }

    /// A tile whose payload is unique to its position, so a test about batching is
    /// not quietly testing the tile cache instead. Use [`repeat`] for that.
    fn rect(x: u16, y: u16, w: u16, h: u16, bytes: usize) -> ServerMsg {
        let mut data = vec![7u8; bytes];
        let stamp = [x.to_le_bytes(), y.to_le_bytes()].concat();
        data[..stamp.len().min(bytes)].copy_from_slice(&stamp[..stamp.len().min(bytes)]);
        ServerMsg::Tile(Tile {
            format: Tile::FORMAT_PNG,
            x,
            y,
            w,
            h,
            data,
        })
    }

    /// The same payload as some other tile, at `(x, y)` — what the cache is for.
    fn repeat(x: u16, y: u16, bytes: usize) -> ServerMsg {
        ServerMsg::Tile(Tile {
            format: Tile::FORMAT_PNG,
            x,
            y,
            w: 320,
            h: 64,
            data: vec![9u8; bytes],
        })
    }

    fn resize() -> ServerMsg {
        ServerMsg::Resize {
            w: 1600,
            h: 1000,
            scale: UNSCALED,
        }
    }

    /// A parsed batch record: `(op, slot, x, y, w, h, payload_len, format)`. A
    /// reference carries no size, payload or format, so those come back zero; a
    /// `VIDEO` record puts its stream id where a tile's slot goes and its flags byte
    /// where a tile's format goes.
    type Parsed = (u8, u16, u16, u16, u16, u16, usize, u8);

    fn records(frame: &[u8]) -> Vec<Parsed> {
        assert_eq!(frame[0], batch::FRAME_KIND);
        assert_eq!(frame[1], 0, "flags must be zero");
        let count = u16::from_le_bytes([frame[2], frame[3]]);
        let mut at = batch::HEADER_LEN;
        let mut out = Vec::new();
        while at < frame.len() {
            let op = frame[at];
            let le = |o: usize| u16::from_le_bytes([frame[at + o], frame[at + o + 1]]);
            match op {
                batch::OP_TILE_REF => {
                    out.push((op, le(1), le(3), le(5), 0, 0, 0, 0));
                    at += batch::TILE_REF_LEN;
                }
                batch::OP_TILE => {
                    let len = u32::from_le_bytes([
                        frame[at + 12],
                        frame[at + 13],
                        frame[at + 14],
                        frame[at + 15],
                    ]) as usize;
                    out.push((op, le(2), le(4), le(6), le(8), le(10), len, frame[at + 1]));
                    at += batch::TILE_HEADER_LEN + len;
                }
                batch::OP_VIDEO => {
                    // `op | stream | flags | x | y | w | h | u32 len`, so the length sits two
                    // bytes further in than a reader written against the pre-flags layout would
                    // look.
                    let len = u32::from_le_bytes([
                        frame[at + 11],
                        frame[at + 12],
                        frame[at + 13],
                        frame[at + 14],
                    ]) as usize;
                    out.push((
                        op,
                        u16::from(frame[at + 1]),
                        le(3),
                        le(5),
                        le(7),
                        le(9),
                        len,
                        frame[at + 2],
                    ));
                    at += batch::VIDEO_HEADER_LEN + len;
                }
                other => panic!("unknown record op {other}"),
            }
        }
        assert_eq!(at, frame.len(), "records must exactly fill the frame");
        assert_eq!(
            out.len(),
            usize::from(count),
            "the header's count must match the records present"
        );
        out
    }

    /// The packets in an audio frame, parsed independently of the writer above —
    /// a reader that shared the writer's arithmetic would agree with it whatever it
    /// did.
    fn packets(frame: &[u8]) -> Vec<Vec<u8>> {
        assert_eq!(frame[0], protocol::audio::FRAME_KIND);
        assert_eq!(frame[1], 0, "flags must be zero");
        let count = u16::from_le_bytes([frame[2], frame[3]]);
        let mut at = protocol::audio::HEADER_LEN;
        let mut out = Vec::new();
        while at < frame.len() {
            let len = usize::from(u16::from_le_bytes([frame[at], frame[at + 1]]));
            at += protocol::audio::PACKET_HEADER_LEN;
            out.push(frame[at..at + len].to_vec());
            at += len;
        }
        assert_eq!(at, frame.len(), "packets must exactly fill the frame");
        assert_eq!(out.len(), usize::from(count), "the header's count must match");
        out
    }

    fn binary(frames: &[WireFrame]) -> Vec<&Vec<u8>> {
        frames
            .iter()
            .filter_map(|f| match f {
                WireFrame::Batch { bytes, .. } | WireFrame::Audio(bytes) => Some(bytes),
                WireFrame::Text(_) => None,
            })
            .collect()
    }

    fn sequences(frames: &[WireFrame]) -> Vec<u32> {
        frames
            .iter()
            .filter_map(|f| match f {
                WireFrame::Batch { sequence, .. } => Some(*sequence),
                WireFrame::Text(_) | WireFrame::Audio(_) => None,
            })
            .collect()
    }

    // The point of the whole module: many tiles, one frame.
    #[test]
    fn a_run_of_tiles_becomes_one_frame() {
        let mut wire = Wire::default();
        let frames = wire.encode((0..8).map(|i| tile(i * 64, 100))).unwrap();
        assert_eq!(frames.len(), 1, "eight tiles must not cost eight frames");
        assert_eq!(sequences(&frames), vec![1]);
        assert_eq!(&binary(&frames)[0][4..8], &1u32.to_le_bytes());
        let records = records(binary(&frames)[0]);
        assert_eq!(records.len(), 8);
        // In order, each carrying its payload and the slot to keep it in.
        for (i, record) in records.iter().enumerate() {
            assert_eq!(record.0, batch::OP_TILE);
            assert_eq!(record.1, i as u16, "each new payload claims the next slot");
            assert_eq!(record.3, i as u16 * 64, "records keep their arrival order");
            assert_eq!(record.6, 100);
        }
        assert_eq!(wire.totals.binary_frames, 1);
        assert_eq!(wire.totals.tiles, 8);
    }

    #[test]
    fn an_exhausted_sequence_returns_an_error_without_wrapping_or_panicking() {
        let mut wire = Wire {
            next_sequence: u32::MAX,
            ..Wire::default()
        };

        assert_eq!(
            wire.encode(vec![tile(0, 100)]).unwrap_err(),
            WireError::SequenceExhausted
        );
        assert_eq!(wire.next_sequence, u32::MAX, "the sequence must not wrap");
        assert_eq!(wire.totals.binary_frames, 0, "no partial frame was emitted");
        assert_eq!(wire.pending.len(), 1, "the failed flush did not drain records");
        assert_eq!(
            wire.encode(Vec::new()).unwrap_err(),
            WireError::SequenceExhausted,
            "an exhausted attachment stays exhausted until it is replaced"
        );
    }

    // The format byte is the codec, and the wire relays it untouched: the tile
    // encoder may send PNG, JPEG, or WebP, and neither batching nor the cache may
    // drop or rewrite it.
    #[test]
    fn the_tile_format_byte_survives_encoding() {
        let mut wire = Wire::default();
        let jpeg = ServerMsg::Tile(Tile {
            format: Tile::FORMAT_JPEG,
            x: 0,
            y: 64, // clear of tile(0, ..) so neither supersedes the other
            w: 320,
            h: 64,
            data: vec![0xFFu8; 900],
        });
        let frames = wire.encode(vec![tile(0, 900), jpeg]).unwrap();
        let records = records(binary(&frames)[0]);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].7, Tile::FORMAT_PNG, "the PNG tile keeps its codec");
        assert_eq!(records[1].7, Tile::FORMAT_JPEG, "the JPEG tile keeps its codec");
    }

    // Ordering across the two frame types is load-bearing: a resize reallocates
    // the client's canvas, so tiles from before it must be sent before it.
    #[test]
    fn a_control_message_flushes_the_tiles_that_preceded_it() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![tile(0, 50), tile(64, 50), resize(), tile(0, 50)])
            .unwrap();
        assert!(
            matches!(frames[0], WireFrame::Batch { .. }),
            "the first two tiles go out before the resize"
        );
        assert!(matches!(frames[1], WireFrame::Text(_)));
        assert!(matches!(frames[2], WireFrame::Batch { .. }));
        assert_eq!(sequences(&frames), vec![1, 2]);
        assert_eq!(frames.len(), 3);
        assert_eq!(records(binary(&frames)[0]).len(), 2);
        assert_eq!(records(binary(&frames)[1]).len(), 1);
    }

    /// Audio through the same encoder, which is why it is still encoded here at all:
    /// one place turns a [`ServerMsg`] into a frame, so the two sockets cannot come to
    /// disagree about a layout, and `Totals` keeps counting audio bytes where the
    /// field measurement already reads them.
    ///
    /// A `Wire` never sees sound and pixels together any more — they are on different
    /// sockets — so what is pinned is the audio path alone: one binary frame per
    /// message, carrying its packets, and no batch invented around it.
    #[test]
    fn audio_alone_through_the_wire_is_one_binary_frame_and_no_batch() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![ServerMsg::Audio(vec![
                bytes::Bytes::from_static(&[1, 2, 3]),
                bytes::Bytes::from_static(&[4, 5]),
            ])])
            .unwrap();

        assert_eq!(frames.len(), 1, "no batch is flushed around it");
        let binary = binary(&frames);
        assert_eq!(binary[0][0], protocol::audio::FRAME_KIND);
        assert_eq!(packets(binary[0]), vec![vec![1, 2, 3], vec![4, 5]]);

        assert_eq!(wire.totals.audio_frames, 1);
        assert_eq!(wire.totals.audio_packets, 2);
        assert_eq!(
            wire.totals.binary_frames, 1,
            "an audio frame is a binary frame too, for the message-size ceiling"
        );
        assert_eq!(wire.totals.tiles, 0, "sound is not pixels");
    }

    /// A frame with no packets is still a frame a client must be able to read
    /// without special-casing: the count is what makes it unambiguous.
    #[test]
    fn an_audio_frame_carries_its_packet_count() {
        let frame = protocol::audio::frame(&[]);
        assert_eq!(frame, vec![protocol::audio::FRAME_KIND, 0, 0, 0]);
        assert!(packets(&frame).is_empty());

        // Lengths are per packet, so packets of different sizes stay separable —
        // which a concatenation with one total length would not.
        let frame = protocol::audio::frame(&[
            bytes::Bytes::from(vec![9; 300]),
            bytes::Bytes::from(vec![7; 1]),
        ]);
        assert_eq!(
            u16::from_le_bytes([frame[2], frame[3]]),
            2,
            "the count, not the byte length"
        );
        assert_eq!(packets(&frame), vec![vec![9; 300], vec![7; 1]]);
    }

    // Exceeding a client's message ceiling kills the socket rather than dropping a
    // frame, so the cap is a hard one.
    #[test]
    fn a_batch_is_split_before_it_exceeds_the_byte_cap() {
        let mut wire = Wire::default();
        let each = 100 * 1024;
        let frames = wire.encode((0..6).map(|i| tile(i * 64, each))).unwrap();
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
        let frames = wire.encode(vec![tile(0, MAX_BATCH_BYTES * 2)]).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(records(binary(&frames)[0])[0].6, MAX_BATCH_BYTES * 2);
    }

    // Nothing may be retained past the run that produced it, or a batch could sit
    // in the encoder waiting for traffic that never comes.
    #[test]
    fn nothing_is_held_back_between_runs() {
        let mut wire = Wire::default();
        assert_eq!(wire.encode(vec![tile(0, 10)]).unwrap().len(), 1);
        assert!(wire.pending.is_empty());
        assert_eq!(wire.pending_bytes, 0);
        // A run with nothing in it produces nothing, rather than an empty frame.
        assert!(wire.encode(Vec::new()).unwrap().is_empty());
        assert_eq!(wire.encode(vec![resize()]).unwrap().len(), 1);
    }

    // A tile a later one paints over completely never needed sending. Safe only
    // inside one frame, where the covered paint is one nothing could have seen.
    #[test]
    fn a_covered_tile_is_dropped_from_the_batch() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![
                rect(0, 0, 320, 64, 50),   // covered exactly
                rect(320, 0, 320, 64, 50), // beside it, untouched
                rect(0, 0, 640, 128, 50),  // covers the first, and the second
            ])
            .unwrap();
        let records = records(binary(&frames)[0]);
        assert_eq!(records.len(), 1, "both earlier tiles were painted over");
        assert_eq!((records[0].2, records[0].4), (0, 640));
        assert_eq!(wire.totals.superseded, 2);
        assert_eq!(wire.totals.tiles, 1, "only what was sent is counted as sent");
    }

    // Partial overlap is not coverage: the part sticking out is still owed.
    #[test]
    fn a_partly_overlapping_tile_keeps_both() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![
                rect(0, 0, 320, 64, 50),
                rect(160, 0, 320, 64, 50), // overlaps half of it
            ])
            .unwrap();
        assert_eq!(records(binary(&frames)[0]).len(), 2);
        assert_eq!(wire.totals.superseded, 0);
    }

    // The dangerous case, and the reason the flush rule exists. Two rects either
    // side of a resize are in different coordinate spaces, so "covers" is
    // meaningless across one — and the earlier tile has already been sent anyway.
    #[test]
    fn coverage_never_reaches_across_a_control_message() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![
                rect(0, 0, 320, 64, 50),
                resize(),
                rect(0, 0, 640, 128, 50), // would cover the first, if it could
            ])
            .unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(
            records(binary(&frames)[0]).len(),
            1,
            "the tile before the resize was already sent, so it cannot be recalled"
        );
        assert_eq!(records(binary(&frames)[1]).len(), 1);
        assert_eq!(wire.totals.superseded, 0);
    }

    // Likewise across a flush forced by the byte cap: those bytes are gone.
    //
    // The two big tiles are at different `y` on purpose — one that covered the
    // other would be dropped rather than flushed, and then this would be testing
    // supersede again instead of the boundary it is named for.
    #[test]
    fn coverage_never_reaches_across_a_flushed_batch() {
        let mut wire = Wire::default();
        let big = MAX_BATCH_BYTES - 1024;
        let frames = wire
            .encode(vec![
                rect(0, 0, 320, 64, big),
                rect(0, 64, 320, 64, big), // no overlap, so the cap forces a flush
                rect(0, 0, 640, 128, 100), // covers both, but only reaches the pending one
            ])
            .unwrap();
        let sent: usize = binary(&frames).iter().map(|f| records(f).len()).sum();
        assert_eq!(sent, 2, "the flushed tile survives, the pending one does not");
        assert_eq!(wire.totals.superseded, 1);
    }

    // Dropping is checked before the cap, so a batch that had room after the drop
    // does not pay for a frame it did not need.
    #[test]
    fn superseding_makes_room_instead_of_forcing_a_flush() {
        let mut wire = Wire::default();
        let big = MAX_BATCH_BYTES - 1024;
        let frames = wire
            .encode(vec![
                rect(0, 0, 320, 64, big),
                rect(0, 0, 640, 128, big), // covers it; must not also flush
            ])
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(records(binary(&frames)[0]).len(), 1);
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
        let frames = wire
            .encode((0..cells).map(|i| tile(i as u16 * 64, payload)))
            .unwrap();

        // v2: one WebSocket frame per tile, each with a 10-byte header. The frame
        // count is the real cost — 80 client events and 80 scheduled decodes.
        assert_eq!(frames.len(), 1, "one repaint, one frame");
        assert!(
            wire.totals.binary_bytes < (payload + 10) as u64 * cells as u64 + 4096,
            "the envelope must not cost more than the per-frame headers it replaced"
        );
    }
    // MARK: the tile cache

    // The cache's whole claim: bytes the client already has become a position.
    #[test]
    fn content_the_client_already_holds_becomes_a_reference() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![repeat(0, 0, 900), repeat(320, 0, 900)])
            .unwrap();
        let records = records(binary(&frames)[0]);

        assert_eq!(records[0].0, batch::OP_TILE, "the first copy carries payload");
        assert_eq!(records[0].1, 0, "and the slot to keep it in");
        assert_eq!(records[0].6, 900);

        assert_eq!(records[1].0, batch::OP_TILE_REF, "the second is a reference");
        assert_eq!(records[1].1, 0, "to the slot the first claimed");
        assert_eq!((records[1].2, records[1].3), (320, 0), "drawn at its own position");

        assert_eq!(wire.totals.refs, 1);
        assert_eq!(
            wire.totals.refs_saved_bytes,
            (batch::TILE_HEADER_LEN + 900 - batch::TILE_REF_LEN) as u64
        );
    }

    // A reference is seven bytes whatever it stands for, which is the point.
    #[test]
    fn a_reference_costs_seven_bytes() {
        let mut wire = Wire::default();
        let first = wire.encode(vec![repeat(0, 0, 20_000)]).unwrap();
        let second = wire.encode(vec![repeat(640, 128, 20_000)]).unwrap();
        assert_eq!(
            binary(&second)[0].len(),
            batch::HEADER_LEN + batch::TILE_REF_LEN
        );
        assert!(binary(&first)[0].len() > 20_000, "the first one paid in full");
    }

    // Content that differs at all is different content, however similar.
    #[test]
    fn a_tile_that_differs_by_one_byte_is_not_a_reference() {
        let mut wire = Wire::default();
        wire.encode(vec![repeat(0, 0, 900)]).unwrap();
        let mut changed = repeat(320, 0, 900);
        if let ServerMsg::Tile(tile) = &mut changed {
            tile.data[500] = 1;
        }
        let frames = wire.encode(vec![changed]).unwrap();
        assert_eq!(records(binary(&frames)[0])[0].0, batch::OP_TILE);
        assert_eq!(wire.totals.refs, 0);
    }

    // Same bytes, different geometry, is not the same tile: a reference carries a
    // position and nothing else, so the client would redraw it at the wrong size.
    #[test]
    fn the_same_bytes_at_a_different_size_are_not_a_reference() {
        let mut wire = Wire::default();
        wire.encode(vec![repeat(0, 0, 900)]).unwrap();
        let frames = wire
            .encode(vec![ServerMsg::Tile(Tile {
                format: Tile::FORMAT_PNG,
                x: 0,
                y: 0,
                w: 64,
                h: 320,
                data: vec![9u8; 900],
            })])
            .unwrap();
        assert_eq!(records(binary(&frames)[0])[0].0, batch::OP_TILE);
        assert_eq!(wire.totals.refs, 0);
    }

    /// An access unit for the whole framebuffer, which is the shape a `video`
    /// target's records have; a region's differs only in its rectangle.
    fn access_unit(bytes: usize) -> ServerMsg {
        region_unit(0, 0, 0, 1280, 800, bytes)
    }

    fn region_unit(stream: u8, x: u16, y: u16, w: u16, h: u16, bytes: usize) -> ServerMsg {
        ServerMsg::Video(VideoUnit {
            stream,
            x,
            y,
            w,
            h,
            keyframe: false,
            data: vec![4u8; bytes],
        })
    }

    /// The unit a decoder may start from, which the flags byte is the only record of.
    fn keyframe_unit(stream: u8, bytes: usize) -> ServerMsg {
        ServerMsg::Video(VideoUnit {
            stream,
            x: 0,
            y: 0,
            w: 1280,
            h: 800,
            keyframe: true,
            data: vec![5u8; bytes],
        })
    }

    /// The keyframe bit, written and read back at the offset the layout puts it — the one
    /// field of a `VIDEO` record a client cannot recover from the payload, because VP9 has
    /// no parameter sets to read it out of. A flags byte in the wrong place would still
    /// parse: `stream` would read as a plausible id and the rectangle as plausible
    /// coordinates, so the bit itself is what has to be asserted.
    #[test]
    fn a_keyframes_flag_survives_the_wire_and_a_delta_frames_absence_does_too() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![
                keyframe_unit(1, 700),
                region_unit(1, 0, 0, 1280, 800, 600),
            ])
            .unwrap();
        let records = records(binary(&frames)[0]);
        assert_eq!(records.len(), 2);
        assert_eq!((records[0].0, records[0].1), (batch::OP_VIDEO, 1));
        assert_eq!(records[0].7, batch::VIDEO_KEYFRAME, "the keyframe bit did not survive");
        assert_eq!(records[0].6, 700, "the payload length is where the flags byte leaves it");
        assert_eq!(records[1].7, 0, "a delta frame must not claim a decoder can start there");
        assert_eq!(records[1].6, 600);
    }

    // Identical bytes are the cache's trigger, and two access units may be identical
    // by accident where two frames of a still screen encode the same. A reference
    // would tell the client to redraw a picture it does not have, from a payload
    // whose meaning was "what changed since the one before it". It is a different
    // record now, so the cache never sees one — this is that, asserted.
    #[test]
    fn an_access_unit_is_never_cached_or_referenced() {
        let mut wire = Wire::default();
        wire.encode(vec![access_unit(900)]).unwrap();
        let frames = wire.encode(vec![access_unit(900)]).unwrap();
        let records = records(binary(&frames)[0]);
        assert_eq!(records[0].0, batch::OP_VIDEO, "an access unit came back as a reference");
        assert_eq!(wire.totals.refs, 0);
        assert_eq!(wire.totals.video, 2, "access units are counted apart from tiles");
        assert_eq!(wire.totals.tiles, 0);
    }

    // The corruption case, stated as a test. Under video every unit covers the whole
    // framebuffer, so each covers its predecessor exactly — and coverage is sound
    // reasoning about pixels nobody could have seen, not about a frame the decoder
    // needs in order to make sense of the next one.
    #[test]
    fn an_access_unit_is_neither_dropped_nor_drops_anything() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![access_unit(900), access_unit(910)])
            .unwrap();
        assert_eq!(records(binary(&frames)[0]).len(), 2, "one access unit superseded another");
        assert_eq!(wire.totals.superseded, 0);

        // And in the other direction: a still tile covering the same rectangle must
        // not take an access unit with it either. That is the cleanup a settled
        // region gets, and it lands *after* the unit it replaces.
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![access_unit(900), rect(0, 0, 1280, 800, 900)])
            .unwrap();
        let records = records(binary(&frames)[0]);
        assert_eq!(records.len(), 2, "a still tile dropped an access unit");
        assert_eq!(records[0].0, batch::OP_VIDEO, "the cleanup overtook what it replaces");
        assert_eq!(records[1].0, batch::OP_TILE);
        assert_eq!(wire.totals.superseded, 0);
    }

    // A region stream's record carries its own rectangle and its stream id, and
    // several may share a batch — which is the whole difference from the shape the
    // whole-desktop transport had.
    #[test]
    fn several_regions_ride_one_batch_each_with_its_own_rectangle() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![
                region_unit(0, 320, 64, 640, 128, 500),
                region_unit(1, 1280, 512, 320, 64, 300),
            ])
            .unwrap();
        let records = records(binary(&frames)[0]);
        assert_eq!(records.len(), 2);
        assert_eq!((records[0].0, records[0].1), (batch::OP_VIDEO, 0), "stream 0");
        assert_eq!((records[0].2, records[0].3), (320, 64), "at its own position");
        assert_eq!((records[0].4, records[0].5), (640, 128), "at its own size");
        assert_eq!(records[0].6, 500);
        assert_eq!((records[1].0, records[1].1), (batch::OP_VIDEO, 1), "stream 1");
        assert_eq!((records[1].2, records[1].3), (1280, 512));
        assert_eq!(wire.totals.video, 2);
        assert_eq!(
            wire.totals.video_bytes,
            (2 * batch::VIDEO_HEADER_LEN + 800) as u64
        );
    }

    // Tiles either side of an access unit still supersede each other: putting a
    // record between them must not make the cache stupider, only safer.
    #[test]
    fn an_access_unit_between_two_tiles_does_not_stop_them_superseding() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![
                rect(0, 0, 320, 64, 400),
                access_unit(500),
                rect(0, 0, 640, 128, 600),
            ])
            .unwrap();
        let records = records(binary(&frames)[0]);
        assert_eq!(records.len(), 2, "the covered tile was not dropped");
        assert_eq!(records[0].0, batch::OP_VIDEO);
        assert_eq!(records[1].0, batch::OP_TILE);
        assert_eq!(wire.totals.superseded, 1);
    }

    // A slot spent on one screen-sized payload is a slot not spent on the dozens of
    // small tiles a returning menu is made of.
    #[test]
    fn a_payload_too_large_for_a_slot_is_sent_uncached() {
        let mut wire = Wire::default();
        let huge = batch::MAX_CACHED_BYTES + 1;
        let first = wire.encode(vec![repeat(0, 0, huge)]).unwrap();
        assert_eq!(records(binary(&first)[0])[0].1, batch::NO_SLOT);

        let second = wire.encode(vec![repeat(0, 0, huge)]).unwrap();
        assert_eq!(
            records(binary(&second)[0])[0].0,
            batch::OP_TILE,
            "never cached, so never referenced"
        );
        assert_eq!(wire.totals.refs, 0);
    }

    // Round robin, and the eviction has to reach the index as well as the slot: a
    // digest left behind would produce a reference to content the client replaced.
    #[test]
    fn a_slot_reused_for_new_content_stops_being_referenced() {
        let mut wire = Wire::default();
        let oldest = repeat(0, 0, 200);
        wire.encode(vec![oldest.clone()]).unwrap();
        // Fill every remaining slot with something else, so slot 0 is reused.
        wire
            .encode((1..batch::SLOT_COUNT).map(|i| rect(i, 0, 320, 64, 200)))
            .unwrap();
        wire.encode(vec![rect(0, 64, 320, 64, 200)]).unwrap();

        let frames = wire.encode(vec![oldest]).unwrap();
        assert_eq!(
            records(binary(&frames)[0])[0].0,
            batch::OP_TILE,
            "the client no longer holds it, so it must be sent again"
        );
    }

    // The recovery path. A client that could not decode a cached tile says so, and
    // everything after that has to arrive whole — the server cannot know which slot
    // is the bad one, and guessing is what a reset avoids.
    #[test]
    fn a_cache_reset_sends_payloads_again() {
        let mut wire = Wire::default();
        wire.encode(vec![repeat(0, 0, 900)]).unwrap();
        assert_eq!(
            records(binary(&wire.encode(vec![repeat(0, 0, 900)]).unwrap())[0])[0].0,
            batch::OP_TILE_REF
        );

        wire.reset_cache();

        let frames = wire.encode(vec![repeat(0, 0, 900)]).unwrap();
        let records = records(binary(&frames)[0]);
        assert_eq!(records[0].0, batch::OP_TILE);
        assert_eq!(records[0].1, 0, "and slot numbering starts over");
        assert_eq!(wire.totals.cache_resets, 1);
    }

    // A reference inside the same batch as the tile it names is fine: records are
    // applied in order, so the client stores it before it is asked to reuse it.
    #[test]
    fn a_reference_may_share_a_batch_with_the_tile_it_names() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![
                repeat(0, 0, 900),
                repeat(0, 64, 900),
                repeat(0, 128, 900),
            ])
            .unwrap();
        assert_eq!(frames.len(), 1);
        let records = records(binary(&frames)[0]);
        assert_eq!(records[0].0, batch::OP_TILE);
        assert_eq!(records[1].0, batch::OP_TILE_REF);
        assert_eq!(records[2].0, batch::OP_TILE_REF);
    }

    // Superseding happens before a slot is claimed, so a tile that never went out
    // cannot leave the server believing the client has it.
    #[test]
    fn a_superseded_tile_claims_no_slot() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![
                repeat(0, 0, 200),        // covered by the next one, never sent
                rect(0, 0, 640, 128, 200),
            ])
            .unwrap();
        assert_eq!(records(binary(&frames)[0]).len(), 1);

        // The covered payload arrives for real now. If it had claimed a slot while
        // being dropped, this would come back as a reference to a tile the client
        // never received.
        let frames = wire.encode(vec![repeat(0, 256, 200)]).unwrap();
        assert_eq!(records(binary(&frames)[0])[0].0, batch::OP_TILE);
    }
}
