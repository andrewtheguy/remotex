//! The AAC-ELD decoder behind Apple High Performance system audio.
//!
//! Compiled only with the `apple-hp-audio` feature, and the sole reason that feature
//! exists: the Mac's `RemoteDesktopSystemAudio` transmitter encodes AAC-ELD (MPEG-4
//! audio object type 39) whatever the negotiation agreed — see
//! `docs/apple-vnc-889.md` — and neither a browser's WebCodecs nor FFmpeg's native
//! decoder will take it, so the gateway has to turn it into PCM itself. Fraunhofer's
//! fdk-aac is the one portable decoder that can, and its licence is not
//! OSI-approved, which is why the default build never links it.
//!
//! The decoder is configured out of band. RTP carries bare access units with no
//! header naming the stream, so the AudioSpecificConfig is stated here, once, from
//! measurement: 48 kHz, stereo, 480-sample frames (the RTP timestamp advances exactly
//! 480 per packet), no SBR and no error-resilience tools. Run against 377 captured
//! frames it decoded 375 cleanly and concealed two; every other reading — 512-sample
//! frames, the resilience flags, SBR — either refused the configuration or concealed
//! most of the stream.

use anyhow::Context as _;
use fdk_aac::dec::{Decoder, Transport};

/// AudioSpecificConfig for what the Mac sends, bit by bit:
///
/// ```text
/// 11111   audioObjectType escape
/// 000111  audioObjectType 39 - 32 = ER AAC ELD
/// 0011    samplingFrequencyIndex 3 = 48 000 Hz
/// 0010    channelConfiguration 2 = stereo
/// 1       frameLengthFlag: 480-sample frames
/// 0 0 0   section / scalefactor / spectral data resilience: off
/// 0       ldSbrPresentFlag: no SBR
/// 0000    eldExtType ELDEXT_TERM
/// ```
pub const AUDIO_SPECIFIC_CONFIG: [u8; 4] = [0xf8, 0xe6, 0x50, 0x00];

/// Samples per channel in one access unit, which is also one RTP packet: 10 ms.
pub const FRAME_SAMPLES: usize = 480;

/// Channels in the stream. Fixed by the configuration above, not read back from
/// the decoder per frame.
pub const CHANNELS: usize = 2;

/// One decoder for one stream's access units.
pub struct EldDecoder {
    decoder: Decoder,
    /// Scratch for one decoded frame; fdk-aac writes interleaved `i16`.
    pcm: Vec<i16>,
}

impl EldDecoder {
    pub fn new() -> anyhow::Result<Self> {
        let mut decoder = Decoder::new(Transport::Raw).context("open the AAC-ELD decoder")?;
        decoder
            .config_raw(&AUDIO_SPECIFIC_CONFIG)
            .context("configure the AAC-ELD decoder for 48 kHz stereo 480-sample frames")?;
        Ok(Self {
            decoder,
            pcm: vec![0; FRAME_SAMPLES * CHANNELS],
        })
    }

    /// Decode one access unit into interleaved little-endian 16-bit PCM appended to
    /// `out`.
    ///
    /// A *concealed* frame — the decoder found the unit damaged and synthesised
    /// something plausible in its place — is still appended, because the alternative
    /// is a 10 ms hole where the decoder had already filled one. Only a frame the
    /// decoder could not produce at all is an error, and the caller keeps going: the
    /// next unit is independently decodable.
    pub fn decode(&mut self, access_unit: &[u8], out: &mut Vec<u8>) -> anyhow::Result<bool> {
        let took = self
            .decoder
            .fill(access_unit)
            .map_err(|e| anyhow::anyhow!("feed the AAC-ELD decoder: {e}"))?;
        anyhow::ensure!(
            took == access_unit.len(),
            "the AAC-ELD decoder took {took} of a {}-byte access unit",
            access_unit.len()
        );
        let concealed = match self.decoder.decode_frame(&mut self.pcm) {
            Ok(()) => false,
            Err(e) if e.is_concealed() => true,
            Err(e) => anyhow::bail!("decode an AAC-ELD access unit: {e}"),
        };
        let produced = self.decoder.decoded_frame_size().min(self.pcm.len());
        out.reserve(produced * 2);
        for sample in &self.pcm[..produced] {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        Ok(concealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The configuration is accepted by the decoder that is actually linked — the
    /// one check that does not need a captured stream.
    #[test]
    fn the_audio_specific_config_is_accepted() {
        EldDecoder::new().expect("fdk-aac accepts the AAC-ELD 48 kHz stereo configuration");
    }

    /// The bit layout above, re-derived: the constant is the bytes and this is the
    /// arithmetic, so a wrong transcription of either shows up here.
    #[test]
    fn the_audio_specific_config_says_what_its_comment_says() {
        let mut bits = Vec::new();
        let mut push = |value: u32, width: u32| {
            for i in (0..width).rev() {
                bits.push(((value >> i) & 1) as u8);
            }
        };
        push(31, 5);
        push(39 - 32, 6);
        push(3, 4);
        push(2, 4);
        push(1, 1); // 480-sample frames
        push(0, 3); // resilience tools off
        push(0, 1); // no SBR
        push(0, 4); // ELDEXT_TERM
        while bits.len() % 8 != 0 {
            bits.push(0);
        }
        let bytes: Vec<u8> =
            bits.chunks(8).map(|byte| byte.iter().fold(0u8, |acc, b| (acc << 1) | b)).collect();
        assert_eq!(bytes, AUDIO_SPECIFIC_CONFIG);
    }

    /// Garbage is refused as a decode error rather than a panic, and the decoder is
    /// still usable afterwards.
    #[test]
    fn a_bad_access_unit_is_an_error_not_a_crash() {
        let mut decoder = EldDecoder::new().unwrap();
        let mut out = Vec::new();
        // Whatever this does — conceal or refuse — it must return.
        let _ = decoder.decode(&[0xff; 40], &mut out);
        let _ = decoder.decode(&[0x00; 40], &mut out);
    }
}
