//! TPKT framing, the X.224 connection sequence, and the security negotiation.
//!
//! RDP's bottom two layers are borrowed whole. TPKT (RFC 1006, ITU-T T.123) gives a
//! TCP stream packet boundaries: four bytes, of which the last two are the length of
//! the whole packet. X.224 class 0 (ITU-T X.224) puts a connection handshake on top
//! of that, of which RDP uses almost nothing — one Connection Request, one Connection
//! Confirm, and after that every PDU for the life of the session rides in a Data
//! TPDU whose three header bytes are always `02 f0 80`.
//!
//! What RDP added is the security negotiation, [\[MS-RDPBCGR\] 2.2.1.1.1]: the client
//! lists the protocols it accepts in the variable part of the Connection Request, and
//! the server names the one it chose in the Connection Confirm. That one field
//! decides the whole shape of the rest of the connection — whether a TLS handshake
//! follows, and whether CredSSP runs inside it.
//!
//! [\[MS-RDPBCGR\] 2.2.1.1.1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/902b090b-9cb3-4efc-92bf-ee13373371e3

use std::fmt;

use super::wire::{Malformed, Reader, Writer};

/// The four bytes in front of every X.224 TPDU.
pub const TPKT_HEADER: usize = 4;

/// TPKT's only version.
const TPKT_VERSION: u8 = 3;

/// The shortest legal TPKT frame: the header, plus the three bytes of the shortest
/// TPDU there is.
const TPKT_MIN: usize = TPKT_HEADER + 3;

/// The three header bytes of an X.224 Data TPDU, the wrapper every PDU after the
/// connection sequence arrives in: length indicator 2, code `DT`, and an EOT byte
/// that is always set because RDP never splits a TPDU across frames.
const DATA_TPDU: [u8; 3] = [0x02, 0xF0, 0x80];

const TPDU_CONNECTION_REQUEST: u8 = 0xE0;
const TPDU_CONNECTION_CONFIRM: u8 = 0xD0;

/// Length indicator, code, DST-REF, SRC-REF and the class byte: the fixed part of a
/// Connection Request or Confirm, before any negotiation data.
const CONNECT_TPDU: usize = 7;

const TYPE_RDP_NEG_REQ: u8 = 0x01;
const TYPE_RDP_NEG_RSP: u8 = 0x02;
const TYPE_RDP_NEG_FAILURE: u8 = 0x03;

/// Every `RDP_NEG_*` structure is the same eight bytes: type, flags, this length, and
/// one 32-bit value.
const NEG_DATA: u16 = 8;

const COOKIE_PREFIX: &str = "Cookie: mstshash=";

/// How much of the user name goes in the cookie.
///
/// [\[MS-RDPBCGR\] 2.2.1.1] caps the whole cookie field at 28 bytes, and the prefix
/// and the trailing CRLF account for 19 of them. `mstsc` truncates here too, so
/// servers are used to seeing a short name; sending the untruncated one risks a
/// strict server refusing a packet it is within its rights to refuse.
///
/// [\[MS-RDPBCGR\] 2.2.1.1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/18a27ef9-6f9a-4501-b000-94b1fe3c2c10
const COOKIE_IDENTIFIER_MAX: usize = 9;

/// A frame too long for the 16-bit length TPKT has room for.
///
/// Not reachable from the connection sequence, whose PDUs are all a few dozen bytes,
/// but every later layer that chunks a payload — MCS, the virtual channels — is
/// chunking it to fit exactly this, and a bug in one of those chunkers should surface
/// as a refused write rather than as a frame whose length field wrapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a {0}-byte frame does not fit TPKT's 16-bit length")]
pub struct TooLong(pub usize);

/// What a connection may be protected with, as the bit field
/// [\[MS-RDPBCGR\] 2.2.1.1.1] puts on the wire.
///
/// A request carries the set the client accepts; a confirm carries the single one the
/// server chose. Both are the same 32 bits, so they are the same type here.
///
/// [\[MS-RDPBCGR\] 2.2.1.1.1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/902b090b-9cb3-4efc-92bf-ee13373371e3
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Security(u32);

impl Security {
    /// No enhanced protection: the RC4 encryption RDP had before it had TLS, keyed
    /// from the server's own certificate in the MCS connect response. This client
    /// never asks for it — the value is zero, so it is also what an empty set means.
    pub const RDP: Self = Self(0x0000_0000);
    /// TLS, with the credentials sent inside it after the session is up.
    pub const SSL: Self = Self(0x0000_0001);
    /// Network Level Authentication: TLS, then CredSSP before anything else, so the
    /// credentials are checked before the server builds a session.
    pub const HYBRID: Self = Self(0x0000_0002);
    /// NLA with the Early User Authorization Result PDU, which lets a server say
    /// "authenticated but not authorised" before the licensing exchange. This client
    /// does not ask for it, so a conformant server will not select it.
    pub const HYBRID_EX: Self = Self(0x0000_0008);

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Security {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// Named, because the numbers show up in connection failures and nobody remembers
/// which bit NLA is.
impl fmt::Debug for Security {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMED: [(Security, &str); 3] =
            [(Security::SSL, "SSL"), (Security::HYBRID, "HYBRID"), (Security::HYBRID_EX, "HYBRID_EX")];
        if self.0 == Self::RDP.0 {
            return f.write_str("RDP");
        }
        let mut separator = "";
        let mut named = 0;
        for (bit, name) in NAMED {
            if self.contains(bit) {
                write!(f, "{separator}{name}")?;
                separator = "|";
                named |= bit.0;
            }
        }
        if self.0 & !named != 0 {
            write!(f, "{separator}{:#x}", self.0 & !named)?;
        }
        Ok(())
    }
}

/// What the server said it can do, alongside the protocol it chose.
/// [\[MS-RDPBCGR\] 2.2.1.2.1]
///
/// [\[MS-RDPBCGR\] 2.2.1.2.1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/b2975bdc-6d56-49ee-9c57-f2ff3a0b6817
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConfirmFlags(u8);

impl ConfirmFlags {
    /// The server accepts the extended blocks in the GCC conference create request —
    /// the monitor layout among them. A server without this flag gets the short form.
    pub const EXTENDED_CLIENT_DATA: Self = Self(0x01);
    /// The server would carry the graphics pipeline over a dynamic channel. This
    /// client decodes bitmaps and does not open it.
    pub const DYNVC_GFX: Self = Self(0x02);

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Why a server refused every protocol the client offered.
/// [\[MS-RDPBCGR\] 2.2.1.2.2]
///
/// [\[MS-RDPBCGR\] 2.2.1.2.2]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/1b3920e7-0116-4345-bc45-f2c4ad012761
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NegotiationFailure {
    #[error("the server requires TLS")]
    SslRequiredByServer,
    #[error("the server does not allow TLS")]
    SslNotAllowedByServer,
    #[error("the server has no certificate to offer")]
    SslCertNotOnServer,
    #[error("the server found the negotiation request's flags inconsistent")]
    InconsistentFlags,
    #[error("the server requires Network Level Authentication")]
    HybridRequiredByServer,
    #[error("the server requires TLS with prior user authorisation")]
    SslWithUserAuthRequiredByServer,
    #[error("the server refused the negotiation with code {0:#x}")]
    Unknown(u32),
}

impl From<u32> for NegotiationFailure {
    fn from(code: u32) -> Self {
        match code {
            0x01 => Self::SslRequiredByServer,
            0x02 => Self::SslNotAllowedByServer,
            0x03 => Self::SslCertNotOnServer,
            0x04 => Self::InconsistentFlags,
            0x05 => Self::HybridRequiredByServer,
            0x06 => Self::SslWithUserAuthRequiredByServer,
            other => Self::Unknown(other),
        }
    }
}

/// The client's first bytes on the connection: an X.224 Connection Request carrying
/// an RDP Negotiation Request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionRequest {
    /// The user name, for a load balancer that routes on it. A gateway dialling a
    /// host directly has nothing to route, but a Connection Broker in front of one
    /// needs this to send the reconnect to the machine that already has the session.
    pub cookie: Option<String>,
    /// Everything this client is willing to be protected with.
    pub protocols: Security,
}

impl ConnectionRequest {
    /// The complete frame to put on the wire, TPKT header and all.
    ///
    /// Infallible: every part of this PDU is either fixed-size or capped, so it
    /// cannot reach the length TPKT would refuse.
    pub fn encode(&self) -> Vec<u8> {
        let name = self.cookie.as_deref().map(identifier);
        let cookie_len = name.map_or(0, |name| COOKIE_PREFIX.len() + name.len() + 2);
        // The length indicator counts every byte after itself, the negotiation data
        // included — it is the variable part of the TPDU, not something appended
        // after it.
        let li = CONNECT_TPDU - 1 + cookie_len + usize::from(NEG_DATA);
        let total = TPKT_HEADER + 1 + li;

        let mut w = Writer::with_capacity(total);
        w.u8(TPKT_VERSION);
        w.u8(0);
        w.u16_be(u16::try_from(total).expect("a capped cookie plus fixed fields is under 64 bytes"));
        w.u8(u8::try_from(li).expect("a capped cookie plus fixed fields is under 64 bytes"));
        w.u8(TPDU_CONNECTION_REQUEST);
        // DST-REF, SRC-REF, and class 0 with no options. RDP leaves all three zero.
        w.zeros(5);
        if let Some(name) = name {
            w.bytes(COOKIE_PREFIX.as_bytes());
            w.bytes(name.as_bytes());
            w.bytes(b"\r\n");
        }
        w.u8(TYPE_RDP_NEG_REQ);
        // No flags: this client asks for neither restricted admin mode nor Remote
        // Credential Guard, and sends no correlation identifier.
        w.u8(0);
        w.u16_le(NEG_DATA);
        w.u32_le(self.protocols.bits());
        w.finish()
    }
}

/// What the server answered the negotiation request with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionConfirm {
    /// The server chose a protocol. It is one of the ones the request offered.
    Negotiated { protocol: Security, flags: ConfirmFlags },
    /// The server sent no negotiation data at all, which is how a server that does
    /// not speak the negotiation says "legacy RDP security". This client does not
    /// implement that, but the distinction is worth keeping: it is a different thing
    /// to tell a person than a refusal.
    Unnegotiated,
    /// The server refused every protocol offered, and said why.
    Refused(NegotiationFailure),
}

impl ConnectionConfirm {
    /// Decode one complete TPKT frame.
    pub fn decode(frame: &[u8]) -> Result<Self, Malformed> {
        const WHAT: &str = "an X.224 Connection Confirm";
        let payload = payload(WHAT, frame)?;

        let mut r = Reader::new(WHAT, payload);
        let li = r.u8()?;
        let code = r.u8()?;
        if code != TPDU_CONNECTION_CONFIRM {
            return Err(r.refuse("its TPDU code", code));
        }
        // DST-REF, SRC-REF and the class byte, none of which RDP uses.
        r.skip(5)?;

        // Bound the negotiation data by the length indicator rather than by the
        // frame: a server is allowed to leave slack after the TPDU, and reading into
        // it would turn padding into a field.
        let negotiation = usize::from(li)
            .checked_sub(CONNECT_TPDU - 1)
            .ok_or_else(|| r.refuse("its length indicator", li))?;
        let negotiation = r.bytes(negotiation)?;
        if negotiation.is_empty() {
            return Ok(Self::Unnegotiated);
        }

        let mut r = Reader::new(WHAT, negotiation);
        let kind = r.u8()?;
        let flags = r.u8()?;
        let length = r.u16_le()?;
        if length != NEG_DATA {
            return Err(r.refuse("its RDP_NEG_DATA length", length));
        }
        let value = r.u32_le()?;
        match kind {
            TYPE_RDP_NEG_RSP => {
                Ok(Self::Negotiated { protocol: Security(value), flags: ConfirmFlags(flags) })
            }
            TYPE_RDP_NEG_FAILURE => Ok(Self::Refused(value.into())),
            other => Err(r.refuse("its RDP_NEG_DATA type", other)),
        }
    }
}

/// `payload` in an X.224 Data TPDU, in a TPKT frame: the wrapper around every MCS PDU
/// the connection sends after the negotiation.
pub fn data(payload: &[u8]) -> Result<Vec<u8>, TooLong> {
    let total = TPKT_HEADER + DATA_TPDU.len() + payload.len();
    let announced = u16::try_from(total).map_err(|_| TooLong(total))?;

    let mut w = Writer::with_capacity(total);
    w.u8(TPKT_VERSION);
    w.u8(0);
    w.u16_be(announced);
    w.bytes(&DATA_TPDU);
    w.bytes(payload);
    Ok(w.finish())
}

/// What one received X.224 Data TPDU carries, given the whole TPKT frame.
pub fn data_payload(frame: &[u8]) -> Result<&[u8], Malformed> {
    const WHAT: &str = "an X.224 Data TPDU";
    let payload = payload(WHAT, frame)?;

    let mut r = Reader::new(WHAT, payload);
    // The length indicator, and then the code. Unlike a Connection Confirm the
    // variable part of a Data TPDU is *not* inside the indicator, so the payload
    // starts after it rather than at its end.
    let li = r.u8()?;
    let code = r.u8()?;
    if code != DATA_TPDU[1] {
        return Err(r.refuse("its TPDU code", code));
    }
    // The EOT byte, plus whatever else a header longer than RDP's own would hold.
    r.skip(usize::from(li).saturating_sub(1))?;
    Ok(r.rest())
}

/// The whole length — the four header bytes included — that a TPKT header announces.
///
/// The transport reads this many bytes and hands the result to a decoder, so it is
/// the one place that decides how much of the stream is one PDU.
pub fn frame_length(header: &[u8; TPKT_HEADER]) -> Result<usize, Malformed> {
    const WHAT: &str = "a TPKT header";
    let mut r = Reader::new(WHAT, header);
    let version = r.u8()?;
    if version != TPKT_VERSION {
        return Err(r.refuse("its version", version));
    }
    r.skip(1)?;
    let announced = r.u16_be()?;
    if usize::from(announced) < TPKT_MIN {
        return Err(r.refuse("its frame length", announced));
    }
    Ok(usize::from(announced))
}

/// The TPDU inside one complete TPKT frame.
///
/// The length in the header is what bounds the TPDU, not the length of the buffer:
/// the transport reads exactly that many bytes, so the two agree in practice, and
/// trusting the header keeps the decision about where a PDU ends in the one place
/// that made it.
fn payload<'a>(what: &'static str, frame: &'a [u8]) -> Result<&'a [u8], Malformed> {
    let header = frame.get(..TPKT_HEADER).ok_or(Malformed::Short {
        what,
        len: frame.len(),
        at: 0,
        need: TPKT_HEADER,
    })?;
    let header: &[u8; TPKT_HEADER] = header.try_into().expect("a slice of exactly TPKT_HEADER");
    let length = frame_length(header)?;
    frame.get(TPKT_HEADER..length).ok_or(Malformed::Short {
        what,
        len: frame.len(),
        at: TPKT_HEADER,
        need: length - TPKT_HEADER,
    })
}

/// The longest prefix of `cookie` that fits the field, cut on a character boundary.
fn identifier(cookie: &str) -> &str {
    let mut end = COOKIE_IDENTIFIER_MAX.min(cookie.len());
    while !cookie.is_char_boundary(end) {
        end -= 1;
    }
    &cookie[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confirm(negotiation: &[u8]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(TPKT_VERSION);
        w.u8(0);
        w.u16_be(u16::try_from(TPKT_HEADER + CONNECT_TPDU + negotiation.len()).unwrap());
        w.u8(u8::try_from(CONNECT_TPDU - 1 + negotiation.len()).unwrap());
        w.u8(TPDU_CONNECTION_CONFIRM);
        w.zeros(5);
        w.bytes(negotiation);
        w.finish()
    }

    /// The bytes `mstsc` puts on the wire, field for field.
    #[test]
    fn a_connection_request_is_the_frame_the_specification_describes() {
        let request = ConnectionRequest {
            cookie: Some("admin".to_owned()),
            protocols: Security::SSL | Security::HYBRID,
        };
        let mut expected = vec![
            0x03, 0x00, 0x00, 0x2B, // TPKT: version 3, reserved, 43 bytes in all
            0x26, // length indicator: 38 bytes after this one
            0xE0, // Connection Request
            0x00, 0x00, 0x00, 0x00, 0x00, // DST-REF, SRC-REF, class 0
        ];
        expected.extend_from_slice(b"Cookie: mstshash=admin\r\n");
        expected.extend_from_slice(&[
            0x01, 0x00, 0x08, 0x00, // RDP_NEG_REQ, no flags, 8 bytes
            0x03, 0x00, 0x00, 0x00, // SSL | HYBRID
        ]);
        assert_eq!(request.encode(), expected);
        assert_eq!(u16::from(expected[3]), u16::try_from(expected.len()).unwrap());
    }

    /// Without a cookie the request is the fixed part alone, and the two length
    /// fields have to follow it down.
    #[test]
    fn a_request_without_a_cookie_is_the_negotiation_alone() {
        let request = ConnectionRequest { cookie: None, protocols: Security::HYBRID };
        assert_eq!(
            request.encode(),
            vec![
                0x03, 0x00, 0x00, 0x13, // 19 bytes in all
                0x0E, 0xE0, 0x00, 0x00, 0x00, 0x00, 0x00, //
                0x01, 0x00, 0x08, 0x00, 0x02, 0x00, 0x00, 0x00,
            ]
        );
    }

    /// A name longer than the field is cut, not refused, and never cut through the
    /// middle of a character.
    #[test]
    fn a_long_user_name_is_cut_to_the_cookie_the_field_holds() {
        let long = ConnectionRequest {
            cookie: Some("administrator".to_owned()),
            protocols: Security::SSL,
        };
        let bytes = long.encode();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("Cookie: mstshash=administr\r\n"), "{text}");

        // Nine bytes lands in the middle of the third character here, so eight go.
        let wide =
            ConnectionRequest { cookie: Some("héllo wörld".to_owned()), protocols: Security::SSL };
        let bytes = wide.encode();
        assert!(String::from_utf8_lossy(&bytes).contains("mstshash=héllo w\r\n"));
        assert_eq!(usize::from(bytes[4]) + 5, bytes.len(), "the length indicator follows the cut");
    }

    #[test]
    fn a_confirm_names_the_protocol_the_server_chose() {
        let frame = confirm(&[0x02, 0x03, 0x08, 0x00, 0x02, 0x00, 0x00, 0x00]);
        assert_eq!(
            ConnectionConfirm::decode(&frame).unwrap(),
            ConnectionConfirm::Negotiated {
                protocol: Security::HYBRID,
                flags: ConfirmFlags(0x03),
            }
        );
        let ConnectionConfirm::Negotiated { flags, .. } = ConnectionConfirm::decode(&frame).unwrap()
        else {
            panic!("a negotiated confirm");
        };
        assert!(flags.contains(ConfirmFlags::EXTENDED_CLIENT_DATA));
        assert!(flags.contains(ConfirmFlags::DYNVC_GFX));
    }

    #[test]
    fn a_server_that_refuses_says_why() {
        let frame = confirm(&[0x03, 0x00, 0x08, 0x00, 0x05, 0x00, 0x00, 0x00]);
        let ConnectionConfirm::Refused(why) = ConnectionConfirm::decode(&frame).unwrap() else {
            panic!("a refusal");
        };
        assert_eq!(why.to_string(), "the server requires Network Level Authentication");

        let frame = confirm(&[0x03, 0x00, 0x08, 0x00, 0x63, 0x00, 0x00, 0x00]);
        assert_eq!(
            ConnectionConfirm::decode(&frame).unwrap(),
            ConnectionConfirm::Refused(NegotiationFailure::Unknown(0x63))
        );
    }

    /// A server with nothing to say about security is not a server that failed.
    #[test]
    fn a_confirm_without_negotiation_data_is_its_own_answer() {
        assert_eq!(ConnectionConfirm::decode(&confirm(&[])).unwrap(), ConnectionConfirm::Unnegotiated);
    }

    #[test]
    fn a_confirm_that_is_not_one_is_refused_rather_than_guessed_at() {
        let mut frame = confirm(&[0x02, 0x00, 0x08, 0x00, 0x02, 0x00, 0x00, 0x00]);
        frame[5] = 0xE0; // a Connection Request coming back at us
        assert!(matches!(
            ConnectionConfirm::decode(&frame).unwrap_err(),
            Malformed::Refused { field: "its TPDU code", value: 0xE0, .. }
        ));

        // A negotiation structure that is not eight bytes long.
        let frame = confirm(&[0x02, 0x00, 0x07, 0x00, 0x02, 0x00, 0x00, 0x00]);
        assert!(matches!(
            ConnectionConfirm::decode(&frame).unwrap_err(),
            Malformed::Refused { value: 7, .. }
        ));

        // A length indicator too small to cover the header it introduces.
        let mut frame = confirm(&[0x02, 0x00, 0x08, 0x00, 0x02, 0x00, 0x00, 0x00]);
        frame[4] = 0x02;
        assert!(matches!(ConnectionConfirm::decode(&frame).unwrap_err(), Malformed::Refused { .. }));
    }

    /// Truncation is the shape a socket produces, so it has to be an error and not a
    /// panic on a slice.
    #[test]
    fn a_frame_that_is_not_the_length_it_claims_is_an_error() {
        let frame = confirm(&[0x02, 0x00, 0x08, 0x00, 0x02, 0x00, 0x00, 0x00]);
        for cut in 0..frame.len() {
            assert!(ConnectionConfirm::decode(&frame[..cut]).is_err(), "cut to {cut} bytes");
        }
    }

    #[test]
    fn a_tpkt_header_announces_the_whole_frame() {
        assert_eq!(frame_length(&[0x03, 0x00, 0x00, 0x2B]).unwrap(), 43);
        // Not TPKT at all: byte 0 of a fast-path PDU is an action, never 3.
        assert!(matches!(
            frame_length(&[0x00, 0x00, 0x00, 0x2B]).unwrap_err(),
            Malformed::Refused { field: "its version", .. }
        ));
        // Shorter than the shortest TPDU it could be carrying.
        assert!(frame_length(&[0x03, 0x00, 0x00, 0x06]).is_err());
    }

    #[test]
    fn a_data_tpdu_round_trips() {
        let frame = data(b"an MCS PDU").unwrap();
        assert_eq!(&frame[..4], &[0x03, 0x00, 0x00, 17]);
        assert_eq!(&frame[4..7], &DATA_TPDU);
        assert_eq!(data_payload(&frame).unwrap(), b"an MCS PDU");

        assert_eq!(data(&vec![0; 65_529]).unwrap_err(), TooLong(65_536));
    }

    /// Nothing in RDP sends a longer Data TPDU header, but X.224 allows one, and
    /// stepping over it is the difference between reading the payload and reading the
    /// tail of a header.
    #[test]
    fn a_data_tpdu_payload_starts_after_the_length_indicator() {
        let payload = [0xAA, 0xBB];
        let mut w = Writer::new();
        w.u8(TPKT_VERSION);
        w.u8(0);
        w.u16_be(4 + 4 + 2);
        w.u8(0x03); // three bytes of header after this one
        w.u8(0xF0);
        w.u8(0x80);
        w.u8(0x00); // one byte this client does not read
        w.bytes(&payload);
        assert_eq!(data_payload(&w.finish()).unwrap(), payload);
    }

    #[test]
    fn security_reads_as_names() {
        assert_eq!(format!("{:?}", Security::RDP), "RDP");
        assert_eq!(format!("{:?}", Security::SSL | Security::HYBRID), "SSL|HYBRID");
        assert_eq!(format!("{:?}", Security(0x11)), "SSL|0x10");
    }
}
