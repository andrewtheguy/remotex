//! Keyboard and mouse, queued here and encoded on the session thread.
//!
//! Every method on [`Input`] turns one call into one fast-path input event and
//! queues it for the session thread, which batches whatever has queued up into as
//! few PDUs as it can. Nothing blocks, nothing can fail visibly, and a call made
//! after the session has ended is dropped — the right shape for input: a keystroke
//! that arrives while the connection is closing has no useful error for the caller
//! to handle, and a `Result` on every key press would be noise.

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use super::proto::display;
use super::proto::input::{Button, Event, MAX_ROTATION};

/// One thing for the session thread to do next time it wakes.
pub(super) enum Command {
    Input(Event),
    /// Ask the server to repaint the whole desktop.
    Refresh,
    /// Ask the server for a new desktop size, over Display Control.
    Resize { width: u32, height: u32, scale_percent: u32 },
    /// Something for the clipboard channel, in the three shapes a clipboard has.
    Clipboard(Clipboard),
    Shutdown,
}

/// One thing to say on the clipboard channel.
///
/// Format ids and bytes, and nothing about what either means: which format is text
/// and how it is encoded belong to whoever is bridging a real clipboard —
/// [`crate::rdp_clipboard`] — not to the wire.
pub(super) enum Clipboard {
    /// Tell the remote what this end's clipboard now holds.
    Advertise(Vec<u32>),
    /// Ask the remote for the bytes of a format it advertised.
    Request(u32),
    /// Answer a paste the remote is waiting on.
    Respond(Option<Vec<u8>>),
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
        self.push(Command::Input(Event::Move { x, y }));
    }

    /// Press or release a button, at a position.
    ///
    /// The position travels with the button event rather than being remembered
    /// from the last move, because that is how RDP's own PDU is shaped — and
    /// because a click whose coordinates came from a *previous* event is the classic
    /// source of "it clicked the wrong thing" on a laggy link.
    pub fn mouse_button(&self, button: MouseButton, down: bool, x: u16, y: u16) {
        self.push(Command::Input(Event::Button { button, down, x, y }));
    }

    /// Scroll, by a signed number of rotation units. 120 is one notch of a
    /// conventional wheel; positive is up (or right).
    ///
    /// The rotation shares its word with the event's flags on the wire, as a
    /// nine-bit two's-complement value beside a separate negative bit, so the
    /// magnitude is held inside nine bits here: a larger one would wrap into the
    /// flag bits and turn a scroll into some other event.
    pub fn wheel(&self, delta: i16, horizontal: bool, x: u16, y: u16) {
        let rotation = delta.clamp(-MAX_ROTATION, MAX_ROTATION);
        self.push(Command::Input(Event::Wheel { rotation, horizontal, x, y }));
    }

    /// Press or release a key, by RDP scancode.
    ///
    /// `extended` is the E0 prefix — right alt, the arrow cluster, the numeric-keypad
    /// Enter and slash, and the Windows keys — which the fast-path keyboard event
    /// carries as a flag beside the code.
    pub fn key(&self, scancode: u8, extended: bool, down: bool) {
        self.push(Command::Input(Event::Key { scancode, extended, down }));
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

    /// Tell the remote what this end's clipboard now holds, as Windows format ids.
    ///
    /// An empty list is a real thing to say and the one to say first: it announces an
    /// empty clipboard, where saying nothing announces that there is no clipboard on
    /// this end at all. Nothing is transferred here — the remote asks for the bytes
    /// when somebody actually pastes, which arrives as
    /// [`Event::ClipboardWanted`](super::Event::ClipboardWanted).
    ///
    /// Needs [`Connect::clipboard`](super::Connect::clipboard) and a server that
    /// opened the channel, which is announced as
    /// [`Event::ClipboardReady`](super::Event::ClipboardReady). Anything sent before
    /// that is dropped, like every other input on a session that cannot carry it.
    pub fn advertise_clipboard(&self, formats: Vec<u32>) {
        self.push(Command::Clipboard(Clipboard::Advertise(formats)));
    }

    /// Ask the remote for the bytes of a format it advertised.
    ///
    /// The answer arrives as [`Event::ClipboardData`](super::Event::ClipboardData),
    /// or as [`Event::ClipboardRefused`](super::Event::ClipboardRefused) — which a
    /// Windows peer sends without saying why, and sometimes answers a second ask for
    /// the same format. The retry is the caller's, for the same reason
    /// [`Input::resize`]'s is: it needs a clock and a policy.
    pub fn request_clipboard(&self, format: u32) {
        self.push(Command::Clipboard(Clipboard::Request(format)));
    }

    /// Answer the paste in [`Event::ClipboardWanted`](super::Event::ClipboardWanted),
    /// with the bytes or with nothing.
    ///
    /// **Every one of those events has to be answered**, `None` included. The remote
    /// application asking is blocked inside its own paste handler until this arrives,
    /// which on Windows is a window that has stopped repainting rather than an error
    /// anybody sees.
    pub fn send_clipboard(&self, data: Option<Vec<u8>>) {
        self.push(Command::Clipboard(Clipboard::Respond(data)));
    }

    pub(super) fn shutdown(&self) {
        // Raised first, so a thread waiting for room stops waiting before it is
        // asked to stop.
        let _ = self.stop.send(true);
        self.push(Command::Shutdown);
    }

    fn push(&self, command: Command) {
        // A closed queue is a session that has ended, and a call after that is
        // dropped by design — see the module doc.
        let _ = self.commands.send(command);
    }
}

/// A mouse button, in the three the RDP fast-path mouse event encodes directly plus
/// the two extended ones.
pub type MouseButton = Button;

/// Bring a requested desktop size inside what MS-RDPEDISP allows.
///
/// Public because a caller that decides *whether to ask at all* has to compare
/// against the size that would really be sent — see [`display::adjust_size`], which
/// is the same function the monitor layout itself is built with, so the comparison
/// and the PDU can never disagree.
pub fn sanitise_size(width: u32, height: u32) -> (u32, u32) {
    display::adjust_size(width, height)
}

/// Bring a requested `DesktopScaleFactor` inside the 100..=500 MS-RDPEDISP allows.
///
/// Clamped rather than refused, with a sharper consequence than the size: a server
/// that finds *either* scale factor out of range must ignore **both**, so an
/// invented density does not cost part of the request — it costs the whole scaling
/// of the desktop, silently.
pub fn sanitise_scale(percent: u32) -> u32 {
    percent.clamp(display::MIN_SCALE, display::MAX_SCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What one call put on the queue.
    fn queued(call: impl FnOnce(&Input)) -> Event {
        let (tx, mut rx) = mpsc::unbounded_channel();
        call(&Input::new(tx));
        match rx.try_recv() {
            Ok(Command::Input(event)) => event,
            _ => panic!("expected one input event"),
        }
    }

    #[test]
    fn a_call_becomes_the_event_the_wire_has_a_shape_for() {
        assert_eq!(queued(|input| input.mouse_move(7, 9)), Event::Move { x: 7, y: 9 });
        assert_eq!(
            queued(|input| input.key(0x48, true, false)),
            Event::Key { scancode: 0x48, extended: true, down: false }
        );
        assert_eq!(
            queued(|input| input.mouse_button(MouseButton::X2, true, 5, 6)),
            Event::Button { button: Button::X2, down: true, x: 5, y: 6 }
        );
    }

    /// The rotation shares its word with the event-type bits on the wire, so an
    /// oversized delta that wrapped into them would turn a scroll into some other
    /// event entirely.
    #[test]
    fn an_oversized_wheel_delta_is_held_to_what_the_field_carries() {
        for delta in [i16::MIN, -32000, -400, 400, 32000, i16::MAX] {
            let Event::Wheel { rotation, .. } = queued(|input| input.wheel(delta, false, 0, 0))
            else {
                panic!("not a wheel event")
            };
            assert!(rotation.abs() <= MAX_ROTATION, "delta {delta} became {rotation}");
            assert_eq!(rotation.signum(), delta.signum(), "delta {delta} changed direction");
        }
        // And the two axes are different events, not the same one with a sign.
        let Event::Wheel { horizontal, .. } = queued(|input| input.wheel(120, true, 0, 0)) else {
            panic!("not a wheel event")
        };
        assert!(horizontal);
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
