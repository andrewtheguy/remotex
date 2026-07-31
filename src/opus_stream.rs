//! Convert live PCM wave buffers into bare 20 ms Opus packets.
//!
//! RDP's 44.1 kHz PCM is retained across buffers, resampled in exact groups to
//! 48 kHz, and framed as 960-sample packets. Each listener owns fresh codec
//! state downstream of the nonblocking RDP queue; quiet remotes emit nothing.

use opus::{Application, Bitrate, Channels, Encoder};
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};

use crate::audio::PcmFormat;

/// The rate Opus encodes at. Not a preference — libopus rejects anything outside
/// 8/12/16/24/48 kHz, and this is the only one of those that is not a downgrade
/// from what RDP delivers.
pub const OPUS_SAMPLE_RATE: u32 = 48_000;

/// Frames per Opus packet, per channel: 20 ms at 48 kHz.
///
/// 20 ms is Opus's default frame and the usual choice: shorter frames spend more
/// of the bitrate on packet overhead, longer ones add latency for nothing here.
pub const FRAME_FRAMES: usize = 960;

/// Target bitrate. Stereo desktop audio including music, so this is well clear of
/// the ~64 kbps where stereo Opus starts to be audibly lossy, and still ~1/15th
/// of the PCM it replaces.
pub const OPUS_BITRATE_BPS: i32 = 96_000;

/// Ceiling for one encoded packet. libopus documents 4000 bytes as the largest
/// worth allowing for; at this bitrate a packet is nearer 240.
const MAX_PACKET_BYTES: usize = 4000;

/// Turns PCM buffers into Opus packets.
pub struct OpusStream {
    encoder: Encoder,
    channels: usize,
    /// `None` when the input is already at [`OPUS_SAMPLE_RATE`].
    resampler: Option<Fft<f32>>,
    /// Input frames consumed per encoded packet: 882 at 44.1 kHz, 960 at 48 kHz.
    frames_per_packet: usize,
    /// Deinterleaved input awaiting a whole packet's worth, one `Vec` per channel.
    /// Deinterleaved because that is what the resampler takes, and because
    /// interpolating across interleaved samples would blend the channels into
    /// each other.
    pending: Vec<Vec<f32>>,
    /// Scratch: the resampler's output, one `Vec` per channel.
    resampled: Vec<Vec<f32>>,
    /// Scratch: one 20 ms frame, interleaved, as libopus wants it.
    frame: Vec<f32>,
    /// Scratch: one encoded packet.
    packet: Vec<u8>,
    /// A trailing byte of a wave buffer that split a sample in half. RDP has no
    /// reason to do this, but the queue carries bytes rather than samples, so
    /// nothing in the types rules it out.
    odd_byte: Option<u8>,
    /// Which channel the next sample belongs to. State rather than a local,
    /// because a buffer can end mid-frame — on an odd sample count the next
    /// buffer's first sample is a right channel, and restarting at left here
    /// would swap the channels for the rest of the session.
    next_channel: usize,
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

        let mut encoder = Encoder::new(OPUS_SAMPLE_RATE, channels, Application::Audio)
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

        let (resampler, frames_per_packet) = if format.sample_rate == OPUS_SAMPLE_RATE {
            (None, FRAME_FRAMES)
        } else {
            // Exact by construction: 882 in, 960 out at 44.1 kHz. `FixedSync::Input`
            // is what makes the input side the fixed one, so the caller controls
            // how much it hands over and the packet size falls out.
            let frames_in = (FRAME_FRAMES as u64 * u64::from(format.sample_rate)
                / u64::from(OPUS_SAMPLE_RATE)) as usize;
            let resampler = Fft::<f32>::new(
                format.sample_rate as usize,
                OPUS_SAMPLE_RATE as usize,
                frames_in,
                channel_count,
                FixedSync::Input,
            )
            .map_err(|e| {
                anyhow::anyhow!("build a {} Hz -> 48 kHz resampler: {e}", format.sample_rate)
            })?;
            (Some(resampler), frames_in)
        };

        let stream = Self {
            encoder,
            channels: channel_count,
            resampler,
            frames_per_packet,
            pending: vec![Vec::new(); channel_count],
            resampled: vec![vec![0.0; FRAME_FRAMES]; channel_count],
            frame: vec![0.0; FRAME_FRAMES * channel_count],
            packet: vec![0; MAX_PACKET_BYTES],
            odd_byte: None,
            next_channel: 0,
            frames_encoded: 0,
        };

        Ok((stream, opus_head(format, pre_skip)))
    }

    /// Encodes one PCM buffer into whatever whole Opus packets it completed.
    ///
    /// Empty when the buffer did not add up to a whole 20 ms frame, which is
    /// normal: the remainder is carried.
    pub fn push(&mut self, pcm: &[u8]) -> Result<Vec<Vec<u8>>, anyhow::Error> {
        self.accumulate(pcm);
        let mut packets = Vec::new();
        // The shortest channel, not the first: a buffer ending mid-frame leaves
        // left one sample ahead of right, and encoding then would read past the
        // end of right's samples.
        while self.buffered_frames() >= self.frames_per_packet {
            packets.push(self.encode_one_frame()?);
        }
        Ok(packets)
    }

    /// Frames encoded so far.
    pub fn frames_encoded(&self) -> u64 {
        self.frames_encoded
    }

    /// Splits interleaved little-endian 16-bit PCM into per-channel `f32`.
    fn accumulate(&mut self, pcm: &[u8]) {
        let mut bytes = pcm.iter().copied();
        // A byte held back from last time pairs with the first byte of this buffer.
        if let Some(low) = self.odd_byte.take() {
            match bytes.next() {
                Some(high) => self.push_sample(i16::from_le_bytes([low, high])),
                None => {
                    self.odd_byte = Some(low);
                    return;
                }
            }
        }
        while let Some(low) = bytes.next() {
            let Some(high) = bytes.next() else {
                self.odd_byte = Some(low);
                break;
            };
            self.push_sample(i16::from_le_bytes([low, high]));
        }
    }

    /// Frames every channel has, which is what a packet can be cut from.
    fn buffered_frames(&self) -> usize {
        self.pending.iter().map(Vec::len).min().unwrap_or(0)
    }

    fn push_sample(&mut self, sample: i16) {
        // i16::MIN maps to exactly -1.0; the asymmetry of two's complement means
        // dividing by 32768 rather than 32767 is what keeps it in range.
        self.pending[self.next_channel].push(f32::from(sample) / 32_768.0);
        self.next_channel = (self.next_channel + 1) % self.channels;
    }

    fn encode_one_frame(&mut self) -> Result<Vec<u8>, anyhow::Error> {
        let taken = self.frames_per_packet;
        if let Some(resampler) = &mut self.resampler {
            let input = SequentialSliceOfVecs::new(&self.pending, self.channels, taken)
                .map_err(|e| anyhow::anyhow!("wrap the resampler input: {e}"))?;
            let mut output =
                SequentialSliceOfVecs::new_mut(&mut self.resampled, self.channels, FRAME_FRAMES)
                    .map_err(|e| anyhow::anyhow!("wrap the resampler output: {e}"))?;
            resampler
                .process_into_buffer(&input, &mut output, None)
                .map_err(|e| anyhow::anyhow!("resample to 48 kHz: {e}"))?;
        } else {
            for (channel, out) in self.resampled.iter_mut().enumerate() {
                out.copy_from_slice(&self.pending[channel][..FRAME_FRAMES]);
            }
        }
        for pending in &mut self.pending {
            pending.drain(..taken);
        }

        for frame in 0..FRAME_FRAMES {
            for channel in 0..self.channels {
                self.frame[frame * self.channels + channel] = self.resampled[channel][frame];
            }
        }

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
        assert_eq!(stream.pending[0].len(), 8192 - 9 * 882);
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

    /// An odd byte count must not shift the channels against each other for the
    /// rest of the stream, which is what dropping the leftover byte would do.
    #[test]
    fn a_half_sample_at_the_end_of_a_buffer_is_carried_not_dropped() {
        let (mut stream, _head) = OpusStream::new(PCM_CD_QUALITY).expect("an encoder");
        stream.push(&[0, 0, 0]).expect("push");
        assert_eq!(stream.odd_byte, Some(0), "the third byte is half a sample");
        assert_eq!(stream.pending[0].len(), 1, "left got one sample");
        assert_eq!(stream.pending[1].len(), 0, "right is waiting for its other half");
        stream.push(&[0]).expect("push");
        assert_eq!(stream.odd_byte, None);
        assert_eq!(stream.pending[1].len(), 1);
    }

    /// The bug in the reference implementation this borrowed from
    /// (`tmp/save_audio_stream`, whose resampler interpolates across interleaved
    /// samples): with the channels blended, a hard-panned signal leaks across.
    /// Pinned here because Opus would happily encode the wrong thing.
    #[test]
    fn resampling_does_not_blend_the_channels() {
        let (mut stream, _head) = OpusStream::new(PCM_CD_QUALITY).expect("an encoder");
        // Left at full positive, right at full negative, for one packet.
        let mut pcm = Vec::new();
        for _ in 0..882 {
            pcm.extend_from_slice(&i16::MAX.to_le_bytes());
            pcm.extend_from_slice(&i16::MIN.to_le_bytes());
        }
        stream.push(&pcm).expect("push");

        let left = &stream.resampled[0];
        let right = &stream.resampled[1];
        // The middle of the frame, past the resampler's edge transient.
        let mid = FRAME_FRAMES / 2;
        assert!(
            left[mid] > 0.9,
            "left should still be near +1.0, got {}",
            left[mid]
        );
        assert!(
            right[mid] < -0.9,
            "right should still be near -1.0, got {}",
            right[mid]
        );
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

        let mut decoder = opus::Decoder::new(OPUS_SAMPLE_RATE, Channels::Stereo).expect("decoder");
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
    /// [`resampling_does_not_blend_the_channels`] would see.
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

        let mut decoder = opus::Decoder::new(OPUS_SAMPLE_RATE, Channels::Stereo).expect("decoder");
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
    fn a_48_kilohertz_source_skips_the_resampler() {
        let format = PcmFormat {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
        };
        let (mut stream, _head) = OpusStream::new(format).expect("an encoder");
        assert!(stream.resampler.is_none());
        assert_eq!(stream.frames_per_packet, FRAME_FRAMES);
        let packets = stream
            .push(&vec![0u8; FRAME_FRAMES * usize::from(format.block_align())])
            .expect("push");
        assert_eq!(packets.len(), 1);
    }

    /// Write the Opus fixtures the macOS viewer's tests decode.
    ///
    /// Checked in because macOS can encode Opus perfectly well, and the point is
    /// that it must not — unlike the tile codecs, where the viewer's tests encode
    /// their own PNG/JPEG payloads at runtime through ImageIO. The
    /// viewer's decoder test has to answer to *this* encoder — the resampler, the 20 ms
    /// framing, the bitrate and the pre-skip in this file — or the two ends agree with
    /// their own fixtures and disagree with each other, which is the failure the
    /// browser's own wire tests are shaped around too (CLAUDE.md).
    ///
    /// Hard-panned, for the reason [`a_hard_panned_signal_still_has_two_channels_after_a_round_trip`]
    /// exists: a channel *swap* or a blend passes every other assertion, and a
    /// dual-mono source decodes with a correlation of exactly 1.0 while looking like a
    /// fault. Left carries the tone, right is silent.
    ///
    /// The packets are framed the way the wire frames them — `u16` length, then the
    /// packet — so the Swift side reads them with the parser it uses in production
    /// rather than with a second format invented for the fixture.
    ///
    /// Run after changing the encoder's configuration:
    ///
    /// ```sh
    /// cargo test --lib -- --ignored --nocapture swift_opus_fixtures
    /// ```
    #[test]
    #[ignore = "writes files into the viewer's test bundle; run explicitly"]
    fn write_swift_opus_fixtures() {
        let dir = swift_fixture_dir();
        std::fs::create_dir_all(&dir).expect("fixture directory");
        let (head, framed) = swift_opus_fixture_bytes();

        std::fs::write(dir.join("head.bin"), &head).unwrap();
        std::fs::write(dir.join("packets.bin"), &framed).unwrap();
        println!("wrote head.bin ({} bytes) and packets.bin ({} bytes)", head.len(), framed.len());
    }

    /// The checked-in fixtures still describe what this encoder produces.
    ///
    /// **Not `#[ignore]`d, and that is the point of it.** The writer above is explicit
    /// because it touches another target's source tree, which means nothing forces it to
    /// be re-run: change the bitrate, the frame size or the resampler and the fixtures
    /// silently become the *old* encoder's output. The viewer's `OpusDecoderTests` would
    /// keep passing on them — stale Opus decodes perfectly well — so the drift would
    /// surface as the cross-end check quietly no longer being one.
    ///
    /// **This compares properties rather than bytes, and it learned that the hard way.**
    /// The first version byte-compared, on the reasoning that libopus is deterministic
    /// for a given input and configuration. It is — for a given *build*. The fixtures are
    /// checked in, so the comparison spans builds, and back when CI compiled
    /// `audiopus_sys`'s vendored source while a dev machine linked a system libopus
    /// (homebrew 1.6.1) the two produced 9960 bytes each and disagreed on the contents,
    /// which broke `Agent validation` on main. `opus-prebuilt` now links the same static
    /// archive everywhere, so that particular divergence is gone — but encoded float
    /// output is still not a reproducible artifact across libopus versions or optimisation
    /// paths, so the property comparison stays rather than betting the check on it.
    ///
    /// So the head is compared exactly — it is byte assembly plus the encoder's
    /// lookahead, and the CI failure confirmed both builds report the same 312 — while
    /// the packets are checked for the things a configuration change moves and a libopus
    /// upgrade does not:
    ///
    /// - **the packet count**, which is pure arithmetic (882 input frames per packet) and
    ///   is what a frame-size change halves or doubles;
    /// - **the frames per packet**, read back through a decoder, which is the same change
    ///   seen from the other side and also catches a rate change;
    /// - **the total size**, within a band wide enough for a version difference and far
    ///   narrower than a bitrate move (96 -> 64 kbps is a third of the bytes);
    /// - **the panning**, which catches a channel count or interleave change.
    ///
    /// What this deliberately no longer claims is that the fixture bytes are *the* bytes
    /// this machine would write. It cannot: two correct builds disagree.
    #[test]
    fn the_checked_in_opus_fixtures_match_this_encoder() {
        const REGENERATE: &str =
            "regenerate with `cargo test --lib -- --ignored --nocapture swift_opus_fixtures`";
        let dir = swift_fixture_dir();
        let (head, framed) = swift_opus_fixture_bytes();

        let read = |name: &str| -> Vec<u8> {
            let path = dir.join(name);
            std::fs::read(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}\n{REGENERATE}", path.display()))
        };

        assert!(
            read("head.bin") == head,
            "head.bin is not what this encoder's OpusHead is — the channel count, the \
             input rate or the encoder's lookahead moved, and the viewer discards the \
             pre-skip from this. {REGENERATE}"
        );

        let checked_in = read("packets.bin");
        let fixture_packets = unframe(&checked_in);
        let produced_packets = unframe(&framed);
        assert_eq!(
            fixture_packets.len(),
            produced_packets.len(),
            "packets.bin holds {} packets and this encoder now cuts {} — the frame size \
             or the resampling changed. {REGENERATE}",
            fixture_packets.len(),
            produced_packets.len()
        );

        // Within 25%: a libopus version difference moves the total by a percent or two
        // (both builds happened to land on exactly 9960 bytes), where the smallest
        // bitrate change worth making moves it by a third.
        let (fixture_len, produced_len) = (checked_in.len() as f64, framed.len() as f64);
        assert!(
            (fixture_len - produced_len).abs() / produced_len < 0.25,
            "packets.bin is {} bytes where this encoder produces {} — that is a bitrate \
             change, not a libopus difference. {REGENERATE}",
            checked_in.len(),
            framed.len()
        );

        // Decoded, because the wire's own framing says nothing about what is inside a
        // packet: this is where a rate or channel change shows up.
        let mut decoder = opus::Decoder::new(OPUS_SAMPLE_RATE, Channels::Stereo)
            .expect("the fixtures are stereo at 48 kHz");
        let mut decoded = vec![0i16; FRAME_FRAMES * 2];
        for (index, packet) in fixture_packets.iter().enumerate() {
            let frames = decoder
                .decode(packet, &mut decoded, false)
                .unwrap_or_else(|e| panic!("fixture packet {index} does not decode: {e}\n{REGENERATE}"));
            assert_eq!(
                frames, FRAME_FRAMES,
                "fixture packet {index} carries {frames} frames, not one 20 ms frame. {REGENERATE}"
            );
        }
        // The last decoded packet is past the encoder settling, so the silent channel is
        // really silent — the fixture is hard-panned on purpose (see the writer).
        let energy = |samples: Vec<i16>| -> f64 {
            samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>()
                / samples.len() as f64
        };
        let left = energy(decoded.iter().copied().step_by(2).collect());
        let right = energy(decoded.iter().copied().skip(1).step_by(2).collect());
        assert!(
            left > 1_000_000.0 && right * 10.0 < left,
            "the fixture should still be hard-panned left, got {left} against {right}. \
             {REGENERATE}"
        );
    }

    /// Split the wire's `u16`-length framing back into packets.
    fn unframe(framed: &[u8]) -> Vec<&[u8]> {
        let mut packets = Vec::new();
        let mut at = 0;
        while at + 2 <= framed.len() {
            let len = usize::from(u16::from_le_bytes([framed[at], framed[at + 1]]));
            at += 2;
            assert!(at + len <= framed.len(), "the framing is truncated");
            packets.push(&framed[at..at + len]);
            at += len;
        }
        packets
    }

    /// Absolute, so both of the tests above work whatever directory cargo was run from.
    fn swift_fixture_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("apps/remotex-viewer/Tests/Fixtures/opus")
    }

    /// The fixture bytes: an `OpusHead` and the packets in the wire's own framing.
    ///
    /// Shared by the writer and the check above so the two cannot disagree about what a
    /// fixture *is* — which would make the check pass while the files on disk were still
    /// wrong.
    fn swift_opus_fixture_bytes() -> (Vec<u8>, Vec<u8>) {
        let (mut stream, head) = OpusStream::new(PCM_CD_QUALITY).expect("an encoder");
        assert_eq!(head.len(), 19, "OpusHead is 19 bytes");

        // 40 packets' worth at 44 100 Hz: 882 input frames become one 20 ms packet.
        let mut pcm = Vec::new();
        for frame in 0..882 * 40 {
            let phase = (frame % 100) as f32 / 100.0 * std::f32::consts::TAU;
            pcm.extend_from_slice(&((phase.sin() * 12_000.0) as i16).to_le_bytes());
            pcm.extend_from_slice(&0i16.to_le_bytes());
        }
        let packets = stream.push(&pcm).expect("push");
        assert!(packets.len() >= 20, "too few packets to be worth a fixture");

        let mut framed = Vec::new();
        for packet in &packets {
            let size = u16::try_from(packet.len()).expect("a packet fits a u16");
            framed.extend_from_slice(&size.to_le_bytes());
            framed.extend_from_slice(packet);
        }
        (head, framed)
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
