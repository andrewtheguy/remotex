//! The four PDUs between a confirmed share and a desktop.
//!
//! Once the capability exchange of [`super::capabilities`] is done, neither side has
//! sent a pixel. What stands between is a fixed handshake, called connection
//! finalization in [MS-RDPBCGR], in which the client sends four small share data PDUs
//! and the server answers with four of its own:
//!
//! 1. **Synchronize** — nothing of substance; it marks the point both sides count
//!    from.
//! 2. **Control (Cooperate)** — this client will share the desktop rather than take
//!    it exclusively.
//! 3. **Control (Request Control)** — and would like to drive it.
//! 4. **Font List** — historically the client's fonts, empty for twenty years, and
//!    still the PDU that means *I am ready*.
//!
//! The server's Font Map is the answer to the last of them, and the moment the share
//! is live: the first update follows it without further ceremony.
//!
//! The four go out together without waiting, because none of them is a question. The
//! server's four come back in its own time, with session PDUs mixed in among them —
//! a logon notification, a monitor layout — which is why the reader here takes a PDU
//! at a time and says what it was rather than expecting a fixed order.

use super::share::{self, Data};
use super::wire::{Malformed, Reader, Writer};

/// `SYNCMSGTYPE_SYNC`, the only message type a Synchronize PDU has.
const SYNCHRONIZE_MESSAGE: u16 = 1;

/// `CTRLACTION_*`.
const REQUEST_CONTROL: u16 = 1;
const GRANTED_CONTROL: u16 = 2;
const COOPERATE: u16 = 4;

/// `FONTLIST_FIRST | FONTLIST_LAST`: this is the whole list, such as it is.
const WHOLE_LIST: u16 = 0x0001 | 0x0002;

/// `entrySize`, which describes entries this client does not send.
const FONT_ENTRY: u16 = 0x0032;

/// What one server PDU during this handshake turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response {
    /// The server has synchronized.
    Synchronize,
    /// The server is sharing rather than handing over.
    Cooperate,
    /// This client may drive the desktop.
    ControlGranted,
    /// The last of the four: the share is live, and updates follow.
    FontMap,
    /// A PDU of the session rather than of this handshake — who logged on, how the
    /// monitors are arranged — which arrives here because the server does not wait
    /// for the handshake to finish before starting the session.
    Aside,
}

/// The four PDUs this client sends, in order, each already framed as a share data
/// PDU on the I/O channel.
pub fn requests(user: u16, share_id: u32) -> [Vec<u8>; 4] {
    [
        share::data(share::SYNCHRONIZE, user, share_id, &synchronize(user)),
        share::data(share::CONTROL, user, share_id, &control(COOPERATE)),
        share::data(share::CONTROL, user, share_id, &control(REQUEST_CONTROL)),
        share::data(share::FONT_LIST, user, share_id, &font_list()),
    ]
}

/// What one of the server's PDUs was.
///
/// A Set Error Info PDU carrying a real code becomes the error here, because that PDU
/// *is* the server's explanation and there is no better place to turn it into one.
pub fn response(pdu: &Data<'_>) -> Result<Response, Malformed> {
    match pdu.kind {
        share::SYNCHRONIZE => Ok(Response::Synchronize),
        share::CONTROL => {
            let mut r = Reader::new("an RDP Control PDU", pdu.body);
            match r.u16_le()? {
                COOPERATE => Ok(Response::Cooperate),
                GRANTED_CONTROL => Ok(Response::ControlGranted),
                other => Err(r.refuse("its action", other)),
            }
        }
        share::FONT_MAP => Ok(Response::FontMap),
        share::SET_ERROR_INFO => {
            const WHAT: &str = "the server ended the session";
            let mut r = Reader::new(WHAT, pdu.body);
            match r.u32_le()? {
                0 => Ok(Response::Aside),
                code => Err(r.report(describe(code), code)),
            }
        }
        share::SAVE_SESSION_INFO | share::MONITOR_LAYOUT => Ok(Response::Aside),
        other => {
            let r = Reader::new("a share data PDU during connection finalization", pdu.body);
            Err(r.refuse("its type", other))
        }
    }
}

/// What a Set Error Info code means, for the codes that do not depend on which
/// protocol was in use. The rest are numbered and left that way: a name invented for
/// a code is worse than the code.
fn describe(code: u32) -> &'static str {
    match code {
        0x0000_0001 => "an administrator disconnected the session",
        0x0000_0002 => "an administrator logged the session off",
        0x0000_0003 => "the session was idle for longer than the host allows",
        0x0000_0004 => "the logon took longer than the host allows",
        0x0000_0005 => "another connection took the session over",
        0x0000_0006 => "the host ran out of memory",
        0x0000_0007 => "the host denied the connection",
        0x0000_0009 => "the account is not allowed to log on to this host",
        0x0000_000A => "the host wants credentials entered at its own logon screen",
        0x0000_000B => "the user disconnected the session",
        0x0000_000C => "the user logged the session off",
        0x0000_0010 => "the host's desktop compositor stopped",
        0x0000_0017 => "the host's logon process stopped",
        0x0000_0018 => "the host's session manager stopped",
        _ => "for a reason this client has no name for",
    }
}

fn synchronize(user: u16) -> Vec<u8> {
    let mut w = Writer::with_capacity(4);
    w.u16_le(SYNCHRONIZE_MESSAGE);
    // The target is this client's own user channel. The field exists because MCS
    // conferences can have more than one user; an RDP session never does.
    w.u16_le(user);
    w.finish()
}

fn control(action: u16) -> Vec<u8> {
    let mut w = Writer::with_capacity(8);
    w.u16_le(action);
    // grantId and controlId, which only a server fills in.
    w.u16_le(0);
    w.u32_le(0);
    w.finish()
}

fn font_list() -> Vec<u8> {
    let mut w = Writer::with_capacity(8);
    // No fonts, of no fonts in total.
    w.u16_le(0);
    w.u16_le(0);
    w.u16_le(WHOLE_LIST);
    w.u16_le(FONT_ENTRY);
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(kind: u8, body: &[u8]) -> Vec<u8> {
        share::data(kind, 0x03EA, 0x0001_0021, body)
    }

    fn decoded(frame: &[u8]) -> Data<'_> {
        match share::decode(frame).unwrap() {
            share::Pdu::Data(data) => data,
            other => panic!("a data PDU, not {other:?}"),
        }
    }

    #[test]
    fn the_four_requests_go_out_in_the_order_the_handshake_defines() {
        let pdus = requests(1007, 0x0001_0021);
        let kinds: Vec<_> = pdus.iter().map(|pdu| decoded(pdu).kind).collect();
        assert_eq!(kinds, vec![
            share::SYNCHRONIZE,
            share::CONTROL,
            share::CONTROL,
            share::FONT_LIST
        ]);
        // The two Control PDUs differ only in their action, and the order matters:
        // cooperating before asking to drive is what a server expects.
        assert_eq!(&decoded(&pdus[1]).body[..2], &COOPERATE.to_le_bytes());
        assert_eq!(&decoded(&pdus[2]).body[..2], &REQUEST_CONTROL.to_le_bytes());
    }

    #[test]
    fn a_synchronize_names_this_clients_own_channel() {
        let pdus = requests(1007, 1);
        assert_eq!(decoded(&pdus[0]).body, &[0x01, 0x00, 0xEF, 0x03]);
    }

    #[test]
    fn the_font_list_says_it_is_the_whole_list_of_nothing() {
        let pdus = requests(1007, 1);
        assert_eq!(decoded(&pdus[3]).body, &[0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x32, 0x00]);
    }

    #[test]
    fn each_answer_is_recognised_by_what_it_is_rather_than_by_when_it_came() {
        let sync = answer(share::SYNCHRONIZE, &[0x01, 0x00, 0xEA, 0x03]);
        assert_eq!(response(&decoded(&sync)).unwrap(), Response::Synchronize);

        let cooperate = answer(share::CONTROL, &[0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(response(&decoded(&cooperate)).unwrap(), Response::Cooperate);

        let granted = answer(share::CONTROL, &[0x02, 0x00, 0xEF, 0x03, 0xEA, 0x03, 0x00, 0x00]);
        assert_eq!(response(&decoded(&granted)).unwrap(), Response::ControlGranted);

        let map = answer(share::FONT_MAP, &[0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00]);
        assert_eq!(response(&decoded(&map)).unwrap(), Response::FontMap);
    }

    #[test]
    fn a_logon_notification_is_stepped_past_rather_than_being_a_wrong_answer() {
        let session = answer(share::SAVE_SESSION_INFO, &[0x00; 20]);
        assert_eq!(response(&decoded(&session)).unwrap(), Response::Aside);
        // As is a Set Error Info that reports no error, which Windows does send.
        let quiet = answer(share::SET_ERROR_INFO, &[0x00, 0x00, 0x00, 0x00]);
        assert_eq!(response(&decoded(&quiet)).unwrap(), Response::Aside);
    }

    #[test]
    fn a_refused_logon_becomes_the_sentence_the_server_sent_rather_than_a_timeout() {
        let denied = answer(share::SET_ERROR_INFO, &0x0000_0009_u32.to_le_bytes());
        assert_eq!(
            response(&decoded(&denied)).unwrap_err().to_string(),
            "the server ended the session: the account is not allowed to log on to this host \
             (0x00000009)"
        );

        let unknown = answer(share::SET_ERROR_INFO, &0x0000_10CA_u32.to_le_bytes());
        assert_eq!(
            response(&decoded(&unknown)).unwrap_err().to_string(),
            "the server ended the session: for a reason this client has no name for (0x000010ca)"
        );
    }

    #[test]
    fn a_pdu_this_handshake_has_no_place_for_names_its_type() {
        let update = answer(0x02, &[0x00; 8]);
        assert_eq!(
            response(&decoded(&update)).unwrap_err().to_string(),
            "a share data PDU during connection finalization carries its type 0x2, which this \
             client does not accept"
        );
    }
}
