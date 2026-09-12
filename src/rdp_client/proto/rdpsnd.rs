//! Sound redirection (MS-RDPEA): the remote's audio, as PCM, one buffer at a time.
//!
//! The server speaks first at every step. It sends its **format list** and version;
//! the client answers with the formats it will take — here exactly one, 44.1 kHz
//! 16-bit stereo PCM, so that the format index every later buffer carries can only
//! mean that — and, to a version 6 host or later, the quality mode it wants. The
//! server then sends a **training** PDU, a bandwidth probe the client confirms by
//! echoing its numbers. After that come the buffers: a **Wave2** PDU carrying its
//! samples whole, or on the older path a **Wave Info** PDU carrying the first four
//! bytes and a headerless **Wave** PDU carrying the rest behind a four-byte pad. Each
//! buffer is confirmed by block number, which is how the server paces itself.
//! **Close** says the host has nothing playing; the next buffer resumes without a
//! new negotiation.
//!
//! The same conversation runs on either of two transports, and the server picks:
//! the static virtual channel named `rdpsnd`, or the dynamic channel
//! [`DVC_NAME`] that a current Windows host opens instead when the client has the
//! dynamic transport at all. This module is the conversation; the session owns the
//! transports and feeds it whole PDUs from whichever one they arrive on.
//!
//! Only linear PCM is asked for, because it is the one format both ends are required
//! to support ([MS-RDPEA] 2.2.2.1) and the one everything downstream is built on.
//! Ported against FreeRDP's `channels/rdpsnd/client/rdpsnd_main.c`.
//!
//! Two things a Windows host does that the specification does not say, both
//! measured. It redirects no sound to a client that did not also name the `rdpdr`
//! channel — see [`super::rdpdr`]. And it negotiates **nothing until something
//! plays**: a session opened onto a quiet desktop shows the dynamic channel opened
//! and not a byte on it, for as long as the desktop stays quiet. The format list
//! arrives with the first sound, so a probe that asserts on it has to be run with a
//! sound playing on the remote, and a session that stays silent is not, by that
//! alone, a session with anything wrong.
//!
//! [MS-RDPEA]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/bea2d5cf-e3b9-4419-92e5-0e074ff9bc5b

use log::debug;

use super::wire::{Malformed, Reader, Writer};

const WHAT: &str = "a sound redirection PDU";

/// The dynamic channel a current Windows host carries this conversation on.
pub const DVC_NAME: &str = "AUDIO_PLAYBACK_DVC";

const SNDC_CLOSE: u8 = 0x01;
const SNDC_WAVE: u8 = 0x02;
const SNDC_SETVOLUME: u8 = 0x03;
const SNDC_SETPITCH: u8 = 0x04;
const SNDC_WAVECONFIRM: u8 = 0x05;
const SNDC_TRAINING: u8 = 0x06;
const SNDC_FORMATS: u8 = 0x07;
const SNDC_QUALITYMODE: u8 = 0x0C;
const SNDC_WAVE2: u8 = 0x0D;

/// The protocol version this client speaks: Windows 8's, which brought Wave2.
const VERSION: u16 = 0x0008;
/// The version from which a host reads a Quality Mode PDU: Windows 7's.
const QUALITY_FROM: u16 = 0x0006;
const HIGH_QUALITY: u16 = 0x0002;
/// `TSSNDCAPS_ALIVE`: this client will take audio. Neither volume nor pitch is
/// claimed, so a conformant host sends neither.
const ALIVE: u32 = 0x0000_0001;
const WAVE_FORMAT_PCM: u16 = 0x0001;
/// An `AUDIO_FORMAT` without the trailing bytes PCM never has.
const FORMAT_BYTES: u16 = 18;

fn refuse(field: &'static str, value: impl Into<u64>) -> Malformed {
    Malformed::Refused { what: WHAT, field, value: value.into() }
}

/// Linear PCM, interleaved, little-endian: the one kind of audio this path carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
}

impl Format {
    /// Bytes in one sample across every channel: `nBlockAlign`.
    pub const fn block_align(self) -> u16 {
        self.channels * (self.bits_per_sample / 8)
    }

    /// Bytes a second occupies: `nAvgBytesPerSec`.
    pub const fn byte_rate(self) -> u32 {
        self.sample_rate * self.block_align() as u32
    }

    fn write(self, w: &mut Writer) {
        w.u16_le(WAVE_FORMAT_PCM);
        w.u16_le(self.channels);
        w.u32_le(self.sample_rate);
        w.u32_le(self.byte_rate());
        w.u16_le(self.block_align());
        w.u16_le(self.bits_per_sample);
        w.u16_le(0); // cbSize
    }

    /// One `AUDIO_FORMAT` off the wire: `Some` if it is PCM whose derived fields
    /// agree with its stated ones, `None` for anything else, which is skipped whole.
    fn read(r: &mut Reader<'_>) -> Result<Option<Self>, Malformed> {
        let tag = r.u16_le()?;
        let channels = r.u16_le()?;
        let sample_rate = r.u32_le()?;
        let byte_rate = r.u32_le()?;
        let block_align = r.u16_le()?;
        let bits_per_sample = r.u16_le()?;
        let extra = r.u16_le()?;
        r.bytes(usize::from(extra))?;
        let format = Self { channels, sample_rate, bits_per_sample };
        let pcm = tag == WAVE_FORMAT_PCM && block_align == format.block_align() && byte_rate == format.byte_rate();
        Ok(pcm.then_some(format))
    }
}

/// The one format asked for. One rather than a list, and that is load-bearing:
/// a buffer names its format by index into the client's list, and with one entry
/// the index can only mean this.
pub const CD_QUALITY: Format = Format { channels: 2, sample_rate: 44_100, bits_per_sample: 16 };

/// What one PDU amounted to, beyond the replies it earned.
#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    /// The host offered [`CD_QUALITY`] and was told it is taken; buffers may follow.
    Negotiated,
    /// The host offered nothing this client takes, and was told so: it will
    /// redirect nothing. `offered` is how many formats it listed.
    NoFormat { offered: u16 },
    /// One buffer of samples in [`CD_QUALITY`].
    Wave(Vec<u8>),
    /// The host has nothing playing.
    Closed,
    Nothing,
}

/// One PDU acted on: what to send back on the channel it came in on, in order, and
/// what it meant.
#[derive(Debug, PartialEq, Eq)]
pub struct Turn {
    pub replies: Vec<Vec<u8>>,
    pub output: Output,
}

/// The client's side of the conversation, on whichever transport carries it.
#[derive(Default)]
pub struct Rdpsnd {
    /// Whether the last format list was answered with [`CD_QUALITY`], so that a
    /// buffer in format 0 is a buffer in it.
    negotiated: bool,
    /// A Wave Info PDU whose samples are still to come in the Wave PDU after it.
    pending: Option<Pending>,
}

struct Pending {
    timestamp: u16,
    format: u16,
    block: u8,
    head: [u8; 4],
    size: usize,
}

impl Rdpsnd {
    pub fn new() -> Self {
        Self::default()
    }

    /// One whole PDU from the server.
    pub fn push(&mut self, pdu: &[u8]) -> Result<Turn, Malformed> {
        if let Some(pending) = self.pending.take() {
            return self.wave(pending, pdu);
        }
        let mut r = Reader::new(WHAT, pdu);
        let kind = r.u8()?;
        r.u8()?; // bPad
        let body_size = usize::from(r.u16_le()?);
        match kind {
            SNDC_FORMATS => self.formats(&mut r),
            SNDC_TRAINING => {
                let timestamp = r.u16_le()?;
                let pack_size = r.u16_le()?;
                // The rest is padding: a probe of the link, with nothing to read.
                Ok(Turn { replies: vec![training_confirm(timestamp, pack_size)], output: Output::Nothing })
            }
            SNDC_WAVE2 => self.wave2(&mut r, body_size),
            SNDC_WAVE => self.wave_info(&mut r, body_size),
            SNDC_CLOSE => Ok(Turn { replies: Vec::new(), output: Output::Closed }),
            // Capabilities this client did not claim; the browser's own volume
            // control is the one within reach of whoever is listening.
            SNDC_SETVOLUME | SNDC_SETPITCH => {
                debug!("rdp: ignoring a sound volume or pitch PDU ({kind:#04x})");
                Ok(Turn { replies: Vec::new(), output: Output::Nothing })
            }
            other => {
                debug!("rdp: ignoring a sound PDU of type {other:#04x}");
                Ok(Turn { replies: Vec::new(), output: Output::Nothing })
            }
        }
    }

    /// The server's format list, answered with ours if it is among them.
    fn formats(&mut self, r: &mut Reader<'_>) -> Result<Turn, Malformed> {
        r.u32_le()?; // dwFlags
        r.u32_le()?; // dwVolume
        r.u32_le()?; // dwPitch
        r.u16_le()?; // wDGramPort
        let count = r.u16_le()?;
        r.u8()?; // cLastBlockConfirmed
        let version = r.u16_le()?;
        r.u8()?; // bPad
        let mut offered = false;
        for _ in 0..count {
            if Format::read(r)? == Some(CD_QUALITY) {
                offered = true;
            }
        }
        self.negotiated = offered;
        let mut replies = vec![client_formats(offered.then_some(CD_QUALITY))];
        if version >= QUALITY_FROM {
            replies.push(quality_mode());
        }
        let output = if offered { Output::Negotiated } else { Output::NoFormat { offered: count } };
        Ok(Turn { replies, output })
    }

    /// A Wave2 PDU: the buffer, whole.
    fn wave2(&mut self, r: &mut Reader<'_>, body_size: usize) -> Result<Turn, Malformed> {
        let timestamp = r.u16_le()?;
        let format = r.u16_le()?;
        let block = r.u8()?;
        r.bytes(3)?; // bPad
        r.u32_le()?; // dwAudioTimeStamp
        let Some(size) = body_size.checked_sub(12) else {
            return Err(refuse("a Wave2 body size", body_size as u64));
        };
        let data = r.bytes(size)?;
        Ok(Turn { replies: vec![wave_confirm(timestamp, block)], output: self.samples(format, data.to_vec()) })
    }

    /// A Wave Info PDU: the buffer's first four bytes, and how many follow in the
    /// Wave PDU that comes next.
    fn wave_info(&mut self, r: &mut Reader<'_>, body_size: usize) -> Result<Turn, Malformed> {
        let timestamp = r.u16_le()?;
        let format = r.u16_le()?;
        let block = r.u8()?;
        r.bytes(3)?; // bPad
        let head: [u8; 4] = r.bytes(4)?.try_into().expect("four bytes were read");
        let size = match body_size.checked_sub(8) {
            Some(size) if size >= 4 => size,
            _ => return Err(refuse("a Wave Info body size", body_size as u64)),
        };
        self.pending = Some(Pending { timestamp, format, block, head, size });
        Ok(Turn { replies: Vec::new(), output: Output::Nothing })
    }

    /// The Wave PDU after a Wave Info: no header of its own, four bytes of pad where
    /// the samples' first four were carried ahead, then the rest.
    fn wave(&mut self, pending: Pending, pdu: &[u8]) -> Result<Turn, Malformed> {
        let mut r = Reader::new(WHAT, pdu);
        r.bytes(4)?;
        let rest = r.bytes(pending.size - 4)?;
        let mut data = pending.head.to_vec();
        data.extend_from_slice(rest);
        Ok(Turn {
            replies: vec![wave_confirm(pending.timestamp, pending.block)],
            output: self.samples(pending.format, data),
        })
    }

    /// A buffer, if it is in the format this client asked for. Confirmed either way
    /// by the caller: a buffer this client will not play is still one the host must
    /// stop holding.
    fn samples(&self, format: u16, data: Vec<u8>) -> Output {
        if !self.negotiated {
            debug!("rdp: dropping a sound buffer that arrived before any format was agreed");
            return Output::Nothing;
        }
        if format != 0 {
            debug!("rdp: dropping a sound buffer in format {format}, which this client did not ask for");
            return Output::Nothing;
        }
        Output::Wave(data)
    }
}

fn header(w: &mut Writer, kind: u8, body: u16) {
    w.u8(kind);
    w.u8(0); // bPad
    w.u16_le(body);
}

/// Client Audio Formats and Version: the one format taken, or none.
fn client_formats(format: Option<Format>) -> Vec<u8> {
    let count = u16::from(format.is_some());
    let mut w = Writer::with_capacity(4 + 20 + usize::from(FORMAT_BYTES));
    header(&mut w, SNDC_FORMATS, 20 + count * FORMAT_BYTES);
    w.u32_le(ALIVE);
    w.u32_le(0); // dwVolume
    w.u32_le(0); // dwPitch
    w.u16_le(0); // wDGramPort: no UDP, this session's sound rides its one connection
    w.u16_le(count);
    w.u8(0); // cLastBlockConfirmed
    w.u16_le(VERSION);
    w.u8(0); // bPad
    if let Some(format) = format {
        format.write(&mut w);
    }
    w.finish()
}

fn quality_mode() -> Vec<u8> {
    let mut w = Writer::with_capacity(8);
    header(&mut w, SNDC_QUALITYMODE, 4);
    w.u16_le(HIGH_QUALITY);
    w.u16_le(0); // Reserved
    w.finish()
}

/// Training Confirm: the timestamp and the pack size, as the server sent them.
fn training_confirm(timestamp: u16, pack_size: u16) -> Vec<u8> {
    let mut w = Writer::with_capacity(8);
    header(&mut w, SNDC_TRAINING, 4);
    w.u16_le(timestamp);
    w.u16_le(pack_size);
    w.finish()
}

/// Wave Confirm: which block was taken, and when.
fn wave_confirm(timestamp: u16, block: u8) -> Vec<u8> {
    let mut w = Writer::with_capacity(8);
    header(&mut w, SNDC_WAVECONFIRM, 4);
    w.u16_le(timestamp);
    w.u8(block);
    w.u8(0); // bPad
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server PDU: header, then body, with the body size the header claims.
    fn server(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![kind, 0];
        v.extend_from_slice(&(body.len() as u16).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    /// One `AUDIO_FORMAT`, its derived fields consistent, with `extra` trailing bytes.
    fn audio_format(tag: u16, channels: u16, rate: u32, bits: u16, extra: &[u8]) -> Vec<u8> {
        let align = channels * bits / 8;
        let mut v = Vec::new();
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
        v.extend_from_slice(&align.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        v.extend_from_slice(extra);
        v
    }

    /// Server Audio Formats and Version.
    fn server_formats(version: u16, formats: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
        body.extend_from_slice(&0u32.to_le_bytes()); // dwVolume
        body.extend_from_slice(&0u32.to_le_bytes()); // dwPitch
        body.extend_from_slice(&0u16.to_le_bytes()); // wDGramPort
        body.extend_from_slice(&(formats.len() as u16).to_le_bytes());
        body.push(0); // cLastBlockConfirmed
        body.extend_from_slice(&version.to_le_bytes());
        body.push(0); // bPad
        for f in formats {
            body.extend_from_slice(f);
        }
        server(SNDC_FORMATS, &body)
    }

    fn cd() -> Vec<u8> {
        audio_format(WAVE_FORMAT_PCM, 2, 44_100, 16, &[])
    }

    /// A Wave2 PDU carrying `data` in format `format`, block `block`.
    fn wave2(format: u16, block: u8, data: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0x1234u16.to_le_bytes()); // wTimeStamp
        body.extend_from_slice(&format.to_le_bytes());
        body.push(block);
        body.extend_from_slice(&[0; 3]);
        body.extend_from_slice(&0u32.to_le_bytes()); // dwAudioTimeStamp
        body.extend_from_slice(data);
        server(SNDC_WAVE2, &body)
    }

    /// The format list is answered with the one format taken, version 8, and a
    /// quality mode for a host new enough to read one; the others on the list —
    /// here one with trailing bytes to skip — are passed over.
    #[test]
    fn the_one_pcm_format_is_taken_and_the_rest_passed_over() {
        let mut snd = Rdpsnd::new();
        let adpcm = audio_format(0x0002, 2, 22_050, 4, &[2, 0]);
        let turn = snd.push(&server_formats(8, &[adpcm, cd()])).unwrap();
        assert_eq!(turn.output, Output::Negotiated);
        assert_eq!(turn.replies.len(), 2);
        let formats = &turn.replies[0];
        assert_eq!(&formats[..4], &[SNDC_FORMATS, 0, 38, 0]);
        assert_eq!(&formats[4..8], &ALIVE.to_le_bytes());
        assert_eq!(&formats[18..20], &1u16.to_le_bytes(), "one format");
        assert_eq!(&formats[21..23], &VERSION.to_le_bytes());
        assert_eq!(&formats[24..], &cd()[..]);
        assert_eq!(turn.replies[1], vec![SNDC_QUALITYMODE, 0, 4, 0, 2, 0, 0, 0]);
    }

    /// A host offering no such PCM is told this client takes nothing, and a version
    /// 5 host is sent no quality mode.
    #[test]
    fn a_host_without_the_format_is_answered_with_none() {
        let mut snd = Rdpsnd::new();
        let mono = audio_format(WAVE_FORMAT_PCM, 1, 44_100, 16, &[]);
        let turn = snd.push(&server_formats(5, &[mono])).unwrap();
        assert_eq!(turn.output, Output::NoFormat { offered: 1 });
        assert_eq!(turn.replies.len(), 1);
        assert_eq!(&turn.replies[0][..4], &[SNDC_FORMATS, 0, 20, 0]);
        assert_eq!(&turn.replies[0][18..20], &0u16.to_le_bytes(), "no formats");
        // A buffer after that is not one this client agreed to.
        let turn = snd.push(&wave2(0, 1, &[1, 2, 3, 4])).unwrap();
        assert_eq!(turn.output, Output::Nothing);
        assert_eq!(turn.replies.len(), 1, "but it is still confirmed");
    }

    /// Training echoes the numbers the server sent; the padding behind them is not read.
    #[test]
    fn training_is_confirmed_with_the_numbers_the_server_sent() {
        let mut snd = Rdpsnd::new();
        let mut body = vec![0x34, 0x12, 0x00, 0x04];
        body.extend_from_slice(&[0xAA; 1020]);
        let turn = snd.push(&server(SNDC_TRAINING, &body)).unwrap();
        assert_eq!(turn.output, Output::Nothing);
        assert_eq!(turn.replies, vec![vec![SNDC_TRAINING, 0, 4, 0, 0x34, 0x12, 0x00, 0x04]]);
    }

    /// A Wave2 in the agreed format is the buffer and a confirm of its block; one in
    /// another format is confirmed and dropped.
    #[test]
    fn a_wave2_is_the_buffer_and_its_confirm() {
        let mut snd = Rdpsnd::new();
        snd.push(&server_formats(8, &[cd()])).unwrap();
        let turn = snd.push(&wave2(0, 7, &[1, 2, 3, 4, 5, 6, 7, 8])).unwrap();
        assert_eq!(turn.output, Output::Wave(vec![1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(turn.replies, vec![vec![SNDC_WAVECONFIRM, 0, 4, 0, 0x34, 0x12, 7, 0]]);
        let turn = snd.push(&wave2(1, 8, &[9, 9])).unwrap();
        assert_eq!(turn.output, Output::Nothing);
        assert_eq!(turn.replies[0][6], 8);
    }

    /// The older two-PDU form: the Wave Info carries the first four bytes, the Wave
    /// PDU the rest behind a pad, and the buffer comes back whole.
    #[test]
    fn a_wave_info_and_its_wave_pdu_come_back_as_one_buffer() {
        let mut snd = Rdpsnd::new();
        snd.push(&server_formats(8, &[cd()])).unwrap();
        // Body: wTimeStamp, wFormatNo 0, cBlockNo 3, pad, the first four bytes; the
        // body size counts the whole buffer of 8 behind the 8 header bytes.
        let mut info = vec![SNDC_WAVE, 0, 16, 0];
        info.extend_from_slice(&[0x11, 0x22, 0, 0, 3, 0, 0, 0, 1, 2, 3, 4]);
        let turn = snd.push(&info).unwrap();
        assert_eq!(turn, Turn { replies: Vec::new(), output: Output::Nothing });
        let turn = snd.push(&[0xEE, 0xEE, 0xEE, 0xEE, 5, 6, 7, 8]).unwrap();
        assert_eq!(turn.output, Output::Wave(vec![1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(turn.replies, vec![vec![SNDC_WAVECONFIRM, 0, 4, 0, 0x11, 0x22, 3, 0]]);
    }

    /// Close is reported and needs no answer; a PDU cut short is an error rather
    /// than a guess.
    #[test]
    fn a_close_is_reported_and_a_short_pdu_refused() {
        let mut snd = Rdpsnd::new();
        assert_eq!(snd.push(&server(SNDC_CLOSE, &[])).unwrap().output, Output::Closed);
        assert!(snd.push(&[SNDC_WAVE2, 0, 20, 0, 1, 2]).is_err());
        assert!(snd.push(&[SNDC_WAVE2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err(), "a body too small for its fields");
    }
}
