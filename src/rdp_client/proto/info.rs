//! The Client Info PDU: who is logging on, and what the session should look like.
//!
//! This is the first PDU of the session proper, and the point at which the connection
//! stops describing itself and starts describing a user. It goes out on the I/O
//! channel as soon as the channels are open, and the server answers it with the
//! licensing PDU of [`super::license`].
//!
//! # Credentials, again
//!
//! The user name and password are in here even though CredSSP has already proved them
//! ([`super::credssp`]). That is not a second authentication: the server has the
//! credentials from CredSSP and uses them, and what this PDU adds is `INFO_AUTOLOGON`
//! — *use them* rather than showing a logon screen with the name filled in. A client
//! that omits them reaches the desktop only after someone types the password again.
//!
//! # What the session is told to be
//!
//! Exactly what mstsc tells a server on a LAN: nothing of the desktop is turned off,
//! and font smoothing and desktop composition are turned on. The session looks the
//! way the user set it up to look, and the same as it does from Microsoft's own
//! client. Audio playback and video redirection are refused outright, because this
//! client has nowhere to put them.

use std::net::IpAddr;

use super::share;
use super::wire::Writer;
use super::x224::TooLong;

/// `INFO_*`, the flags that describe the logon:
///
/// - `0x0001` there is a mouse.
/// - `0x0002` no secure attention sequence is needed to log on, which is what makes
///   an automatic logon possible at all.
/// - `0x0008` log on with the credentials in this PDU rather than prompting.
/// - `0x0010` every string in this PDU is UTF-16.
/// - `0x0020` open the shell maximized.
/// - `0x0040` tell the client when the logon has finished.
/// - `0x0100` the Windows key is the remote session's, not the local machine's.
/// - `0x0001_0000` report a failed logon rather than just disconnecting.
/// - `0x0002_0000` the mouse has a wheel.
/// - `0x0008_0000` do not redirect audio — said only for a target that asked for no
///   sound. With it set the session has no audio device to redirect at all, so a
///   target that did ask leaves it out.
/// - `0x0040_0000` do not redirect video either.
///
/// Bulk compression is deliberately absent. It is a second decompressor on every PDU
/// to save bytes on a link that is a LAN, and a compressed PDU this client cannot
/// unpack is a dead session rather than a slow one.
const FLAGS: u32 = 0x0000_0001
    | 0x0000_0002
    | 0x0000_0008
    | 0x0000_0010
    | 0x0000_0020
    | 0x0000_0040
    | 0x0000_0100
    | 0x0001_0000
    | 0x0002_0000
    | 0x0040_0000;

/// `INFO_NOAUDIOPLAYBACK`.
const NO_AUDIO: u32 = 0x0008_0000;

/// `PERF_ENABLE_FONT_SMOOTHING` and `PERF_ENABLE_DESKTOP_COMPOSITION`, and none of
/// the `PERF_DISABLE_*` flags. Font smoothing is off unless it is asked for, so
/// leaving this zero would not leave the session alone either. See the module docs.
const PERFORMANCE: u32 = 0x0000_0080 | 0x0000_0100;

/// `clientDir`. The field is meant to be where the client is installed, and a Windows
/// server logs it; mstsc's value is what every server has always been told, and there
/// is nothing truthful to put here instead.
const CLIENT_DIR: &str = "C:\\Windows\\System32\\mstscax.dll";

/// `AF_INET` and `AF_INET6`, as the field spells them.
const ADDRESS_INET: u16 = 0x0002;
const ADDRESS_INET6: u16 = 0x0017;

/// `TS_TIME_ZONE_INFORMATION`: a bias, two names of 64 bytes, two transition dates and
/// two more biases. All zero — the session takes the server's own time zone, which is
/// the right answer for a gateway that is not where the user is.
const TIME_ZONE: usize = 172;

/// Who is logging on, and from where.
pub struct ClientInfo<'a> {
    pub username: &'a str,
    pub password: &'a str,
    /// The NetBIOS domain, when the target names one.
    pub domain: Option<&'a str>,
    /// This end of the socket. The server shows it in its own session list and does
    /// nothing else with it: it is not an identity and not a route back.
    pub address: IpAddr,
    /// Whether the target asked for the remote's sound. Without it the logon says
    /// there is nothing here to play audio, and the host redirects none.
    pub audio: bool,
}

impl ClientInfo<'_> {
    /// The whole PDU, security header included, ready for an MCS Send Data Request.
    pub fn encode(&self) -> Result<Vec<u8>, TooLong> {
        // No alternate shell and no working directory: this client opens a desktop,
        // not a program inside one.
        let domain = utf16(self.domain.unwrap_or(""));
        let username = utf16(self.username);
        let password = utf16(self.password);
        let address = utf16(&self.address.to_string());
        let directory = utf16(CLIENT_DIR);

        let total = 4 + 4 + 4 + 5 * 2 + 5 * 2
            + domain.len() + username.len() + password.len()
            + 2 + 2 + address.len() + 2
            + 2 + directory.len() + 2
            + TIME_ZONE + 4 + 4;
        if u16::try_from(total).is_err() {
            return Err(TooLong(total));
        }

        let mut w = Writer::with_capacity(total);
        share::write_security_header(&mut w, share::INFO_PACKET);
        // The code page is read only when the GCC client core data asked for a
        // keyboard layout of zero, and it did not.
        w.u32_le(0);
        w.u32_le(if self.audio { FLAGS } else { FLAGS | NO_AUDIO });
        // Five lengths, then the five strings. A length counts the characters and
        // not the terminator that follows them; the two after the strings count the
        // terminator, because the fields they measure are allowed to be absent and a
        // length of zero has to mean something different from an empty string.
        w.u16_le(length(&domain));
        w.u16_le(length(&username));
        w.u16_le(length(&password));
        w.u16_le(0);
        w.u16_le(0);
        terminated(&mut w, &domain);
        terminated(&mut w, &username);
        terminated(&mut w, &password);
        terminated(&mut w, &[]);
        terminated(&mut w, &[]);

        w.u16_le(match self.address {
            IpAddr::V4(_) => ADDRESS_INET,
            IpAddr::V6(_) => ADDRESS_INET6,
        });
        w.u16_le(length(&address) + 2);
        terminated(&mut w, &address);
        w.u16_le(length(&directory) + 2);
        terminated(&mut w, &directory);

        w.zeros(TIME_ZONE);
        // The session identifier is the one being reconnected to, and nothing is.
        w.u32_le(0);
        w.u32_le(PERFORMANCE);
        // The fields after this — an auto-reconnect cookie, and the reserved pair
        // after it — are optional and a server reads them only if they are there.
        Ok(w.finish())
    }
}

/// A string as this PDU carries it: UTF-16, little-endian, without a terminator.
fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn terminated(w: &mut Writer, text: &[u8]) {
    w.bytes(text);
    w.u16_le(0);
}

fn length(text: &[u8]) -> u16 {
    u16::try_from(text.len()).expect("the whole PDU was measured against a 16-bit length first")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logon() -> ClientInfo<'static> {
        ClientInfo {
            username: "andrew",
            password: "secret",
            domain: None,
            address: IpAddr::from([10, 0, 0, 2]),
            audio: false,
        }
    }

    /// A target that asked for sound leaves `INFO_NOAUDIOPLAYBACK` out; one that did
    /// not says it, so the host redirects nothing.
    #[test]
    fn the_no_audio_flag_follows_whether_sound_was_asked_for() {
        let flags = |audio: bool| {
            let bytes = ClientInfo { audio, ..logon() }.encode().unwrap();
            u32::from_le_bytes(bytes[8..12].try_into().unwrap())
        };
        assert_eq!(flags(false) & NO_AUDIO, NO_AUDIO);
        assert_eq!(flags(true) & NO_AUDIO, 0);
    }

    #[test]
    fn the_pdu_says_it_is_the_logon_and_asks_to_be_logged_on() {
        let bytes = logon().encode().unwrap();
        assert_eq!(&bytes[..4], &[0x40, 0x00, 0x00, 0x00]);
        assert_eq!(&bytes[4..8], &[0x00; 4]);
        let flags = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert_eq!(flags & 0x0000_0008, 0x0000_0008, "INFO_AUTOLOGON");
        assert_eq!(flags & 0x0000_0010, 0x0000_0010, "INFO_UNICODE");
        assert_eq!(flags & 0x0000_0080, 0, "no bulk compression is asked for");
    }

    #[test]
    fn a_name_is_measured_without_its_terminator_and_written_with_one() {
        let bytes = logon().encode().unwrap();
        // Domain, user name, password, alternate shell, working directory.
        assert_eq!(&bytes[12..22], &[0, 0, 12, 0, 12, 0, 0, 0, 0, 0]);
        // Then the same five, each terminated: an empty domain is two bytes.
        assert_eq!(&bytes[22..24], &[0, 0]);
        assert_eq!(&bytes[24..36], &utf16("andrew")[..]);
        assert_eq!(&bytes[36..38], &[0, 0]);
    }

    #[test]
    fn an_address_is_measured_with_its_terminator_because_absent_is_not_empty() {
        let bytes = logon().encode().unwrap();
        // Past the five strings: 24 + 12 + 2 + 12 + 2 + 2 + 2.
        let at = 24 + 12 + 2 + 12 + 2 + 2 + 2;
        assert_eq!(&bytes[at..at + 2], &ADDRESS_INET.to_le_bytes());
        let announced = u16::from_le_bytes(bytes[at + 2..at + 4].try_into().unwrap());
        assert_eq!(usize::from(announced), utf16("10.0.0.2").len() + 2);
    }

    #[test]
    fn an_ipv6_client_says_so_in_the_family_rather_than_in_the_address() {
        let info = ClientInfo { address: IpAddr::from([0xfd, 0, 0, 0, 0, 0, 0, 1]), ..logon() };
        let bytes = info.encode().unwrap();
        assert!(
            bytes.windows(2).any(|pair| pair == ADDRESS_INET6.to_le_bytes()),
            "the family field says INET6"
        );
    }

    #[test]
    fn the_session_looks_the_way_mstsc_would_leave_it() {
        let bytes = logon().encode().unwrap();
        let performance = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap());
        assert_eq!(performance, 0x180, "font smoothing and composition on, nothing disabled");
        // And the time zone before it is the server's, not ours.
        let zone = bytes.len() - 4 - 4 - TIME_ZONE;
        assert!(bytes[zone..zone + TIME_ZONE].iter().all(|byte| *byte == 0));
    }
}
