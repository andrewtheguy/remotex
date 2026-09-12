//! Device redirection (MS-RDPEFS), with no devices in it.
//!
//! A Windows host redirects no sound for a client that has not also named the
//! `rdpdr` channel — FreeRDP's own client turns device redirection on whenever
//! sound is asked for, with the comment "rdpsnd requires rdpdr to be registered",
//! and this client measured the same: a session naming `rdpsnd` alone is numbered a
//! channel the host never speaks on. So this is the channel's opening handshake and
//! nothing after it. The server announces itself; the client confirms, gives its
//! name, and answers the capability exchange with the general set alone. With no
//! devices there is no device list to announce and no I/O request will ever arrive.
//!
//! [MS-RDPEFS] 2.2.2, ported against FreeRDP's `channels/rdpdr/client`.
//!
//! [MS-RDPEFS]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpefs/34d9de58-b2b5-40b6-b970-f82d4603bdb5

use log::debug;

use super::wire::{Malformed, Reader, Writer};

const WHAT: &str = "a device redirection PDU";

/// `RDPDR_CTYP_CORE`: the component every PDU here belongs to.
const CORE: u16 = 0x4472;

const SERVER_ANNOUNCE: u16 = 0x496E;
const CLIENTID_CONFIRM: u16 = 0x4343;
const CLIENT_NAME: u16 = 0x434E;
const SERVER_CAPABILITY: u16 = 0x5350;
const CLIENT_CAPABILITY: u16 = 0x4350;
const USER_LOGGEDON: u16 = 0x554C;

/// The protocol this client speaks: major 1, minor 13 — RDP 10's — or the server's
/// if lower.
const VERSION_MAJOR: u16 = 0x0001;
const VERSION_MINOR: u16 = 0x000D;

const CAP_GENERAL: u16 = 0x0001;
const GENERAL_VERSION_2: u32 = 0x0000_0002;
/// A general capability set: the eight-byte header and thirty-six of fields.
const GENERAL_LENGTH: u16 = 8 + 36;
/// Every `RDPDR_IRP_MJ_*` request, as FreeRDP claims them — moot with no device to
/// send one to, but what a client of this version says.
const IO_CODES: u32 = 0x0000_FFFF;
/// `RDPDR_DEVICE_REMOVE_PDUS | RDPDR_CLIENT_DISPLAY_NAME_PDU | RDPDR_USER_LOGGEDON_PDU`.
const EXTENDED_PDUS: u32 = 0x0000_0007;
const ENABLE_ASYNCIO: u32 = 0x0000_0001;

/// What this end calls itself in the Client Name Request. Shown nowhere a person
/// looks, since nothing is redirected under it.
const COMPUTER_NAME: &str = "remotex";

/// The client's side of the handshake.
#[derive(Debug, Default)]
pub struct Rdpdr {
    /// The version agreed with the server, once it has announced its own.
    version: Option<(u16, u16)>,
    client_id: u32,
}

impl Rdpdr {
    pub fn new() -> Self {
        Self::default()
    }

    /// One whole PDU from the server: what to send back, in order.
    pub fn push(&mut self, pdu: &[u8]) -> Result<Vec<Vec<u8>>, Malformed> {
        let mut r = Reader::new(WHAT, pdu);
        let component = r.u16_le()?;
        let packet = r.u16_le()?;
        if component != CORE {
            debug!("rdp: ignoring a device redirection PDU for component {component:#06x}");
            return Ok(Vec::new());
        }
        debug!("rdp: device redirection PDU {packet:#06x}, {} bytes", pdu.len());
        match packet {
            SERVER_ANNOUNCE => {
                let major = r.u16_le()?;
                let minor = r.u16_le()?;
                self.client_id = r.u32_le()?;
                let version = (major.min(VERSION_MAJOR), minor.min(VERSION_MINOR));
                self.version = Some(version);
                debug!("rdp: the host announces device redirection {major}.{minor}; answering as {}.{}", version.0, version.1);
                Ok(vec![self.clientid_confirm(version), client_name()])
            }
            SERVER_CAPABILITY => {
                let count = r.u16_le()?;
                r.u16_le()?; // Padding
                let mut io_codes = IO_CODES;
                for _ in 0..count {
                    let kind = r.u16_le()?;
                    let length = r.u16_le()?;
                    let version = r.u32_le()?;
                    let Some(body) = length.checked_sub(8) else {
                        return Err(Malformed::Refused { what: WHAT, field: "a capability length", value: u64::from(length) });
                    };
                    let mut body = Reader::new(WHAT, r.bytes(usize::from(body))?);
                    if kind == CAP_GENERAL {
                        body.u32_le()?; // osType
                        body.u32_le()?; // osVersion
                        body.u16_le()?; // protocolMajorVersion
                        body.u16_le()?; // protocolMinorVersion
                        io_codes &= body.u32_le()?;
                    }
                    let _ = version;
                }
                let version = self.version.unwrap_or((VERSION_MAJOR, VERSION_MINOR));
                Ok(vec![client_capability(version, io_codes)])
            }
            CLIENTID_CONFIRM => {
                let major = r.u16_le()?;
                let minor = r.u16_le()?;
                self.client_id = r.u32_le()?;
                self.version = Some((major, minor));
                // With no devices there is no list to announce, so the handshake
                // ends here, as FreeRDP's does with nothing to redirect.
                Ok(Vec::new())
            }
            USER_LOGGEDON => Ok(Vec::new()),
            other => {
                debug!("rdp: ignoring a device redirection PDU of type {other:#06x}");
                Ok(Vec::new())
            }
        }
    }

    fn clientid_confirm(&self, version: (u16, u16)) -> Vec<u8> {
        let mut w = Writer::with_capacity(12);
        w.u16_le(CORE);
        w.u16_le(CLIENTID_CONFIRM);
        w.u16_le(version.0);
        w.u16_le(version.1);
        w.u32_le(self.client_id);
        w.finish()
    }
}

/// Client Name Request: this end's name, UTF-16 with its terminator.
fn client_name() -> Vec<u8> {
    let name: Vec<u8> = COMPUTER_NAME.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
    let mut w = Writer::with_capacity(16 + name.len());
    w.u16_le(CORE);
    w.u16_le(CLIENT_NAME);
    w.u32_le(1); // UnicodeFlag
    w.u32_le(0); // CodePage
    w.u32_le(name.len() as u32);
    w.bytes(&name);
    w.finish()
}

/// Client Core Capability Response: the general capability set, and no other, since
/// there is no device type here to describe.
fn client_capability(version: (u16, u16), io_codes: u32) -> Vec<u8> {
    let mut w = Writer::with_capacity(8 + usize::from(GENERAL_LENGTH));
    w.u16_le(CORE);
    w.u16_le(CLIENT_CAPABILITY);
    w.u16_le(1); // numCapabilities
    w.u16_le(0); // Padding
    w.u16_le(CAP_GENERAL);
    w.u16_le(GENERAL_LENGTH);
    w.u32_le(GENERAL_VERSION_2);
    w.u32_le(0); // osType, ignored on receipt
    w.u32_le(0); // osVersion, must be zero
    w.u16_le(version.0);
    w.u16_le(version.1);
    w.u32_le(io_codes);
    w.u32_le(0); // ioCode2, must be zero
    w.u32_le(EXTENDED_PDUS);
    w.u32_le(ENABLE_ASYNCIO);
    w.u32_le(0); // extraFlags2, must be zero
    w.u32_le(0); // SpecialTypeDeviceCap: no special devices before logon
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(packet: u16, body: &[u8]) -> Vec<u8> {
        let mut v = CORE.to_le_bytes().to_vec();
        v.extend_from_slice(&packet.to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    /// The announce is answered with a confirm at the lower of the two versions and
    /// the client's name in UTF-16, terminator counted.
    #[test]
    fn the_announce_is_confirmed_at_the_lower_version_and_the_name_given() {
        let mut rdpdr = Rdpdr::new();
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&0x000Cu16.to_le_bytes()); // RDP 6.x's minor, below ours
        body.extend_from_slice(&0x0000_0007u32.to_le_bytes());
        let replies = rdpdr.push(&server(SERVER_ANNOUNCE, &body)).unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], [0x72, 0x44, 0x43, 0x43, 1, 0, 0x0C, 0, 7, 0, 0, 0]);
        let name = &replies[1];
        assert_eq!(&name[..4], &[0x72, 0x44, 0x4E, 0x43]);
        assert_eq!(&name[4..8], &1u32.to_le_bytes(), "Unicode");
        assert_eq!(&name[12..16], &16u32.to_le_bytes(), "seven characters and a terminator");
        assert_eq!(&name[16..18], b"r\0");
        assert_eq!(&name[28..30], b"x\0");
        assert_eq!(&name[30..], &[0, 0], "the terminator");
    }

    /// The capability request is answered with the general set alone, its I/O codes
    /// the intersection with the server's; a client id confirm ends the handshake
    /// with nothing to announce.
    #[test]
    fn capabilities_are_answered_with_the_general_set_alone() {
        let mut rdpdr = Rdpdr::new();
        let mut body = Vec::new();
        body.extend_from_slice(&2u16.to_le_bytes()); // numCapabilities
        body.extend_from_slice(&0u16.to_le_bytes());
        // General, version 2: osType, osVersion, 1.13, ioCode1 with two bits off.
        body.extend_from_slice(&CAP_GENERAL.to_le_bytes());
        body.extend_from_slice(&GENERAL_LENGTH.to_le_bytes());
        body.extend_from_slice(&GENERAL_VERSION_2.to_le_bytes());
        body.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0x0D, 0]);
        body.extend_from_slice(&0x0000_FFFCu32.to_le_bytes());
        body.extend_from_slice(&[0; 20]);
        // A drive set, skipped by its length.
        body.extend_from_slice(&[4, 0, 8, 0, 2, 0, 0, 0]);
        let replies = rdpdr.push(&server(SERVER_CAPABILITY, &body)).unwrap();
        assert_eq!(replies.len(), 1);
        let caps = &replies[0];
        assert_eq!(&caps[..8], &[0x72, 0x44, 0x50, 0x43, 1, 0, 0, 0]);
        assert_eq!(&caps[8..16], &[1, 0, 44, 0, 2, 0, 0, 0]);
        assert_eq!(&caps[28..32], &0x0000_FFFCu32.to_le_bytes(), "the server's codes and ours");
        assert_eq!(caps.len(), 8 + 44);

        let confirm = rdpdr.push(&server(CLIENTID_CONFIRM, &[1, 0, 0x0D, 0, 9, 0, 0, 0])).unwrap();
        assert!(confirm.is_empty());
        assert_eq!(rdpdr.client_id, 9);
    }

    /// Another component, an unknown packet, and a logged-on notice each earn nothing;
    /// a PDU cut short is an error.
    #[test]
    fn what_is_not_the_handshake_is_left_alone() {
        let mut rdpdr = Rdpdr::new();
        assert!(rdpdr.push(&[0x52, 0x50, 0x01, 0x00, 1, 2]).unwrap().is_empty(), "the printer component");
        assert!(rdpdr.push(&server(0x4952, &[0; 8])).unwrap().is_empty(), "an I/O request nobody asked for");
        assert!(rdpdr.push(&server(USER_LOGGEDON, &[])).unwrap().is_empty());
        assert!(rdpdr.push(&server(SERVER_ANNOUNCE, &[1, 0])).is_err());
    }
}
