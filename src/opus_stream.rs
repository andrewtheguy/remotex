//! Convert live PCM wave buffers into bare 20 ms Opus packets.
//!
//! The PCM arrives already deinterleaved and resampled to 48 kHz by
//! [`crate::pcm48`]; this is only the codec. Each listener owns fresh codec state
//! downstream of the nonblocking RDP queue; quiet remotes emit nothing.

use opus::{Application, Bitrate, Channels, Encoder};

use crate::audio::PcmFormat;
use crate::pcm48::{Pcm48, SAMPLE_RATE};

/// The WebCodecs codec string for what this produces.
pub const OPUS_CODEC: &str = "opus";

/// Frames per Opus packet, per channel: 20 ms at 48 kHz.
///
/// 20 ms is Opus's default frame and the usual choice: shorter frames spend more
/// of the bitrate on packet overhead, longer ones add latency for nothing here.
pub const FRAME_FRAMES: usize = 960;

/// Target bitrate. Stereo desktop audio including music, so this is well clear of
/// the ~64 kbps where stereo Opus starts to be audibly lossy, and still ~1/15th
/// of the PCM it replaces — which is the whole of what the other option
/// (`src/pcm_stream.rs`) gives back to avoid encoding at all.
pub const OPUS_BITRATE_BPS: i32 = 96_000;

/// Ceiling for one encoded packet. libopus documents 4000 bytes as the largest
/// worth allowing for; at this bitrate a packet is nearer 240.
const MAX_PACKET_BYTES: usize = 4000;

/// Turns PCM buffers into Opus packets.
pub struct OpusStream {
    encoder: Encoder,
    /// Deinterleave and resample, which is codec-independent.
    pcm: Pcm48,
    /// Scratch: one 20 ms frame, interleaved, as libopus wants it.
    frame: Vec<f32>,
    /// Scratch: one encoded packet.
    packet: Vec<u8>,
    /// Frames encoded so far. Nothing branches on it; it is the number the
    /// per-buffer diagnostic in [`crate::audio`] compares against elapsed time,
    /// which is how a stream drifting from real time shows itself.
    frames_encoded: u64,
}

impl OpusStream {
    /// Starts a stream for `format`, returning the encoder and the `OpusHead` bytes
    /// a decoder has to be configured with before the first packet.
    ///
    /// Fails if libopus will not encode this shape — in practice only a channel
    /// count other than 1 or 2, which the single advertised format rules out.
    pub fn new(format: PcmFormat) -> Result<(Self, Vec<u8>), anyhow::Error> {
        let channels = match format.channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            other => anyhow::bail!("opus carries 1 or 2 channels, not {other}"),
        };
        let channel_count = usize::from(format.channels);

        let mut encoder = Encoder::new(SAMPLE_RATE, channels, Application::Audio)
            .map_err(|e| anyhow::anyhow!("create the opus encoder: {e}"))?;
        encoder
            .set_bitrate(Bitrate::Bits(OPUS_BITRATE_BPS))
            .map_err(|e| anyhow::anyhow!("set the opus bitrate: {e}"))?;

        // The encoder's own delay, in 48 kHz samples. Written into `OpusHead` so a
        // decoder discards it instead of playing it as leading silence.
        let pre_skip = encoder
            .get_lookahead()
            .map_err(|e| anyhow::anyhow!("read the opus lookahead: {e}"))?
            .max(0) as u16;

        let stream = Self {
            encoder,
            pcm: Pcm48::new(format)?,
            frame: vec![0.0; FRAME_FRAMES * channel_count],
            packet: vec![0; MAX_PACKET_BYTES],
            frames_encoded: 0,
        };

        Ok((stream, opus_head(format, pre_skip)))
    }

    /// Encodes one PCM buffer into whatever whole Opus packets it completed.
    ///
    /// Empty when the buffer did not add up to a whole 20 ms frame, which is
    /// normal: the remainder is carried.
    pub fn push(&mut self, pcm: &[u8]) -> Result<Vec<Vec<u8>>, anyhow::Error> {
        self.pcm.push(pcm)?;
        let mut packets = Vec::new();
        while self.pcm.ready_frames() >= FRAME_FRAMES {
            packets.push(self.encode_one_frame()?);
        }
        Ok(packets)
    }

    /// Samples per packet at [`SAMPLE_RATE`], which is what the client turns into a
    /// packet duration.
    pub fn packet_frames(&self) -> u32 {
        FRAME_FRAMES as u32
    }

    /// Frames encoded so far.
    pub fn frames_encoded(&self) -> u64 {
        self.frames_encoded
    }

    fn encode_one_frame(&mut self) -> Result<Vec<u8>, anyhow::Error> {
        self.pcm.take_f32(FRAME_FRAMES, &mut self.frame);
        let len = self
            .encoder
            .encode_float(&self.frame, &mut self.packet)
            .map_err(|e| anyhow::anyhow!("encode an opus packet: {e}"))?;
        self.frames_encoded += 1;
        Ok(self.packet[..len].to_vec())
    }
}

/// RFC 7845 identification header carried by `AudioFormat`.
/// Opus decodes at 48 kHz; the input sample rate is metadata.
pub fn opus_head(format: PcmFormat, pre_skip: u16) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(format.channels as u8);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&format.sample_rate.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain, unchanged
    head.push(0); // channel mapping family: mono or stereo, no mapping table
    head
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::PCM_CD_QUALITY;

    /// 20 ms of 44.1 kHz stereo silence, as bytes on the queue.
    fn silence(frames: usize) -> Vec<u8> {
        vec![0u8; frames * usize::from(PCM_CD_QUALITY.block_align())]
    }

    #[test]
    fn the_stream_hands_back_a_usable_opus_head() {
        let (_stream, head) = OpusStream::new(PCM_CD_QUALITY).expect("an encoder");
        assert_eq!(head.len(), 19, "the fixed part, with no mapping table");
        assert_eq!(&head[0..8], b"OpusHead");
        assert_eq!(head[8], 1, "version");
        assert_eq!(head[9], 2, "stereo");
        let pre_skip = u16::from_le_bytes(head[10..12].try_into().unwrap());
        assert!(pre_skip > 0, "the encoder's lookahead, not a hardcoded zero");
        assert_eq!(
            u32::from_le_bytes(head[12..16].try_into().unwrap()),
            44_100,
            "OpusHead records the rate the audio arrived at"
        );
        assert_eq!(head[18], 0, "channel mapping family");
    }

    /// The buffer sizes RDP actually sends do not line up with Opus frames, so
    /// this is the arithmetic that matters: whole frames out, remainder carried,
    /// nothing padded and nothing dropped.
    #[test]
    fn only_whole_frames_are_encoded_and_the_remainder_is_carried() {
        let (mut stream, _head) = OpusStream::new(PCM_CD_QUALITY).expect("an encoder");

        // One frame needs 882 input frames at 44.1 kHz.
        assert!(
            stream.push(&silence(881)).expect("push").is_empty(),
            "one frame short of a packet produces nothing"
        );
        assert_eq!(
            stream.push(&silence(1)).expect("push").len(),
            1,
            "the 882nd frame completes a packet"
        );

        // 32768 bytes is what the tested Windows host sends: 8192 frames, which is
        // nine whole packets (7938 frames) with 254 left over.
        assert_eq!(stream.push(&silence(8192)).expect("push").len(), 9);
        assert_eq!(stream.pcm.carried_frames(), 8192 - 9 * 882);
        assert_eq!(stream.frames_encoded(), 10);
    }

    /// Every packet has to be a packet — a decoder handed an empty one has nothing
    /// to do with it, and an empty `Vec` is what a mis-sliced encode would produce.
    #[test]
    fn every_packet_carries_bytes() {
        let (mut stream, _head) = OpusStream::new(PCM_CD_QUALITY).expect("an encoder");
        let packets = stream.push(&silence(882 * 3)).expect("push");
        assert_eq!(packets.len(), 3);
        assert!(packets.iter().all(|packet| !packet.is_empty()));
    }

    /// Encode a tone and decode it back, so the test fails if the bytes are
    /// well-framed nonsense — the failure a framing-only assertion would miss.
    ///
    /// libopus is doing the decoding, which matters more than it did while there
    /// was a container: `ffprobe` used to be the check that this agrees with
    /// something other than itself, and without Ogg there is nothing for a third
    /// party demuxer to read. The remaining outside readers are this decoder and
    /// the browser's own (`server::tests::serve_a_test_tone`).
    #[test]
    fn a_tone_survives_the_round_trip() {
        let (mut stream, _head) = OpusStream::new(PCM_CD_QUALITY).expect("an encoder");

        // 441 Hz at 44.1 kHz: exactly 100 samples a cycle, and a whole number of
        // cycles per packet, so there is no discontinuity to blame a failure on.
        let mut pcm = Vec::new();
        for frame in 0..882 * 20 {
            let phase = (frame % 100) as f32 / 100.0 * std::f32::consts::TAU;
            let sample = (phase.sin() * 12_000.0) as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        let packets = stream.push(&pcm).expect("push");
        assert_eq!(packets.len(), 20);

        let mut decoder = opus::Decoder::new(SAMPLE_RATE, Channels::Stereo).expect("decoder");
        let mut decoded = vec![0i16; FRAME_FRAMES * 2];
        // Decode up to a packet in the middle: the first few are the encoder
        // settling, and a decoder needs the ones before it either way.
        for packet in &packets[..15] {
            decoder.decode(packet, &mut decoded, false).expect("decode");
        }
        let peak = decoded.iter().map(|s| s.abs()).max().expect("samples");
        assert!(
            peak > 6_000,
            "the decoded frame should carry the tone, peak was {peak}"
        );
    }

    /// The channels must still be distinguishable after the *whole* path —
    /// resample, interleave, encode, decode — and not merely after the resampler.
    /// This is what catches a wrong channel count in `OpusHead` or a transposed
    /// interleave in the frame buffer, neither of which
    /// [`crate::pcm48`]'s blend test would see.
    ///
    /// It is also the answer to a real false alarm: a live capture from the test
    /// host decoded with an L/R correlation of exactly 1.0000, which looks like
    /// blended channels and is in fact a dual-mono source. A hard-panned signal is
    /// the only input that tells the two apart.
    #[test]
    fn a_hard_panned_signal_still_has_two_channels_after_a_round_trip() {
        let (mut stream, _head) = OpusStream::new(PCM_CD_QUALITY).expect("an encoder");

        // Left carries a tone, right is silent.
        let mut pcm = Vec::new();
        for frame in 0..882 * 20 {
            let phase = (frame % 100) as f32 / 100.0 * std::f32::consts::TAU;
            pcm.extend_from_slice(&((phase.sin() * 12_000.0) as i16).to_le_bytes());
            pcm.extend_from_slice(&0i16.to_le_bytes());
        }
        let packets = stream.push(&pcm).expect("push");

        let mut decoder = opus::Decoder::new(SAMPLE_RATE, Channels::Stereo).expect("decoder");
        let mut decoded = vec![0i16; FRAME_FRAMES * 2];
        // Past the encoder settling, so the silence on the right is really silence.
        for packet in packets.iter().take(15) {
            decoder.decode(packet, &mut decoded, false).expect("decode");
        }
        let energy = |samples: &[i16]| -> f64 {
            samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>()
                / samples.len() as f64
        };
        let left = energy(&decoded.iter().copied().step_by(2).collect::<Vec<_>>());
        let right = energy(&decoded.iter().copied().skip(1).step_by(2).collect::<Vec<_>>());
        assert!(left > 1_000_000.0, "the left channel should carry the tone: {left}");
        assert!(
            right * 10.0 < left,
            "the right channel should be far quieter than the left, got {right} against {left}"
        );
    }

    #[test]
    fn an_impossible_channel_count_is_refused_rather_than_encoded() {
        let format = PcmFormat {
            channels: 6,
            sample_rate: 48_000,
            bits_per_sample: 16,
        };
        assert!(OpusStream::new(format).is_err());
    }
}
