//! Per-attachment conversion from [`ServerMsg`] to WebSocket frames. Control
//! messages flush the pending batch to preserve ordering against the access units
//! around them, and batches are bounded.

use crate::protocol::{self, ServerMsg, VideoUnit, WireFrame, batch};

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
    /// Access units accumulated for the batch currently being built. Every one is
    /// sent, in order: each is a link in a chain, and a dropped one decodes wrongly
    /// until the next keyframe.
    pending: Vec<VideoUnit>,
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
                // Video and audio are the two without a text encoding. Matched
                // rather than assumed: this runs on the socket's own task, so a
                // variant added later without a `text_frame` arm should cost that
                // one message, not the whole attachment.
                None => match msg {
                    ServerMsg::Video(unit) => self.push(unit, &mut frames)?,
                    // Audio has no pixel-order dependency, so do not delay it
                    // behind the current batch.
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

    fn push(&mut self, unit: VideoUnit, frames: &mut Vec<WireFrame>) -> Result<(), WireError> {
        let len = unit.record_len();
        if !self.pending.is_empty()
            && (self.pending_bytes + len > MAX_BATCH_BYTES
                || self.pending.len() >= MAX_BATCH_RECORDS)
        {
            self.flush(frames)?;
        }
        self.pending_bytes += len;
        self.pending.push(unit);
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
        // Each unit's share of the queue budget moves to the batch, which is where
        // its bytes are from here on.
        let mut held = Vec::with_capacity(self.pending.len());
        for unit in self.pending.drain(..) {
            self.totals.video(unit.record_len());
            unit.write_record(&mut frame);
            held.push(unit.held);
        }
        self.pending_bytes = 0;
        self.totals.frame(frame.len());
        frames.push(WireFrame::Batch { sequence, bytes: frame, held });
        Ok(())
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

    fn text(&mut self, len: usize) {
        self.text_frames += 1;
        self.text_bytes += len as u64;
    }
}

impl std::fmt::Display for Totals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} binary frames / {} bytes carrying {} video records / {} bytes, \
             {} text frames / {} bytes, largest binary {} bytes, \
             {} audio frames / {} bytes carrying {} opus packets",
            self.binary_frames,
            self.binary_bytes,
            self.video,
            self.video_bytes,
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
        ServerMsg::Video(VideoUnit { w: 1600, h: 1000, keyframe, data, held: Held::default() })
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
