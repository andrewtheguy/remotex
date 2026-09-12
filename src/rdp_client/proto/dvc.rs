//! The dynamic virtual channels, which all ride on one static one.
//!
//! A static virtual channel is asked for by name before the connection sequence
//! starts and numbered by the server ([`super::gcc`]); a dynamic one is opened while
//! the session is live, by the server, over the static channel named `drdynvc`. That
//! indirection buys a client one thing it wants — Display Control, the channel a
//! desktop is resized over — and a handful of PDUs to get there:
//!
//! 1. The server sends a **Capabilities Request** naming a version. The answer is the
//!    same version back: there is nothing to negotiate, only to agree to.
//! 2. The server sends a **Create Request** for each channel it wants open, by name.
//!    A client answers **Create Response** with a status — accepted, or [`NO_LISTENER`]
//!    for a channel it has no use for. Refusing is normal: a Windows host offers
//!    several, and this client takes one.
//! 3. **Data** travels in both directions, addressed by the number the Create Request
//!    gave the channel.
//! 4. **Close** ends a channel from either side.
//!
//! # Two fields whose width is in the header
//!
//! The first byte of every PDU packs the command into its top four bits and the width
//! of the two fields that may follow into the rest: a channel number is one, two or
//! four bytes depending on two bits, and a Data First's length the same. It saves
//! three bytes on a PDU that is sent a few times a session, and it is the only place
//! in RDP where a field's width is written rather than fixed — so it is read and
//! written here in one place, [`read_field`] and [`write_field`], rather than at each
//! call site.
//!
//! [MS-RDPEDYC] 2.2.

use super::wire::{Malformed, Reader, Writer};

/// The most data one dynamic channel PDU may carry, so that it and its header fit
/// inside the smallest chunk a static channel can be given.
pub const MAX_DATA: usize = 1590;

/// `DYNVC_CREATE_RSP` accepted the channel.
pub const ACCEPTED: u32 = 0x0000_0000;
/// `DYNVC_CREATE_RSP` refused it: nothing above this module wanted that name.
pub const NO_LISTENER: u32 = 0xC000_0001;

/// Commands, in the top four bits of the first byte.
const CREATE: u8 = 0x01;
const DATA_FIRST: u8 = 0x02;
const DATA: u8 = 0x03;
const CLOSE: u8 = 0x04;
const CAPABILITIES: u8 = 0x05;

/// The field widths, in two bits each: one byte, two, or four.
const BYTE: u8 = 0x00;
const SHORT: u8 = 0x01;
const LONG: u8 = 0x02;

const WHAT: &str = "a dynamic virtual channel PDU";

/// What a server said on the dynamic channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message<'a> {
    /// The version to agree to, which [`capabilities_response`] hands straight back.
    Capabilities { version: u16 },
    /// A channel the server wants open, and the name it goes by.
    Create { channel: u32, name: &'a str },
    /// A channel the server has finished with.
    Close { channel: u32 },
    /// A whole PDU for one channel, with the pieces of a split one already joined.
    Data { channel: u32, data: &'a [u8] },
}

/// A payload longer than one dynamic channel PDU.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a {0}-byte dynamic channel payload does not fit one PDU")]
pub struct TooLong(pub usize);

/// The server's side of the dynamic channel, one PDU at a time.
///
/// Stateful for one reason: a Data First says how long the whole payload is and
/// carries the start of it, and the Data PDUs after it carry the rest. Everything a
/// Display Control session receives arrives whole, so the buffer stays empty — but a
/// split payload is the server's decision, not this client's.
#[derive(Debug, Default)]
pub struct Incoming {
    /// The channel a split payload is being gathered for, and how long it will be.
    gathering: Option<(u32, usize)>,
    buffer: Vec<u8>,
}

impl Incoming {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read one PDU, and hand back the message if this PDU completed one.
    pub fn push<'a>(&'a mut self, bytes: &'a [u8]) -> Result<Option<Message<'a>>, Malformed> {
        let mut r = Reader::new(WHAT, bytes);
        let header = r.u8()?;
        let (command, sp, cb_id) = (header >> 4, (header >> 2) & 0x03, header & 0x03);

        match command {
            CAPABILITIES => {
                r.u8()?; // Pad
                Ok(Some(Message::Capabilities { version: r.u16_le()? }))
            }
            CREATE => {
                let channel = read_field(&mut r, cb_id)?;
                let name = r.rest();
                let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
                let length = u32::try_from(name.len()).unwrap_or(u32::MAX);
                let name = std::str::from_utf8(name)
                    .map_err(|_| r.refuse("a channel name that is not text, of", length))?;
                Ok(Some(Message::Create { channel, name }))
            }
            CLOSE => Ok(Some(Message::Close { channel: read_field(&mut r, cb_id)? })),
            DATA_FIRST => {
                let channel = read_field(&mut r, cb_id)?;
                let announced = read_field(&mut r, sp)?;
                let total = usize::try_from(announced).unwrap_or(usize::MAX);
                let data = r.rest();
                if data.len() >= total {
                    return Err(self.abandon("a first piece already as long as its", announced));
                }
                self.gathering = Some((channel, total));
                self.buffer.clear();
                self.buffer.extend_from_slice(data);
                Ok(None)
            }
            DATA => {
                let channel = read_field(&mut r, cb_id)?;
                let data = r.rest();
                let Some((gathering, total)) = self.gathering else {
                    return Ok(Some(Message::Data { channel, data }));
                };
                if channel != gathering {
                    return Err(self.abandon("a piece for channel", channel));
                }
                self.buffer.extend_from_slice(data);
                if self.buffer.len() < total {
                    return Ok(None);
                }
                if self.buffer.len() > total {
                    let gathered = u32::try_from(self.buffer.len()).unwrap_or(u32::MAX);
                    return Err(self.abandon("pieces coming to", gathered));
                }
                self.gathering = None;
                Ok(Some(Message::Data { channel, data: &self.buffer }))
            }
            other => Err(r.refuse("a command", other)),
        }
    }

    /// Forget what was being gathered, and say why it was given up on.
    fn abandon(&mut self, field: &'static str, value: impl Into<u64>) -> Malformed {
        self.gathering = None;
        self.buffer.clear();
        Malformed::Refused { what: WHAT, field, value: value.into() }
    }
}

/// Agree to the version the server asked for.
pub fn capabilities_response(version: u16) -> Vec<u8> {
    let mut w = Writer::with_capacity(4);
    w.u8(header(CAPABILITIES, BYTE, BYTE));
    w.u8(0); // Pad
    w.u16_le(version);
    w.finish()
}

/// Accept or refuse a channel the server asked to open. `status` is [`ACCEPTED`] or
/// [`NO_LISTENER`].
pub fn create_response(channel: u32, status: u32) -> Vec<u8> {
    let width = field_width(channel);
    let mut w = Writer::with_capacity(9);
    w.u8(header(CREATE, BYTE, width));
    write_field(&mut w, width, channel);
    w.u32_le(status);
    w.finish()
}

/// Close a channel, which a client does to answer a server's Close or to give one up.
pub fn close(channel: u32) -> Vec<u8> {
    let width = field_width(channel);
    let mut w = Writer::with_capacity(5);
    w.u8(header(CLOSE, BYTE, width));
    write_field(&mut w, width, channel);
    w.finish()
}

/// A payload for one channel. Refused rather than split if it does not fit one PDU:
/// the only thing this client sends on a dynamic channel is a monitor layout, which
/// is sixty-four bytes.
pub fn data(channel: u32, payload: &[u8]) -> Result<Vec<u8>, TooLong> {
    if payload.len() > MAX_DATA {
        return Err(TooLong(payload.len()));
    }
    let width = field_width(channel);
    let mut w = Writer::with_capacity(5 + payload.len());
    w.u8(header(DATA, BYTE, width));
    write_field(&mut w, width, channel);
    w.bytes(payload);
    Ok(w.finish())
}

/// The first byte: the command, and the width of the two fields that may follow it.
fn header(command: u8, sp: u8, cb_id: u8) -> u8 {
    (command << 4) | (sp << 2) | cb_id
}

/// How wide to write a value: the narrowest of the three the field allows.
fn field_width(value: u32) -> u8 {
    match value {
        0..=0xFF => BYTE,
        0x100..=0xFFFF => SHORT,
        _ => LONG,
    }
}

fn write_field(w: &mut Writer, width: u8, value: u32) {
    match width {
        BYTE => w.u8(u8::try_from(value).expect("a width chosen for this value")),
        SHORT => w.u16_le(u16::try_from(value).expect("a width chosen for this value")),
        _ => w.u32_le(value),
    }
}

fn read_field(r: &mut Reader<'_>, width: u8) -> Result<u32, Malformed> {
    match width {
        BYTE => Ok(u32::from(r.u8()?)),
        SHORT => Ok(u32::from(r.u16_le()?)),
        LONG => r.u32_le(),
        other => Err(r.refuse("a field width", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server PDU, built the way a server builds one.
    fn server(command: u8, sp: u8, cb_id: u8, rest: &[u8]) -> Vec<u8> {
        let mut bytes = vec![header(command, sp, cb_id)];
        bytes.extend_from_slice(rest);
        bytes
    }

    #[test]
    fn the_version_a_server_asks_for_is_the_one_that_goes_back() {
        let mut incoming = Incoming::new();
        let request = server(CAPABILITIES, BYTE, BYTE, &[0x00, 0x02, 0x00, 0, 0, 0, 0, 0, 0, 0, 0]);
        let Some(Message::Capabilities { version }) = incoming.push(&request).unwrap() else {
            panic!("a capabilities request");
        };
        assert_eq!(version, 2);
        assert_eq!(capabilities_response(version), vec![0x50, 0x00, 0x02, 0x00]);
    }

    #[test]
    fn a_create_request_carries_the_name_the_answer_is_decided_by() {
        let mut incoming = Incoming::new();
        let request = server(CREATE, BYTE, BYTE, b"\x03Microsoft::Windows::RDS::DisplayControl\0");
        assert_eq!(
            incoming.push(&request).unwrap(),
            Some(Message::Create {
                channel: 3,
                name: "Microsoft::Windows::RDS::DisplayControl"
            })
        );

        assert_eq!(create_response(3, ACCEPTED), vec![0x10, 0x03, 0, 0, 0, 0]);
        assert_eq!(create_response(3, NO_LISTENER), vec![0x10, 0x03, 0x01, 0x00, 0x00, 0xC0]);
    }

    /// The width bits are the one place in RDP where a field's size is written rather
    /// than fixed, in both directions.
    #[test]
    fn a_channel_number_is_written_as_narrowly_as_it_fits_and_read_as_wide_as_it_says() {
        assert_eq!(close(0x02), vec![0x40, 0x02]);
        assert_eq!(close(0x0102), vec![0x41, 0x02, 0x01]);
        assert_eq!(close(0x0001_0002), vec![0x42, 0x02, 0x00, 0x01, 0x00]);

        let mut incoming = Incoming::new();
        for (width, rest, expected) in [
            (BYTE, &[0x02][..], 0x02),
            (SHORT, &[0x02, 0x01][..], 0x0102),
            (LONG, &[0x02, 0x00, 0x01, 0x00][..], 0x0001_0002),
        ] {
            let pdu = server(CLOSE, BYTE, width, rest);
            assert_eq!(incoming.push(&pdu).unwrap(), Some(Message::Close { channel: expected }));
        }
    }

    #[test]
    fn a_whole_data_pdu_arrives_borrowed_from_the_bytes_it_came_in() {
        let mut incoming = Incoming::new();
        let pdu = server(DATA, BYTE, BYTE, &[0x03, 1, 2, 3]);
        assert_eq!(
            incoming.push(&pdu).unwrap(),
            Some(Message::Data { channel: 3, data: &[1, 2, 3] })
        );
        assert_eq!(data(3, &[1, 2, 3]).unwrap(), pdu);
    }

    #[test]
    fn the_pieces_of_a_split_payload_are_gathered_until_the_length_the_first_gave() {
        let mut incoming = Incoming::new();
        // A Data First whose length field is two bytes wide, saying six follow.
        let first = server(DATA_FIRST, SHORT, BYTE, &[0x03, 0x06, 0x00, 1, 2]);
        assert_eq!(incoming.push(&first).unwrap(), None);
        assert_eq!(incoming.push(&server(DATA, BYTE, BYTE, &[0x03, 3, 4])).unwrap(), None);
        assert_eq!(
            incoming.push(&server(DATA, BYTE, BYTE, &[0x03, 5, 6])).unwrap(),
            Some(Message::Data { channel: 3, data: &[1, 2, 3, 4, 5, 6] })
        );

        // And the next whole PDU is not appended to the one that finished.
        assert_eq!(
            incoming.push(&server(DATA, BYTE, BYTE, &[0x03, 7])).unwrap(),
            Some(Message::Data { channel: 3, data: &[7] })
        );
    }

    #[test]
    fn a_piece_for_another_channel_is_refused_and_the_sequence_given_up_on() {
        let mut incoming = Incoming::new();
        let first = server(DATA_FIRST, BYTE, BYTE, &[0x03, 0x06, 1, 2]);
        assert_eq!(incoming.push(&first).unwrap(), None);
        let err = incoming.push(&server(DATA, BYTE, BYTE, &[0x04, 3])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "a dynamic virtual channel PDU carries a piece for channel 0x4, which this client \
             does not accept"
        );

        // The sequence is gone, so the next PDU for the channel stands on its own.
        assert_eq!(
            incoming.push(&server(DATA, BYTE, BYTE, &[0x03, 9])).unwrap(),
            Some(Message::Data { channel: 3, data: &[9] })
        );
    }

    #[test]
    fn a_command_this_client_has_no_answer_for_is_refused_by_number() {
        let mut incoming = Incoming::new();
        // Soft-Sync, which belongs to a multitransport tunnel this client never asks
        // for, and which a server therefore never has cause to send.
        let err = incoming.push(&server(0x08, BYTE, BYTE, &[0x00])).unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "a command", value: 8, .. }));
    }

    #[test]
    fn a_payload_too_long_for_one_pdu_is_refused_rather_than_split() {
        assert_eq!(data(3, &[0; MAX_DATA + 1]).unwrap_err().to_string(),
            "a 1591-byte dynamic channel payload does not fit one PDU");
        assert!(data(3, &[0; MAX_DATA]).is_ok());
    }
}
