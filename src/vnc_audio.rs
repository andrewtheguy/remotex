//! wlshare's audio extension: the desktop's sound carried on the RFB connection
//! itself, as FLAC.
//!
//! The extension is private to wlshare, and this is the client it is for:
//!
//! - The client lists the pseudo-encoding [`ENCODING`] (`WLSF`) in
//!   `SetEncodings`, as this engine does on every plain `vnc` target that asked
//!   for audio.
//! - A server that speaks it announces so with an **empty pseudo-rectangle** of
//!   that encoding inside a `FramebufferUpdate` — the only announcement there
//!   is, the same shape ExtendedDesktopSize uses. A server that does not speak
//!   it says nothing, which is how the extension is discovered rather than
//!   configured.
//! - The client then names the format it wants ([`set_format`]) and turns the
//!   stream on ([`enable`]); [`disable`] turns it off again. These are the
//!   client messages of the QEMU Audio extension `rfbproto` registers, message
//!   type [`MSG_QEMU`] submessage [`SUBMESSAGE_AUDIO`], taken as they are.
//! - The server answers with [`ServerAudio::Begin`], a run of FLAC frames, one
//!   to a [`MSG_FRAME`] message, and [`ServerAudio::End`]. Begin and end are
//!   QEMU's messages again; the frames are wlshare's own.
//!
//! QEMU's own pseudo-encoding, -259, is not listed: what it promises is raw
//! samples, and a server that answers it — QEMU itself — would send them. A
//! target on such a server gets a desktop and no sound, as on any server that
//! does not speak this extension.
//!
//! FLAC is lossless, so [`FrameDecoder`] hands the bridge exactly the samples
//! wlshare captured, while the connection carries about two-thirds of their
//! PCM rate or less, and a silent desktop a few bytes a frame. The FLAC stream
//! header (`STREAMINFO`) is never sent: everything in it follows from the format
//! this client set and the extension's one rule, that every frame is
//! [`BLOCK_FRAMES`] frames of it, so the decoder builds it here ([`streaminfo`]).
//!
//! wlshare is the server this was built against; see docs/wlshare-audio.md.
//! Apple's dialects are not asked: neither Screen Sharing subtype speaks this
//! extension, and High Performance carries its system audio over the separate
//! media stream in [`crate::vnc_apple_audio`].

use anyhow::Context as _;
use symphonia_bundle_flac::FlacDecoder;
use symphonia_core::codecs::audio::well_known::CODEC_ID_FLAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder as _, AudioDecoderOptions};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

use crate::audio::PcmFormat;

/// The extension's pseudo-encoding, the ASCII bytes `WLSF`. Listed in
/// `SetEncodings`; answered with an empty rectangle of the same number.
pub const ENCODING: i32 = 0x574c_5346;

/// The message type every QEMU extension shares, in both directions: this
/// client's set-format, enable and disable, and the server's begin and end.
pub const MSG_QEMU: u8 = 255;
/// The submessage under [`MSG_QEMU`] that is audio.
pub const SUBMESSAGE_AUDIO: u8 = 1;

/// The FLAC frame message's type, server → client: three bytes of padding, a
/// `u32` length and one FLAC frame follow. Outside every registered RFB message
/// type.
pub const MSG_FRAME: u8 = 0xE4;
/// The bytes of a frame message after its type and before the frame.
pub const FRAME_HEADER_LEN: usize = 7;

/// Client operation: start sending audio.
const CLIENT_ENABLE: u16 = 0;
/// Client operation: stop sending audio.
const CLIENT_DISABLE: u16 = 1;
/// Client operation: the sample format the client wants, which follows.
const CLIENT_SET_FORMAT: u16 = 2;

/// Server operation: the stream stopped.
const SERVER_END: u16 = 0;
/// Server operation: a stream started.
const SERVER_BEGIN: u16 = 1;

/// What this client asks every server for, and so the one format its wave
/// buffers are in: 48 kHz stereo signed 16-bit, little-endian.
///
/// The format is the *client's* to choose — the server converts whatever the
/// desktop plays into it — so there is nothing to negotiate and nothing to
/// resample: this is Opus's own rate, and the passthrough encoder's PCM.
pub const SOURCE_FORMAT: PcmFormat = PcmFormat {
    channels: 2,
    sample_rate: 48_000,
    bits_per_sample: 16,
};

/// The frames in every FLAC frame: twenty milliseconds of [`SOURCE_FORMAT`],
/// which the extension fixes at the frequency over fifty.
pub const BLOCK_FRAMES: u16 = (SOURCE_FORMAT.sample_rate / 50) as u16;

/// A sample's encoding, as QEMU's set-format numbers them. Only [`Self::S16`]
/// is ever asked for; the rest wlshare carries are named so the number sent is
/// a choice rather than a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    U8 = 0,
    S8 = 1,
    U16 = 2,
    S16 = 3,
}

/// The format a `set-format` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample: SampleFormat,
    /// 1 or 2; the extension allows no more.
    pub channels: u8,
    /// Samples per second per channel.
    pub frequency: u32,
}

/// [`SOURCE_FORMAT`] as the extension states it.
pub const WANTED: AudioFormat = AudioFormat {
    sample: SampleFormat::S16,
    channels: 2,
    frequency: 48_000,
};

fn client_op(operation: u16) -> [u8; 4] {
    let op = operation.to_be_bytes();
    [MSG_QEMU, SUBMESSAGE_AUDIO, op[0], op[1]]
}

/// Client → server: the format every following FLAC frame decodes to.
///
/// | Offset | Type | Field |
/// |---|---|---|
/// | 0 | U8 | 255 |
/// | 1 | U8 | 1 |
/// | 2 | U16 | operation, 2 |
/// | 4 | U8 | sample format |
/// | 5 | U8 | channels |
/// | 6 | U32 | frequency |
pub fn set_format(format: AudioFormat) -> [u8; 10] {
    let mut msg = [0u8; 10];
    msg[..4].copy_from_slice(&client_op(CLIENT_SET_FORMAT));
    msg[4] = format.sample as u8;
    msg[5] = format.channels;
    msg[6..].copy_from_slice(&format.frequency.to_be_bytes());
    msg
}

/// Client → server: start the stream.
pub fn enable() -> [u8; 4] {
    client_op(CLIENT_ENABLE)
}

/// Client → server: stop the stream.
pub fn disable() -> [u8; 4] {
    client_op(CLIENT_DISABLE)
}

/// What a message-255 submessage from the server turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerAudio {
    /// A stream started; FLAC frames follow.
    Begin,
    /// The stream stopped.
    End,
}

/// Read the three bytes after a message type of [`MSG_QEMU`]: the submessage
/// and the operation.
///
/// Anything but begin and end is an error rather than a skip: the QEMU
/// submessages have no common length field, so a message this client cannot
/// measure leaves the stream at an offset nothing can recover from. That
/// includes QEMU's data operation, raw samples, which a server that announced
/// this extension never sends.
pub fn parse_server(header: [u8; 3]) -> anyhow::Result<ServerAudio> {
    anyhow::ensure!(
        header[0] == SUBMESSAGE_AUDIO,
        "the server sent QEMU submessage {}, and audio ({SUBMESSAGE_AUDIO}) is the only one \
         this client advertised or can measure",
        header[0]
    );
    let operation = u16::from_be_bytes([header[1], header[2]]);
    match operation {
        SERVER_BEGIN => Ok(ServerAudio::Begin),
        SERVER_END => Ok(ServerAudio::End),
        other => anyhow::bail!(
            "the server sent audio operation {other}; wlshare's extension carries sound as FLAC \
             frames and sends only begin and end under this type"
        ),
    }
}

/// The length of the FLAC frame a frame message's header, the bytes after its
/// type, announces.
///
/// | Offset | Type | Field |
/// |---|---|---|
/// | 0 | U8 | message type, `0xE4` |
/// | 1 | U8[3] | padding |
/// | 4 | U32 | length of the frame |
/// | 8 | U8[] | one FLAC frame |
pub fn frame_length(header: [u8; FRAME_HEADER_LEN]) -> u32 {
    u32::from_be_bytes([header[3], header[4], header[5], header[6]])
}

/// The `STREAMINFO` block wlshare's frames decode against, built from
/// [`SOURCE_FORMAT`] as the FLAC specification lays it out: the block size as
/// both minimum and maximum, the frame sizes and total samples unknown, and no
/// MD5.
pub fn streaminfo() -> [u8; 34] {
    let mut info = [0u8; 34];
    info[0..2].copy_from_slice(&BLOCK_FRAMES.to_be_bytes());
    info[2..4].copy_from_slice(&BLOCK_FRAMES.to_be_bytes());
    // Bytes 4..10 are the minimum and maximum frame sizes, zero for unknown.
    // Then the rate (20 bits), channels - 1 (3), bits per sample - 1 (5) and
    // total samples (36, zero for unknown).
    let packed = (u64::from(SOURCE_FORMAT.sample_rate) << 44)
        | (u64::from(SOURCE_FORMAT.channels - 1) << 41)
        | (u64::from(SOURCE_FORMAT.bits_per_sample - 1) << 36);
    info[10..18].copy_from_slice(&packed.to_be_bytes());
    info
}

/// One stream's decoder: a FLAC frame in, its samples out as the interleaved
/// little-endian 16-bit stereo [`SOURCE_FORMAT`] names.
///
/// Made at each begin, since frames are numbered from zero again there. Each
/// frame decodes on its own, so one wlshare dropped for a session that fell
/// behind costs nothing but its own twenty milliseconds.
pub struct FrameDecoder {
    decoder: FlacDecoder,
    samples: Vec<i16>,
}

impl FrameDecoder {
    pub fn new() -> anyhow::Result<Self> {
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_FLAC).with_extra_data(Box::new(streaminfo()));
        let decoder = FlacDecoder::try_new(&params, &AudioDecoderOptions::default())
            .context("setting up the FLAC decoder")?;
        Ok(Self { decoder, samples: Vec::new() })
    }

    /// The samples of one FLAC frame, which must be exactly [`BLOCK_FRAMES`]
    /// frames of [`SOURCE_FORMAT`]: a frame of any other shape is not one this
    /// client asked for.
    pub fn decode(&mut self, frame: Vec<u8>) -> anyhow::Result<Vec<u8>> {
        // symphonia's decoder expects a demuxer to have vetted the frame: it
        // takes the channels from the STREAMINFO built here, not from the
        // frame, and leaves the frame's CRC-16 unchecked. Both are this
        // client's to check.
        check_frame(&frame)?;
        let packet = Packet::new(0, Timestamp::new(0), Duration::new(u64::from(BLOCK_FRAMES)), frame);
        let decoded = self.decoder.decode(&packet).context("decoding a FLAC frame")?;
        anyhow::ensure!(
            decoded.frames() == usize::from(BLOCK_FRAMES)
                && decoded.spec().channels().count() == usize::from(SOURCE_FORMAT.channels),
            "a FLAC frame of {} frames and {} channels, not the {BLOCK_FRAMES} and {} asked for",
            decoded.frames(),
            decoded.spec().channels().count(),
            SOURCE_FORMAT.channels
        );
        // The decoder scales to 32 bits; the conversion to 16 takes the top
        // half, which is exactly the sample that went in.
        decoded.copy_to_vec_interleaved(&mut self.samples);
        Ok(self.samples.iter().flat_map(|sample| sample.to_le_bytes()).collect())
    }
}

/// Check a FLAC frame's header against [`SOURCE_FORMAT`] and [`BLOCK_FRAMES`],
/// and its CRC-8 and CRC-16, as the FLAC specification lays them out.
fn check_frame(frame: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(frame.len() >= 8, "a FLAC frame of {} bytes", frame.len());
    anyhow::ensure!(frame[..2] == [0xFF, 0xF8], "not a fixed-blocking FLAC frame");
    let (block_code, rate_code) = (frame[2] >> 4, frame[2] & 0xF);
    let (channel_code, size_code) = (frame[3] >> 4, (frame[3] >> 1) & 0b111);
    anyhow::ensure!(
        matches!(channel_code, 0x1 | 0x8..=0xA),
        "a FLAC frame with channel assignment {channel_code}, not stereo"
    );
    anyhow::ensure!(
        matches!(size_code, 0 | 0x4) && frame[3] & 1 == 0,
        "a FLAC frame of sample size code {size_code}, not 16 bits"
    );
    // The frame number, UTF-8 coded: a lead byte's leading ones count its bytes.
    let number_len = match frame[4].leading_ones() {
        0 => 1,
        n @ 2..=7 => n as usize,
        _ => anyhow::bail!("a FLAC frame number that is not UTF-8 coded"),
    };
    let mut at = 4 + number_len;
    let mut field = |len: usize| -> anyhow::Result<u32> {
        let bytes = frame.get(at..at + len).context("a truncated FLAC frame header")?;
        at += len;
        Ok(bytes.iter().fold(0, |value, &byte| value << 8 | u32::from(byte)))
    };
    let block = match block_code {
        0x6 => field(1)? + 1,
        0x7 => field(2)? + 1,
        _ => 0,
    };
    anyhow::ensure!(block == u32::from(BLOCK_FRAMES), "a FLAC frame whose block is not {BLOCK_FRAMES} frames");
    let rate = match rate_code {
        0x0 | 0xA => SOURCE_FORMAT.sample_rate,
        0xC => field(1)? * 1000,
        0xD => field(2)?,
        0xE => field(2)? * 10,
        _ => 0,
    };
    anyhow::ensure!(rate == SOURCE_FORMAT.sample_rate, "a FLAC frame not at {} Hz", SOURCE_FORMAT.sample_rate);
    anyhow::ensure!(
        frame.get(at) == Some(&crc8(&frame[..at])),
        "a FLAC frame header whose CRC-8 does not match"
    );
    let (body, crc) = frame.split_at(frame.len() - 2);
    anyhow::ensure!(
        u16::from_be_bytes([crc[0], crc[1]]) == crc16(body),
        "a FLAC frame whose CRC-16 does not match"
    );
    Ok(())
}

/// FLAC's header CRC: polynomial 0x07, initialised to zero.
fn crc8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |crc, &byte| {
        (0..8).fold(crc ^ byte, |crc, _| if crc & 0x80 != 0 { crc << 1 ^ 0x07 } else { crc << 1 })
    })
}

/// FLAC's frame CRC: polynomial 0x8005, initialised to zero.
fn crc16(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0, |crc, &byte| {
        (0..8).fold(crc ^ u16::from(byte) << 8, |crc, _| {
            if crc & 0x8000 != 0 { crc << 1 ^ 0x8005 } else { crc << 1 }
        })
    })
}

#[cfg(test)]
mod tests {
    use flacenc::bitsink::ByteSink;
    use flacenc::component::{BitRepr as _, StreamInfo};
    use flacenc::error::Verify as _;
    use flacenc::source::{Fill as _, FrameBuf};

    use super::*;

    /// A decoder for the client messages written from the extension's text —
    /// type, submessage, big-endian operation, and for set-format a sample
    /// format byte, a channel byte and a big-endian frequency — rather than
    /// from the builders above.
    fn decode_client(msg: &[u8]) -> (u16, Option<AudioFormat>) {
        assert_eq!(msg[0], 255, "every QEMU message is type 255");
        assert_eq!(msg[1], 1, "submessage 1 is audio");
        let operation = u16::from_be_bytes([msg[2], msg[3]]);
        if operation != 2 {
            assert_eq!(msg.len(), 4, "enable and disable carry nothing");
            return (operation, None);
        }
        assert_eq!(msg.len(), 10);
        let sample = match msg[4] {
            0 => SampleFormat::U8,
            1 => SampleFormat::S8,
            2 => SampleFormat::U16,
            3 => SampleFormat::S16,
            other => panic!("sample format {other}"),
        };
        let frequency = u32::from_be_bytes([msg[6], msg[7], msg[8], msg[9]]);
        (operation, Some(AudioFormat { sample, channels: msg[5], frequency }))
    }

    #[test]
    fn the_client_messages_have_the_documented_layouts() {
        assert_eq!(decode_client(&enable()), (0, None));
        assert_eq!(decode_client(&disable()), (1, None));
        assert_eq!(decode_client(&set_format(WANTED)), (2, Some(WANTED)));
        // The format asked for is the format the wave buffers are declared in.
        assert_eq!(WANTED.sample, SampleFormat::S16);
        assert_eq!(u32::from(WANTED.channels), u32::from(SOURCE_FORMAT.channels));
        assert_eq!(WANTED.frequency, SOURCE_FORMAT.sample_rate);
        assert_eq!(SOURCE_FORMAT.bits_per_sample, 16);
        // And the bytes themselves, for a reader with only the spec table.
        assert_eq!(set_format(WANTED), [255, 1, 0, 2, 3, 2, 0, 0, 0xBB, 0x80]);
        assert_eq!(enable(), [255, 1, 0, 0]);
        assert_eq!(disable(), [255, 1, 0, 1]);
    }

    #[test]
    fn the_encoding_is_wlshares() {
        assert_eq!(ENCODING.to_be_bytes(), *b"WLSF");
        assert_eq!(BLOCK_FRAMES, 960, "20 ms at 48 kHz");
    }

    #[test]
    fn begin_and_end_are_the_only_server_operations() {
        assert_eq!(parse_server([1, 0, 1]).unwrap(), ServerAudio::Begin);
        assert_eq!(parse_server([1, 0, 0]).unwrap(), ServerAudio::End);
        // QEMU's raw data is not carried, and nothing else under type 255 is
        // advertised; none of it is framed by a length this client could step
        // over.
        assert!(parse_server([1, 0, 2]).is_err());
        assert!(parse_server([1, 0, 3]).is_err());
        assert!(parse_server([0, 0, 1]).is_err());
        assert!(parse_server([2, 0, 1]).is_err());
    }

    #[test]
    fn a_frame_message_header_is_padding_and_a_length() {
        let wire = [0xE4, 0, 0, 0, 0, 0, 0x01, 0x02];
        assert_eq!(wire[0], MSG_FRAME);
        assert_eq!(frame_length(wire[1..].try_into().unwrap()), 0x0102);
    }

    /// The header's fields, read back by a reader written from the FLAC
    /// specification's bit layout rather than the packing above.
    #[test]
    fn the_streaminfo_is_the_asked_format_in_twenty_millisecond_blocks() {
        let info = streaminfo();
        assert_eq!(u16::from_be_bytes([info[0], info[1]]), 960);
        assert_eq!(u16::from_be_bytes([info[2], info[3]]), 960);
        assert_eq!(&info[4..10], &[0; 6]);
        let rate = (u32::from(info[10]) << 12) | (u32::from(info[11]) << 4) | (u32::from(info[12]) >> 4);
        assert_eq!(rate, 48_000);
        assert_eq!(((info[12] >> 1) & 0b111) + 1, 2, "channels");
        assert_eq!((((info[12] & 1) << 4) | (info[13] >> 4)) + 1, 16, "bits per sample");
        assert_eq!(&info[18..], &[0; 16], "no MD5");
    }

    /// Frames as wlshare makes them — flacenc, in fixed 960-frame blocks —
    /// which shares nothing with the symphonia decoder under test.
    fn encode(pcm: &[u8]) -> Vec<Vec<u8>> {
        let block = usize::from(BLOCK_FRAMES);
        let mut info = StreamInfo::new(48_000, 2, 16).unwrap();
        info.set_block_sizes(block, block).unwrap();
        let config = flacenc::config::Encoder::default().into_verified().unwrap();
        let mut framebuf = FrameBuf::with_size(2, block).unwrap();
        pcm.chunks(block * 4)
            .enumerate()
            .map(|(n, chunk)| {
                framebuf.fill_le_bytes(chunk, 2).unwrap();
                let frame = flacenc::encode_fixed_size_frame(&config, &framebuf, n, &info).unwrap();
                let mut sink = ByteSink::new();
                frame.write(&mut sink).unwrap();
                sink.into_inner()
            })
            .collect()
    }

    /// A tone with noise on it, 16-bit stereo, whole blocks of it.
    fn signal(blocks: usize) -> Vec<u8> {
        let mut seed = 0x2545_f491_u32;
        let mut pcm = Vec::new();
        for n in 0..blocks * usize::from(BLOCK_FRAMES) {
            for channel in 0..2 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let t = n as f64 / 48_000.0;
                let tone = (t * 440.0 * f64::from(channel + 1) * std::f64::consts::TAU).sin() * 0.7;
                let noise = (f64::from(seed) / f64::from(u32::MAX) - 0.5) * 0.2;
                pcm.extend_from_slice(&(((tone + noise) * 32_767.0) as i16).to_le_bytes());
            }
        }
        pcm
    }

    #[test]
    fn frames_decode_to_exactly_the_samples_that_were_encoded() {
        let pcm = signal(4);
        let frames = encode(&pcm);
        assert_eq!(frames.len(), 4);
        let mut decoder = FrameDecoder::new().unwrap();
        let decoded: Vec<u8> = frames.into_iter().flat_map(|frame| decoder.decode(frame).unwrap()).collect();
        assert!(decoded == pcm, "FLAC is lossless");
    }

    /// A frame lost on the server's side costs its own samples and nothing
    /// after it: the next decodes as though nothing happened.
    #[test]
    fn a_missing_frame_costs_only_its_own_samples() {
        let pcm = signal(3);
        let mut frames = encode(&pcm);
        frames.remove(1);
        let mut decoder = FrameDecoder::new().unwrap();
        let block = usize::from(BLOCK_FRAMES) * 4;
        assert_eq!(decoder.decode(frames.remove(0)).unwrap(), &pcm[..block]);
        assert_eq!(decoder.decode(frames.remove(0)).unwrap(), &pcm[2 * block..]);
    }

    #[test]
    fn a_frame_of_the_wrong_size_or_garbage_is_refused() {
        let mut decoder = FrameDecoder::new().unwrap();
        let short = &signal(1)[..480 * 4];
        let mut info = StreamInfo::new(48_000, 2, 16).unwrap();
        info.set_block_sizes(480, 480).unwrap();
        let config = flacenc::config::Encoder::default().into_verified().unwrap();
        let mut framebuf = FrameBuf::with_size(2, 480).unwrap();
        framebuf.fill_le_bytes(short, 2).unwrap();
        let frame = flacenc::encode_fixed_size_frame(&config, &framebuf, 0, &info).unwrap();
        let mut sink = ByteSink::new();
        frame.write(&mut sink).unwrap();
        assert!(decoder.decode(sink.into_inner()).is_err(), "half a block");
        assert!(decoder.decode(vec![0xAB; 64]).is_err(), "not a frame");
        // A whole block, but of one channel.
        let mut info = StreamInfo::new(48_000, 1, 16).unwrap();
        info.set_block_sizes(960, 960).unwrap();
        let mut framebuf = FrameBuf::with_size(1, 960).unwrap();
        framebuf.fill_le_bytes(&signal(1)[..960 * 2], 2).unwrap();
        let frame = flacenc::encode_fixed_size_frame(&config, &framebuf, 0, &info).unwrap();
        let mut sink = ByteSink::new();
        frame.write(&mut sink).unwrap();
        assert!(decoder.decode(sink.into_inner()).is_err(), "mono");
        // A good frame with one bit of its samples flipped.
        let mut damaged = encode(&signal(1)).remove(0);
        let middle = damaged.len() / 2;
        damaged[middle] ^= 1;
        assert!(decoder.decode(damaged).is_err(), "a bit flipped");
        // And a good frame still decodes after both.
        let pcm = signal(1);
        assert_eq!(decoder.decode(encode(&pcm).remove(0)).unwrap(), pcm);
    }
}
