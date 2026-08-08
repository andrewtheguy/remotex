//! Hand the remote's PCM to the client untouched.
//!
//! No encoder and no resampler: the bytes a wave buffer arrives in are the bytes
//! that go on the wire, and the client turns them straight into an `AudioBuffer`.
//! [`crate::pcm48`] is not in this path at all — resampling 44.1 kHz to 48 would
//! be arithmetic on every sample, which is the one thing this option exists to
//! avoid.
//!
//! It costs the whole of what a codec was saving: 1.41 Mbit/s against Opus's 96
//! kbit/s, and the [`PcmFormat::byte_rate`] doc calls that the number both codecs
//! exist to shrink. What it buys is that nothing between the remote's mixer and
//! the browser's output examines a sample. Guacamole ships only this — its one
//! encoder is `raw_encoder.c`, emitting `audio/L16;rate=44100,channels=2` — which
//! is the existence proof that a desktop gateway can carry sound this way.
//!
//! One packet per wave buffer, matching the remote's own pacing rather than a
//! codec's granule, and again that is Guacamole's arrangement: it flushes per
//! wave PDU. There is nothing to accumulate for, so accumulating would only add
//! delay.

use bytes::Bytes;

use crate::audio::PcmFormat;

/// What the client is told this is.
///
/// Not a WebCodecs codec string — WebCodecs has no PCM decoder, and this path
/// deliberately does not reach one. The name spells the sample format out because
/// it is the one thing `audioFormat`'s other fields do not carry: signed 16-bit
/// little-endian, interleaved. An 8-bit or float stream would be a different name
/// here rather than the same name silently misread, which is the same reason
/// Guacamole distinguishes `audio/L8` from `audio/L16`.
pub const PCM_CODEC: &str = "pcm-s16le";

/// Bits per sample this carries. The gateway advertises exactly one RDPSND format
/// ([`crate::audio::PCM_CD_QUALITY`]) and it is 16-bit, so this is a check on that
/// promise rather than a branch: anything else is refused at construction instead
/// of being mislabelled by [`PCM_CODEC`].
const BITS_PER_SAMPLE: u16 = 16;

/// Ceiling on one packet, because the wire writes a packet's length as a `u16`
/// ([`crate::protocol::audio::frame`], which panics rather than truncate).
///
/// An encoder makes this unreachable by construction — libopus caps a packet at
/// 4000 bytes — but a passthrough packet is a whole wave buffer, and how large
/// those get is the remote's business, not this gateway's. The tested Windows
/// host sends 32 KiB, comfortably under; a host that sent 64 would otherwise take
/// the process down, since this binary is `panic = "abort"`.
///
/// Splitting is not a compromise of the passthrough claim: the samples are
/// untouched and in order, and a client that plays two consecutive packets hears
/// exactly what one would have been. The rounding below keeps each piece on a
/// frame boundary.
const MAX_PACKET_BYTES: usize = u16::MAX as usize;

/// Passes wave buffers through, cut on frame boundaries.
pub struct PcmStream {
    /// Bytes in one frame across every channel: 4 at CD quality.
    block_align: usize,
    /// [`MAX_PACKET_BYTES`] rounded down to a whole frame, so a split never lands
    /// mid-sample.
    max_packet: usize,
    sample_rate: u32,
    /// A trailing part-frame, held for the next buffer.
    ///
    /// RDP has no reason to split a frame across buffers and the tested host does
    /// not, but the queue carries bytes rather than frames and nothing in the
    /// types rules it out. Shipping a part-frame would shift every channel against
    /// the others for the rest of the session — the client reads this stream as
    /// interleaved frames from byte zero — so the remainder waits here for the
    /// bytes that complete it. This is the only state in the whole path.
    carry: Vec<u8>,
    /// Frames passed through so far, which the per-buffer diagnostic in
    /// [`crate::audio`] compares against elapsed time.
    frames_encoded: u64,
}

impl PcmStream {
    /// Starts a passthrough stream for `format`.
    ///
    /// The second half of the pair is the decoder configuration, and it is empty:
    /// there is no decoder to configure. `audioFormat`'s rate and channel count
    /// are the whole description, which is what makes this the one option that
    /// needs no WebCodecs decoder to play.
    pub fn new(format: PcmFormat) -> Result<(Self, Vec<u8>), anyhow::Error> {
        anyhow::ensure!(
            format.channels > 0,
            "a pcm format with no channels cannot be carried"
        );
        anyhow::ensure!(
            format.bits_per_sample == BITS_PER_SAMPLE,
            "passthrough carries {BITS_PER_SAMPLE}-bit samples, not {}-bit",
            format.bits_per_sample
        );

        let block_align = usize::from(format.block_align());
        let stream = Self {
            block_align,
            max_packet: MAX_PACKET_BYTES - MAX_PACKET_BYTES % block_align,
            sample_rate: format.sample_rate,
            carry: Vec::new(),
            frames_encoded: 0,
        };
        Ok((stream, Vec::new()))
    }

    /// One wave buffer as one packet, or nothing when it did not complete a frame.
    ///
    /// More than one only past `MAX_PACKET_BYTES`, which no tested remote
    /// reaches. The common case copies **nothing** and touches no sample: `carry`
    /// is empty, the buffer divides by the frame size, and the packet *is* the
    /// buffer — a refcounted slice of the [`Bytes`] the bridge already holds. The
    /// only path that copies is a remote splitting a frame across buffers, which
    /// none has been seen to do.
    pub fn push(&mut self, pcm: &Bytes) -> Result<Vec<Bytes>, anyhow::Error> {
        let packet = if self.carry.is_empty() {
            let whole = pcm.len() - pcm.len() % self.block_align;
            self.carry.extend_from_slice(&pcm[whole..]);
            pcm.slice(..whole)
        } else {
            let mut joined = std::mem::take(&mut self.carry);
            joined.extend_from_slice(pcm);
            let whole = joined.len() - joined.len() % self.block_align;
            self.carry.extend_from_slice(&joined[whole..]);
            joined.truncate(whole);
            Bytes::from(joined)
        };
        if packet.is_empty() {
            return Ok(Vec::new());
        }
        self.frames_encoded += (packet.len() / self.block_align) as u64;

        if packet.len() <= self.max_packet {
            return Ok(vec![packet]);
        }
        Ok((0..packet.len())
            .step_by(self.max_packet)
            .map(|at| packet.slice(at..(at + self.max_packet).min(packet.len())))
            .collect())
    }

    /// The rate the client plays this at — the remote's own, because nothing here
    /// changed it. The only codec for which this is not 48 kHz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Zero, meaning a packet's length is its own.
    ///
    /// Every other codec has a granule and the client needs to be told it. This
    /// one takes its packet size from the remote, so there is no single number to
    /// send — and none is needed, since a PCM packet's frame count is its byte
    /// count divided by the frame size the client already has.
    pub fn packet_frames(&self) -> u32 {
        0
    }

    /// Frames passed through so far.
    pub fn frames_encoded(&self) -> u64 {
        self.frames_encoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::PCM_CD_QUALITY;

    #[test]
    fn a_wave_buffer_arrives_on_the_wire_byte_for_byte() {
        let (mut stream, head) = PcmStream::new(PCM_CD_QUALITY).expect("a passthrough stream");
        assert!(head.is_empty(), "there is no decoder to configure");

        // A recognisable buffer rather than silence: this test is the whole claim
        // of the option, so it has to be able to see a sample that changed.
        let mut wave = Vec::new();
        for frame in 0..882i16 {
            wave.extend_from_slice(&frame.to_le_bytes());
            wave.extend_from_slice(&(-frame).to_le_bytes());
        }

        let packets = stream.push(&Bytes::from(wave.clone())).expect("push");
        assert_eq!(packets.len(), 1, "one buffer in, one packet out");
        assert_eq!(packets[0], wave, "not a sample of it was touched");
        assert_eq!(stream.frames_encoded(), 882);
    }

    /// The remote's pacing, not a granule: buffer sizes that would leave an encoder
    /// holding a remainder produce a packet each here, immediately.
    #[test]
    fn every_buffer_becomes_its_own_packet_whatever_its_size() {
        let (mut stream, _head) = PcmStream::new(PCM_CD_QUALITY).expect("a stream");
        // 32768 bytes is what the tested Windows host sends, and 100 is a size no
        // codec's frame divides. Both go straight out.
        for bytes in [32_768, 400, 4] {
            let packets = stream.push(&Bytes::from(vec![7u8; bytes])).expect("push");
            assert_eq!(packets.len(), 1);
            assert_eq!(packets[0].len(), bytes);
        }
        assert_eq!(stream.frames_encoded(), (32_768 + 400 + 4) / 4);
    }

    /// A part-frame must not go out, because the client reads this stream as
    /// interleaved frames from byte zero: ship three bytes of a four-byte frame
    /// and left becomes right for the rest of the session.
    #[test]
    fn a_split_frame_is_carried_rather_than_shipped_or_dropped() {
        let (mut stream, _head) = PcmStream::new(PCM_CD_QUALITY).expect("a stream");

        let packets = stream.push(&Bytes::from_static(&[1, 2, 3])).expect("push");
        assert!(packets.is_empty(), "three bytes is not a frame yet");
        assert_eq!(stream.frames_encoded(), 0);

        let packets = stream.push(&Bytes::from_static(&[4, 5, 6])).expect("push");
        assert_eq!(
            packets,
            vec![vec![1, 2, 3, 4]],
            "the frame is completed from the held bytes, in order"
        );
        assert_eq!(stream.carry, vec![5, 6], "and the new remainder is held");
        assert_eq!(stream.frames_encoded(), 1);
    }

    /// A buffer that completes nothing must not end the stream. [`crate::audio`]
    /// reads an empty result as "keep reading", and an empty *packet* would be a
    /// buffer the client can do nothing with.
    #[test]
    fn nothing_that_goes_out_is_ever_an_empty_packet() {
        let (mut stream, _head) = PcmStream::new(PCM_CD_QUALITY).expect("a stream");
        assert!(stream.push(&Bytes::new()).expect("push").is_empty());
        assert!(stream.push(&Bytes::from_static(&[9])).expect("push").is_empty());
        assert!(stream.push(&Bytes::from_static(&[9, 9, 9])).expect("push")[0].len() == 4);
    }

    /// A buffer too large for the wire's `u16` packet length is split rather than
    /// sent, because sending it panics [`crate::protocol::audio::frame`] and this
    /// binary aborts on panic. Nothing observed sends one — the point is that
    /// whether anything ever does is the remote's choice and not this gateway's.
    #[test]
    fn a_buffer_past_the_wires_packet_length_is_split_on_a_frame_boundary() {
        let (mut stream, _head) = PcmStream::new(PCM_CD_QUALITY).expect("a stream");
        let huge: Vec<u8> = (0..200_000u32).map(|byte| byte as u8).collect();
        let packets = stream.push(&Bytes::from(huge.clone())).expect("push");

        assert!(packets.len() > 1, "one packet could not have carried it");
        for packet in &packets {
            assert!(packet.len() <= usize::from(u16::MAX), "fits the length field");
            assert_eq!(packet.len() % 4, 0, "and lands on a frame boundary");
        }
        // The split is the *only* thing that happened: put back together, it is
        // the buffer that went in.
        assert_eq!(packets.concat(), huge);
        assert_eq!(stream.frames_encoded(), 200_000 / 4);
    }

    /// The rate is the remote's, and this is the only codec for which that is
    /// true — everything else resamples to 48 kHz on the way to an encoder.
    #[test]
    fn the_client_is_told_the_remotes_own_rate() {
        let (stream, _head) = PcmStream::new(PCM_CD_QUALITY).expect("a stream");
        assert_eq!(stream.sample_rate(), 44_100);
        assert_ne!(stream.sample_rate(), crate::pcm48::SAMPLE_RATE);
        assert_eq!(stream.packet_frames(), 0, "packets describe their own length");
    }

    /// [`PCM_CODEC`] names a 16-bit sample format, so a stream that is not 16-bit
    /// has to be refused rather than labelled with it.
    #[test]
    fn a_sample_format_the_codec_name_does_not_describe_is_refused() {
        let eight_bit = PcmFormat {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 8,
        };
        assert!(PcmStream::new(eight_bit).is_err());

        let no_channels = PcmFormat {
            channels: 0,
            sample_rate: 44_100,
            bits_per_sample: 16,
        };
        assert!(PcmStream::new(no_channels).is_err());
    }
}
