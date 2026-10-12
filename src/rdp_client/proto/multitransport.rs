//! Multitransport bootstrapping ([MS-RDPBCGR] 2.2.15): the host offering to carry
//! part of the session over UDP, and this client turning it down.
//!
//! A client that claims a UDP transport in its core data
//! ([`super::gcc::ConferenceCreateRequest::multitransport`]) may be sent an
//! Initiate Multitransport Request on the message channel once licensing is done:
//! an id, the transport, and a cookie for the client to present over the new
//! connection. This client opens no such connection. It answers every request with
//! `E_ABORT` ([MS-RDPBCGR] 3.2.5.15.1), which a client that offered Soft-Sync must
//! send whatever happens, and the session carries on over TCP.
//!
//! The claim exists to find out whether a Windows host draws differently for a
//! client that offers UDP, before anything is built to use it.

use super::share;
use super::wire::{Malformed, Reader, Writer};

/// `SEC_TRANSPORT_REQ`, on every request the host sends.
pub const REQUEST: u16 = 0x0002;
/// `SEC_TRANSPORT_RSP`, on every response.
const RESPONSE: u16 = 0x0004;

/// `TRANSPORTTYPE_UDPFECR`: reliable RDP-UDP.
const UDP_RELIABLE: u32 = 0x0000_0001;
/// `SOFTSYNC_TCP_TO_UDP`: dynamic channels may move from TCP to UDP.
const SOFT_SYNC: u32 = 0x0000_0200;

/// What `CS_MULTITRANSPORT` claims: reliable UDP with Soft-Sync, which is what a
/// current client that moves its dynamic channels to UDP offers.
pub const OFFERED: u32 = UDP_RELIABLE | SOFT_SYNC;

/// `E_ABORT`: the client could not establish the transport.
const E_ABORT: u32 = 0x8000_4004;

/// One Initiate Multitransport Request, as far as it is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: u32,
    /// `requestedProtocol`: 1 for reliable UDP, 2 for lossy.
    pub protocol: u16,
}

/// Whether a message-channel PDU, from its security header on, is a multitransport
/// request rather than network detection.
pub fn is_request(payload: &[u8]) -> bool {
    let mut r = Reader::new("a message-channel PDU", payload);
    share::read_security_header(&mut r).is_ok_and(|flags| flags & REQUEST != 0)
}

/// One Initiate Multitransport Request PDU, from its security header on, and the
/// Initiate Multitransport Response that turns it down.
pub fn decline(payload: &[u8]) -> Result<(Request, Vec<u8>), Malformed> {
    const WHAT: &str = "an Initiate Multitransport Request PDU";
    let mut r = Reader::new(WHAT, payload);
    let flags = share::read_security_header(&mut r)?;
    if flags & REQUEST == 0 {
        return Err(r.refuse("its security flags", flags));
    }
    let id = r.u32_le()?;
    let protocol = r.u16_le()?;
    r.skip(2)?; // reserved
    r.bytes(16)?; // securityCookie, for a connection this client does not open
    let mut w = Writer::with_capacity(12);
    share::write_security_header(&mut w, RESPONSE);
    w.u32_le(id);
    w.u32_le(E_ABORT);
    Ok((Request { id, protocol }, w.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request from its security header on, written field by field.
    fn request(flags: u16, id: u32, protocol: u16, cookie: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&flags.to_le_bytes());
        bytes.extend_from_slice(&[0, 0]); // flagsHi
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&protocol.to_le_bytes());
        bytes.extend_from_slice(&[0, 0]); // reserved
        bytes.extend_from_slice(cookie);
        bytes
    }

    #[test]
    fn a_request_is_turned_down_by_its_id() {
        let pdu = request(REQUEST, 0x1234_5678, 1, &[0xAB; 16]);
        assert!(is_request(&pdu));
        let (said, reply) = decline(&pdu).unwrap();
        assert_eq!(said, Request { id: 0x1234_5678, protocol: 1 });
        assert_eq!(
            reply,
            [0x04, 0x00, 0x00, 0x00, 0x78, 0x56, 0x34, 0x12, 0x04, 0x40, 0x00, 0x80],
            "SEC_TRANSPORT_RSP, the id, and E_ABORT"
        );
    }

    #[test]
    fn network_detection_is_not_a_request() {
        assert!(!is_request(&[0x00, 0x10, 0x00, 0x00, 0x06, 0x00, 0x01, 0x00, 0x01, 0x10]));
        assert!(!is_request(&[0x02]), "too short for a security header");
    }

    #[test]
    fn a_request_without_its_cookie_is_refused() {
        assert!(decline(&request(REQUEST, 1, 1, &[0; 15])).is_err());
        assert!(decline(&request(0x1000, 1, 1, &[0; 16])).is_err());
    }
}
