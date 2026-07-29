//! Optional 2x display built with the private `CGVirtualDisplay` API.
//!
//! Creation verifies the asynchronous result because `applySettings:` may
//! succeed while macOS silently selects a 1x mode. The live CoreGraphics mode
//! is authoritative; ScreenCaptureKit's point scale is not. The creation size
//! fixes an immutable density and pixel envelope for later client-driven size
//! and scale changes. Private-API failure degrades to a real display.

use log::{debug, info, warn};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{msg_send, sel};
use objc2_core_foundation::{CGPoint, CGSize};
use objc2_core_graphics::{CGDisplayBounds, CGDisplayIsActive, CGDisplayIsOnline};
use objc2_foundation::{NSArray, NSString};

/// The backing scale a display of our own is always created at.
///
/// Not configurable: 2x is the entire reason to have one. A 1x display of our
/// own would be the Mac's screen with extra steps.
pub const SCALE: f64 = 2.0;

/// Top of the HiDPI density window, in dpi.
///
/// The display is created here rather than in the middle of the window, because
/// the direction that matters is *down*: a browser window smaller than the
/// display is the common case, and every step down costs density.
const MAX_DPI: f64 = 250.0;

/// Minimum useful display size. Config validation uses the same bounds.
pub const MIN_WIDTH_POINTS: u32 = 800;
pub const MIN_HEIGHT_POINTS: u32 = 600;

/// Deadline for the asynchronous WindowServer configuration update.
const SETTLE_TIMEOUT_MS: u64 = 1500;
const SETTLE_POLL_MS: u64 = 25;

/// A live virtual display. Dropping this removes it from the desktop.
///
/// The display belongs to the process that created it, which is the whole
/// containment story: an agent that crashes cannot leave a display behind, and
/// there is no cleanup path to get wrong.
pub struct VirtualDisplay {
    /// `CGVirtualDisplay`. Held so that dropping it removes the display, and
    /// messaged by [`VirtualDisplay::set_scale`].
    handle: Handle,
    /// The CoreGraphics display id.
    id: u32,
    /// The point size the display was created at. Also the largest mode macOS
    /// can render at 2x, since `maxPixels` was set to twice it.
    base_points: (u32, u32),
}

/// The Objective-C object, separated so the `Send` justification has somewhere
/// to live.
struct Handle(Retained<AnyObject>);

// SAFETY: `CGVirtualDisplay` is a plain Objective-C object whose methods talk to
// the WindowServer; nothing in it is tied to a thread the way AppKit's views
// are. The agent creates it on the main thread at startup and thereafter only
// holds it — the one call that crosses a thread is the release on drop.
unsafe impl Send for Handle {}

impl VirtualDisplay {
    /// Create a 2x display `points` wide and tall, in points.
    ///
    /// Fails rather than degrading if HiDPI does not engage: a 1x display at
    /// twice the requested size is worse than no display of our own, and the
    /// caller can fall back to the Mac's real screen and say so.
    pub fn create(points: (u32, u32)) -> anyhow::Result<Self> {
        let points = (
            points.0.max(MIN_WIDTH_POINTS),
            points.1.max(MIN_HEIGHT_POINTS),
        );
        // Worked out once and used three ways: the descriptor's ceiling, the
        // physical size that places the mode in the density window, and the log
        // line. A second calculation of it is a second place for `SCALE` to be
        // spelled as a literal 2 and drift.
        let pixels = (
            (f64::from(points.0) * SCALE) as u32,
            (f64::from(points.1) * SCALE) as u32,
        );
        // Physical size that puts the created mode at the top of the density
        // window. 25.4 mm to the inch.
        let mm = (
            f64::from(pixels.0) / MAX_DPI * 25.4,
            f64::from(pixels.1) / MAX_DPI * 25.4,
        );

        let descriptor = descriptor(pixels, mm, SERIAL)?;
        let class = class("CGVirtualDisplay")?;
        let allocated: Allocated<AnyObject> = unsafe { msg_send![class, alloc] };
        let display: Retained<AnyObject> =
            unsafe { msg_send![allocated, initWithDescriptor: &*descriptor] };
        let id: u32 = unsafe { msg_send![&*display, displayID] };
        anyhow::ensure!(id != 0, "CGVirtualDisplay returned no display id");

        let handle = Handle(display);
        // The mode is listed at the **point** size with `hiDPI = 1`, which is
        // what makes macOS supply twice as many pixels behind it. Listing it at
        // the pixel size instead produces a display of the same point size with
        // no extra pixels — the trap this whole module is arranged around.
        let settings = settings(points)?;
        let applied: bool = unsafe { msg_send![&*handle.0, applySettings: &*settings] };
        anyhow::ensure!(applied, "applySettings: refused {}x{}", points.0, points.1);

        let settled = await_hidpi_bounds(id, points);
        // Bounds alone cannot tell a working display from a disabled one: a
        // display the WindowServer has decided to keep offline reports the size
        // it was asked for and is in no other way present — not in the active
        // list, not in ScreenCaptureKit, not capturable. Measured against an
        // identity an earlier run had left in that state, which survived a
        // reboot. Checked here so it is reported where it happens rather than
        // surfacing later as "the display is not in the shareable list".
        if !CGDisplayIsOnline(id) || !CGDisplayIsActive(id) {
            return Err(Offline {
                id,
                online: CGDisplayIsOnline(id),
                active: CGDisplayIsActive(id),
            }
            .into());
        }
        let Some(shown) = settled else {
            // Twice the request is the signature of HiDPI not engaging; anything
            // else past the ceiling means the WindowServer is somewhere we did not
            // predict.
            let bounds = CGDisplayBounds(id);
            anyhow::bail!(
                "display {id} came up {}x{} points, past the {}x{} it was created at — HiDPI did \
                 not engage (density outside the {MAX_DPI} dpi window?)",
                bounds.size.width as u32,
                bounds.size.height as u32,
                points.0,
                points.1
            );
        };
        if shown != points {
            // Not a problem, and worth a line: it is the remembered arrangement
            // [`SERIAL`] exists to preserve, doing its job.
            info!(
                "virtualdisplay: display {id} came up at {}x{} points rather than the {}x{} it was \
                 created at — macOS remembered the mode this identity was last set to",
                shown.0, shown.1, points.0, points.1
            );
        }

        info!(
            "virtualdisplay: created display {id} at {}x{} points ({}x{} pixels at {SCALE}x), \
             {:.0}x{:.0} mm; its resolution is now the Mac's to change",
            points.0,
            points.1,
            pixels.0,
            pixels.1,
            mm.0,
            mm.1,
        );
        Ok(Self {
            handle,
            id,
            base_points: points,
        })
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// The point size the display was created at.
    ///
    /// Not "the size it is now": macOS may be showing any mode off the list it
    /// derived from this one. It is carried into [`crate::capture::Target`]
    /// because it is what says how many pixels back a given mode — see there.
    pub fn base_points(&self) -> (u32, u32) {
        self.base_points
    }

    /// Set 1x or 2x while preserving current point size. Size is read under the
    /// caller's lock so a concurrent resize cannot be overwritten. Returns
    /// `false` when no WindowServer change is needed.
    pub fn set_scale(&self, want_hidpi: bool) -> anyhow::Result<bool> {
        let now = crate::capture::display_scale(self.id);
        // Compared against the midpoint rather than for equality: `now` is a
        // ratio of two integers read back from a mode, and the question is only
        // which of the two densities it is.
        if (now >= 1.5) == want_hidpi {
            return Ok(false);
        }
        let points = crate::capture::display_points(self.id);
        let settings = settings_at(points, want_hidpi)?;
        let applied: bool = unsafe { msg_send![&*self.handle.0, applySettings: &*settings] };
        anyhow::ensure!(
            applied,
            "applySettings: refused {}x{} at {}",
            points.0,
            points.1,
            if want_hidpi { "2x" } else { "1x" }
        );
        info!(
            "virtualdisplay: display {} is now {} at {}x{} points",
            self.id,
            if want_hidpi { "2x" } else { "1x" },
            points.0,
            points.1
        );
        Ok(true)
    }

    /// Resize the display to `points`, keeping the density it is in.
    ///
    /// The second thing about this display that is not the Mac's to decide, and
    /// for the same reason as the first ([`VirtualDisplay::set_scale`]): a desktop
    /// nobody sits in front of has no right size of its own, only the right size
    /// for the window someone is looking at it through. Unlike density, this is
    /// asked for rather than followed — a person presses a button, a window drag
    /// does not — because every apply relays every window on the desktop. See
    /// [`rxa_proto::msg::GatewayMsg::ResizeDisplay`].
    ///
    /// `points` is clamped into the envelope creation fixed (see
    /// [`size_in_envelope`]) rather than refused, because there is no way to
    /// refuse: past `maxPixels` the WindowServer answers YES and halves the
    /// result.
    ///
    /// The density is read live and re-applied rather than assumed, so a resize
    /// does not quietly undo the one a client's `HostScale` set. What it cannot
    /// preserve is a density the new size cannot hold: below roughly 57% of the
    /// created width the mode falls out of the HiDPI window and comes back 1x
    /// whichever entry is asked for. That is applied rather than clamped away.
    /// Clamping to keep 2x would answer a request for a window-sized desktop with
    /// a size nobody asked for, and 2x below that floor is not obtainable at any
    /// size that could be substituted — so the honest answer is the asked-for size
    /// at the density it can hold. [`crate::capture::mode_scale`] then reports the
    /// truth and both clients present it correctly, softer.
    ///
    /// Returns whether anything was asked of the WindowServer. `Ok(false)` is the
    /// display already being that size — the common case for a button pressed
    /// twice on a window that did not move — and skipping it is not an
    /// optimisation: a guest's display stack can wedge after enough mode changes
    /// and need a reboot to clear.
    pub fn set_size(&self, points: (u32, u32)) -> anyhow::Result<bool> {
        let want = size_in_envelope(points, self.base_points);
        // Whether the request was clamped and whether it changes anything are two
        // questions, and a window dragged past the envelope answers yes to the
        // first and no to the second on every press after the first. Reported
        // together so the log never says a size was clamped without also saying
        // what came of it — the pair read separately is how "it clamped again"
        // gets mistaken for "it resized again".
        let clamped = if want == points {
            String::new()
        } else {
            format!(
                " (asked for {}x{}, clamped into the {}x{} envelope its descriptor fixed at \
                 creation)",
                points.0, points.1, self.base_points.0, self.base_points.1
            )
        };
        if want == crate::capture::display_points(self.id) {
            debug!(
                "virtualdisplay: display {} is already {}x{} points; not reconfiguring{clamped}",
                self.id, want.0, want.1
            );
            return Ok(false);
        }

        // Required, not defaulted. `display_scale` answers 1x for a display with
        // no mode to read, and a display has none for a few tens of milliseconds
        // around any reconfigure — so defaulting here would turn "resized just
        // after a density change" into "silently dropped to 1x", which nothing
        // would put back: a client sends `HostScale` once per screen change and
        // both clients dedupe it.
        let hidpi = crate::capture::mode_scale(self.id).ok_or_else(|| {
            anyhow::anyhow!(
                "display {} publishes no mode to read a density from — it is mid-reconfigure, so \
                 this resize is dropped rather than guessed at",
                self.id
            )
        })? >= 1.5;

        let settings = settings_at(want, hidpi)?;
        let applied: bool = unsafe { msg_send![&*self.handle.0, applySettings: &*settings] };
        anyhow::ensure!(
            applied,
            "applySettings: refused {}x{} points at {}",
            want.0,
            want.1,
            if hidpi { "2x" } else { "1x" }
        );

        // Equality, unlike creation's [`await_hidpi_bounds`]: there any remembered
        // mode inside the envelope is a right answer, here one size was asked for a
        // moment ago and anything else is the silent halving `maxPixels` does —
        // which the clamp above should have made unreachable, so seeing it means
        // the envelope is not what this code thinks it is. Warned rather than
        // failed: the display is in whatever mode it is in either way, and the poll
        // that announces geometry will find it.
        match await_bounds(self.id, |size| size == want) {
            Some(_) => info!(
                "virtualdisplay: display {} is now {}x{} points at {}{clamped}",
                self.id,
                want.0,
                want.1,
                if hidpi { "2x" } else { "1x" }
            ),
            None => {
                let bounds = CGDisplayBounds(self.id);
                warn!(
                    "virtualdisplay: display {} was asked for {}x{} points and is {}x{} after \
                     {SETTLE_TIMEOUT_MS} ms",
                    self.id,
                    want.0,
                    want.1,
                    bounds.size.width as u32,
                    bounds.size.height as u32
                );
            }
        }
        Ok(true)
    }
}

/// The point size a resize can actually land on, given the envelope the
/// descriptor fixed at creation.
///
/// Pure, and split out for the same reason [`backable_at_scale`] is: the rule is
/// testable without a WindowServer and the `applySettings:` around it is not.
///
/// Both bounds are hard, and they fail in opposite directions. Past `created`
/// nothing refuses — `maxPixels` silently *halves* the result while
/// `applySettings:` still returns YES — so the ceiling is enforced here or not at
/// all. Under [`MIN_WIDTH_POINTS`]x[`MIN_HEIGHT_POINTS`] nothing refuses either,
/// and a client with a 300-point window would be handed a desktop smaller than
/// the dialogs macOS puts on it.
///
/// Per axis, and the aspect ratio is deliberately not preserved: the client asked
/// for its window's shape, and a desktop clamped on one axis is letterboxed by the
/// client exactly as any other answer would be.
///
/// `max` then `min` rather than `u32::clamp`, which panics when the bounds cross.
/// They cannot today — [`VirtualDisplay::create`] floors the created size at these
/// same two constants — and this order's bias is toward the ceiling, which is the
/// bound whose violation is the silent one.
fn size_in_envelope(want: (u32, u32), created: (u32, u32)) -> (u32, u32) {
    (
        want.0.max(MIN_WIDTH_POINTS).min(created.0),
        want.1.max(MIN_HEIGHT_POINTS).min(created.1),
    )
}

/// Poll until `CGDisplayBounds` reports a size the created display can back at
/// [`SCALE`], and return it — or `None` if the deadline passes first.
///
/// Not "until it reports the size we asked for". macOS remembers a mode against a
/// display identity and [`SERIAL`] is fixed on purpose, so a display
/// whose resolution has been changed on the Mac comes back at *that* mode: the
/// remembered arrangement working as intended, and the whole point of a resolution
/// the guest owns. Demanding the created size here rejected exactly that, and the
/// message blamed density — a display set to 1024x640 on the Mac made the agent
/// fall back to the real screen at every launch afterwards.
///
/// What has to hold instead is the thing `maxPixels` decides: at or under the
/// created size macOS can put twice the pixels behind the mode, and past it it
/// provably cannot. That is where a 1x display lands, at twice the request.
fn await_hidpi_bounds(id: u32, points: (u32, u32)) -> Option<(u32, u32)> {
    await_bounds(id, |size| backable_at_scale(size, points))
}

/// Poll `CGDisplayBounds` until it reports a size `accept` likes, and return it —
/// or `None` once [`SETTLE_TIMEOUT_MS`] passes.
///
/// One loop for two questions, because they differ only in what counts as settled
/// and a second copy of the deadline would drift from this one. The two are not
/// interchangeable: creation accepts anything inside the envelope (see
/// [`await_hidpi_bounds`]), while [`VirtualDisplay::set_size`] demands the size it
/// asked for — a ceiling test there would be satisfied by the *old*, smaller
/// bounds on the very first poll of a display that is growing.
fn await_bounds(id: u32, accept: impl Fn((u32, u32)) -> bool) -> Option<(u32, u32)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(SETTLE_TIMEOUT_MS);
    loop {
        let bounds = CGDisplayBounds(id);
        let size = (bounds.size.width as u32, bounds.size.height as u32);
        if accept(size) {
            return Some(size);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(SETTLE_POLL_MS));
    }
}

/// Whether `size` is a mode a display created at `created` points can put
/// [`SCALE`] times the pixels behind.
///
/// The same rule `capture::owned_scale` reads a live mode with, and it shares that
/// rule's one blind spot: a `(low resolution)` 1x mode at or under
/// the created size reads as 2x here too. Nothing can tell them apart — the three
/// geometry reads in the module docs all refuse to — and the failure this check
/// exists for is not that one. A display whose *creation* did not engage HiDPI
/// comes up at twice the request, well past the ceiling.
///
/// Zero on either axis is a display that has not published a configuration yet
/// (or is not there at all), which must not pass for "comfortably under".
fn backable_at_scale(size: (u32, u32), created: (u32, u32)) -> bool {
    size.0 > 0 && size.1 > 0 && size.0 <= created.0 && size.1 <= created.1
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        // Releasing the object is what removes the display; this only says so in
        // the log, because "my second screen vanished" deserves a line.
        info!("virtualdisplay: removing display {}", self.id);
    }
}

/// The serial number this display always reports.
///
/// Fixed, for the same reason a monitor's is: it is burned into the hardware, and
/// macOS files an arrangement against vendor, product and serial so that a screen
/// you plug back in comes back where you left it — same position, same mode, still
/// the primary if that is how you set it up. A display whose identity changed
/// between launches would be a new monitor every time, and would forget all of it.
///
/// So there is nothing here to work around. The arrangement macOS restores at
/// startup is taken as given: the agent reports the geometry it finds and never
/// applies a second configuration of its own accord. Everything downstream follows
/// from that — `active` on the wire is whichever display the Mac currently calls
/// main, and the configured size is only ever an *initial* one (see
/// [`crate::config::Config::virtual_display_initial_size`]), because after the
/// first launch the remembered mode is what the display comes up in.
///
/// A client's [`VirtualDisplay::set_size`] then lands in exactly that remembered
/// mode, which is the intended consequence rather than a leak: a resize asked for
/// from a viewer sticks the way one made in System Settings sticks, and comes back
/// after a restart the way a monitor comes back where you left it. Nothing reverts
/// it on disconnect, and nothing writes it to the config file — the setting stays
/// the envelope, and the display keeps the size it was last put in.
///
/// The one state this cannot undo is an arrangement remembered as *offline* — see
/// [`Offline`], which is reported rather than routed around.
const SERIAL: u32 = 1;

/// A display that was created but that the WindowServer will not bring online.
///
/// Reported rather than worked around, and that is the same answer a real display
/// gets. macOS files arrangement state against a monitor's identity and can decide
/// to keep one offline; the resolution is on the Mac, in System Settings, exactly
/// as it would be for a panel that came back dark. Minting a new identity here
/// would produce a working display by throwing away the arrangement that identity
/// stands for, which is the one thing a monitor never does to you.
#[derive(Debug)]
struct Offline {
    id: u32,
    online: bool,
    active: bool,
}

impl std::fmt::Display for Offline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "display {} was created but macOS will not bring it online (online={}, \
             active={}) — it is holding arrangement state against this display's \
             identity, which only the Mac can clear: open System Settings > Displays, \
             or reset the display arrangement there",
            self.id, self.online, self.active
        )
    }
}

impl std::error::Error for Offline {}

/// Look up a private class, with an error that names it.
fn class(name: &str) -> anyhow::Result<&'static AnyClass> {
    let c_name = std::ffi::CString::new(name)?;
    AnyClass::get(&c_name).ok_or_else(|| {
        anyhow::anyhow!(
            "{name} is not in the Objective-C runtime — the private virtual display API is \
             gone or renamed on this macOS"
        )
    })
}

/// The descriptor: identity, physical size, and the pixel ceiling.
fn descriptor(
    pixels: (u32, u32),
    mm: (f64, f64),
    serial: u32,
) -> anyhow::Result<Retained<AnyObject>> {
    let class = class("CGVirtualDisplayDescriptor")?;
    let descriptor: Retained<AnyObject> = unsafe { msg_send![class, new] };
    let name = NSString::from_str("remotex");

    unsafe {
        let _: () = msg_send![&*descriptor, setName: &*name];
        // The framebuffer ceiling, and the same pixel size the HiDPI check is
        // made against. Asking for more than this later does not fail — it
        // silently halves the result — so it is set to exactly the largest size
        // this display will ever be asked for.
        let _: () = msg_send![&*descriptor, setMaxPixelsWide: pixels.0];
        let _: () = msg_send![&*descriptor, setMaxPixelsHigh: pixels.1];
        let _: () = msg_send![&*descriptor, setSizeInMillimeters: CGSize::new(mm.0, mm.1)];
        // sRGB primaries. macOS wants a colour space and these are the ordinary
        // one; nothing in the capture path depends on the choice.
        let _: () = msg_send![&*descriptor, setRedPrimary: CGPoint::new(0.6800, 0.3200)];
        let _: () = msg_send![&*descriptor, setGreenPrimary: CGPoint::new(0.2650, 0.6900)];
        let _: () = msg_send![&*descriptor, setBluePrimary: CGPoint::new(0.1500, 0.0600)];
        let _: () = msg_send![&*descriptor, setWhitePoint: CGPoint::new(0.3127, 0.3290)];
        // An identity of our own, so this display is tellable apart from a
        // paravirtual one, whose vendor and model are both zero.
        let _: () = msg_send![&*descriptor, setVendorID: 0x7278_u32];
        let _: () = msg_send![&*descriptor, setProductID: 0x6167_u32];
        let _: () = msg_send![&*descriptor, setSerialNum: serial];
    }
    set_queue(&descriptor);
    Ok(descriptor)
}

/// The settings the display is created with: one mode, listed in points, HiDPI on.
fn settings(points: (u32, u32)) -> anyhow::Result<Retained<AnyObject>> {
    settings_at(points, true)
}

/// The same, at a chosen density.
///
/// `hidpi` is the whole difference between the two, and it is what decides how
/// many pixels sit behind the mode: with it on, the mode listed at `points` is
/// backed at [`SCALE`]; with it off, one pixel per point. The mode is listed in
/// **points** either way — listing it at the pixel size instead produces a
/// display of the same point size with no extra pixels, which is the trap this
/// module is arranged around.
fn settings_at(points: (u32, u32), hidpi: bool) -> anyhow::Result<Retained<AnyObject>> {
    let mode_class = class("CGVirtualDisplayMode")?;
    let allocated: Allocated<AnyObject> = unsafe { msg_send![mode_class, alloc] };
    let mode: Retained<AnyObject> = unsafe {
        msg_send![allocated, initWithWidth: points.0, height: points.1, refreshRate: 60.0_f64]
    };
    let modes = NSArray::from_retained_slice(&[mode]);

    let class = class("CGVirtualDisplaySettings")?;
    let settings: Retained<AnyObject> = unsafe { msg_send![class, new] };
    unsafe {
        let _: () = msg_send![&*settings, setHiDPI: u32::from(hidpi)];
        let _: () = msg_send![&*settings, setRotation: 0_u32];
        let _: () = msg_send![&*settings, setModes: &*modes];
    }
    Ok(settings)
}

/// Give the descriptor the main dispatch queue.
///
/// The queue is where the API would call a termination handler. We install none
/// — a display we own goes away when we drop it, and there is no other end for
/// it to come to — but the property is set anyway, because every working
/// example sets it and a nil queue is not a case worth being the first to test.
fn set_queue(descriptor: &Retained<AnyObject>) {
    #[link(name = "System", kind = "dylib")]
    unsafe extern "C" {
        /// `dispatch_get_main_queue()` is a macro over this symbol.
        static _dispatch_main_q: std::ffi::c_void;
    }

    // `AnyObject` is untyped, so the check goes through the runtime like every
    // other call in this module.
    let responds: bool =
        unsafe { msg_send![&**descriptor, respondsToSelector: sel!(setQueue:)] };
    if !responds {
        warn!("virtualdisplay: descriptor has no setQueue: — leaving the queue unset");
        return;
    }
    let queue: *mut AnyObject = (&raw const _dispatch_main_q).cast_mut().cast();
    unsafe {
        let _: () = msg_send![&**descriptor, setQueue: queue];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Creating a display needs a WindowServer, so what is testable here is the
    // rule creation is accepted or rejected by.
    #[test]
    fn a_display_at_or_under_the_size_it_was_created_at_is_2x() {
        let created = (1600, 1000);
        assert!(backable_at_scale(created, created), "the created mode itself");
        // The case that sent the agent back to the real screen at every launch:
        // the Mac had been set to 1024x640, and macOS restores that mode for an
        // identity it has seen.
        assert!(backable_at_scale((1024, 640), created));
        assert!(backable_at_scale((320, 200), created));
    }

    #[test]
    fn a_display_past_it_is_not() {
        let created = (1600, 1000);
        // Twice the request: the signature of HiDPI not engaging at creation,
        // which is the failure this check exists for.
        assert!(!backable_at_scale((3200, 2000), created));
        // One axis is enough — `maxPixels` is a ceiling on both.
        assert!(!backable_at_scale((1601, 1000), created));
        assert!(!backable_at_scale((1600, 1001), created));
    }

    // A display that has not published a configuration yet reads 0x0, which is
    // arithmetically "under" the created size and must not settle the poll.
    #[test]
    fn nothing_published_yet_is_not_a_settled_display() {
        assert!(!backable_at_scale((0, 0), (1600, 1000)));
        assert!(!backable_at_scale((1600, 0), (1600, 1000)));
        assert!(!backable_at_scale((0, 1000), (1600, 1000)));
    }

    // The ceiling `maxPixels` enforces by silently halving, so a request past it
    // is brought back here or discovered later as a mysteriously half-size
    // desktop that `applySettings:` said yes to.
    #[test]
    fn a_resize_past_the_created_size_is_clamped_to_it() {
        let created = (1600, 1000);
        assert_eq!(size_in_envelope((1920, 1200), created), created);
        // One axis over is enough, and only that axis moves: the aspect ratio is
        // the client's window's, not something to preserve on its behalf.
        assert_eq!(size_in_envelope((1601, 900), created), (1600, 900));
        assert_eq!(size_in_envelope((1200, 1001), created), (1200, 1000));
        // The largest thing a u16 of points can say still lands on the ceiling.
        assert_eq!(size_in_envelope((65_535, 65_535), created), created);
    }

    #[test]
    fn a_resize_under_the_floor_is_clamped_up_to_it() {
        let created = (1600, 1000);
        assert_eq!(
            size_in_envelope((320, 200), created),
            (MIN_WIDTH_POINTS, MIN_HEIGHT_POINTS)
        );
        // A client whose window has not laid out yet reports nothing at all.
        assert_eq!(size_in_envelope((0, 0), created), (800, 600));
        assert_eq!(size_in_envelope((1200, 100), created), (1200, 600));
    }

    #[test]
    fn a_resize_inside_the_envelope_is_left_alone() {
        let created = (1600, 1000);
        assert_eq!(size_in_envelope((1280, 800), created), (1280, 800));
        // Both bounds are inclusive, so neither is an off-by-one.
        assert_eq!(size_in_envelope(created, created), created);
        assert_eq!(size_in_envelope((800, 600), created), (800, 600));
    }

    // What ties the clamp to the rest of the module: every size a resize can
    // reach is one the display can put SCALE times the pixels behind, so a resize
    // is never what makes `capture::owned_scale` read a mode as 1x.
    #[test]
    fn every_size_a_resize_can_reach_is_inside_the_hidpi_ceiling() {
        let created = (1600, 1000);
        for want in [
            (0, 0),
            (1, 1),
            (640, 480),
            (1280, 800),
            (1600, 1000),
            (9999, 9999),
        ] {
            assert!(
                backable_at_scale(size_in_envelope(want, created), created),
                "{want:?}"
            );
        }
    }

    // `create` floors the created size at the same two constants, so the bounds
    // cannot cross today. If they ever did, the ceiling must win: past it the
    // WindowServer lies about what it did, under the floor the desktop is merely
    // small.
    #[test]
    fn the_ceiling_wins_if_the_two_bounds_ever_crossed() {
        assert_eq!(size_in_envelope((1000, 1000), (400, 300)), (400, 300));
    }
}
