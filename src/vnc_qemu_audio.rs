//! The QEMU Audio extension: the desktop's sound carried on the RFB connection
//! itself.
//!
//! This is the one audio extension `rfbproto` registers, and the only way a
//! generic VNC server can be asked for sound. Pseudo-encoding
//! [`ENCODING`] (-259) and message type [`MSG_QEMU`] with submessage
//! [`SUBMESSAGE_AUDIO`] are the whole of it, in both directions:
//!
//! - The client lists the pseudo-encoding in `SetEncodings`, as this engine does
//!   on every plain `vnc` target that asked for audio.
//! - A server that speaks it announces so with an **empty pseudo-rectangle** of
//!   that encoding inside a `FramebufferUpdate` — the only announcement there
//!   is, the same shape ExtendedDesktopSize uses. A server that does not speak
//!   it says nothing, which is how the extension is discovered rather than
//!   configured.
//! - The client then names the format it wants ([`set_format`]) and turns the
//!   stream on ([`enable`]); [`disable`] turns it off again.
//! - The server answers with [`ServerAudio::Begin`], a run of
//!   [`ServerAudio::Data`] messages carrying raw samples, and
//!   [`ServerAudio::End`].
//!
//! `rfbproto` says nothing about the byte order of a sample wider than eight
//! bits. QEMU writes host-native samples and gtk-vnc reads little-endian ones,
//! so **samples are little-endian** — which is also what
//! [`crate::audio::AudioBridge`] takes, so the samples reach the queue as they
//! arrive, uncopied and unconverted.
//!
//! wlshare is the server this was built against; see docs/wlshare-audio.md.
//! Apple's dialects are not asked: neither Screen Sharing subtype speaks this
//! extension, and High Performance carries its system audio over the separate
//! media stream in [`crate::vnc_apple_audio`].

use crate::audio::PcmFormat;

/// The extension's pseudo-encoding. Listed in `SetEncodings`; answered with an
/// empty rectangle of the same number.
pub const ENCODING: i32 = -259;

/// The message type every QEMU extension shares, in both directions.
pub const MSG_QEMU: u8 = 255;
/// The submessage under [`MSG_QEMU`] that is audio.
pub const SUBMESSAGE_AUDIO: u8 = 1;

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
/// Server operation: samples follow, after their `u32` length.
const SERVER_DATA: u16 = 2;

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

/// A sample's encoding, as the extension numbers them. Only [`Self::S16`] is
/// ever asked for; the rest are named so the number sent is a choice rather
/// than a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    U8 = 0,
    S8 = 1,
    U16 = 2,
    S16 = 3,
    U32 = 4,
    S32 = 5,
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

/// Client → server: the format every following [`ServerAudio::Data`] is in.
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
    /// A stream started; data follows.
    Begin,
    /// The stream stopped.
    End,
    /// Samples follow: this many bytes of them, read by the caller.
    Data { bytes: u32 },
}

/// Read the three bytes after a message type of [`MSG_QEMU`]: the submessage
/// and the operation. A `Data` operation's `u32` length is read by the caller,
/// which is also what reads the samples.
///
/// Anything but audio is an error rather than a skip: the QEMU submessages have
/// no common length field, so a message this client cannot measure leaves the
/// stream at an offset nothing can recover from.
pub fn parse_server(header: [u8; 3], length: u32) -> anyhow::Result<ServerAudio> {
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
        SERVER_DATA => Ok(ServerAudio::Data { bytes: length }),
        other => anyhow::bail!("the server sent audio operation {other}, which the extension does not define"),
    }
}

/// Whether an operation carries the `u32` length that [`parse_server`] needs —
/// asked of the raw operation before the length is read, because only `Data`
/// has one.
pub fn carries_length(header: [u8; 3]) -> bool {
    header[0] == SUBMESSAGE_AUDIO && u16::from_be_bytes([header[1], header[2]]) == SERVER_DATA
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
            4 => SampleFormat::U32,
            5 => SampleFormat::S32,
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
    fn the_encoding_is_the_registered_one() {
        assert_eq!(ENCODING, -259);
        assert_eq!(ENCODING.to_be_bytes(), [0xFF, 0xFF, 0xFE, 0xFD]);
    }

    #[test]
    fn the_server_operations_parse_and_only_data_is_measured() {
        assert_eq!(parse_server([1, 0, 1], 0).unwrap(), ServerAudio::Begin);
        assert_eq!(parse_server([1, 0, 0], 0).unwrap(), ServerAudio::End);
        assert_eq!(
            parse_server([1, 0, 2], 1920).unwrap(),
            ServerAudio::Data { bytes: 1920 }
        );
        assert!(carries_length([1, 0, 2]));
        assert!(!carries_length([1, 0, 1]));
        assert!(!carries_length([1, 0, 0]));
        assert!(!carries_length([2, 0, 2]), "another submessage is not audio data");
    }

    #[test]
    fn an_unmeasurable_submessage_or_operation_is_fatal() {
        // Nothing else under type 255 is advertised, and none of it is framed by
        // a length this client could step over.
        assert!(parse_server([0, 0, 1], 0).is_err());
        assert!(parse_server([2, 0, 1], 0).is_err());
        assert!(parse_server([1, 0, 3], 0).is_err());
    }
}
