//! MS-RDPECLIP: the clipboard, on a static virtual channel of its own.
//!
//! One channel, [`CHANNEL_NAME`], carried by [`super::channel`] rather than by the
//! dynamic transport [`super::display`] rides — the clipboard predates that
//! transport, and a Windows host opens it as the client asked for it in `CS_NET`.
//!
//! # The negotiation
//!
//! The server speaks first, and the client's whole side of the sequence follows from
//! what it says:
//!
//! ```text
//!   server  ──  Monitor Ready  ──▶
//!           ◀──  Clipboard Capabilities, Format List  ──
//!           ──  Format List Response  ──▶
//! ```
//!
//! After that either end may announce a copy with a **Format List** — a list of
//! format ids and nothing else — and either end may ask for the bytes of one with a
//! **Format Data Request**, answered by a **Format Data Response**. Both directions
//! are lazy: a copy costs an announcement, and the bytes cost a second round trip
//! that only happens if somebody pastes.
//!
//! # What this module decides, and what it does not
//!
//! Nothing here knows what a clipboard *holds*. A format is a number and its
//! contents are bytes, and choosing a format, converting text, and deciding what to
//! advertise all belong above — [`crate::rdp_clipboard`] is where they live. This is
//! the wire: six PDUs, in both directions.
//!
//! Two of the negotiation's dials are turned off here, because turning them on would
//! be promising something:
//!
//! - **Short format names.** `CB_USE_LONG_FORMAT_NAMES` is not asked for, so both
//!   ends use the fixed thirty-six byte entry — an id and a name field this client
//!   never reads. MS-RDPECLIP 3.1.5.2 makes long names the form only when *both*
//!   ends offer them, so not offering settles the format of every list in either
//!   direction.
//! - **No file transfer and no locking.** `CB_STREAM_FILECLIP_ENABLED` and
//!   `CB_CAN_LOCK_CLIPDATA` stay clear, so a server has no reason to send the file
//!   contents and clipboard-lock PDUs; one that sends them anyway is ignored by name.
//!
//! [MS-RDPECLIP] 2.2.

use super::wire::{Malformed, Reader, Writer};

/// The name the channel is asked for under in `CS_NET`, and the name a server
/// answers on.
pub const CHANNEL_NAME: &str = "cliprdr";

/// `CLIPRDR_HEADER.msgType`, of the six this client speaks.
const MONITOR_READY: u16 = 0x0001;
const FORMAT_LIST: u16 = 0x0002;
const FORMAT_LIST_RESPONSE: u16 = 0x0003;
const FORMAT_DATA_REQUEST: u16 = 0x0004;
const FORMAT_DATA_RESPONSE: u16 = 0x0005;
const CLIP_CAPS: u16 = 0x0007;

/// `CLIPRDR_HEADER.msgFlags`. A response carries exactly one of the two, and this
/// client reads the absence of `CB_RESPONSE_OK` as the failure it is rather than
/// taking the body of a PDU nobody vouched for.
const RESPONSE_OK: u16 = 0x0001;

/// The type, the flags and the length every PDU on this channel starts with.
const HEADER: usize = 8;

/// `CB_CAPSTYPE_GENERAL`, and the twelve bytes one of them takes.
const CAPSTYPE_GENERAL: u16 = 0x0001;
const GENERAL_CAPABILITY: u16 = 12;

/// `CB_CAPS_VERSION_2`, which is what every Windows host since Vista announces.
const CAPS_VERSION_2: u32 = 0x0000_0002;

/// One `CLIPRDR_SHORT_FORMAT_NAME`: the format id, and the thirty-two bytes of name
/// that follow it.
const SHORT_NAME: usize = 4 + 32;

const WHAT: &str = "a clipboard PDU";

/// One thing the remote clipboard said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message<'a> {
    /// What the server's clipboard can do, which it says before the Monitor Ready.
    ///
    /// Nothing branches on it: this client speaks the short-name form of one text
    /// format whatever a server offers, and offering less is what settles the form
    /// (see the module doc). It is read for the record and for the log, where a
    /// server that turns out to want something else is a line rather than a mystery.
    Capabilities { version: u32, flags: u32 },
    /// The server's clipboard is live. The client answers with [`capabilities`] and
    /// a [`format_list`] — even an empty one, which is what tells the server there
    /// is a clipboard on this end at all.
    MonitorReady,
    /// The remote copied something, and these are the formats it can produce it in.
    Formats(&'a [u8]),
    /// What the server made of a [`format_list`] this client sent.
    ListResponse { ok: bool },
    /// The remote is pasting, and **is waiting**: every one of these has to be
    /// answered with a [`data_response`], including with nothing. A request left
    /// unanswered is a remote application stopped inside its own paste handler.
    DataRequest { format: u32 },
    /// The bytes of a format this client asked for, or `None` for the
    /// `CB_RESPONSE_FAIL` a peer sends when it could not produce them — which it
    /// does not say why about.
    Data(Option<&'a [u8]>),
    /// A PDU this client has nothing to do with: the file-transfer and
    /// clipboard-locking pairs it did not ask for, and a temporary directory it has
    /// no files to put in.
    Ignored(u16),
}

/// The format ids out of a [`Message::Formats`].
///
/// Short names are what the capability exchange settled on, so each entry is
/// [`SHORT_NAME`] bytes and only its first four are read. A list that is not a whole
/// number of them is read as long names instead — a server that ignored the
/// negotiation still copied something, and the ids are in the same place at the front
/// of each entry either way.
pub fn formats(list: &[u8]) -> Result<Vec<u32>, Malformed> {
    let mut r = Reader::new(WHAT, list);
    let mut formats = Vec::new();
    if list.len().is_multiple_of(SHORT_NAME) {
        while !r.is_empty() {
            formats.push(r.u32_le()?);
            r.skip(SHORT_NAME - 4)?;
        }
        return Ok(formats);
    }
    while !r.is_empty() {
        formats.push(r.u32_le()?);
        // A long name is UTF-16 and terminated, so the terminator is the pair of zero
        // bytes — not one byte, which would leave the next id misaligned.
        loop {
            if r.u16_le()? == 0 {
                break;
            }
        }
    }
    Ok(formats)
}

/// Read one whole PDU off the channel.
pub fn decode(pdu: &[u8]) -> Result<Message<'_>, Malformed> {
    let mut r = Reader::new(WHAT, pdu);
    let kind = r.u16_le()?;
    let flags = r.u16_le()?;
    let length = usize::try_from(r.u32_le()?).unwrap_or(usize::MAX);
    // The announced length is the body's, and it has to be there. Anything past it
    // is padding a peer added, and is not part of what it said.
    let body = r.rest().get(..length).ok_or(Malformed::Short {
        what: WHAT,
        len: pdu.len(),
        at: HEADER,
        need: length,
    })?;
    let ok = flags & RESPONSE_OK != 0;
    Ok(match kind {
        CLIP_CAPS => capabilities_of(body)?,
        MONITOR_READY => Message::MonitorReady,
        FORMAT_LIST => Message::Formats(body),
        FORMAT_LIST_RESPONSE => Message::ListResponse { ok },
        FORMAT_DATA_REQUEST => {
            let mut r = Reader::new(WHAT, body);
            Message::DataRequest { format: r.u32_le()? }
        }
        FORMAT_DATA_RESPONSE => Message::Data(ok.then_some(body)),
        other => Message::Ignored(other),
    })
}

/// The general capability set out of the server's capabilities PDU, or a version and
/// flags of zero for a server that sent none.
///
/// The sets are a list, and only one of them has ever existed; the rest are stepped
/// over by the length each announces rather than assumed away, so a server that adds
/// one is read rather than refused.
fn capabilities_of(body: &[u8]) -> Result<Message<'static>, Malformed> {
    let mut r = Reader::new(WHAT, body);
    let count = r.u16_le()?;
    r.skip(2)?; // pad
    let mut general = (0, 0);
    for _ in 0..count {
        let kind = r.u16_le()?;
        let length = r.u16_le()?;
        // The length counts the two fields just read, so a set that claims less than
        // its own header would put the reader back where it started, forever.
        let Some(rest) = usize::from(length).checked_sub(4) else {
            return Err(r.refuse("a capability set length", length));
        };
        let set = r.bytes(rest)?;
        if kind == CAPSTYPE_GENERAL {
            let mut r = Reader::new(WHAT, set);
            general = (r.u32_le()?, r.u32_le()?);
        }
    }
    Ok(Message::Capabilities { version: general.0, flags: general.1 })
}

/// This client's capabilities: one general set, with nothing in it turned on.
///
/// Sent as soon as the server says its clipboard is ready, and before the format
/// list — the flags in here are what decide the shape of every list that follows, so
/// a list sent first would be one neither end had agreed the form of.
pub fn capabilities() -> Vec<u8> {
    let body = 4 + usize::from(GENERAL_CAPABILITY);
    let mut w = Writer::with_capacity(HEADER + body);
    header(&mut w, CLIP_CAPS, 0, body);
    w.u16_le(1); // one capability set
    w.zeros(2); // pad
    w.u16_le(CAPSTYPE_GENERAL);
    w.u16_le(GENERAL_CAPABILITY);
    w.u32_le(CAPS_VERSION_2);
    // Short format names, no file transfer, no clipboard locking — see the module
    // doc. Every one of those is a PDU pair this client would then owe an answer.
    w.u32_le(0);
    w.finish()
}

/// What this end's clipboard now holds, as format ids.
///
/// An empty list is a real answer, and the one to send before a browser has copied
/// anything: it says the clipboard is empty, where sending nothing at all would say
/// there is no client here.
pub fn format_list(formats: &[u32]) -> Vec<u8> {
    let body = formats.len() * SHORT_NAME;
    let mut w = Writer::with_capacity(HEADER + body);
    header(&mut w, FORMAT_LIST, 0, body);
    for &format in formats {
        w.u32_le(format);
        // The name, which for every format this gateway carries is the empty string:
        // a short name field only names the *private* formats, and CF_UNICODETEXT is
        // one of Windows' own.
        w.zeros(SHORT_NAME - 4);
    }
    w.finish()
}

/// The answer a server's format list is owed. Always `CB_RESPONSE_OK`: what arrived
/// was a list of numbers, and there is nothing in one to fail on.
pub fn format_list_response() -> Vec<u8> {
    let mut w = Writer::with_capacity(HEADER);
    header(&mut w, FORMAT_LIST_RESPONSE, RESPONSE_OK, 0);
    w.finish()
}

/// Ask the remote for the bytes of a format it advertised.
pub fn data_request(format: u32) -> Vec<u8> {
    let mut w = Writer::with_capacity(HEADER + 4);
    header(&mut w, FORMAT_DATA_REQUEST, 0, 4);
    w.u32_le(format);
    w.finish()
}

/// Answer a paste: the bytes, or `None` for the failure that carries none.
///
/// `None` is the answer to a format this end cannot produce, and it is an answer —
/// the remote is blocked in its paste until one arrives.
pub fn data_response(data: Option<&[u8]>) -> Vec<u8> {
    let data = data.unwrap_or_default();
    let flags = match data.is_empty() {
        // Empty *and* successful is a legal response, but this client only ever
        // produces bytes for text it holds, so an empty one is the refusal.
        true => 0,
        false => RESPONSE_OK,
    };
    let mut w = Writer::with_capacity(HEADER + data.len());
    header(&mut w, FORMAT_DATA_RESPONSE, flags, data.len());
    w.bytes(data);
    w.finish()
}

/// The eight bytes every PDU on this channel starts with.
fn header(w: &mut Writer, kind: u16, flags: u16, length: usize) {
    w.u16_le(kind);
    w.u16_le(flags);
    w.u32_le(u32::try_from(length).expect("a clipboard PDU this client built itself"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CF_UNICODETEXT`, the one format the gateway above this carries.
    const TEXT: u32 = 13;

    /// A PDU as a server writes one.
    fn pdu(kind: u16, flags: u16, body: &[u8]) -> Vec<u8> {
        let mut w = Writer::new();
        header(&mut w, kind, flags, body.len());
        w.bytes(body);
        w.finish()
    }

    /// The server says what it can do before it says its clipboard is ready. What it
    /// offers changes nothing here, and is read so that it can be said out loud.
    #[test]
    fn the_servers_capabilities_are_read_for_the_record() {
        // Byte for byte what a Windows 11 host sends: version 2, and the five flags
        // that come with file transfer and clipboard locking.
        let windows = pdu(CLIP_CAPS, 0, &[
            0x01, 0x00, 0x00, 0x00, // one set, pad
            0x01, 0x00, 0x0C, 0x00, // CB_CAPSTYPE_GENERAL, 12 bytes
            0x02, 0x00, 0x00, 0x00, // CB_CAPS_VERSION_2
            0x3E, 0x00, 0x00, 0x00, // long names, file clip, locking, huge files
        ]);
        assert_eq!(decode(&windows).unwrap(), Message::Capabilities { version: 2, flags: 0x3E });

        // This client's own, through the same decoder: one set and no flags at all.
        assert_eq!(
            decode(&capabilities()).unwrap(),
            Message::Capabilities { version: CAPS_VERSION_2, flags: 0 }
        );

        // A server with nothing to say, and one whose set this client does not know:
        // both are a clipboard that works, so neither is an error.
        let empty = pdu(CLIP_CAPS, 0, &[0, 0, 0, 0]);
        assert_eq!(decode(&empty).unwrap(), Message::Capabilities { version: 0, flags: 0 });
        let unknown = [0x01, 0x00, 0x00, 0x00, 0x09, 0x00, 0x06, 0x00, 0xAB, 0xCD];
        let other = pdu(CLIP_CAPS, 0, &unknown);
        assert_eq!(decode(&other).unwrap(), Message::Capabilities { version: 0, flags: 0 });
    }

    /// A capability set whose length does not even cover its own header would leave
    /// the reader where it started — once per set, forever.
    #[test]
    fn a_capability_set_shorter_than_its_own_header_is_refused() {
        let lying = pdu(CLIP_CAPS, 0, &[0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00]);
        assert!(matches!(decode(&lying).unwrap_err(), Malformed::Refused { .. }));
    }

    #[test]
    fn the_servers_opening_pdu_is_read_as_the_one_that_needs_answering() {
        let ready = pdu(MONITOR_READY, 0, &[]);
        assert_eq!(decode(&ready).unwrap(), Message::MonitorReady);
    }

    /// The capability exchange is what settles the shape of every format list after
    /// it, so these bytes are worth pinning: one general set, version 2, no flags.
    #[test]
    fn the_capabilities_ask_for_short_names_and_nothing_else() {
        assert_eq!(
            capabilities(),
            vec![
                0x07, 0x00, // CB_CLIP_CAPS
                0x00, 0x00, // no flags
                0x10, 0x00, 0x00, 0x00, // 16 bytes of body
                0x01, 0x00, // one capability set
                0x00, 0x00, // pad
                0x01, 0x00, // CB_CAPSTYPE_GENERAL
                0x0C, 0x00, // 12 bytes
                0x02, 0x00, 0x00, 0x00, // CB_CAPS_VERSION_2
                0x00, 0x00, 0x00, 0x00, // no long names, no files, no locking
            ]
        );
    }

    #[test]
    fn a_format_list_is_one_short_name_entry_per_format() {
        let list = format_list(&[TEXT]);
        assert_eq!(list.len(), HEADER + SHORT_NAME);
        assert_eq!(&list[..8], &[0x02, 0x00, 0x00, 0x00, 0x24, 0x00, 0x00, 0x00]);
        assert_eq!(&list[8..12], &[0x0D, 0x00, 0x00, 0x00]);
        assert!(list[12..].iter().all(|&byte| byte == 0), "the name field is empty");

        // And it reads back as what it said, through the same decoder a server's
        // list goes through.
        let Message::Formats(body) = decode(&list).unwrap() else { panic!("not a format list") };
        assert_eq!(formats(body).unwrap(), vec![TEXT]);
    }

    /// Advertising nothing is what a client with an empty clipboard says, and it is
    /// not the same as saying nothing.
    #[test]
    fn an_empty_clipboard_is_advertised_as_an_empty_list() {
        let list = format_list(&[]);
        assert_eq!(list, vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        let Message::Formats(body) = decode(&list).unwrap() else {
            panic!("not a format list")
        };
        assert!(formats(body).unwrap().is_empty());
    }

    /// What a Windows host sends after a copy in Notepad: several formats, in the
    /// short form the capability exchange settled on.
    #[test]
    fn a_servers_short_name_list_gives_up_its_ids() {
        let mut body = Vec::new();
        for format in [TEXT, 1, 16, 7] {
            body.extend_from_slice(&format.to_le_bytes());
            body.extend_from_slice(&[0; 32]);
        }
        let announced = pdu(FORMAT_LIST, 0, &body);
        let Message::Formats(list) = decode(&announced).unwrap() else {
            panic!("not a format list")
        };
        assert_eq!(formats(list).unwrap(), vec![TEXT, 1, 16, 7]);
    }

    /// A server that uses long names anyway still copied something. The ids are at
    /// the front of each entry either way, and the entries are what tell the two
    /// forms apart: a whole number of short ones, or not.
    #[test]
    fn a_long_name_list_is_read_when_it_cannot_be_short_ones() {
        let mut body = Vec::new();
        body.extend_from_slice(&TEXT.to_le_bytes());
        body.extend_from_slice(&[0, 0]); // an empty name
        body.extend_from_slice(&49_u32.to_le_bytes());
        for unit in "Rich Text Format".encode_utf16().chain(std::iter::once(0)) {
            body.extend_from_slice(&unit.to_le_bytes());
        }
        assert!(!body.len().is_multiple_of(SHORT_NAME), "this list cannot be short names");
        assert_eq!(formats(&body).unwrap(), vec![TEXT, 49]);
    }

    /// A name field that never terminates is a truncated PDU, not a loop.
    #[test]
    fn an_unterminated_long_name_is_short_rather_than_endless() {
        let body = [TEXT.to_le_bytes().as_slice(), &[0x41, 0x00, 0x42]].concat();
        assert!(matches!(formats(&body).unwrap_err(), Malformed::Short { .. }));
    }

    #[test]
    fn a_paste_names_the_format_it_is_waiting_for() {
        let request = data_request(TEXT);
        assert_eq!(request, vec![0x04, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x0D, 0, 0, 0]);
        assert_eq!(decode(&request).unwrap(), Message::DataRequest { format: TEXT });
        // And a request for a format nobody offered still reads: what to do about one
        // is the caller's, and it has to answer it either way.
        let other = data_request(0xDEAD);
        assert_eq!(decode(&other).unwrap(), Message::DataRequest { format: 0xDEAD });
    }

    /// The two answers to a request, and the bit that tells them apart. A body
    /// without `CB_RESPONSE_OK` is not data that happens to be unvouched for — it is
    /// the failure.
    #[test]
    fn a_response_is_data_only_when_the_peer_said_it_succeeded() {
        let body = [b'h', 0, b'i', 0, 0, 0];
        let (answered, failed, empty) = (
            pdu(FORMAT_DATA_RESPONSE, RESPONSE_OK, &body),
            pdu(FORMAT_DATA_RESPONSE, 0x0002, &body),
            pdu(FORMAT_DATA_RESPONSE, 0, &[]),
        );
        assert_eq!(decode(&answered).unwrap(), Message::Data(Some(&body[..])));
        assert_eq!(decode(&failed).unwrap(), Message::Data(None));
        assert_eq!(decode(&empty).unwrap(), Message::Data(None));
    }

    #[test]
    fn what_this_client_answers_a_paste_with_says_which_of_the_two_it_is() {
        let bytes = [b'h', 0, b'i', 0, 0, 0];
        let (answered, refused, empty) =
            (data_response(Some(&bytes)), data_response(None), data_response(Some(&[])));
        assert_eq!(decode(&answered).unwrap(), Message::Data(Some(&bytes[..])));
        assert_eq!(decode(&refused).unwrap(), Message::Data(None));
        // An empty answer is the refusal too: this client only ever has bytes for
        // text it is holding.
        assert_eq!(decode(&empty).unwrap(), Message::Data(None));
    }

    #[test]
    fn the_answer_to_a_format_list_carries_whether_it_was_taken() {
        let (taken, failed) =
            (format_list_response(), pdu(FORMAT_LIST_RESPONSE, 0x0002, &[]));
        assert_eq!(decode(&taken).unwrap(), Message::ListResponse { ok: true });
        assert_eq!(decode(&failed).unwrap(), Message::ListResponse { ok: false });
    }

    /// The PDUs of the two features this client did not ask for. A server should not
    /// send them, and one that does is ignored by number rather than ending the
    /// session over a clipboard.
    #[test]
    fn the_pdus_of_features_that_were_not_asked_for_are_ignored_by_number() {
        for kind in [0x0006, 0x0008, 0x0009, 0x000A, 0x000B] {
            let other = pdu(kind, 0, &[1, 2, 3]);
            assert_eq!(decode(&other).unwrap(), Message::Ignored(kind));
        }
    }

    /// Both of these come off a socket, so neither may panic.
    #[test]
    fn a_truncated_pdu_is_short_rather_than_a_panic() {
        assert!(matches!(decode(&[0x01, 0x00, 0x00]).unwrap_err(), Malformed::Short { .. }));
        // A header announcing a body that is not there.
        let lying = pdu(FORMAT_DATA_RESPONSE, RESPONSE_OK, &[1, 2, 3]);
        assert!(matches!(decode(&lying[..HEADER + 1]).unwrap_err(), Malformed::Short { .. }));
        // A request whose body is too short for the format id it should carry.
        let mut w = Writer::new();
        header(&mut w, FORMAT_DATA_REQUEST, 0, 2);
        w.bytes(&[0x0D, 0x00]);
        assert!(matches!(decode(&w.finish()).unwrap_err(), Malformed::Short { .. }));
    }

    /// Padding past the announced length is not part of what the peer said, and the
    /// fields before it still read.
    #[test]
    fn padding_past_the_announced_length_is_not_part_of_the_body() {
        let mut w = Writer::new();
        header(&mut w, FORMAT_DATA_RESPONSE, RESPONSE_OK, 2);
        w.bytes(&[b'A', 0, 0xFF, 0xFF]);
        assert_eq!(decode(&w.finish()).unwrap(), Message::Data(Some(&[b'A', 0][..])));
    }
}
