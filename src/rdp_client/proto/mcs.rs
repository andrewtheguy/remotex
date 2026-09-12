//! T.125 MCS: the channels every later PDU travels on.
//!
//! MCS is a conferencing protocol RDP borrowed whole. Its model is a domain that
//! users attach to and channels that users join, and RDP uses exactly one domain, one
//! user, and a handful of channels — the I/O channel that carries the desktop, and one
//! per static virtual channel. Everything after this module's work is done is an MCS
//! Send Data PDU addressed to one of those channels.
//!
//! Getting there is a fixed sequence, and each step is a round trip:
//!
//! 1. **Connect-Initial / Connect-Response** — the domain parameters, wrapped around
//!    the GCC conference of [`super::gcc`]. This is the one that matters: the server's
//!    answer carries the channel numbers.
//! 2. **Erect Domain Request** — announces this client is the top of a domain of one.
//!    Unanswered.
//! 3. **Attach User Request / Confirm** — gets the user identifier that addresses
//!    every PDU from here on.
//! 4. **Channel Join Request / Confirm**, once per channel, the user's own included.
//!
//! # Two encodings, ten bytes apart
//!
//! The connect PDUs are BER ([`super::der`]) and everything after them is PER
//! ([`super::per`]), because T.125 defines the first two that way and the rest the
//! other. There is no seam to see on the wire — the X.224 data TPDU is the same in
//! both cases — so the only defence is that the two live in different functions here
//! and each says which it is.
//!
//! Every function in this module takes or returns a whole TPKT frame, so a caller
//! reads a frame off the socket and hands it over without unwrapping anything.

use std::fmt;

use super::wire::{Malformed, Reader, Writer};
use super::x224::{self, TooLong};
use super::{der, per};

/// The lower bound MCS gives user identifiers. A user is written on the wire as the
/// distance from it, and a channel is not — see [`per::write_integer16`].
const BASE_USER: u16 = 1001;

/// `Connect-Initial`, `[APPLICATION 101]`.
const CONNECT_INITIAL: u8 = 101;
/// `Connect-Response`, `[APPLICATION 102]`.
const CONNECT_RESPONSE: u8 = 102;

/// The domain selectors, which identify the domain among several on one connection.
/// RDP has one domain, so both ends write the same single byte and neither reads it.
const DOMAIN_SELECTOR: [u8; 1] = [0x01];

/// `DomainMCSPDU` CHOICE indices. The index is written in the top six bits of a byte,
/// leaving the bottom two for whichever OPTIONAL fields the PDU has.
const ERECT_DOMAIN_REQUEST: u8 = 1;
const DISCONNECT_PROVIDER_ULTIMATUM: u8 = 8;
const ATTACH_USER_REQUEST: u8 = 10;
const ATTACH_USER_CONFIRM: u8 = 11;
const CHANNEL_JOIN_REQUEST: u8 = 14;
const CHANNEL_JOIN_CONFIRM: u8 = 15;
const SEND_DATA_REQUEST: u8 = 25;
const SEND_DATA_INDICATION: u8 = 26;

/// `dataPriority` high, `segmentation` both first and last — the only combination RDP
/// ever sends, because RDP does its own chunking above MCS.
const WHOLE_HIGH_PRIORITY: u8 = 0x70;

/// The most a Send Data payload can be, which is what PER's length field holds.
pub const MAX_SEND_DATA: usize = per::MAX_LENGTH as usize;

/// `DomainParameters`, which both ends use to agree on the size of the domain.
///
/// Three of these go out — what is wanted, the least that would do, the most that
/// could be used — and the server picks. Nothing in this client varies with the
/// answer, so the values are the ones every RDP client has sent for twenty years and
/// the response's copy is read past.
struct DomainParameters {
    max_channel_ids: u32,
    max_user_ids: u32,
    max_token_ids: u32,
    max_mcs_pdu_size: u32,
}

impl DomainParameters {
    const TARGET: Self =
        Self { max_channel_ids: 34, max_user_ids: 2, max_token_ids: 0, max_mcs_pdu_size: 0xFFFF };
    const MINIMUM: Self =
        Self { max_channel_ids: 1, max_user_ids: 1, max_token_ids: 1, max_mcs_pdu_size: 0x420 };
    const MAXIMUM: Self = Self {
        max_channel_ids: 0xFFFF,
        max_user_ids: 0xFC17,
        max_token_ids: 0xFFFF,
        max_mcs_pdu_size: 0xFFFF,
    };

    fn write(&self, w: &mut Writer) {
        let mut fields = Writer::with_capacity(32);
        der::write_integer(&mut fields, self.max_channel_ids);
        der::write_integer(&mut fields, self.max_user_ids);
        der::write_integer(&mut fields, self.max_token_ids);
        der::write_integer(&mut fields, 1); // numPriorities
        der::write_integer(&mut fields, 0); // minThroughput
        der::write_integer(&mut fields, 1); // maxHeight
        der::write_integer(&mut fields, self.max_mcs_pdu_size);
        der::write_integer(&mut fields, 2); // protocolVersion
        let fields = fields.finish();
        der::write_tag(w, der::SEQUENCE, fields.len());
        w.bytes(&fields);
    }
}

/// An MCS Connect-Initial carrying a GCC Conference Create Request.
pub fn connect_initial(conference: &[u8]) -> Result<Vec<u8>, TooLong> {
    let mut body = Writer::with_capacity(conference.len() + 128);
    der::write_octet_string(&mut body, &DOMAIN_SELECTOR); // callingDomainSelector
    der::write_octet_string(&mut body, &DOMAIN_SELECTOR); // calledDomainSelector
    der::write_boolean(&mut body, true); // upwardFlag: this client is the top
    DomainParameters::TARGET.write(&mut body);
    DomainParameters::MINIMUM.write(&mut body);
    DomainParameters::MAXIMUM.write(&mut body);
    der::write_octet_string(&mut body, conference); // userData
    let body = body.finish();

    let mut w = Writer::with_capacity(body.len() + 4);
    der::write_application_tag(&mut w, CONNECT_INITIAL, body.len());
    w.bytes(&body);
    x224::data(&w.finish())
}

/// The GCC Conference Create Response inside an MCS Connect-Response.
pub fn connect_response(frame: &[u8]) -> Result<&[u8], Malformed> {
    const WHAT: &str = "an MCS Connect-Response";
    let mut r = Reader::new(WHAT, x224::data_payload(frame)?);
    let mut fields = Reader::new(WHAT, der::expect_application(&mut r, CONNECT_RESPONSE)?);

    let result = der::read_enumerated(&mut fields)?;
    if result != 0 {
        return Err(fields.refuse("a refusal to connect, MCS result", result));
    }
    der::read_integer(&mut fields)?; // calledConnectId, which nothing addresses
    der::skip(&mut fields)?; // domainParameters: the server's answer, which changes nothing
    der::expect(&mut fields, der::OCTET_STRING)
}

/// An Erect Domain Request. The server does not answer it.
pub fn erect_domain_request() -> Vec<u8> {
    let mut w = Writer::with_capacity(5);
    w.u8(ERECT_DOMAIN_REQUEST << 2);
    per::write_integer(&mut w, 0); // subHeight: no sub-domains below this client
    per::write_integer(&mut w, 0); // subInterval
    frame("an Erect Domain Request", &w.finish())
}

/// An Attach User Request, which is the CHOICE byte and nothing else.
pub fn attach_user_request() -> Vec<u8> {
    frame("an Attach User Request", &[ATTACH_USER_REQUEST << 2])
}

/// The user identifier the server assigned, out of an Attach User Confirm.
pub fn attach_user_confirm(bytes: &[u8]) -> Result<u16, Malformed> {
    const WHAT: &str = "an MCS Attach User Confirm";
    let mut r = domain_pdu(WHAT, bytes, ATTACH_USER_CONFIRM)?;
    result(&mut r)?;
    // `initiator` is OPTIONAL, and a successful confirm always carries it — it is the
    // only thing the PDU is for.
    per::read_integer16(&mut r, BASE_USER)
}

/// A Channel Join Request for one channel. The user's own channel is joined the same
/// way as any other, and first.
pub fn channel_join_request(user: u16, channel: u16) -> Vec<u8> {
    let mut w = Writer::with_capacity(5);
    w.u8(CHANNEL_JOIN_REQUEST << 2);
    per::write_integer16(&mut w, user, BASE_USER); // initiator
    per::write_integer16(&mut w, channel, 0); // channelId
    frame("a Channel Join Request", &w.finish())
}

/// The channel that was joined, out of a Channel Join Confirm.
///
/// A server may join a client to a different channel than it asked for, which is why
/// the number comes back rather than being assumed.
pub fn channel_join_confirm(bytes: &[u8]) -> Result<u16, Malformed> {
    const WHAT: &str = "an MCS Channel Join Confirm";
    let mut r = domain_pdu(WHAT, bytes, CHANNEL_JOIN_CONFIRM)?;
    result(&mut r)?;
    per::read_integer16(&mut r, BASE_USER)?; // initiator, which is this client
    per::read_integer16(&mut r, 0)?; // requested, which is what we asked for
    per::read_integer16(&mut r, 0)
}

/// A Send Data Request: everything the client says after the connection sequence,
/// addressed to one channel.
pub fn send_data_request(user: u16, channel: u16, payload: &[u8]) -> Result<Vec<u8>, TooLong> {
    if payload.len() > MAX_SEND_DATA {
        return Err(TooLong(payload.len()));
    }
    let mut w = Writer::with_capacity(payload.len() + 8);
    w.u8(SEND_DATA_REQUEST << 2);
    per::write_integer16(&mut w, user, BASE_USER); // initiator
    per::write_integer16(&mut w, channel, 0); // channelId
    w.u8(WHOLE_HIGH_PRIORITY);
    per::write_length(
        &mut w,
        u16::try_from(payload.len()).expect("the length was just bounded by MAX_SEND_DATA"),
    );
    w.bytes(payload);
    x224::data(&w.finish())
}

/// Leave the conference, which is how a client that meant to disconnect says so: the
/// server keeps the session logged on for whoever connects next, rather than tearing
/// it down as it would for a socket that simply stopped answering.
pub fn disconnect_provider_ultimatum(reason: Reason) -> Vec<u8> {
    let reason = reason.0;
    // The reason is a three-bit ENUMERATED straddling two bytes, which is PER packing
    // bits rather than bytes for once — the same straddle [`domain_pdu`] reads.
    let mut w = Writer::with_capacity(2);
    w.u8((DISCONNECT_PROVIDER_ULTIMATUM << 2) | ((reason >> 1) & 0x03));
    w.u8(reason << 7);
    frame("a Disconnect Provider Ultimatum", &w.finish())
}

/// Why a conference ended, in the MCS `Reason` both ends write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reason(u8);

impl Reason {
    /// The one this client sends: the person is done, and the session stays on the
    /// host for their next connection.
    pub const USER_REQUESTED: Self = Self(3);

    /// Whether an ultimatum that arrived means an orderly end rather than a fault. A
    /// server hanging up on purpose, and a client that asked to leave, are the two
    /// that do.
    pub fn is_orderly(self) -> bool {
        matches!(self, Self(1) | Self(3))
    }

    pub fn describe(self) -> &'static str {
        match self.0 {
            0 => "the domain was disconnected",
            1 => "the host disconnected the session",
            2 => "a token was purged",
            3 => "the session was disconnected at the user's request",
            4 => "a channel was purged",
            _ => "for a reason this client has no name for",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.describe(), self.0)
    }
}

/// One channel's worth of what the server said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendData<'a> {
    pub channel: u16,
    pub payload: &'a [u8],
}

/// What arrived on the connection once the conference is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indication<'a> {
    /// A PDU addressed to a channel.
    Data(SendData<'a>),
    /// The server left the conference. Every session ends with one of these, from
    /// one end or the other.
    Disconnect(Reason),
}

/// A Send Data Indication: everything the server says after the connection sequence,
/// and the one other PDU it may send instead — which is not an error here, unlike in
/// the middle of the connection sequence, because it is how a session ends.
pub fn send_data_indication(frame: &[u8]) -> Result<Indication<'_>, Malformed> {
    const WHAT: &str = "an MCS Send Data Indication";
    let payload = x224::data_payload(frame)?;
    if let Some(reason) = ultimatum(WHAT, payload)? {
        return Ok(Indication::Disconnect(reason));
    }
    let mut r = domain_pdu(WHAT, frame, SEND_DATA_INDICATION)?;
    per::read_integer16(&mut r, BASE_USER)?; // initiator, which is the server
    let channel = per::read_integer16(&mut r, 0)?;
    r.u8()?; // dataPriority and segmentation, which RDP never splits
    let length = usize::from(per::read_length(&mut r)?);
    Ok(Indication::Data(SendData { channel, payload: r.bytes(length)? }))
}

/// A whole TPKT frame around a PDU whose size is known at compile time to fit one.
fn frame(what: &'static str, pdu: &[u8]) -> Vec<u8> {
    x224::data(pdu).unwrap_or_else(|_| unreachable!("{what} is a handful of bytes"))
}

/// The start of a domain PDU: unwrap the framing, check the CHOICE, and turn the one
/// other thing a server may answer with into a sentence.
fn domain_pdu<'a>(
    what: &'static str,
    frame: &'a [u8],
    expected: u8,
) -> Result<Reader<'a>, Malformed> {
    let payload = x224::data_payload(frame)?;
    let mut r = Reader::new(what, payload);
    let choice = r.u8()?;
    let found = choice >> 2;
    if found == expected {
        return Ok(r);
    }
    if let Some(reason) = ultimatum(what, payload)? {
        let field = "a Disconnect Provider Ultimatum instead, whose reason is";
        return Err(Malformed::Refused { what, field, value: u64::from(reason.0) });
    }
    Err(r.refuse("an MCS PDU type", found))
}

/// The reason, if the PDU that starts here is a Disconnect Provider Ultimatum.
///
/// The reason is a three-bit ENUMERATED straddling the CHOICE byte and the next,
/// which is PER packing bits rather than bytes for once.
fn ultimatum(what: &'static str, payload: &[u8]) -> Result<Option<Reason>, Malformed> {
    let mut r = Reader::new(what, payload);
    let choice = r.u8()?;
    if choice >> 2 != DISCONNECT_PROVIDER_ULTIMATUM {
        return Ok(None);
    }
    Ok(Some(Reason(((choice & 0x03) << 1) | (r.u8()? >> 7))))
}

/// The `result` field both confirms start with.
fn result(r: &mut Reader<'_>) -> Result<(), Malformed> {
    match r.u8()? {
        0 => Ok(()),
        other => Err(r.refuse("an unsuccessful MCS result", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload of a frame this module built, for inspecting what went on the wire.
    fn payload(frame: &[u8]) -> &[u8] {
        x224::data_payload(frame).unwrap()
    }

    /// A frame carrying a PDU, the way a server would send one.
    fn server(pdu: &[u8]) -> Vec<u8> {
        x224::data(pdu).unwrap()
    }

    #[test]
    fn a_connect_initial_wraps_the_conference_in_the_parameters_the_server_expects() {
        let conference = vec![0xAB; 300];
        let bytes = connect_initial(&conference).unwrap();
        let mut r = Reader::new("a test", payload(&bytes));
        let mut fields = Reader::new("a test", der::expect_application(&mut r, 101).unwrap());
        assert!(r.is_empty());

        assert_eq!(der::expect(&mut fields, der::OCTET_STRING).unwrap(), &DOMAIN_SELECTOR);
        assert_eq!(der::expect(&mut fields, der::OCTET_STRING).unwrap(), &DOMAIN_SELECTOR);
        assert_eq!(der::expect(&mut fields, der::BOOLEAN).unwrap(), &[0xFF]);
        for expected in [34_u32, 1, 0xFFFF] {
            let mut parameters =
                Reader::new("a test", der::expect(&mut fields, der::SEQUENCE).unwrap());
            assert_eq!(der::read_integer(&mut parameters).unwrap(), expected);
        }
        assert_eq!(der::expect(&mut fields, der::OCTET_STRING).unwrap(), conference);
        assert!(fields.is_empty());
    }

    /// The response's user data is what the GCC decoder is given, and the fields
    /// before it are stepped over rather than guessed at.
    #[test]
    fn a_connect_response_yields_the_conference_the_server_answered_with() {
        let mut body = Writer::new();
        der::write_tag(&mut body, der::ENUMERATED, 1);
        body.u8(0); // rt-successful
        der::write_integer(&mut body, 0); // calledConnectId
        DomainParameters::TARGET.write(&mut body);
        der::write_octet_string(&mut body, &[1, 2, 3]);
        let body = body.finish();

        let mut w = Writer::new();
        der::write_application_tag(&mut w, CONNECT_RESPONSE, body.len());
        w.bytes(&body);
        assert_eq!(connect_response(&server(&w.finish())).unwrap(), &[1, 2, 3]);
    }

    #[test]
    fn a_refused_connection_names_the_mcs_result_rather_than_decoding_on() {
        let mut body = Writer::new();
        der::write_tag(&mut body, der::ENUMERATED, 1);
        body.u8(1); // rt-domain-merging
        let body = body.finish();
        let mut w = Writer::new();
        der::write_application_tag(&mut w, CONNECT_RESPONSE, body.len());
        w.bytes(&body);

        let err = connect_response(&server(&w.finish())).unwrap_err();
        assert_eq!(
            err.to_string(),
            "an MCS Connect-Response carries a refusal to connect, MCS result 0x1, which this \
             client does not accept"
        );
    }

    /// The three fixed PDUs, byte for byte: they never vary, so a golden value is the
    /// whole test.
    #[test]
    fn the_pdus_that_never_change_are_the_bytes_the_specification_gives() {
        assert_eq!(payload(&erect_domain_request()), &[0x04, 0x01, 0x00, 0x01, 0x00]);
        assert_eq!(payload(&attach_user_request()), &[0x28]);
        // User 1007 is six above the base; channel 1003 is not counted from anything.
        assert_eq!(payload(&channel_join_request(1007, 1003)), &[
            0x38, 0x00, 0x06, 0x03, 0xEB
        ]);
    }

    #[test]
    fn a_confirm_yields_the_identifier_it_was_asked_for() {
        assert_eq!(attach_user_confirm(&server(&[0x2E, 0x00, 0x00, 0x06])).unwrap(), 1007);
        assert_eq!(
            channel_join_confirm(&server(&[0x3E, 0x00, 0x00, 0x06, 0x03, 0xEB, 0x03, 0xEB]))
                .unwrap(),
            1003
        );
    }

    #[test]
    fn an_unsuccessful_confirm_is_an_error_rather_than_a_channel_number() {
        let err = channel_join_confirm(&server(&[0x3E, 0x03, 0x00, 0x06, 0x03, 0xEB, 0x00, 0x00]))
            .unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "an unsuccessful MCS result", .. }));
    }

    /// The answer a server that has decided to end the connection sends instead of
    /// whatever was expected. Naming it is the difference between "wrong PDU" and
    /// "the server hung up, and here is why".
    #[test]
    fn a_server_that_hangs_up_says_so_rather_than_being_read_as_a_wrong_pdu() {
        let err = attach_user_confirm(&server(&[0x21, 0x80])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "an MCS Attach User Confirm carries a Disconnect Provider Ultimatum instead, whose \
             reason is 0x3, which this client does not accept"
        );

        let err = attach_user_confirm(&server(&[0x38, 0x00])).unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "an MCS PDU type", value: 14, .. }));
    }

    #[test]
    fn send_data_goes_out_and_comes_back_on_the_channel_it_names() {
        let bytes = send_data_request(1007, 1003, b"hello").unwrap();
        assert_eq!(payload(&bytes), &[
            0x64, 0x00, 0x06, 0x03, 0xEB, 0x70, 0x05, b'h', b'e', b'l', b'l', b'o'
        ]);

        // A payload past 127 bytes needs PER's two-byte length, and the reader takes
        // either form.
        let long = send_data_request(1007, 1003, &[0xAB; 200]).unwrap();
        assert_eq!(payload(&long)[6..8], [0x80, 0xC8]);

        // The same shape with the indication's CHOICE, which is what a server sends.
        let mut indication = payload(&bytes).to_vec();
        indication[0] = SEND_DATA_INDICATION << 2;
        let indication = server(&indication);
        let decoded = send_data_indication(&indication).unwrap();
        assert_eq!(decoded, Indication::Data(SendData { channel: 1003, payload: b"hello" }));
    }

    /// The same two bytes each way, and the straddle is the whole of it: the reason
    /// is split across them, so a reason above one is where a wrong mask shows up.
    #[test]
    fn leaving_the_conference_is_written_and_read_the_same_way() {
        let bytes = disconnect_provider_ultimatum(Reason::USER_REQUESTED);
        assert_eq!(payload(&bytes), &[0x21, 0x80]);
        assert_eq!(ultimatum("a test", &[0x21, 0x80]).unwrap(), Some(Reason::USER_REQUESTED));
        for reason in 0..=4_u8 {
            let written = [
                (DISCONNECT_PROVIDER_ULTIMATUM << 2) | ((reason >> 1) & 0x03),
                reason << 7,
            ];
            let read = ultimatum("a test", &written).unwrap();
            assert_eq!(read, Some(Reason(reason)), "reason {reason}");
        }
    }

    /// Mid-session an ultimatum is the session ending, not a PDU in the wrong place —
    /// which is the opposite of what it means during the connection sequence.
    #[test]
    fn a_server_that_hangs_up_on_a_live_session_is_read_as_the_end_of_it() {
        let frame = server(&[0x21, 0x80]);
        let decoded = send_data_indication(&frame).unwrap();
        assert_eq!(decoded, Indication::Disconnect(Reason(3)));
        let Indication::Disconnect(reason) = decoded else { panic!("a disconnect") };
        assert!(reason.is_orderly());
        assert_eq!(reason.to_string(), "the session was disconnected at the user's request (3)");
        assert!(!Reason(0).is_orderly(), "a domain that went away is not an orderly end");
        assert!(Reason(1).is_orderly(), "and a host that hung up on purpose is");
    }

    #[test]
    fn a_payload_too_long_for_one_send_data_is_refused_rather_than_truncated() {
        assert!(send_data_request(1007, 1003, &vec![0; MAX_SEND_DATA + 1]).is_err());
    }
}
