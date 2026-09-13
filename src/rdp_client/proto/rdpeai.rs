//! Audio input redirection (MS-RDPEAI): a microphone on this end, as a recording device on
//! the host.
//!
//! One dynamic channel carries it, [`CHANNEL_NAME`], and the host opens it and speaks
//! first at every step (3.1.5.1). It sends its **version**, and this end answers with its
//! own. It sends the **formats** it records in, and this end answers with the ones it will
//! produce — preceded, as 3.2.5.1.4 requires, by an Incoming Data PDU. When an application
//! over there starts recording the host sends **Open**, naming one of this end's formats
//! by index and how many frames each packet must hold; this end confirms the format with a
//! Format Change PDU and reports success with an Open Reply (3.2.5.1.7, 3.2.5.1.8). From
//! then on every packet of audio is an Incoming Data PDU and a Data PDU, back to back
//! (3.2.5.2). A later Format Change from the host is confirmed the same way. Nothing a
//! host sends out of sequence or malformed is answered, and nothing ends the session:
//! 3.1.5 has both ends ignore it.
//!
//! # One format
//!
//! The audio this end produces is whatever the gateway decodes the browser's microphone
//! into, so any 16-bit linear PCM will do, and exactly one is offered — the host's own
//! choice of index can then only mean it, the bargain [`super::rdpsnd`] strikes in the
//! other direction. Of the PCM formats the host lists, the one chosen is mono if any is,
//! at 16 kHz if the host has it, and otherwise at the lowest rate above 16 kHz or the
//! highest below: a microphone here is speech, so a rate past what speech needs is bytes
//! for nothing. Only rates that are a whole number of samples in 20 ms at 48 kHz are
//! candidates, because that is what lets the caller resample from 48 kHz in exact groups.
//!
//! # Packets
//!
//! A Data PDU holds exactly `FramesPerPacket` frames (2.2.2.3). The caller's PCM arrives
//! in whatever lengths it was decoded in, so it is gathered here and cut into packets of
//! that size; a remainder waits for the next buffer. A Data PDU is usually longer than one
//! dynamic channel PDU, so the session sends it through `dvc::pieces`.
//!
//! [MS-RDPEAI]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeai/d04ffa42-5a0f-4f80-abb1-cc26f71c9452

use log::debug;

use super::wire::{Malformed, Reader, Writer};

const WHAT: &str = "an audio input PDU";

/// The dynamic channel the host opens for the microphone (2.1).
pub const CHANNEL_NAME: &str = "AUDIO_INPUT";

/// The highest version this end speaks: version 2, the one a current Windows host sends.
pub const VERSION: u32 = 2;

const MSG_SNDIN_VERSION: u8 = 0x01;
const MSG_SNDIN_FORMATS: u8 = 0x02;
const MSG_SNDIN_OPEN: u8 = 0x03;
const MSG_SNDIN_OPEN_REPLY: u8 = 0x04;
const MSG_SNDIN_DATA_INCOMING: u8 = 0x05;
const MSG_SNDIN_DATA: u8 = 0x06;
const MSG_SNDIN_FORMATCHANGE: u8 = 0x07;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const BITS_PER_SAMPLE: u16 = 16;
/// An `AUDIO_FORMAT` with no trailing bytes, which PCM never has.
const FORMAT_BYTES: u32 = 18;

/// `S_OK`, and `E_INVALIDARG` for an Open naming a format this end never offered.
const S_OK: u32 = 0;
const E_INVALIDARG: u32 = 0x8007_0057;

/// The rate a microphone carrying speech is best sent at.
const SPEECH_RATE: u32 = 16_000;
/// The rates the caller can produce, which is every rate a 20 ms group at 48 kHz
/// resamples to exactly, within what a recording device is sensibly asked for.
const MIN_RATE: u32 = 8_000;
const MAX_RATE: u32 = 48_000;

/// The 16-bit linear PCM the host records in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub channels: u16,
    pub sample_rate: u32,
}

impl Format {
    /// Bytes in one frame across every channel: `nBlockAlign`.
    pub const fn block_align(self) -> u16 {
        self.channels * (BITS_PER_SAMPLE / 8)
    }

    const fn byte_rate(self) -> u32 {
        self.sample_rate * self.block_align() as u32
    }

    /// Whether the caller can produce this: one or two channels, at a rate an exact 20 ms
    /// group at 48 kHz resamples to.
    fn producible(self) -> bool {
        matches!(self.channels, 1 | 2)
            && (MIN_RATE..=MAX_RATE).contains(&self.sample_rate)
            && (960 * u64::from(self.sample_rate)).is_multiple_of(48_000)
    }

    fn write(self, w: &mut Writer) {
        w.u16_le(WAVE_FORMAT_PCM);
        w.u16_le(self.channels);
        w.u32_le(self.sample_rate);
        w.u32_le(self.byte_rate());
        w.u16_le(self.block_align());
        w.u16_le(BITS_PER_SAMPLE);
        w.u16_le(0); // cbSize
    }

    /// One `AUDIO_FORMAT`: `Some` for 16-bit PCM whose derived fields agree with its
    /// stated ones, `None` for anything else, which is stepped over whole.
    fn read(r: &mut Reader<'_>) -> Result<Option<Self>, Malformed> {
        let tag = r.u16_le()?;
        let channels = r.u16_le()?;
        let sample_rate = r.u32_le()?;
        let byte_rate = r.u32_le()?;
        let block_align = r.u16_le()?;
        let bits = r.u16_le()?;
        let extra = r.u16_le()?;
        r.skip(usize::from(extra))?;
        let format = Self { channels, sample_rate };
        let pcm = tag == WAVE_FORMAT_PCM
            && bits == BITS_PER_SAMPLE
            && channels > 0
            && block_align == format.block_align()
            && byte_rate == format.byte_rate();
        Ok(pcm.then_some(format))
    }
}

/// The one format to offer out of the host's list, as the module docs describe, or `None`
/// if the host lists nothing this end produces.
fn choose(offered: &[Format]) -> Option<Format> {
    let producible: Vec<Format> = offered.iter().copied().filter(|f| f.producible()).collect();
    let mono: Vec<Format> = producible.iter().copied().filter(|f| f.channels == 1).collect();
    let pool = if mono.is_empty() { producible } else { mono };
    let above = pool.iter().filter(|f| f.sample_rate >= SPEECH_RATE).min_by_key(|f| f.sample_rate);
    let below = pool.iter().filter(|f| f.sample_rate < SPEECH_RATE).max_by_key(|f| f.sample_rate);
    above.or(below).copied()
}

/// What a turn amounted to, beyond the messages it put on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// The host agreed a version: microphone redirection is on offer.
    Negotiated { version: u32 },
    /// The host listed its formats and this end offered `format` back.
    Offered(Format),
    /// The host listed `offered` formats and none of them is one this end produces, so it
    /// was offered nothing and will never open.
    NoFormat { offered: u32 },
    /// An application on the host started recording: PCM in `format` is wanted from now on.
    Opened(Format),
    /// The recording ended, with the channel.
    Closed,
}

/// One event acted on: whole messages for the channel, and what they meant.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Turn {
    pub replies: Vec<Vec<u8>>,
    pub outputs: Vec<Output>,
}

/// The client's side of audio input redirection, on its one channel.
#[derive(Debug, Default)]
pub struct Rdpeai {
    /// The channel, while the host holds it open.
    channel: Option<u32>,
    /// The format offered, once the host has listed its own.
    format: Option<Format>,
    /// Frames each Data PDU holds, while the host is recording.
    frames_per_packet: Option<usize>,
    /// PCM gathered towards the next packet.
    pending: Vec<u8>,
}

impl Rdpeai {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether dynamic channel `channel` is this end's.
    pub fn owns(&self, channel: u32) -> bool {
        self.channel == Some(channel)
    }

    /// The channel, while the host holds it open.
    pub fn channel(&self) -> Option<u32> {
        self.channel
    }

    /// The host opened [`CHANNEL_NAME`] as `channel`. It speaks first, so nothing is sent;
    /// a second opening starts the conversation again.
    pub fn opened(&mut self, channel: u32) -> Turn {
        let mut turn = Turn::default();
        debug!("rdp: the host opened audio input on dynamic channel {channel}");
        if self.recording() {
            turn.outputs.push(Output::Closed);
        }
        *self = Self { channel: Some(channel), ..Self::default() };
        turn
    }

    /// The host closed the channel, and the recording with it.
    pub fn closed(&mut self) -> Turn {
        let mut turn = Turn::default();
        if self.recording() {
            turn.outputs.push(Output::Closed);
        }
        *self = Self::default();
        turn
    }

    /// Whether the host is recording, so PCM is wanted.
    pub fn recording(&self) -> bool {
        self.frames_per_packet.is_some()
    }

    /// One whole message from the host. A malformed or out-of-sequence one is ignored
    /// (3.1.5), and said at `debug`.
    pub fn push(&mut self, message: &[u8]) -> Turn {
        let mut turn = Turn::default();
        if let Err(e) = self.on_message(message, &mut turn) {
            debug!("rdp: ignoring {e}");
        }
        turn
    }

    /// PCM in the opened format, cut into Data PDUs as whole packets gather. Nothing is
    /// taken while the host is not recording.
    pub fn sample(&mut self, pcm: &[u8]) -> Turn {
        let mut turn = Turn::default();
        let (Some(frames), Some(format)) = (self.frames_per_packet, self.format) else {
            return turn;
        };
        let packet = frames * usize::from(format.block_align());
        self.pending.extend_from_slice(pcm);
        let mut taken = 0;
        while self.pending.len() - taken >= packet {
            turn.replies.push(vec![MSG_SNDIN_DATA_INCOMING]);
            let mut data = Vec::with_capacity(1 + packet);
            data.push(MSG_SNDIN_DATA);
            data.extend_from_slice(&self.pending[taken..taken + packet]);
            turn.replies.push(data);
            taken += packet;
        }
        self.pending.drain(..taken);
        turn
    }

    fn on_message(&mut self, message: &[u8], turn: &mut Turn) -> Result<(), Malformed> {
        let mut r = Reader::new(WHAT, message);
        match r.u8()? {
            MSG_SNDIN_VERSION => {
                let version = r.u32_le()?;
                if version == 0 {
                    return Err(r.refuse("a version", version));
                }
                let agreed = version.min(VERSION);
                turn.replies.push(version_pdu(agreed));
                turn.outputs.push(Output::Negotiated { version: agreed });
            }
            MSG_SNDIN_FORMATS => {
                let count = r.u32_le()?;
                r.skip(4)?; // cbSizeFormatsPacket, which the client ignores
                let mut offered = Vec::new();
                for _ in 0..count {
                    offered.extend(Format::read(&mut r)?);
                }
                let chosen = choose(&offered);
                self.format = chosen;
                self.frames_per_packet = None;
                self.pending.clear();
                turn.replies.push(vec![MSG_SNDIN_DATA_INCOMING]);
                turn.replies.push(formats_pdu(chosen));
                turn.outputs.push(match chosen {
                    Some(format) => Output::Offered(format),
                    None => Output::NoFormat { offered: count },
                });
            }
            MSG_SNDIN_OPEN => {
                let frames = r.u32_le()?;
                let initial = r.u32_le()?;
                // The format to capture in follows; this end captures in the one it
                // offered, which is what the data must be encoded in either way.
                let Some(format) = self.format.filter(|_| initial == 0) else {
                    turn.replies.push(open_reply_pdu(E_INVALIDARG));
                    return Err(r.refuse("an initial format", initial));
                };
                if frames == 0 {
                    turn.replies.push(open_reply_pdu(E_INVALIDARG));
                    return Err(r.refuse("frames per packet", frames));
                }
                let was_recording = self.recording();
                self.frames_per_packet = Some(frames as usize);
                self.pending.clear();
                turn.replies.push(format_change_pdu(initial));
                turn.replies.push(open_reply_pdu(S_OK));
                if !was_recording {
                    turn.outputs.push(Output::Opened(format));
                }
            }
            MSG_SNDIN_FORMATCHANGE => {
                let index = r.u32_le()?;
                if self.format.is_none() || index != 0 {
                    return Err(r.refuse("a new format", index));
                }
                self.pending.clear();
                turn.replies.push(format_change_pdu(index));
            }
            kind => return Err(r.refuse("a message id", kind)),
        }
        Ok(())
    }
}

fn version_pdu(version: u32) -> Vec<u8> {
    let mut w = Writer::with_capacity(5);
    w.u8(MSG_SNDIN_VERSION);
    w.u32_le(version);
    w.finish()
}

/// This end's Sound Formats PDU: the chosen format, or none. `cbSizeFormatsPacket` is the
/// whole PDU, since this end appends no extra data (2.2.2.2).
fn formats_pdu(format: Option<Format>) -> Vec<u8> {
    let count = u32::from(format.is_some());
    let size = 9 + count * FORMAT_BYTES;
    let mut w = Writer::with_capacity(size as usize);
    w.u8(MSG_SNDIN_FORMATS);
    w.u32_le(count);
    w.u32_le(size);
    if let Some(format) = format {
        format.write(&mut w);
    }
    w.finish()
}

fn format_change_pdu(index: u32) -> Vec<u8> {
    let mut w = Writer::with_capacity(5);
    w.u8(MSG_SNDIN_FORMATCHANGE);
    w.u32_le(index);
    w.finish()
}

fn open_reply_pdu(result: u32) -> Vec<u8> {
    let mut w = Writer::with_capacity(5);
    w.u8(MSG_SNDIN_OPEN_REPLY);
    w.u32_le(result);
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONO_16K: Format = Format { channels: 1, sample_rate: 16_000 };

    fn pcm_format(channels: u16, rate: u32) -> Vec<u8> {
        let mut w = Writer::new();
        Format { channels, sample_rate: rate }.write(&mut w);
        w.finish()
    }

    fn formats_from_host(formats: &[Vec<u8>]) -> Vec<u8> {
        let mut message = vec![MSG_SNDIN_FORMATS];
        message.extend_from_slice(&(formats.len() as u32).to_le_bytes());
        message.extend_from_slice(&0xDEAD_BEEF_u32.to_le_bytes()); // arbitrary from a host
        for format in formats {
            message.extend_from_slice(format);
        }
        message
    }

    fn open(frames: u32, initial: u32) -> Vec<u8> {
        let mut message = vec![MSG_SNDIN_OPEN];
        message.extend_from_slice(&frames.to_le_bytes());
        message.extend_from_slice(&initial.to_le_bytes());
        message.extend_from_slice(&pcm_format(2, 44_100));
        message
    }

    /// A channel negotiated through to recording 16 kHz mono in packets of `frames`.
    fn recording(frames: u32) -> Rdpeai {
        let mut mic = Rdpeai::new();
        mic.opened(7);
        mic.push(&[MSG_SNDIN_VERSION, 2, 0, 0, 0]);
        mic.push(&formats_from_host(&[pcm_format(1, 16_000)]));
        mic.push(&open(frames, 0));
        assert!(mic.recording());
        mic
    }

    /// The Version PDUs of 4.1.1 and 4.1.2: a version 1 host is answered with version 1,
    /// byte for byte, and a later host with the highest this end speaks.
    #[test]
    fn the_version_is_answered_as_the_specification_shows() {
        let mut mic = Rdpeai::new();
        mic.opened(3);
        let turn = mic.push(&[0x01, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(turn.replies, vec![vec![0x01, 0x01, 0x00, 0x00, 0x00]]);
        assert_eq!(turn.outputs, vec![Output::Negotiated { version: 1 }]);
        assert_eq!(mic.push(&[0x01, 0x07, 0, 0, 0]).replies, vec![vec![0x01, 0x02, 0, 0, 0]]);
    }

    /// The host's list of 4.1.3, whose PCM entries are 44.1, 22.05 and 11.025 kHz: an
    /// Incoming Data PDU (4.1.4) goes first, then one format, mono at 22.05 kHz — the
    /// lowest rate above speech the host lists that this end can produce, since 11.025 kHz
    /// is not a whole number of samples in 20 ms at 48 kHz.
    #[test]
    fn the_hosts_format_list_is_answered_with_one_format() {
        let host = formats_from_host(&[
            pcm_format(2, 44_100),
            pcm_format(1, 44_100),
            pcm_format(1, 22_050),
            pcm_format(1, 11_025),
            // ADPCM, with its extra bytes, stepped over.
            [&[0x02, 0x00, 0x01, 0x00, 0x44, 0xAC, 0, 0, 0x47, 0xAD, 0, 0, 0x00, 0x08, 0x04, 0x00, 0x02, 0x00][..], &[0xF4, 0x07]].concat(),
        ]);
        let mut mic = Rdpeai::new();
        mic.opened(3);
        let turn = mic.push(&host);
        let chosen = Format { channels: 1, sample_rate: 22_050 };
        assert_eq!(turn.outputs, vec![Output::Offered(chosen)]);
        assert_eq!(turn.replies[0], vec![0x05]);
        assert_eq!(
            turn.replies[1],
            vec![
                0x02, 0x01, 0x00, 0x00, 0x00, 0x1B, 0x00, 0x00, 0x00, // header, 1 format, 27 bytes
                0x01, 0x00, 0x01, 0x00, 0x22, 0x56, 0x00, 0x00, // PCM, mono, 22050
                0x44, 0xAC, 0x00, 0x00, 0x02, 0x00, 0x10, 0x00, 0x00, 0x00, // 44100 B/s, 2, 16, 0
            ]
        );
    }

    #[test]
    fn speech_rates_mono_first() {
        let f = |channels, sample_rate| Format { channels, sample_rate };
        assert_eq!(choose(&[f(1, 48_000), f(1, 16_000), f(1, 8_000)]), Some(f(1, 16_000)));
        assert_eq!(choose(&[f(2, 16_000), f(1, 44_100)]), Some(f(1, 44_100)), "mono before a better rate");
        assert_eq!(choose(&[f(1, 8_000), f(1, 12_000)]), Some(f(1, 12_000)), "the highest below");
        assert_eq!(choose(&[f(2, 32_000)]), Some(f(2, 32_000)), "stereo when nothing is mono");
        assert_eq!(choose(&[f(1, 11_025), f(3, 16_000), f(1, 96_000)]), None);
    }

    /// A host listing nothing this end produces is told so with an empty list, and never
    /// opens.
    #[test]
    fn no_producible_format_is_an_empty_list() {
        let mut mic = Rdpeai::new();
        mic.opened(3);
        let turn = mic.push(&formats_from_host(&[pcm_format(1, 11_025)]));
        assert_eq!(turn.outputs, vec![Output::NoFormat { offered: 1 }]);
        assert_eq!(turn.replies[1], vec![0x02, 0, 0, 0, 0, 0x09, 0, 0, 0]);
        let turn = mic.push(&open(160, 0));
        assert_eq!(turn.replies, vec![open_reply_pdu(E_INVALIDARG)]);
        assert!(!mic.recording());
    }

    /// The Open of 4.1.6 is confirmed with a Format Change for its initial format (4.1.7)
    /// and then an `S_OK` Open Reply (4.1.8), in that order.
    #[test]
    fn an_open_is_confirmed_then_answered() {
        let mut mic = Rdpeai::new();
        mic.opened(3);
        mic.push(&formats_from_host(&[pcm_format(1, 16_000)]));
        let turn = mic.push(&open(320, 0));
        assert_eq!(turn.replies, vec![vec![0x07, 0, 0, 0, 0], vec![0x04, 0x00, 0x00, 0x00, 0x00]]);
        assert_eq!(turn.outputs, vec![Output::Opened(MONO_16K)]);
    }

    /// An Open before any format list names nothing, and is refused.
    #[test]
    fn an_open_out_of_sequence_is_refused() {
        let mut mic = Rdpeai::new();
        mic.opened(3);
        let turn = mic.push(&open(320, 0));
        assert_eq!(turn.replies, vec![open_reply_pdu(E_INVALIDARG)]);
        assert!(turn.outputs.is_empty());
    }

    /// PCM is cut into packets of exactly FramesPerPacket frames, each an Incoming Data PDU
    /// then a Data PDU (4.2.1, 4.2.2), with a remainder carried to the next buffer.
    #[test]
    fn pcm_goes_out_in_whole_packets() {
        let mut mic = recording(4);
        let turn = mic.sample(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(turn.replies, vec![vec![0x05], vec![0x06, 1, 2, 3, 4, 5, 6, 7, 8]]);
        let turn = mic.sample(&[11, 12, 13, 14, 15, 16]);
        assert_eq!(turn.replies, vec![vec![0x05], vec![0x06, 9, 10, 11, 12, 13, 14, 15, 16]]);
        assert!(mic.sample(&[]).replies.is_empty());
    }

    #[test]
    fn nothing_is_sent_before_the_host_records() {
        let mut mic = Rdpeai::new();
        mic.opened(3);
        mic.push(&formats_from_host(&[pcm_format(1, 16_000)]));
        assert!(mic.sample(&[0; 640]).replies.is_empty());
    }

    /// A Format Change from the host is confirmed with the same index (4.3), and a partial
    /// packet in the old format goes with it.
    #[test]
    fn a_format_change_is_confirmed() {
        let mut mic = recording(4);
        mic.sample(&[1, 2]);
        let turn = mic.push(&[0x07, 0, 0, 0, 0]);
        assert_eq!(turn.replies, vec![vec![0x07, 0, 0, 0, 0]]);
        assert!(mic.sample(&[3, 4, 5, 6, 7, 8]).replies.is_empty(), "the old half-packet was dropped");
        assert!(mic.push(&[0x07, 1, 0, 0, 0]).replies.is_empty(), "an index never offered");
    }

    #[test]
    fn closing_ends_the_recording() {
        let mut mic = recording(4);
        assert!(mic.owns(7));
        assert_eq!(mic.closed().outputs, vec![Output::Closed]);
        assert!(!mic.owns(7));
        assert!(mic.sample(&[0; 16]).replies.is_empty());
        assert!(mic.closed().outputs.is_empty());
    }

    #[test]
    fn malformed_and_unknown_messages_are_ignored() {
        let mut mic = recording(4);
        assert_eq!(mic.push(&[]), Turn::default());
        assert_eq!(mic.push(&[0x03, 1, 0]), Turn::default());
        assert_eq!(mic.push(&[0x42]), Turn::default());
        assert_eq!(mic.push(&[0x01, 0, 0, 0, 0]), Turn::default(), "version zero");
        assert!(mic.recording());
    }
}
