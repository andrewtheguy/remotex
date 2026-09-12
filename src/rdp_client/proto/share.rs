//! The headers a PDU wears once the connection sequence is over.
//!
//! Everything from the Client Info PDU onwards travels on the I/O channel inside an
//! MCS Send Data PDU, and inside that it wears one or two more headers. This module
//! is those headers, and the dispatch that says which kind of PDU has arrived.
//!
//! # The security header, which is nearly never there
//!
//! [MS-RDPBCGR] gives a PDU an optional four-byte security header, and which PDUs
//! carry one depends on the encryption the server chose. This client refuses every
//! RDP encryption method and level — see [`super::gcc`] — so under the rule for
//! `ENCRYPTION_LEVEL_NONE` exactly two PDUs carry one: the Client Info PDU going out
//! and the licensing PDU coming back. Each is written by the module that owns it
//! ([`super::info`], [`super::license`]) out of the two functions here; every other
//! PDU starts at the share control header with nothing in front of it.
//!
//! # Two headers, six bytes and twelve
//!
//! The **share control header** says how long the PDU is, which of four kinds it is,
//! and which MCS user sent it. Three of those kinds — Demand Active, Confirm Active,
//! Deactivate All — are whole PDUs in their own right. The fourth, a *data* PDU,
//! carries a second **share data header** naming one of some forty types, and that is
//! what nearly every PDU of a running session turns out to be.
//!
//! Both headers carry a length, and neither is the length of the same thing: the
//! control header's counts itself and everything after it, and a server may then pad
//! the MCS payload out past it. The declared length is what the PDU is, so that is
//! what [`decode`] hands back.

use super::wire::{Malformed, Reader, Writer};

/// `SEC_INFO_PKT`: this PDU is the Client Info PDU.
pub const INFO_PACKET: u16 = 0x0040;

/// `SEC_LICENSE_PKT`: this PDU belongs to the licensing exchange.
pub const LICENSE_PACKET: u16 = 0x0080;

/// Flags, and a second half that means nothing unless a flag says otherwise — which
/// for this client it never does, so it is written as zero and read past.
pub fn write_security_header(w: &mut Writer, flags: u16) {
    w.u16_le(flags);
    w.u16_le(0);
}

/// The flags of a security header, leaving the reader on the PDU itself.
pub fn read_security_header(r: &mut Reader<'_>) -> Result<u16, Malformed> {
    let flags = r.u16_le()?;
    r.skip(2)?;
    Ok(flags)
}

/// `TS_PROTOCOL_VERSION`, which occupies the type field above its bottom four bits
/// and has never been anything else.
const VERSION: u16 = 0x0010;

/// The bottom four bits of the type field, which are the type.
const TYPE: u16 = 0x000F;

/// Share control PDU types.
const DEMAND_ACTIVE: u16 = 1;
const CONFIRM_ACTIVE: u16 = 3;
const DEACTIVATE_ALL: u16 = 6;
const DATA: u16 = 7;

/// totalLength, pduType, pduSource.
const CONTROL_HEADER: usize = 6;

/// shareId, a pad byte, streamId, uncompressedLength, pduType2, compressedType,
/// compressedLength.
const DATA_HEADER: usize = 12;

/// `STREAM_LOW`. The priority is advisory, and RDP has never had more than one queue
/// for it to choose between.
const STREAM_LOW: u8 = 1;

/// The top half of the compression byte, where the flags live. Any of them set means
/// the body is compressed, which a server may not do unless the Client Info PDU asked
/// for it — and [`super::info`] does not.
const COMPRESSED: u8 = 0xF0;

/// Share data PDU types this client knows by name. The rest arrive as numbers and are
/// refused where they arrive, so that a PDU is never silently dropped.
pub const CONTROL: u8 = 0x14;
pub const SYNCHRONIZE: u8 = 0x1F;
pub const SAVE_SESSION_INFO: u8 = 0x26;
pub const FONT_LIST: u8 = 0x27;
pub const FONT_MAP: u8 = 0x28;
pub const SET_ERROR_INFO: u8 = 0x2F;
pub const MONITOR_LAYOUT: u8 = 0x37;

/// One PDU off the I/O channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pdu<'a> {
    /// The server's capabilities and the share identifier every later PDU names. See
    /// [`super::capabilities`].
    DemandActive(&'a [u8]),
    /// The share is being torn down. Either a Demand Active follows and the session
    /// is rebuilt around a new size, or the connection is ending.
    DeactivateAll,
    Data(Data<'a>),
}

/// A share *data* PDU: the kind almost everything is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Data<'a> {
    /// The share this PDU belongs to, which the server named in its Demand Active.
    pub share_id: u32,
    /// `pduType2`, one of the constants above.
    pub kind: u8,
    pub body: &'a [u8],
}

/// A Confirm Active PDU, which is the only share control PDU this client sends.
pub fn confirm_active(user: u16, body: &[u8]) -> Vec<u8> {
    let total = CONTROL_HEADER + body.len();
    let mut w = Writer::with_capacity(total);
    control_header(&mut w, CONFIRM_ACTIVE, user, total);
    w.bytes(body);
    w.finish()
}

/// A share data PDU of type `kind`.
pub fn data(kind: u8, user: u16, share_id: u32, body: &[u8]) -> Vec<u8> {
    let total = CONTROL_HEADER + DATA_HEADER + body.len();
    let mut w = Writer::with_capacity(total);
    control_header(&mut w, DATA, user, total);
    w.u32_le(share_id);
    w.u8(0);
    w.u8(STREAM_LOW);
    // The uncompressed length is the whole PDU, both headers included. Nothing reads
    // it while the body is uncompressed, which for this client is always.
    w.u16_le(announced(total));
    w.u8(kind);
    // Neither compressed nor, therefore, compressed by any particular method, and
    // with no compressed length to give.
    w.u8(0);
    w.u16_le(0);
    w.bytes(body);
    w.finish()
}

/// What arrived on the I/O channel, given the payload of a Send Data Indication.
pub fn decode(payload: &[u8]) -> Result<Pdu<'_>, Malformed> {
    const WHAT: &str = "a share control header";

    let mut r = Reader::new(WHAT, payload);
    let total = r.u16_le()?;
    let kind = r.u16_le()?;
    let _source = r.u16_le()?;
    if kind & !TYPE != VERSION {
        return Err(r.refuse("its version", kind & !TYPE));
    }
    let body = match usize::from(total).checked_sub(CONTROL_HEADER) {
        Some(length) => r.bytes(length)?,
        None => return Err(r.refuse("its length", total)),
    };

    match kind & TYPE {
        DEMAND_ACTIVE => Ok(Pdu::DemandActive(body)),
        // The PDU carries a source descriptor this client has no use for: a server
        // deactivating a share says nothing about why in it.
        DEACTIVATE_ALL => Ok(Pdu::DeactivateAll),
        DATA => Ok(Pdu::Data(share_data(body)?)),
        other => Err(r.refuse("its type", other)),
    }
}

fn share_data(body: &[u8]) -> Result<Data<'_>, Malformed> {
    const WHAT: &str = "a share data header";

    let mut r = Reader::new(WHAT, body);
    let share_id = r.u32_le()?;
    r.skip(1)?;
    let _stream = r.u8()?;
    // The uncompressed length, which says nothing the control header did not.
    r.skip(2)?;
    let kind = r.u8()?;
    let compression = r.u8()?;
    if compression & COMPRESSED != 0 {
        return Err(r.refuse("its compression flags", compression));
    }
    r.skip(2)?;
    Ok(Data { share_id, kind, body: r.rest() })
}

fn control_header(w: &mut Writer, kind: u16, source: u16, total: usize) {
    w.u16_le(announced(total));
    w.u16_le(VERSION | kind);
    w.u16_le(source);
}

fn announced(total: usize) -> u16 {
    u16::try_from(total).expect("a PDU this client sends is a small fraction of a TPKT frame")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_data_pdu_comes_back_as_the_share_and_type_it_went_out_as() {
        let frame = data(SYNCHRONIZE, 1007, 0x0001_0021, &[0x01, 0x00, 0xEF, 0x03]);
        assert_eq!(frame.len(), 6 + 12 + 4);
        // The length appears twice, and both mean the whole PDU.
        assert_eq!(&frame[..2], &22_u16.to_le_bytes());
        assert_eq!(&frame[12..14], &22_u16.to_le_bytes());
        // The type field is the type above the version, and the source is the user.
        assert_eq!(&frame[2..6], &[0x17, 0x00, 0xEF, 0x03]);

        assert_eq!(
            decode(&frame).unwrap(),
            Pdu::Data(Data {
                share_id: 0x0001_0021,
                kind: SYNCHRONIZE,
                body: &[0x01, 0x00, 0xEF, 0x03],
            })
        );
    }

    #[test]
    fn a_confirm_active_carries_the_body_and_nothing_of_its_own() {
        let frame = confirm_active(1007, b"capabilities");
        assert_eq!(&frame[..6], &[0x12, 0x00, 0x13, 0x00, 0xEF, 0x03]);
        assert_eq!(&frame[6..], b"capabilities");
    }

    #[test]
    fn the_declared_length_bounds_the_body_rather_than_the_buffer() {
        // Windows pads a PDU out past its own length. The padding is not the body,
        // and reading it as body would put a decoder four bytes into nothing.
        let mut frame = data(FONT_MAP, 1007, 1, &[0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x00]);
        frame.extend_from_slice(&[0xFF; 4]);
        let Pdu::Data(pdu) = decode(&frame).unwrap() else { panic!("a data PDU") };
        assert_eq!(pdu.body.len(), 8);
    }

    #[test]
    fn a_deactivate_all_is_recognised_without_its_descriptor() {
        let frame = [0x09, 0x00, 0x16, 0x00, 0xEA, 0x03, 0x01, 0x00, 0x00];
        assert_eq!(decode(&frame).unwrap(), Pdu::DeactivateAll);
    }

    #[test]
    fn a_compressed_body_is_refused_rather_than_handed_on_as_itself() {
        // The server may only compress if the Client Info PDU asked it to, so a
        // compressed body means the two ends disagree about the session.
        let mut frame = data(SAVE_SESSION_INFO, 1007, 1, &[0xAB; 16]);
        frame[15] = 0x20 | 0x02;
        assert_eq!(
            decode(&frame).unwrap_err().to_string(),
            "a share data header carries its compression flags 0x22, which this client does not \
             accept"
        );
    }

    #[test]
    fn an_unknown_share_control_type_names_itself() {
        // A Server Redirection Packet, which this client does not follow.
        let frame = [0x06, 0x00, 0x1A, 0x00, 0xEA, 0x03];
        assert_eq!(
            decode(&frame).unwrap_err().to_string(),
            "a share control header carries its type 0xa, which this client does not accept"
        );
    }

    #[test]
    fn a_length_shorter_than_the_header_it_is_in_is_refused() {
        let frame = [0x04, 0x00, 0x17, 0x00, 0xEA, 0x03, 0x00, 0x00];
        assert_eq!(
            decode(&frame).unwrap_err().to_string(),
            "a share control header carries its length 0x4, which this client does not accept"
        );
    }
}
