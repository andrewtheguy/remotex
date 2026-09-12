//! What a client asks of a desktop that is already up, and the last thing a server
//! says about one.
//!
//! Three share data PDUs on the I/O channel, none of which belongs to a handshake:
//! they arrive, or go out, in the middle of a session that is otherwise nothing but
//! updates and input.
//!
//! # Asking for pixels again
//!
//! A client that has lost part of the desktop — a bitmap that would not decode, a
//! framebuffer just resized, a share that has only this moment gone live — has two
//! ways to ask for it back, and a server says in its General capability set which of
//! them it implements:
//!
//! - **Refresh Rect** names the rectangles to repaint. It is the PDU the protocol
//!   provides for exactly this.
//! - **Suppress Output** says whether to send updates at all. Turning updates off and
//!   on again makes a server paint the whole desktop, which is the documented
//!   workaround for a host that answers Refresh Rect by doing nothing.
//!
//! [`repaint`] prefers Suppress Output where a server offers it, for that reason.
//!
//! # The reason a session ended
//!
//! A server that is closing a session says why first, in a Set Error Info PDU — and
//! it sends the same PDU carrying zero to mean *nothing is wrong*, which a quiet
//! Windows session does from time to time. [`error_info`] is that distinction, and
//! the code becomes a sentence here because this PDU is the whole of the server's
//! explanation.
//!
//! [MS-RDPBCGR] 2.2.11.2.1, 2.2.11.3.1 and 2.2.5.1.1.

use super::share;
use super::wire::{Malformed, Reader, Writer};

/// `PDUTYPE2_REFRESH_RECT`.
const REFRESH_RECT: u8 = 0x21;

/// `PDUTYPE2_SUPPRESS_OUTPUT`.
const SUPPRESS_OUTPUT: u8 = 0x23;

/// `ALLOW_DISPLAY_UPDATES`, beside which a rectangle says what to paint. Its
/// opposite, `SUPPRESS_DISPLAY_UPDATES`, is zero and carries no rectangle at all.
const ALLOW: u8 = 0x01;

/// The PDUs that ask a server to paint the whole desktop again, in the order they
/// must be sent — which is none at all for a server that offers neither way.
///
/// `width` and `height` are the desktop's, and the rectangle sent is all of it.
pub fn repaint(
    user: u16,
    share_id: u32,
    width: u16,
    height: u16,
    refresh_rect: bool,
    suppress_output: bool,
) -> Vec<Vec<u8>> {
    if suppress_output {
        return vec![
            suppress(user, share_id, None),
            suppress(user, share_id, Some((width, height))),
        ];
    }
    if refresh_rect {
        return vec![refresh(user, share_id, width, height)];
    }
    Vec::new()
}

/// A Refresh Rect PDU naming the whole desktop.
fn refresh(user: u16, share_id: u32, width: u16, height: u16) -> Vec<u8> {
    let mut w = Writer::with_capacity(12);
    w.u8(1); // numberOfAreas
    w.zeros(3);
    rectangle(&mut w, width, height);
    share::data(REFRESH_RECT, user, share_id, &w.finish())
}

/// A Suppress Output PDU. `Some` turns updates back on for that desktop; `None`
/// turns them off.
fn suppress(user: u16, share_id: u32, desktop: Option<(u16, u16)>) -> Vec<u8> {
    let mut w = Writer::with_capacity(12);
    w.u8(if desktop.is_some() { ALLOW } else { 0 });
    w.zeros(3);
    if let Some((width, height)) = desktop {
        rectangle(&mut w, width, height);
    }
    share::data(SUPPRESS_OUTPUT, user, share_id, &w.finish())
}

/// `TS_RECTANGLE16` covering a desktop of this size. Its edges are *inclusive*, so a
/// desktop is one short of its own width — and an empty one would have to name a
/// negative edge, which is why a zero dimension stays zero.
fn rectangle(w: &mut Writer, width: u16, height: u16) {
    w.u16_le(0); // left
    w.u16_le(0); // top
    w.u16_le(width.saturating_sub(1));
    w.u16_le(height.saturating_sub(1));
}

/// Read a Set Error Info PDU. `Ok(())` is the server saying nothing is wrong.
///
/// Anything else is the end of the session, and the error carries the server's own
/// reason: there is nothing further to ask it, and nowhere better to turn the code
/// into a sentence.
pub fn error_info(body: &[u8]) -> Result<(), Malformed> {
    const WHAT: &str = "the server ended the session";
    let mut r = Reader::new(WHAT, body);
    match r.u32_le()? {
        0 => Ok(()),
        code => Err(r.report(describe(code), code)),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(frame: &[u8]) -> share::Data<'_> {
        match share::decode(frame).unwrap() {
            share::Pdu::Data(data) => data,
            other => panic!("a data PDU, not {other:?}"),
        }
    }

    /// Off and on again, which is what makes a host that ignores Refresh Rect paint
    /// the desktop. The order is the whole of it: the second PDU alone asks for
    /// nothing.
    #[test]
    fn a_server_that_suppresses_output_is_asked_that_way_and_in_that_order() {
        let pdus = repaint(1007, 0x0001_0021, 1920, 1080, true, true);
        assert_eq!(pdus.len(), 2, "a server that offers both is asked the reliable way");
        let off = decoded(&pdus[0]);
        assert_eq!(off.kind, SUPPRESS_OUTPUT);
        assert_eq!(off.body, &[0, 0, 0, 0], "no rectangle goes with a suppression");
        let on = decoded(&pdus[1]);
        assert_eq!(on.kind, SUPPRESS_OUTPUT);
        assert_eq!(on.body, &[ALLOW, 0, 0, 0, 0, 0, 0, 0, 0x7F, 0x07, 0x37, 0x04]);
    }

    #[test]
    fn a_server_that_only_refreshes_rectangles_is_asked_for_the_whole_desktop() {
        let pdus = repaint(1007, 0x0001_0021, 1920, 1080, true, false);
        assert_eq!(pdus.len(), 1);
        let refresh = decoded(&pdus[0]);
        assert_eq!(refresh.kind, REFRESH_RECT);
        assert_eq!(refresh.body, &[1, 0, 0, 0, 0, 0, 0, 0, 0x7F, 0x07, 0x37, 0x04]);
    }

    /// A server that offers neither is asked for nothing rather than sent a PDU it
    /// said it would not read.
    #[test]
    fn a_server_that_offers_neither_way_is_not_asked() {
        assert!(repaint(1007, 1, 1920, 1080, false, false).is_empty());
    }

    #[test]
    fn a_quiet_error_info_is_the_server_saying_nothing_is_wrong() {
        error_info(&[0, 0, 0, 0]).expect("a code of zero is not an error");
    }

    #[test]
    fn an_error_info_becomes_the_sentence_the_server_meant_by_it() {
        let err = error_info(&0x0000_0009_u32.to_le_bytes()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the server ended the session: the account is not allowed to log on to this host \
             (0x00000009)"
        );
        let err = error_info(&0x0000_10CA_u32.to_le_bytes()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the server ended the session: for a reason this client has no name for (0x000010ca)"
        );
    }
}
