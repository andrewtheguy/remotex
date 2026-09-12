//! Keyboard and mouse, encoded here and sent on the session thread.
//!
//! Every method on [`Input`] turns one call into one fast-path input event and
//! queues it for the session thread, which batches whatever has queued up into as
//! few PDUs as it can. Nothing blocks, nothing can fail visibly, and a call made
//! after the session has ended is dropped — the right shape for input: a keystroke
//! that arrives while the connection is closing has no useful error for the caller
//! to handle, and a `Result` on every key press would be noise.

use std::sync::Arc;

use ironrdp::displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp::pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp::pdu::input::mouse::PointerFlags;
use ironrdp::pdu::input::mouse_x::PointerXFlags;
use ironrdp::pdu::input::{MousePdu, MouseXPdu};
use tokio::sync::{mpsc, watch};

/// A mouse button, in the three the RDP fast-path mouse event encodes directly
/// plus the two extended ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// "Back" on most mice.
    X1,
    /// "Forward" on most mice.
    X2,
}

/// One thing for the session thread to do next time it wakes.
pub(super) enum Command {
    Input(FastPathInputEvent),
    /// Ask the server to repaint the whole desktop.
    Refresh,
    /// Ask the server for a new desktop size, over Display Control.
    Resize { width: u32, height: u32, scale_percent: u32 },
    Shutdown,
}

/// The input side of a [`Session`](super::Session).
///
/// Cheap to clone, and every clone feeds the same queue.
#[derive(Clone)]
pub struct Input {
    commands: mpsc::UnboundedSender<Command>,
    /// Raised by [`Input::shutdown`] beside the queued [`Command::Shutdown`].
    ///
    /// The queued command is what ends the session properly, but the session
    /// thread only sees its queue between events; a thread parked on a full event
    /// queue would not reach it until the caller drained one. This is the same
    /// news on a path that needs no room — see `Active::deliver`.
    stop: Arc<watch::Sender<bool>>,
}

/// The largest rotation one wheel event carries: the wire field is nine-bit two's
/// complement, and the magnitude is kept symmetric so a notch up and a notch down
/// are always the same size.
const MAX_WHEEL_ROTATION: i16 = 255;

impl Input {
    pub(super) fn new(commands: mpsc::UnboundedSender<Command>) -> Self {
        Self { commands, stop: Arc::new(watch::channel(false).0) }
    }

    /// Watches for [`Input::shutdown`], for the session thread.
    pub(super) fn stopped(&self) -> watch::Receiver<bool> {
        self.stop.subscribe()
    }

    /// An input whose queue nobody drains, for a test that needs a handle and no
    /// session behind it — which is exactly the "a call after the session ended is
    /// dropped" contract.
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        Self::new(mpsc::unbounded_channel().0)
    }

    /// Move the pointer, pressing nothing.
    pub fn mouse_move(&self, x: u16, y: u16) {
        self.mouse(PointerFlags::MOVE, 0, x, y);
    }

    /// Press or release a button, at a position.
    ///
    /// The position travels with the button event rather than being remembered
    /// from the last move, because that is how RDP's own PDU is shaped — and
    /// because a click whose coordinates came from a *previous* event is the classic
    /// source of "it clicked the wrong thing" on a laggy link.
    pub fn mouse_button(&self, button: MouseButton, down: bool, x: u16, y: u16) {
        let event = match button {
            MouseButton::Left | MouseButton::Middle | MouseButton::Right => {
                let mut flags = match button {
                    MouseButton::Left => PointerFlags::LEFT_BUTTON,
                    MouseButton::Middle => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
                    _ => PointerFlags::RIGHT_BUTTON,
                };
                if down {
                    flags |= PointerFlags::DOWN;
                }
                mouse_event(flags, 0, x, y)
            }
            // The two extra buttons go on a different event entirely, with their own
            // DOWN bit.
            MouseButton::X1 | MouseButton::X2 => {
                let mut flags = if button == MouseButton::X1 {
                    PointerXFlags::BUTTON1
                } else {
                    PointerXFlags::BUTTON2
                };
                if down {
                    flags |= PointerXFlags::DOWN;
                }
                FastPathInputEvent::MouseEventEx(MouseXPdu { flags, x_position: x, y_position: y })
            }
        };
        self.push(Command::Input(event));
    }

    /// Scroll, by a signed number of rotation units. 120 is one notch of a
    /// conventional wheel; positive is up (or right).
    ///
    /// The rotation shares its word with the event's flags on the wire, as a
    /// nine-bit two's-complement value beside a separate negative bit. IronRDP
    /// writes both from the signed number, so the only thing left to do here is
    /// keep the magnitude inside nine bits — a larger one would wrap into the flag
    /// bits and turn a scroll into some other event.
    pub fn wheel(&self, delta: i16, horizontal: bool, x: u16, y: u16) {
        let flags = if horizontal { PointerFlags::HORIZONTAL_WHEEL } else { PointerFlags::VERTICAL_WHEEL };
        self.mouse(flags, delta.clamp(-MAX_WHEEL_ROTATION, MAX_WHEEL_ROTATION), x, y);
    }

    /// Press or release a key, by RDP scancode.
    ///
    /// `extended` is the E0 prefix — right alt, the arrow cluster, the numeric-keypad
    /// Enter and slash, and the Windows keys — which the fast-path keyboard event
    /// carries as a flag beside the code.
    pub fn key(&self, scancode: u8, extended: bool, down: bool) {
        let mut flags = KeyboardFlags::empty();
        if extended {
            flags |= KeyboardFlags::EXTENDED;
        }
        if !down {
            flags |= KeyboardFlags::RELEASE;
        }
        self.push(Command::Input(FastPathInputEvent::KeyboardEvent(flags, scancode)));
    }

    /// Ask the server to repaint the whole desktop.
    pub fn refresh(&self) {
        self.push(Command::Refresh);
    }

    /// Ask the server to change the desktop size.
    ///
    /// Needs [`Connect::resize`](super::Connect::resize) and a server that offers
    /// Display Control — which is announced as
    /// [`Event::ResizeReady`](super::Event::ResizeReady). A request made before that
    /// arrives is *held*, not dropped, and the most recent one is sent as soon as
    /// the channel comes up; a request made on a session that never gets the channel
    /// is dropped, like every other input on a session that cannot carry it.
    ///
    /// **Nothing happens synchronously.** The server answers by resizing the
    /// desktop — a Deactivation-Reactivation Sequence — which arrives as
    /// [`Event::Resize`](super::Event::Resize) with the size it actually chose. A
    /// server that declines sends nothing at all, so the framebuffer's size is what
    /// `Event::Resize` says, never what was requested here.
    ///
    /// **A single request is not enough.** A Windows host ignores a monitor layout
    /// that arrives while it is still bringing the session up, and reports nothing
    /// about having ignored it. There is no "ready now" to wait for, so the retry
    /// belongs to the caller: ask again until `Event::Resize` arrives or the caller
    /// gives up. This does not do it, because a retry ladder needs a clock and a
    /// policy, and both belong to whoever owns the timers.
    ///
    /// `scale_percent` is the desktop's **DesktopScaleFactor**: 100 for an ordinary
    /// display and 200 for a 2x one. It rides the same PDU as the size and cannot be
    /// sent without one — a size sent without it tells the server to forget a scale
    /// it is already applying.
    ///
    /// The size and the scale are adjusted to what MS-RDPEDISP permits before they
    /// are queued; see [`sanitise_size`] and [`sanitise_scale`]. This does not
    /// rate-limit: every request costs the remote a desktop resize, so a caller
    /// driving this from a window has to debounce.
    pub fn resize(&self, width: u32, height: u32, scale_percent: u32) {
        let (width, height) = sanitise_size(width, height);
        self.push(Command::Resize { width, height, scale_percent: sanitise_scale(scale_percent) });
    }

    pub(super) fn shutdown(&self) {
        // Raised first, so a thread waiting for room stops waiting before it is
        // asked to stop.
        let _ = self.stop.send(true);
        self.push(Command::Shutdown);
    }

    fn mouse(&self, flags: PointerFlags, rotation: i16, x: u16, y: u16) {
        self.push(Command::Input(mouse_event(flags, rotation, x, y)));
    }

    fn push(&self, command: Command) {
        // A closed queue is a session that has ended, and a call after that is
        // dropped by design — see the module doc.
        let _ = self.commands.send(command);
    }
}

fn mouse_event(flags: PointerFlags, rotation: i16, x: u16, y: u16) -> FastPathInputEvent {
    FastPathInputEvent::MouseEvent(MousePdu {
        flags,
        number_of_wheel_rotation_units: rotation,
        x_position: x,
        y_position: y,
    })
}

/// Bring a requested desktop size inside what MS-RDPEDISP allows.
///
/// Public because a caller that decides *whether to ask at all* has to compare
/// against the size that would really be sent. Asking a server for the desktop it
/// already has is not free — it answers with a full desktop resize — so a client
/// that compares an unadjusted 1281 against a live 1280 asks again on every
/// viewport report, forever.
///
/// Adjusted rather than refused: a caller sizing a desktop to a viewport has an
/// arbitrary number of pixels, and refusing a resize because a window happens to be
/// 1281 pixels wide would be a bug with no fix at the call site.
///
/// - **The width must be even** (MS-RDPEDISP 2.2.2.2.1). Rounded *down*, so the
///   desktop stays inside the viewport that asked for it rather than growing a
///   scrollbar by one pixel.
/// - Both dimensions are clamped to 200..=8192.
///
/// IronRDP's own rule, so the comparison and the monitor layout that is encoded can
/// never disagree.
pub fn sanitise_size(width: u32, height: u32) -> (u32, u32) {
    MonitorLayoutEntry::adjust_display_size(width, height)
}

/// Bring a requested `DesktopScaleFactor` inside the 100..=500 MS-RDPEDISP allows.
///
/// Clamped rather than refused, with a sharper consequence than the size: a server
/// that finds *either* scale factor out of range must ignore **both**, so an
/// invented density does not cost part of the request — it costs the whole scaling
/// of the desktop, silently.
pub fn sanitise_scale(percent: u32) -> u32 {
    percent.clamp(100, 500)
}

#[cfg(test)]
mod tests {
    use ironrdp::core::encode_vec;

    use super::*;

    /// What one call put on the queue.
    fn queued(call: impl FnOnce(&Input)) -> FastPathInputEvent {
        let (tx, mut rx) = mpsc::unbounded_channel();
        call(&Input::new(tx));
        match rx.try_recv() {
            Ok(Command::Input(event)) => event,
            _ => panic!("expected one input event"),
        }
    }

    /// The flags word of a mouse event, as it goes on the wire.
    fn mouse_flags(event: &FastPathInputEvent) -> u16 {
        let FastPathInputEvent::MouseEvent(pdu) = event else { panic!("not a mouse event: {event:?}") };
        let bytes = encode_vec(pdu).expect("a mouse event encodes");
        u16::from_le_bytes([bytes[0], bytes[1]])
    }

    /// The encoding that is easiest to get backwards: a negative rotation sets the
    /// negative bit *and* carries a nine-bit two's-complement magnitude, so −120 is
    /// `WHEEL | NEGATIVE | 0x88`.
    #[test]
    fn a_negative_wheel_sets_the_negative_bit_and_a_nine_bit_magnitude() {
        let down = mouse_flags(&queued(|input| input.wheel(-120, false, 0, 0)));
        assert_eq!(down, 0x0200 | 0x0100 | 0x88);
        let up = mouse_flags(&queued(|input| input.wheel(120, false, 0, 0)));
        assert_eq!(up, 0x0200 | 120);
        // And the two axes are different events, not the same one with a sign.
        assert_eq!(mouse_flags(&queued(|input| input.wheel(120, true, 0, 0))) & 0x0400, 0x0400);
    }

    /// The rotation shares its word with the event-type bits, so an oversized delta
    /// that wrapped into them would turn a scroll into some other event entirely.
    #[test]
    fn an_oversized_wheel_delta_cannot_reach_the_event_bits() {
        for delta in [i16::MIN, -32000, -400, 400, 32000, i16::MAX] {
            let flags = mouse_flags(&queued(|input| input.wheel(delta, false, 0, 0)));
            assert_eq!(flags & !0x01FF, 0x0200, "delta {delta} corrupted the event bits");
        }
    }

    #[test]
    fn a_key_carries_its_release_and_extended_bits() {
        let FastPathInputEvent::KeyboardEvent(flags, code) = queued(|input| input.key(0x48, true, false))
        else {
            panic!("not a key event")
        };
        assert_eq!(code, 0x48);
        assert_eq!(flags, KeyboardFlags::EXTENDED | KeyboardFlags::RELEASE);
        let FastPathInputEvent::KeyboardEvent(flags, _) = queued(|input| input.key(0x1E, false, true))
        else {
            panic!("not a key event")
        };
        assert!(flags.is_empty(), "a plain press carries no flags");
    }

    /// The side buttons go on the extended mouse event; the other three never do.
    #[test]
    fn the_side_buttons_ride_the_extended_mouse_event() {
        let FastPathInputEvent::MouseEventEx(pdu) =
            queued(|input| input.mouse_button(MouseButton::X2, true, 5, 6))
        else {
            panic!("X2 is an extended mouse event")
        };
        assert_eq!(pdu.flags, PointerXFlags::BUTTON2 | PointerXFlags::DOWN);
        assert_eq!((pdu.x_position, pdu.y_position), (5, 6));
        let flags = mouse_flags(&queued(|input| input.mouse_button(MouseButton::Left, false, 0, 0)));
        assert_eq!(flags, 0x1000, "a left release is the button bit and no DOWN");
    }

    /// Every one of these is a size a viewport really produces, and an odd width is
    /// the one that fails *silently* on a Windows host.
    #[test]
    fn a_requested_size_is_brought_inside_what_the_protocol_allows() {
        assert_eq!(sanitise_size(1281, 800), (1280, 800), "an odd width rounds down");
        assert_eq!(sanitise_size(1920, 1080), (1920, 1080), "a legal size is left alone");
        assert_eq!(sanitise_size(0, 0), (200, 200), "below the minimum");
        assert_eq!(sanitise_size(u32::MAX, u32::MAX), (8192, 8192), "above the maximum");
        for (width, _) in [sanitise_size(0, 600), sanitise_size(u32::MAX, 600), sanitise_size(1, 1)] {
            assert_eq!(width % 2, 0, "the clamp produced an odd width");
        }
    }

    /// The queued command is what ends the session, but a session thread parked on
    /// a full event queue reaches its commands only once the caller takes an event.
    /// The watch is the same news by a route that needs nothing of the caller.
    #[test]
    fn a_shutdown_is_announced_as_well_as_queued() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let input = Input::new(tx);
        let stop = input.stopped();
        assert!(!*stop.borrow(), "nothing has asked this session to stop");
        input.shutdown();
        assert!(*stop.borrow(), "a thread with no room to send is told anyway");
        assert!(matches!(rx.try_recv(), Ok(Command::Shutdown)), "and the command still goes to the loop");
    }

    #[test]
    fn a_scale_factor_cannot_leave_the_range_that_makes_it_legal() {
        assert_eq!(sanitise_scale(100), 100);
        assert_eq!(sanitise_scale(200), 200);
        assert_eq!(sanitise_scale(0), 100, "\"unset\" is the same request, badly spelled");
        assert_eq!(sanitise_scale(u32::MAX), 500);
    }
}
