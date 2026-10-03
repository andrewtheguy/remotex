//! wlshare's audio extension: the desktop's sound carried on the RFB connection
//! itself, as Opus or as FLAC, and passed to the browser as it came.
//!
//! The extension is private to wlshare, and this is the client it is for:
//!
//! - The client lists the pseudo-encoding [`ENCODING`] (`WLSF`) in
//!   `SetEncodings`, as this engine does in every `wlshare` session started
//!   with sound, and beside it [`ENCODING_OPUS`] (`WLOP`) unless that sound is
//!   lossless: wlshare codes the sound as FLAC for a list without it and as
//!   Opus for one with. The codec is this client's to choose, from what was
//!   chosen at the picker, and nothing in wlshare's configuration.
//! - A server that speaks it announces so with an **empty pseudo-rectangle** of
//!   [`ENCODING`] inside a `FramebufferUpdate` — the only announcement there
//!   is, the same shape ExtendedDesktopSize uses. A server that does not speak
//!   it says nothing, which is how the extension is discovered rather than
//!   configured.
//! - The client then names the format it wants ([`set_format`]), the rate Opus
//!   is to be coded at ([`set_bitrate`]), and turns the stream on ([`enable`]);
//!   [`disable`] turns it off again. Set-format, enable and disable are the
//!   client messages of the QEMU Audio extension `rfbproto` registers, message
//!   type [`MSG_QEMU`] submessage [`SUBMESSAGE_AUDIO`], taken as they are;
//!   set-bitrate is wlshare's own operation beside them, and may be sent again
//!   while the stream runs.
//! - The server answers with [`ServerAudio::Begin`], a run of frames, one to a
//!   [`MSG_FRAME`] message, and [`ServerAudio::End`]. Begin and end are QEMU's
//!   messages again; the frames are wlshare's own, each one FLAC frame or one
//!   Opus packet.
//!
//! QEMU's own pseudo-encoding, -259, is not listed: what it promises is raw
//! samples, and a server that answers it — QEMU itself — would send them. A
//! target on such a server gets a desktop and no sound, as on any server that
//! does not speak this extension.
//!
//! Nothing is decoded or coded here. An Opus packet is what the browser's own
//! decoder takes, so it goes to the bridge as it came ([`PASSED_OPUS`]), made by
//! the encoder this gateway codes an RDP host's sound with (`sound-opus`), at
//! the rate the target's audio keys and their walk arrive at. A FLAC frame goes
//! the same way in a session started with lossless sound, for
//! the page's own decoder ([`PASSED_FLAC`]). Neither stream's header is sent by
//! wlshare: everything in one follows from the format this client set and the
//! extension's one rule, that every frame is [`BLOCK_FRAMES`] frames of it.
//!
//! wlshare is the server this was built against; see docs/wlshare-audio.md.
//! Apple's dialects are not asked: neither Screen Sharing subtype speaks this
//! extension.

use crate::audio::PcmFormat;

/// The extension's pseudo-encoding, the ASCII bytes `WLSF`. Listed in
/// `SetEncodings`; answered with an empty rectangle of the same number.
pub const ENCODING: i32 = 0x574c_5346;
/// Listed beside [`ENCODING`], asks for the sound as Opus in place of FLAC: the
/// ASCII bytes `WLOP`.
pub const ENCODING_OPUS: i32 = 0x574c_4f50;

/// The message type every QEMU extension shares, in both directions: this
/// client's set-format, enable and disable, and the server's begin and end.
pub const MSG_QEMU: u8 = 255;
/// The submessage under [`MSG_QEMU`] that is audio.
pub const SUBMESSAGE_AUDIO: u8 = 1;

/// The frame message's type, server → client: three bytes of padding, a `u32`
/// length and one FLAC frame or one Opus packet follow. Outside every registered RFB message
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
/// Client operation: the rate Opus is to be coded at, which follows. wlshare's
/// own, beside QEMU's three.
const CLIENT_SET_BITRATE: u16 = 3;

/// Server operation: the stream stopped.
const SERVER_END: u16 = 0;
/// Server operation: a stream started.
const SERVER_BEGIN: u16 = 1;

/// What this client asks every server for, and so the one format the sound is
/// coded from: 48 kHz stereo signed 16-bit, little-endian.
///
/// The format is the *client's* to choose — the server converts whatever the
/// desktop plays into it — so there is nothing to negotiate: this is Opus's own
/// rate, and one the browser plays without resampling.
pub const SOURCE_FORMAT: PcmFormat = PcmFormat {
    channels: 2,
    sample_rate: 48_000,
    bits_per_sample: 16,
};

/// The frames in every FLAC frame and Opus packet: twenty milliseconds of [`SOURCE_FORMAT`],
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

/// Client → server: the format every following FLAC frame decodes to, and the
/// one Opus is coded from.
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

/// Client → server: the rate, in bits per second, Opus is coded at from the
/// next packet on, before a stream or during one. wlshare takes 6 000 to
/// 510 000, libopus's bounds and the config's ([`crate::config`]); a rate
/// outside them ends the connection.
///
/// | Offset | Type | Field |
/// |---|---|---|
/// | 0 | U8 | 255 |
/// | 1 | U8 | 1 |
/// | 2 | U16 | operation, 3 |
/// | 4 | U32 | bits per second |
pub fn set_bitrate(bps: u32) -> [u8; 8] {
    let mut msg = [0u8; 8];
    msg[..4].copy_from_slice(&client_op(CLIENT_SET_BITRATE));
    msg[4..].copy_from_slice(&bps.to_be_bytes());
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
    /// A stream started; its frames follow.
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
             frames or Opus packets and sends only begin and end under this type"
        ),
    }
}

/// The length of the frame a frame message's header, the bytes after its
/// type, announces.
///
/// | Offset | Type | Field |
/// |---|---|---|
/// | 0 | U8 | message type, `0xE4` |
/// | 1 | U8[3] | padding |
/// | 4 | U32 | length of the frame |
/// | 8 | U8[] | one FLAC frame or one Opus packet |
pub fn frame_length(header: [u8; FRAME_HEADER_LEN]) -> u32 {
    u32::from_be_bytes([header[3], header[4], header[5], header[6]])
}

/// A lossless session's FLAC frames as the browser is told of them:
/// [`SOURCE_FORMAT`] in blocks of [`BLOCK_FRAMES`],
/// with no head, since each frame states its own shape and the page's decoder
/// holds it to this one.
pub const PASSED_FLAC: crate::audio::PassedFormat = crate::audio::PassedFormat {
    codec: crate::audio::FLAC_CODEC,
    sample_rate: SOURCE_FORMAT.sample_rate,
    channels: SOURCE_FORMAT.channels,
    packet_frames: BLOCK_FRAMES as u32,
    head: &[],
};

/// An Opus stream's packets as the browser is told of them: the same stream an
/// encoder here makes of an RDP host's sound, behind the `OpusHead` wlshare
/// never sends — two channels, the encoder's lookahead as the pre-skip
/// ([`sound_opus::PRE_SKIP`], 312) and [`SOURCE_FORMAT`]'s rate, as
/// [`sound_opus::Stream::head`] lays it out.
pub const PASSED_OPUS: crate::audio::PassedFormat = crate::audio::PassedFormat {
    codec: crate::opus_stream::OPUS_CODEC,
    sample_rate: SOURCE_FORMAT.sample_rate,
    channels: SOURCE_FORMAT.channels,
    packet_frames: BLOCK_FRAMES as u32,
    head: b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00",
};

/// What a session's sound is passed to the browser as: wlshare's Opus, or its
/// FLAC in one started with its sound lossless.
pub fn passed(lossless: bool) -> crate::audio::PassedFormat {
    if lossless { PASSED_FLAC } else { PASSED_OPUS }
}

#[cfg(test)]
mod tests {
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
        // wlshare's own operation: 96 000 bit/s, big-endian.
        assert_eq!(set_bitrate(96_000), [255, 1, 0, 3, 0, 1, 0x77, 0]);
    }

    #[test]
    fn the_encoding_is_wlshares() {
        assert_eq!(ENCODING.to_be_bytes(), *b"WLSF");
        assert_eq!(ENCODING_OPUS.to_be_bytes(), *b"WLOP");
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

    /// What the browser is told of a passed stream is what wlshare was asked
    /// for: the head an Opus decoder is configured from is the one the shared
    /// encoder lays out for this format, and FLAC has none.
    #[test]
    fn a_passed_stream_is_described_as_the_format_that_was_asked_for() {
        let stream = sound_opus::Stream { rate: SOURCE_FORMAT.sample_rate, channels: SOURCE_FORMAT.channels as u8 };
        assert_eq!(PASSED_OPUS.head, stream.head(sound_opus::PRE_SKIP, SOURCE_FORMAT.sample_rate).unwrap());
        assert_eq!(stream.block(), usize::from(BLOCK_FRAMES));
        for format in [PASSED_OPUS, PASSED_FLAC] {
            assert_eq!((format.sample_rate, format.channels, format.packet_frames), (48_000, 2, 960));
        }
        assert_eq!((passed(false).codec, passed(true).codec), ("opus", "flac"));
        assert!(PASSED_FLAC.head.is_empty());
    }
}
