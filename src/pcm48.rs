//! Redirected PCM, deinterleaved and resampled to 48 kHz.
//!
//! Everything upstream of an *encoder* lives here: RDP's 44.1 kHz little-endian
//! i16 is retained across wave buffers, split per channel, and resampled in exact
//! groups to 48 kHz. A codec then draws whatever packet size it wants out of the
//! result.
//!
//! Not on the passthrough path, which reaches the wire without any of this — see
//! [`crate::pcm_stream`]. Resampling is precisely what that option exists not to
//! do, so nothing there may start routing through here.
//!
//! The group size is deliberately *not* a packet size. 882 in and 960 out is
//! exact at 44.1 kHz, and that exactness is what lets the resampler be driven from
//! its input side. Fixing the group and buffering the output is what keeps a
//! codec's own granule from constraining the resampler.
//!
//! That exactness is a **precondition**, not a happy accident: [`Pcm48`] accepts
//! only rates for which one group is a whole number of input frames, and refuses
//! the rest rather than rounding them (see [`group_frames_in`]). The gateway
//! advertises one RDPSND format and it qualifies, so the refusal is for a server
//! that answers with something other than what it was offered.

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};

use crate::audio::PcmFormat;

/// The rate every codec here encodes at, and the rate the browser decodes at.
///
/// Not a preference. libopus rejects anything outside 8/12/16/24/48 kHz, and 48 is
/// the only one of those that is not a downgrade from what RDP delivers. It is
/// also the rate the browser builds its `AudioContext` at, a round trip before it
/// learns what it is getting — so an encoded stream needs no resampling at
/// playback either.
pub const SAMPLE_RATE: u32 = 48_000;

/// Output frames in one resampler group, per channel: 20 ms at 48 kHz.
const GROUP_FRAMES: usize = 960;

/// Input frames that make one group at `rate`: 882 at 44.1 kHz, 960 at 48 kHz.
///
/// `None` when `rate` does not make a whole number of them, which is the
/// invariant the module doc above is about rather than a corner case: the group
/// is exact, and that is what lets the resampler be driven from its input side
/// with nothing accumulating a fractional remainder. Truncating instead would
/// build a resampler whose ratio is not the stream's, and the drift would be
/// silent — a stream slowly running ahead of or behind real time with no error
/// anywhere to explain it. [`Pcm48::new`] therefore refuses such a rate outright.
pub fn group_frames_in(rate: u32) -> Option<usize> {
    let frames = GROUP_FRAMES as u64 * u64::from(rate);
    frames
        .is_multiple_of(u64::from(SAMPLE_RATE))
        .then(|| (frames / u64::from(SAMPLE_RATE)) as usize)
}

/// Turns wave buffers into interleaved 48 kHz `f32`.
pub struct Pcm48 {
    channels: usize,
    /// `None` when the input is already at [`SAMPLE_RATE`].
    resampler: Option<Fft<f32>>,
    /// Input frames consumed per group: 882 at 44.1 kHz, 960 at 48 kHz.
    frames_per_group: usize,
    /// Deinterleaved input awaiting a whole group, one `Vec` per channel.
    /// Deinterleaved because that is what the resampler takes, and because
    /// interpolating across interleaved samples would blend the channels into
    /// each other.
    pending: Vec<Vec<f32>>,
    /// Scratch: the resampler's output, one `Vec` per channel.
    resampled: Vec<Vec<f32>>,
    /// Resampled frames a codec has not taken yet, interleaved.
    ready: Vec<f32>,
    /// A trailing byte of a wave buffer that split a sample in half. RDP has no
    /// reason to do this, but the queue carries bytes rather than samples, so
    /// nothing in the types rules it out.
    odd_byte: Option<u8>,
    /// Which channel the next sample belongs to. State rather than a local,
    /// because a buffer can end mid-frame — on an odd sample count the next
    /// buffer's first sample is a right channel, and restarting at left here
    /// would swap the channels for the rest of the session.
    next_channel: usize,
}

impl Pcm48 {
    /// Starts resampling `format` to [`SAMPLE_RATE`].
    pub fn new(format: PcmFormat) -> Result<Self, anyhow::Error> {
        let channels = usize::from(format.channels);
        anyhow::ensure!(channels > 0, "a pcm format with no channels cannot be encoded");

        let (resampler, frames_per_group) = if format.sample_rate == SAMPLE_RATE {
            (None, GROUP_FRAMES)
        } else {
            // `FixedSync::Input` is what makes the input side the fixed one, so the
            // caller controls how much it hands over and the group size falls out.
            let frames_in = group_frames_in(format.sample_rate).ok_or_else(|| {
                anyhow::anyhow!(
                    "{} Hz does not divide into whole {SAMPLE_RATE} Hz groups, \
                     so it cannot be resampled with a fixed group",
                    format.sample_rate
                )
            })?;
            let resampler = Fft::<f32>::new(
                format.sample_rate as usize,
                SAMPLE_RATE as usize,
                frames_in,
                channels,
                FixedSync::Input,
            )
            .map_err(|e| {
                anyhow::anyhow!("build a {} Hz -> 48 kHz resampler: {e}", format.sample_rate)
            })?;
            (Some(resampler), frames_in)
        };

        Ok(Self {
            channels,
            resampler,
            frames_per_group,
            pending: vec![Vec::new(); channels],
            resampled: vec![vec![0.0; GROUP_FRAMES]; channels],
            ready: Vec::new(),
            odd_byte: None,
            next_channel: 0,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Absorb one wave buffer, resampling every whole group it completed.
    ///
    /// What it did not complete is carried: nothing is padded and nothing is
    /// dropped, down to a byte that split a sample in half.
    pub fn push(&mut self, pcm: &[u8]) -> Result<(), anyhow::Error> {
        self.accumulate(pcm);
        // The shortest channel, not the first: a buffer ending mid-frame leaves
        // left one sample ahead of right, and resampling then would read past the
        // end of right's samples.
        while self.buffered_frames() >= self.frames_per_group {
            self.resample_one_group()?;
        }
        Ok(())
    }

    /// Resampled frames waiting to be taken, per channel.
    pub fn ready_frames(&self) -> usize {
        self.ready.len() / self.channels
    }

    /// Take exactly `frames` interleaved frames as `f32`, which is what libopus wants.
    ///
    /// Panics if `frames` exceeds [`Self::ready_frames`] or `out` is too small —
    /// both are the caller's own arithmetic rather than anything the stream decides.
    pub fn take_f32(&mut self, frames: usize, out: &mut [f32]) {
        let samples = frames * self.channels;
        out[..samples].copy_from_slice(&self.ready[..samples]);
        self.ready.drain(..samples);
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

    /// Frames every channel has, which is what a group can be cut from.
    fn buffered_frames(&self) -> usize {
        self.pending.iter().map(Vec::len).min().unwrap_or(0)
    }

    fn push_sample(&mut self, sample: i16) {
        // i16::MIN maps to exactly -1.0; the asymmetry of two's complement means
        // dividing by 32768 rather than 32767 is what keeps it in range.
        self.pending[self.next_channel].push(f32::from(sample) / 32_768.0);
        self.next_channel = (self.next_channel + 1) % self.channels;
    }

    fn resample_one_group(&mut self) -> Result<(), anyhow::Error> {
        let taken = self.frames_per_group;
        if let Some(resampler) = &mut self.resampler {
            let input = SequentialSliceOfVecs::new(&self.pending, self.channels, taken)
                .map_err(|e| anyhow::anyhow!("wrap the resampler input: {e}"))?;
            let mut output =
                SequentialSliceOfVecs::new_mut(&mut self.resampled, self.channels, GROUP_FRAMES)
                    .map_err(|e| anyhow::anyhow!("wrap the resampler output: {e}"))?;
            resampler
                .process_into_buffer(&input, &mut output, None)
                .map_err(|e| anyhow::anyhow!("resample to 48 kHz: {e}"))?;
        } else {
            for (channel, out) in self.resampled.iter_mut().enumerate() {
                out.copy_from_slice(&self.pending[channel][..GROUP_FRAMES]);
            }
        }
        for pending in &mut self.pending {
            pending.drain(..taken);
        }

        self.ready.reserve(GROUP_FRAMES * self.channels);
        for frame in 0..GROUP_FRAMES {
            for channel in 0..self.channels {
                self.ready.push(self.resampled[channel][frame]);
            }
        }
        Ok(())
    }

    /// Input frames held back because they did not complete a group.
    ///
    /// Test-only, and nothing branches on it: it is how the remainder assertions
    /// see that a partial group was carried rather than dropped.
    #[cfg(test)]
    pub fn carried_frames(&self) -> usize {
        self.buffered_frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::PCM_CD_QUALITY;

    /// Silence as bytes on the queue, at CD quality.
    fn silence(frames: usize) -> Vec<u8> {
        vec![0u8; frames * usize::from(PCM_CD_QUALITY.block_align())]
    }

    /// The buffer sizes RDP actually sends do not line up with resampler groups, so
    /// this is the arithmetic that matters: whole groups out, remainder carried.
    #[test]
    fn only_whole_groups_are_resampled_and_the_remainder_is_carried() {
        let mut pcm = Pcm48::new(PCM_CD_QUALITY).expect("a resampler");

        // One group needs 882 input frames at 44.1 kHz.
        pcm.push(&silence(881)).expect("push");
        assert_eq!(pcm.ready_frames(), 0, "one frame short of a group produces nothing");
        pcm.push(&silence(1)).expect("push");
        assert_eq!(pcm.ready_frames(), GROUP_FRAMES, "the 882nd frame completes a group");

        // 32768 bytes is what the tested Windows host sends: 8192 frames, which is
        // nine whole groups (7938 frames) with 254 left over.
        pcm.take_f32(GROUP_FRAMES, &mut vec![0.0; GROUP_FRAMES * 2]);
        pcm.push(&silence(8192)).expect("push");
        assert_eq!(pcm.ready_frames(), 9 * GROUP_FRAMES);
        assert_eq!(pcm.carried_frames(), 8192 - 9 * 882);
    }

    /// An odd byte count must not shift the channels against each other for the
    /// rest of the stream, which is what dropping the leftover byte would do.
    #[test]
    fn a_half_sample_at_the_end_of_a_buffer_is_carried_not_dropped() {
        let mut pcm = Pcm48::new(PCM_CD_QUALITY).expect("a resampler");
        pcm.push(&[0, 0, 0]).expect("push");
        assert_eq!(pcm.odd_byte, Some(0), "the third byte is half a sample");
        assert_eq!(pcm.pending[0].len(), 1, "left got one sample");
        assert_eq!(pcm.pending[1].len(), 0, "right is waiting for its other half");
        pcm.push(&[0]).expect("push");
        assert_eq!(pcm.odd_byte, None);
        assert_eq!(pcm.pending[1].len(), 1);
    }

    /// The bug in the reference implementation this borrowed from
    /// (`tmp/save_audio_stream`, whose resampler interpolates across interleaved
    /// samples): with the channels blended, a hard-panned signal leaks across.
    /// Pinned here because a codec would happily encode the wrong thing.
    #[test]
    fn resampling_does_not_blend_the_channels() {
        let mut pcm = Pcm48::new(PCM_CD_QUALITY).expect("a resampler");
        // Left at full positive, right at full negative, for one group.
        let mut wave = Vec::new();
        for _ in 0..882 {
            wave.extend_from_slice(&i16::MAX.to_le_bytes());
            wave.extend_from_slice(&i16::MIN.to_le_bytes());
        }
        pcm.push(&wave).expect("push");

        // The middle of the group, past the resampler's edge transient.
        let mid = GROUP_FRAMES / 2;
        let left = pcm.ready[mid * 2];
        let right = pcm.ready[mid * 2 + 1];
        assert!(left > 0.9, "left should still be near +1.0, got {left}");
        assert!(right < -0.9, "right should still be near -1.0, got {right}");
    }

    /// The fixed group only works where it is exact, so a rate that would need a
    /// fractional one is refused rather than rounded to a resampler whose ratio is
    /// not the stream's — which would drift with nothing to say why.
    #[test]
    fn a_rate_that_makes_no_whole_group_is_refused_rather_than_rounded() {
        // The rates RDP could plausibly offer, and what they cost: 44.1 and 22.05
        // are exact, 11.025 is 220.5 input frames and is not.
        assert_eq!(group_frames_in(44_100), Some(882));
        assert_eq!(group_frames_in(48_000), Some(GROUP_FRAMES));
        assert_eq!(group_frames_in(22_050), Some(441));
        assert_eq!(group_frames_in(11_025), None, "220.5 frames is not a group");

        let awkward = PcmFormat {
            channels: 2,
            sample_rate: 11_025,
            bits_per_sample: 16,
        };
        // Matched rather than `expect_err`, which would need `Pcm48: Debug` — and
        // the resampler it holds has none.
        let Err(err) = Pcm48::new(awkward) else {
            panic!("11025 Hz has no whole group and must not build a resampler");
        };
        assert!(format!("{err:#}").contains("11025"), "{err:#}");
    }

    #[test]
    fn a_48_kilohertz_source_skips_the_resampler() {
        let format = PcmFormat {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
        };
        let mut pcm = Pcm48::new(format).expect("a resampler");
        assert!(pcm.resampler.is_none());
        assert_eq!(pcm.frames_per_group, GROUP_FRAMES);
        pcm.push(&vec![0u8; GROUP_FRAMES * usize::from(format.block_align())])
            .expect("push");
        assert_eq!(pcm.ready_frames(), GROUP_FRAMES);
    }

    /// `i16::MIN` has to land on exactly -1.0 and nothing may exceed full scale,
    /// because a sample that wraps is a click on every peak. The client's own
    /// passthrough conversion is the inverse of this and pins the same two ends
    /// (`frontend/src/audioPlayer.test.ts`).
    #[test]
    fn the_ends_of_the_range_survive_the_conversion_to_f32() {
        let format = PcmFormat {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
        };
        let mut pcm = Pcm48::new(format).expect("a resampler");
        let mut wave = Vec::new();
        for _ in 0..GROUP_FRAMES {
            wave.extend_from_slice(&i16::MAX.to_le_bytes());
            wave.extend_from_slice(&i16::MIN.to_le_bytes());
        }
        pcm.push(&wave).expect("push");

        let mut out = vec![0.0f32; GROUP_FRAMES * 2];
        pcm.take_f32(GROUP_FRAMES, &mut out);
        assert!(out[0] < 1.0 && out[0] > 0.9999, "left, just under +1: {}", out[0]);
        assert_eq!(out[1], -1.0, "right came back exactly full negative");
        assert_eq!(pcm.ready_frames(), 0, "taking consumes");
    }
}
