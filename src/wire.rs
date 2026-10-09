//! Per-attachment conversion from [`ServerMsg`] to WebSocket frames. Control
//! messages flush the pending batch to preserve ordering against the records
//! around them, and batches are bounded.

use crate::protocol::{self, GraphicsUnit, ServerMsg, VideoUnit, WireFrame, batch};

/// Record bytes per batch, below client WebSocket limits and large enough to
/// amortize per-frame overhead.
const MAX_BATCH_BYTES: usize = 256 * 1024;

/// Records per batch, bounded below the `u16` wire limit and client work limit.
const MAX_BATCH_RECORDS: usize = 4096;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireError {
    #[error("screen batch sequence exhausted; reconnect the attachment")]
    SequenceExhausted,
}

/// Per-attachment encoder for the server -> client direction.
pub struct Wire {
    /// Records accumulated for the batch currently being built. Every one is sent,
    /// in order: an access unit is a link in a chain, and a dropped one decodes
    /// wrongly until the next keyframe.
    pending: Vec<Record>,
    /// What `pending` will serialize to, so the byte cap can be checked without
    /// serializing to find out.
    pending_bytes: usize,
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
                // the units that preceded it before the state change.
                Some(json) => {
                    self.flush(&mut frames)?;
                    self.totals.text(json.len());
                    frames.push(WireFrame::Text(json));
                }
                // Pictures and audio are the ones without a text encoding. Matched
                // rather than assumed: this runs on the socket's own task, so a
                // variant added later without a `text_frame` arm should cost that
                // one message, not the whole attachment.
                None => match msg {
                    ServerMsg::Video(unit) => self.push(Record::Video(unit), &mut frames)?,
                    // A frame's last run is its batch's last record: the browser
                    // shows a batch once it has drawn all of it, and acknowledges
                    // it then, which is this frame's acknowledgement to the host.
                    ServerMsg::Graphics(unit) => {
                        let ends_frame = unit.frame.is_some();
                        self.push(Record::Graphics(unit), &mut frames)?;
                        if ends_frame {
                            self.flush(&mut frames)?;
                        }
                    }
                    // Audio has no pixel-order dependency, so do not delay it
                    // behind the current batch.
                    ServerMsg::Audio(packets) => {
                        let frame = protocol::audio::frame(&packets);
                        self.totals.audio(frame.len(), packets.len());
                        frames.push(WireFrame::Audio(frame));
                    }
                    ServerMsg::AudioGap => {
                        let frame = protocol::audio::gap();
                        self.totals.audio(frame.len(), 0);
                        frames.push(WireFrame::Audio(frame));
                    }
                    other => log::warn!("wire: dropping {other:?}, which has no encoding"),
                },
            }
        }
        self.flush(&mut frames)?;
        Ok(frames)
    }

    fn push(&mut self, record: Record, frames: &mut Vec<WireFrame>) -> Result<(), WireError> {
        let len = record.len();
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
        // Each record's share of the queue budget moves to the batch, which is where
        // its bytes are from here on.
        let mut held = Vec::with_capacity(self.pending.len());
        let mut painted = Vec::new();
        for record in self.pending.drain(..) {
            held.push(match record {
                Record::Video(unit) => {
                    self.totals.video(unit.record_len());
                    unit.write_record(&mut frame);
                    unit.held
                }
                Record::Graphics(unit) => {
                    self.totals.graphics(unit.record_len());
                    unit.write_record(&mut frame);
                    painted.extend(unit.frame);
                    unit.held
                }
            });
        }
        self.pending_bytes = 0;
        self.totals.frame(frame.len());
        frames.push(WireFrame::Batch { sequence, bytes: frame, held, painted });
        Ok(())
    }
}

/// One record of a batch.
enum Record {
    Video(VideoUnit),
    Graphics(GraphicsUnit),
}

impl Record {
    fn len(&self) -> usize {
        match self {
            Record::Video(unit) => unit.record_len(),
            Record::Graphics(unit) => unit.record_len(),
        }
    }
}

/// Per-attachment wire counters logged on teardown.
#[derive(Default)]
pub struct Totals {
    pub binary_frames: u64,
    pub binary_bytes: u64,
    pub text_frames: u64,
    pub text_bytes: u64,
    pub largest_binary: u64,
    /// Access units and their record bytes.
    pub video: u64,
    pub video_bytes: u64,
    /// Runs of a passed graphics pipeline and their record bytes.
    pub graphics: u64,
    pub graphics_bytes: u64,
    /// Audio packets and their binary-frame bytes. On a separate socket, so on any
    /// one `Wire` these and the video counters are mutually exclusive.
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

    fn video(&mut self, len: usize) {
        self.video += 1;
        self.video_bytes += len as u64;
    }

    fn graphics(&mut self, len: usize) {
        self.graphics += 1;
        self.graphics_bytes += len as u64;
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
            "{} binary frames / {} bytes carrying {} video records / {} bytes \
             and {} graphics records / {} bytes, \
             {} text frames / {} bytes, largest binary {} bytes, \
             {} audio frames / {} bytes carrying {} audio packets",
            self.binary_frames,
            self.binary_bytes,
            self.video,
            self.video_bytes,
            self.graphics,
            self.graphics_bytes,
            self.text_frames,
            self.text_bytes,
            self.largest_binary,
            self.audio_frames,
            self.audio_bytes,
            self.audio_packets,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Held, UNSCALED};

    /// An access unit whose payload is `bytes` long, stamped with `seed` so a test
    /// can tell units apart after parsing.
    fn unit(seed: u8, bytes: usize, keyframe: bool) -> ServerMsg {
        let mut data = vec![7u8; bytes];
        if let Some(first) = data.first_mut() {
            *first = seed;
        }
        ServerMsg::Video(VideoUnit { w: 1600, h: 1000, strip: None, keyframe, data, held: Held::default() })
    }

    fn resize() -> ServerMsg {
        ServerMsg::Resize { w: 1600, h: 1000, scale: UNSCALED }
    }

    /// A parsed `VIDEO` record: `(flags, w, h, payload)`.
    type Parsed = (u8, u16, u16, Vec<u8>);

    /// The records of a batch, parsed independently of the writer above — a reader
    /// that shared the writer's arithmetic would agree with it whatever it did.
    fn records(frame: &[u8]) -> Vec<Parsed> {
        assert_eq!(frame[0], batch::FRAME_KIND);
        assert_eq!(frame[1], 0, "flags must be zero");
        let count = u16::from_le_bytes([frame[2], frame[3]]);
        let mut at = batch::HEADER_LEN;
        let mut out = Vec::new();
        while at < frame.len() {
            assert_eq!(frame[at], batch::OP_VIDEO, "unknown record op");
            let le = |o: usize| u16::from_le_bytes([frame[at + o], frame[at + o + 1]]);
            let len = u32::from_le_bytes([frame[at + 6], frame[at + 7], frame[at + 8], frame[at + 9]])
                as usize;
            let start = at + batch::VIDEO_HEADER_LEN;
            out.push((frame[at + 1], le(2), le(4), frame[start..start + len].to_vec()));
            at = start + len;
        }
        assert_eq!(at, frame.len(), "records must exactly fill the frame");
        assert_eq!(out.len(), usize::from(count), "the header's count must match the records present");
        out
    }

    /// The packets in an audio frame, parsed independently of the writer.
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

    /// Units that arrive together share a batch, in order, each whole.
    #[test]
    fn a_run_of_units_becomes_one_frame_in_order() {
        let mut wire = Wire::default();
        let frames = wire.encode((0..4).map(|i| unit(i, 100, i == 0))).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(sequences(&frames), vec![1]);
        assert_eq!(&binary(&frames)[0][4..8], &1u32.to_le_bytes());
        let records = records(binary(&frames)[0]);
        assert_eq!(records.len(), 4);
        for (i, (flags, w, h, payload)) in records.iter().enumerate() {
            assert_eq!(payload[0], i as u8, "records keep their arrival order");
            assert_eq!((*w, *h), (1600, 1000));
            assert_eq!(payload.len(), 100);
            let keyframe = if i == 0 { batch::VIDEO_KEYFRAME } else { 0 };
            assert_eq!(*flags, keyframe, "the keyframe bit is the encoder's, and only it");
        }
        assert_eq!(wire.totals.binary_frames, 1);
        assert_eq!(wire.totals.video, 4);
    }

    #[test]
    fn an_exhausted_sequence_returns_an_error_without_wrapping_or_panicking() {
        let mut wire = Wire { next_sequence: u32::MAX, ..Wire::default() };

        assert_eq!(wire.encode(vec![unit(0, 100, true)]).unwrap_err(), WireError::SequenceExhausted);
        assert_eq!(wire.next_sequence, u32::MAX, "the sequence must not wrap");
        assert_eq!(wire.totals.binary_frames, 0, "no partial frame was emitted");
        assert_eq!(wire.pending.len(), 1, "the failed flush did not drain records");
        assert_eq!(
            wire.encode(Vec::new()).unwrap_err(),
            WireError::SequenceExhausted,
            "an exhausted attachment stays exhausted until it is replaced"
        );
    }

    // Ordering across the two frame types is load-bearing: a resize reallocates
    // the client's canvas, so units from before it must be sent before it.
    #[test]
    fn a_control_message_flushes_the_units_that_preceded_it() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![unit(0, 50, true), unit(1, 50, false), resize(), unit(2, 50, true)])
            .unwrap();
        assert!(matches!(frames[0], WireFrame::Batch { .. }), "the first two go out before the resize");
        assert!(matches!(frames[1], WireFrame::Text(_)));
        assert!(matches!(frames[2], WireFrame::Batch { .. }));
        assert_eq!(sequences(&frames), vec![1, 2]);
        assert_eq!(frames.len(), 3);
        assert_eq!(records(binary(&frames)[0]).len(), 2);
        assert_eq!(records(binary(&frames)[1]).len(), 1);
    }

    /// Audio through the same encoder, which is why it is still encoded here at all:
    /// one place turns a [`ServerMsg`] into a frame, so the two sockets cannot come to
    /// disagree about a layout.
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
        assert_eq!(wire.totals.video, 0, "sound is not pixels");
    }

    /// A gap goes out as a frame of its own, flagged and empty, in its place
    /// among the packets: before the first that follows what was dropped.
    #[test]
    fn a_gap_is_a_flagged_frame_of_no_packets_in_its_place() {
        let mut wire = Wire::default();
        let frames = wire
            .encode(vec![
                ServerMsg::Audio(vec![bytes::Bytes::from_static(&[1])]),
                ServerMsg::AudioGap,
                ServerMsg::Audio(vec![bytes::Bytes::from_static(&[2])]),
            ])
            .unwrap();
        let binary = binary(&frames);
        assert_eq!(binary.len(), 3);
        assert_eq!(packets(binary[0]), vec![vec![1]]);
        assert_eq!(binary[1][..], [protocol::audio::FRAME_KIND, protocol::audio::GAP, 0, 0]);
        assert_eq!(packets(binary[2]), vec![vec![2]]);
        assert_eq!(wire.totals.audio_packets, 2);
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
        assert_eq!(u16::from_le_bytes([frame[2], frame[3]]), 2, "the count, not the byte length");
        assert_eq!(packets(&frame), vec![vec![9; 300], vec![7; 1]]);
    }

    // Exceeding a client's message ceiling kills the socket rather than dropping a
    // frame, so the cap is a hard one.
    #[test]
    fn a_batch_is_split_before_it_exceeds_the_byte_cap() {
        let mut wire = Wire::default();
        let each = 100 * 1024;
        let frames = wire.encode((0..6).map(|i| unit(i, each, false))).unwrap();
        assert!(frames.len() > 1, "600 KB of units cannot be one frame");
        for frame in binary(&frames) {
            assert!(
                frame.len() <= MAX_BATCH_BYTES + batch::VIDEO_HEADER_LEN + batch::HEADER_LEN,
                "frame of {} bytes exceeds the cap",
                frame.len()
            );
        }
        // Split, not dropped: every unit is still there, in order.
        let seen: Vec<u8> =
            binary(&frames).iter().flat_map(|f| records(f)).map(|r| r.3[0]).collect();
        assert_eq!(seen, (0..6).collect::<Vec<_>>());
    }

    // A keyframe larger than the cap on its own still has to be sent: a cap that
    // silently dropped it would leave the decoder with nothing to start from.
    #[test]
    fn a_single_oversized_unit_is_still_sent() {
        let mut wire = Wire::default();
        let frames = wire.encode(vec![unit(0, MAX_BATCH_BYTES * 2, true)]).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(records(binary(&frames)[0])[0].3.len(), MAX_BATCH_BYTES * 2);
    }

    // Nothing may be retained past the run that produced it, or a batch could sit
    // in the encoder waiting for traffic that never comes.
    #[test]
    fn nothing_is_held_back_between_runs() {
        let mut wire = Wire::default();
        assert_eq!(wire.encode(vec![unit(0, 10, true)]).unwrap().len(), 1);
        assert!(wire.pending.is_empty());
        assert_eq!(wire.pending_bytes, 0);
        // A run with nothing in it produces nothing, rather than an empty frame.
        assert!(wire.encode(Vec::new()).unwrap().is_empty());
        assert_eq!(wire.encode(vec![resize()]).unwrap().len(), 1);
    }

    /// A run of a passed graphics pipeline is a record in its place: whole, in order
    /// with the units around it, and behind the message that starts its pipeline.
    #[test]
    fn graphics_runs_are_records_in_their_place() {
        let run = |seed: u8, len: usize| {
            ServerMsg::Graphics(crate::protocol::GraphicsUnit { data: vec![seed; len], frame: None, held: Held::default() })
        };
        let mut wire = Wire::default();
        let frames = wire.encode(vec![ServerMsg::GraphicsStart, run(1, 5), run(2, 700), resize(), run(3, 1)]).unwrap();
        let WireFrame::Text(text) = &frames[0] else {
            panic!("the start goes out first, as text: {:?}", frames[0]);
        };
        assert_eq!(text, r#"{"type":"graphicsStart"}"#);

        // Parsed by hand: op, then a length, then the commands.
        let mut seen = Vec::new();
        for frame in binary(&frames) {
            let count = usize::from(u16::from_le_bytes([frame[2], frame[3]]));
            let mut at = batch::HEADER_LEN;
            for _ in 0..count {
                assert_eq!(frame[at], 0x04, "a GRAPHICS record");
                let len = u32::from_le_bytes([frame[at + 1], frame[at + 2], frame[at + 3], frame[at + 4]]) as usize;
                let commands = &frame[at + 5..at + 5 + len];
                assert!(commands.iter().all(|b| *b == commands[0]), "the run is whole");
                seen.push((commands[0], len));
                at += 5 + len;
            }
            assert_eq!(at, frame.len(), "records must exactly fill the frame");
        }
        assert_eq!(seen, vec![(1, 5), (2, 700), (3, 1)]);
        assert_eq!(binary(&frames).len(), 2, "the resize between them flushed the first batch");
        assert_eq!((wire.totals.graphics, wire.totals.graphics_bytes), (3, 5 + 700 + 1 + 3 * 5));
    }

    /// The run that ends a frame ends its batch, which carries what that frame is
    /// owed: the browser shows a batch once it has drawn all of it, and its
    /// acknowledgment of the batch is the frame's.
    #[test]
    fn a_frames_last_run_ends_its_batch() {
        use crate::protocol::{GraphicsUnit, Painted};
        let run = |seed: u8, ends: bool| {
            let frame = ends.then(Painted::default);
            ServerMsg::Graphics(GraphicsUnit { data: vec![seed; 4], frame, held: Held::default() })
        };
        let mut wire = Wire::default();
        let frames =
            wire.encode(vec![run(1, false), run(2, true), run(3, true), run(4, false), run(5, false)]).unwrap();
        let shape: Vec<(u16, usize)> = frames
            .iter()
            .map(|frame| match frame {
                WireFrame::Batch { bytes, painted, .. } => (u16::from_le_bytes([bytes[2], bytes[3]]), painted.len()),
                other => panic!("expected a batch, got {other:?}"),
            })
            .collect();
        assert_eq!(shape, vec![(2, 1), (1, 1), (2, 0)], "records, and frames owed, in each batch");
    }

    /// A unit's share of the queue budget rides the batch it went out in.
    #[test]
    fn a_batch_carries_every_units_queue_share() {
        let mut wire = Wire::default();
        let frames = wire.encode((0..3).map(|i| unit(i, 10, false))).unwrap();
        match &frames[0] {
            WireFrame::Batch { held, .. } => assert_eq!(held.len(), 3),
            other => panic!("expected a batch, got {other:?}"),
        }
    }
}
