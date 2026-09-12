//! What the user did, on its way to the host.
//!
//! Input takes the fast path in both directions: the same framing that carries the
//! server's updates in [`super::fastpath`] carries keystrokes and mouse events back,
//! and for the same reason — there is no share header, no security header and no
//! channel, just an action, a length, and the events.
//!
//! # Many events, one PDU
//!
//! A PDU carries a count and then that many events, which is what makes input cheap
//! to batch: a mouse dragged across a desktop is a hundred moves a second, and each
//! one of them in its own PDU would be a hundred writes a second. The count lives in
//! four bits of the first byte and so stops at fifteen; past that it moves to a byte
//! of its own and reaches [`MAX_EVENTS`]. [`pdus`] does that arithmetic and hands back
//! as few PDUs as the events will fit in.
//!
//! # The wheel shares a word with the flags
//!
//! A mouse event has one 16-bit field for both its flags and, when it is a wheel
//! event, how far the wheel turned. The rotation is the low nine bits — eight of
//! magnitude and a sign bit above them — and the flags are above that, so a rotation
//! that does not fit does not overflow into a larger number: it overflows into
//! `PTRFLAGS_MOVE` and turns a scroll into a jump. [`MAX_ROTATION`] is where that is
//! stopped, and it is stopped here rather than trusted to the caller, because the
//! caller is holding a number of pixels from a browser.
//!
//! [MS-RDPBCGR] 2.2.8.1.2.

use super::wire::Writer;

/// `FASTPATH_INPUT_ACTION_FASTPATH`, in the low two bits of the first byte. The rest
/// of that byte is the event count and two flags — secure checksum and encryption —
/// that this client never sets, its connection being inside TLS.
const ACTION: u8 = 0x00;

/// How many events the count in the header can hold before it needs a byte of its
/// own.
const HEADER_EVENTS: usize = 15;

/// The most events one PDU can carry, the separate count being one byte.
pub const MAX_EVENTS: usize = 255;

/// Where a one-byte length stops and the two-byte form begins.
const SHORT_LENGTH: usize = 0x7F;

/// `eventCode`, in the top three bits of each event's header byte.
const SCANCODE: u8 = 0x0;
const MOUSE: u8 = 0x1;
const EXTENDED_MOUSE: u8 = 0x2;

/// `eventFlags` for a scancode event, in the low five bits of the same byte.
const RELEASE: u8 = 0x01;
const EXTENDED: u8 = 0x02;

/// `pointerFlags` of a mouse event.
const WHEEL_NEGATIVE: u16 = 0x0100;
const VERTICAL_WHEEL: u16 = 0x0200;
const HORIZONTAL_WHEEL: u16 = 0x0400;
const MOVE: u16 = 0x0800;
const LEFT_BUTTON: u16 = 0x1000;
const RIGHT_BUTTON: u16 = 0x2000;
const MIDDLE_BUTTON: u16 = 0x4000;
const DOWN: u16 = 0x8000;

/// `pointerFlags` of an extended mouse event, which shares only its `DOWN` bit with
/// the ordinary one.
const X1_BUTTON: u16 = 0x0001;
const X2_BUTTON: u16 = 0x0002;

/// The largest rotation one wheel event can carry.
///
/// The wire field is nine-bit two's complement, so it reaches -256. The magnitude is
/// held symmetric instead, because a notch up and a notch down being different sizes
/// is a bug nobody would find by reading the code.
pub const MAX_ROTATION: i16 = 255;

/// A mouse button, in the three RDP's own mouse event carries and the two that travel
/// on an event of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    /// "Back" on most mice.
    X1,
    /// "Forward" on most mice.
    X2,
}

/// One thing the user did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A key by RDP scancode. `extended` is the E0 prefix — right alt, the arrow
    /// cluster, the numeric-keypad Enter and slash, and the Windows keys.
    Key { scancode: u8, extended: bool, down: bool },
    /// The pointer, pressing nothing.
    Move { x: u16, y: u16 },
    /// A button, at a position. The position travels with the press rather than
    /// being remembered from the last move, because that is how the PDU is shaped —
    /// and a click whose coordinates came from an earlier event is where "it clicked
    /// the wrong thing" on a slow link comes from.
    Button { button: Button, down: bool, x: u16, y: u16 },
    /// The wheel, by a signed number of rotation units: 120 is one notch of a
    /// conventional wheel, and positive is up, or right. Clamped to
    /// [`MAX_ROTATION`].
    Wheel { rotation: i16, horizontal: bool, x: u16, y: u16 },
}

/// Every event, as few PDUs as they fit in. An empty slice is no PDUs at all.
pub fn pdus(events: &[Event]) -> impl Iterator<Item = Vec<u8>> + '_ {
    events.chunks(MAX_EVENTS).map(pdu)
}

/// One PDU carrying between one and [`MAX_EVENTS`] events.
fn pdu(events: &[Event]) -> Vec<u8> {
    let mut body = Writer::new();
    for event in events {
        event.write(&mut body);
    }
    let body = body.finish();

    // The count goes in the header's four bits if it fits, and in a byte after the
    // length if it does not. A header count of zero is what says to look there.
    let count = u8::try_from(events.len()).expect("a chunk of at most MAX_EVENTS");
    let counted = usize::from(events.len() > HEADER_EVENTS);

    // The length field measures the whole PDU, itself included, so how long the PDU
    // is decides how many bytes that takes — and taking two makes it one longer.
    let mut length = 1 + 1 + counted + body.len();
    if length > SHORT_LENGTH {
        length += 1;
    }

    let mut w = Writer::with_capacity(length);
    w.u8(if counted == 0 { ACTION | (count << 2) } else { ACTION });
    if length > SHORT_LENGTH {
        // Two bytes, most significant first, with the top bit marking the form.
        w.u16_be(0x8000 | u16::try_from(length).expect("a PDU of at most MAX_EVENTS events"));
    } else {
        w.u8(u8::try_from(length).expect("a length below the two-byte form"));
    }
    if counted == 1 {
        w.u8(count);
    }
    w.bytes(&body);
    w.finish()
}

impl Event {
    fn write(&self, w: &mut Writer) {
        match *self {
            Event::Key { scancode, extended, down } => {
                let mut flags = if down { 0 } else { RELEASE };
                if extended {
                    flags |= EXTENDED;
                }
                w.u8(header(SCANCODE, flags));
                w.u8(scancode);
            }
            Event::Move { x, y } => mouse(w, MOVE, 0, x, y),
            Event::Button { button: Button::X1, down, x, y } => {
                extended_mouse(w, X1_BUTTON, down, x, y);
            }
            Event::Button { button: Button::X2, down, x, y } => {
                extended_mouse(w, X2_BUTTON, down, x, y);
            }
            Event::Button { button, down, x, y } => {
                let flags = match button {
                    Button::Left => LEFT_BUTTON,
                    Button::Middle => MIDDLE_BUTTON,
                    _ => RIGHT_BUTTON,
                };
                mouse(w, flags | if down { DOWN } else { 0 }, 0, x, y);
            }
            Event::Wheel { rotation, horizontal, x, y } => {
                let flags = if horizontal { HORIZONTAL_WHEEL } else { VERTICAL_WHEEL };
                mouse(w, flags, rotation, x, y);
            }
        }
    }
}

/// The byte every event starts with: what kind it is, and up to five bits that mean
/// whatever that kind says they mean.
fn header(code: u8, flags: u8) -> u8 {
    (code << 5) | (flags & 0x1F)
}

fn mouse(w: &mut Writer, flags: u16, rotation: i16, x: u16, y: u16) {
    let rotation = rotation.clamp(-MAX_ROTATION, MAX_ROTATION);
    // The magnitude is the low byte of the two's complement, and the sign is a bit
    // above it rather than the ninth bit of the number.
    let sign = if rotation < 0 { WHEEL_NEGATIVE } else { 0 };
    let magnitude = u16::from(rotation.to_le_bytes()[0]);
    w.u8(header(MOUSE, 0));
    w.u16_le(flags | sign | magnitude);
    w.u16_le(x);
    w.u16_le(y);
}

fn extended_mouse(w: &mut Writer, button: u16, down: bool, x: u16, y: u16) {
    w.u8(header(EXTENDED_MOUSE, 0));
    w.u16_le(button | if down { DOWN } else { 0 });
    w.u16_le(x);
    w.u16_le(y);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(event: Event) -> Vec<u8> {
        one_pdu(&[event])
    }

    #[test]
    fn a_key_carries_its_release_and_extended_bits_beside_the_code() {
        assert_eq!(
            one(Event::Key { scancode: 0x1E, extended: false, down: true }),
            vec![0x04, 0x04, 0x00, 0x1E]
        );
        assert_eq!(
            one(Event::Key { scancode: 0x48, extended: true, down: false }),
            vec![0x04, 0x04, RELEASE | EXTENDED, 0x48]
        );
    }

    #[test]
    fn a_move_presses_nothing_and_turns_no_wheel() {
        assert_eq!(
            one(Event::Move { x: 0x0102, y: 0x0304 }),
            vec![0x04, 0x09, 0x20, 0x00, 0x08, 0x02, 0x01, 0x04, 0x03]
        );
    }

    #[test]
    fn a_button_carries_its_own_down_bit() {
        let down = one(Event::Button { button: Button::Left, down: true, x: 1, y: 2 });
        assert_eq!(&down[3..5], (LEFT_BUTTON | DOWN).to_le_bytes());
        let up = one(Event::Button { button: Button::Left, down: false, x: 1, y: 2 });
        assert_eq!(&up[3..5], LEFT_BUTTON.to_le_bytes());
        let middle = one(Event::Button { button: Button::Middle, down: true, x: 1, y: 2 });
        assert_eq!(&middle[3..5], (MIDDLE_BUTTON | DOWN).to_le_bytes());
    }

    /// The two extra buttons are a different event code, not a fourth and fifth flag.
    #[test]
    fn the_side_buttons_ride_an_event_of_their_own() {
        let x1 = one(Event::Button { button: Button::X1, down: true, x: 1, y: 2 });
        assert_eq!(x1[2], EXTENDED_MOUSE << 5);
        assert_eq!(&x1[3..5], (X1_BUTTON | DOWN).to_le_bytes());
        let x2 = one(Event::Button { button: Button::X2, down: false, x: 1, y: 2 });
        assert_eq!(x2[2], EXTENDED_MOUSE << 5);
        assert_eq!(&x2[3..5], X2_BUTTON.to_le_bytes());
    }

    /// The sign is a bit beside the magnitude, not the top bit of it.
    #[test]
    fn a_negative_wheel_sets_the_negative_bit_and_an_eight_bit_magnitude() {
        let down = one(Event::Wheel { rotation: -1, horizontal: false, x: 1, y: 2 });
        let flags = u16::from_le_bytes([down[3], down[4]]);
        assert_eq!(flags, VERTICAL_WHEEL | WHEEL_NEGATIVE | 0x00FF);
        let up = one(Event::Wheel { rotation: 120, horizontal: true, x: 1, y: 2 });
        let flags = u16::from_le_bytes([up[3], up[4]]);
        assert_eq!(flags, HORIZONTAL_WHEEL | 120);
    }

    /// A rotation past nine bits would not be a larger scroll; it would set
    /// `PTRFLAGS_MOVE` and become a jump.
    #[test]
    fn an_oversized_rotation_cannot_reach_the_flag_bits() {
        for rotation in [i16::MAX, MAX_ROTATION + 1] {
            let pdu = one(Event::Wheel { rotation, horizontal: false, x: 1, y: 2 });
            let flags = u16::from_le_bytes([pdu[3], pdu[4]]);
            assert_eq!(flags, VERTICAL_WHEEL | u16::try_from(MAX_ROTATION).unwrap());
        }
        for rotation in [i16::MIN, -MAX_ROTATION - 1] {
            let pdu = one(Event::Wheel { rotation, horizontal: false, x: 1, y: 2 });
            let flags = u16::from_le_bytes([pdu[3], pdu[4]]);
            assert_eq!(flags, VERTICAL_WHEEL | WHEEL_NEGATIVE | 0x0001);
        }
    }

    #[test]
    fn up_to_fifteen_events_are_counted_in_the_header_byte() {
        let events = vec![Event::Key { scancode: 1, extended: false, down: true }; HEADER_EVENTS];
        let pdu = one_pdu(&events);
        assert_eq!(pdu[0], ACTION | (15 << 2));
        assert_eq!(pdu[1], 2 + 15 * 2, "the length counts itself and the header");
        assert_eq!(pdu.len(), usize::from(pdu[1]));
    }

    #[test]
    fn past_fifteen_the_count_moves_to_a_byte_of_its_own() {
        let events = vec![Event::Key { scancode: 1, extended: false, down: true }; 16];
        let pdu = one_pdu(&events);
        assert_eq!(pdu[0], ACTION, "a header count of zero says to look further on");
        assert_eq!(pdu[1], 3 + 16 * 2);
        assert_eq!(pdu[2], 16);
        assert_eq!(pdu.len(), usize::from(pdu[1]));
    }

    /// Past 127 bytes the length takes two, and taking two makes the PDU one longer.
    #[test]
    fn a_long_pdu_measures_itself_in_two_bytes() {
        let events = vec![Event::Move { x: 1, y: 2 }; 18];
        let pdu = one_pdu(&events);
        let length = u16::from_be_bytes([pdu[1], pdu[2]]);
        assert_eq!(length, 0x8000 | 130);
        assert_eq!(pdu.len(), 130);
        assert_eq!(pdu[3], 18, "the count still follows the length");
    }

    #[test]
    fn more_events_than_one_pdu_holds_become_two() {
        let events = vec![Event::Move { x: 1, y: 2 }; MAX_EVENTS + 1];
        let pdus: Vec<_> = pdus(&events).collect();
        assert_eq!(pdus.len(), 2);
        assert_eq!(pdus[0][3], u8::try_from(MAX_EVENTS).unwrap());
        assert_eq!(pdus[1][0], ACTION | (1 << 2));
    }

    #[test]
    fn nothing_to_send_is_nothing_sent() {
        assert_eq!(pdus(&[]).count(), 0);
    }

    fn one_pdu(events: &[Event]) -> Vec<u8> {
        let mut pdus = pdus(events);
        let pdu = pdus.next().expect("a PDU");
        assert!(pdus.next().is_none(), "and only one");
        pdu
    }
}
