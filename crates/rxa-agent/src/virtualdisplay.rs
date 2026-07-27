//! A display of our own, from the private `CGVirtualDisplay` API.
//!
//! The Mac's real screen belongs to whoever is sitting at it: it cannot be
//! resized without rearranging their windows, and on a VM guest it has no HiDPI
//! mode at any size, so a Retina browser draws it magnified and soft. A display
//! we create ourselves has neither problem — we choose its pixel size, and we
//! choose to have it drawn at 2x.
//!
//! ## The API is private, and it lies about the result
//!
//! There is no entitlement and no compatibility promise; see
//! `docs/roadmap.md`. What matters more day to day is that a *wrong*
//! configuration does not fail — it silently produces a 1x display, and three
//! of the obvious ways to check report success anyway:
//!
//! - `CGDisplayBounds` gives the intended **point** size whether or not the
//!   backing store is 2x, so it cannot tell the two apart on its own. What it
//!   *can* do is catch the failure: a display that dropped to 1x comes back at
//!   *twice* the requested point size, which is past the ceiling `maxPixels`
//!   fixes — so bounds at or under the created size mean HiDPI engaged. That is
//!   the check [`await_hidpi_bounds`] makes, by the same rule
//!   [`crate::capture::owned_scale`] reads a live mode with.
//! - `CGDisplayCopyDisplayMode` returns NULL and `CGDisplayCopyAllDisplayModes`
//!   returns nothing, so the usual geometry reads do not work here at all —
//!   which is why `capture.rs` has a separate path for these displays.
//! - `SCContentFilter.pointPixelScale` reports 1.00 on a genuine 2x display, so
//!   capture size must be set from what we asked for, never derived from it.
//!
//! ## Created once, and then it is macOS's
//!
//! Nothing here resizes the display, because nothing outside the Mac decides its
//! resolution. It is created at the configured size and appears in System
//! Settings > Displays like any other screen, with the mode list macOS derives
//! from the descriptor — including a `(low resolution)` 1x entry beside each
//! HiDPI one. Whoever is using the Mac changes it there; the agent notices the
//! new size the same way it notices one on a real display, and tells the gateway
//! (`capture::Capture::follow_display`).
//!
//! What the descriptor fixes forever is the room that list has to work in.
//! `sizeInMillimeters` and `maxPixels` cannot be changed afterwards, and HiDPI
//! engages only while pixel density — mode pixels over physical size — stays
//! inside a window measured at roughly 149 to 264 dpi on macOS 26.5.2. The
//! display is created with its density at the *top* of that window ([`MAX_DPI`]),
//! which is also where `maxPixels` sits, so the configured size is the largest
//! mode that can be 2x and everything macOS offers below it has density to
//! spare.

use log::{info, warn};
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

/// Floor for a created display, in points.
///
/// Nothing enforces a minimum in the API — this is here so a nonsensical
/// configured size cannot ask for a 2x1 desktop. Public because
/// [`crate::config`] validates against the same number: a size the config
/// accepts and the clamp below then quietly changes would be a saved setting
/// that does not describe the display.
pub const MIN_POINTS: u32 = 320;

/// How long to wait for the WindowServer to publish a new configuration.
///
/// Applying settings is asynchronous: `applySettings:` returns immediately and
/// `CGDisplayBounds` catches up a moment later. Measured at 134–580 ms across
/// seven live resizes, so a second and a half is generous without being a hang.
const SETTLE_TIMEOUT_MS: u64 = 1500;
const SETTLE_POLL_MS: u64 = 25;

/// A live virtual display. Dropping this removes it from the desktop.
///
/// The display belongs to the process that created it, which is the whole
/// containment story: an agent that crashes cannot leave a display behind, and
/// there is no cleanup path to get wrong.
pub struct VirtualDisplay {
    /// `CGVirtualDisplay`. Nothing calls into it after creation — it is held so
    /// that dropping it removes the display.
    _handle: Handle,
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
        let points = (points.0.max(MIN_POINTS), points.1.max(MIN_POINTS));
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
            _handle: handle,
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(SETTLE_TIMEOUT_MS);
    loop {
        let bounds = CGDisplayBounds(id);
        let size = (bounds.size.width as u32, bounds.size.height as u32);
        if backable_at_scale(size, points) {
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
/// The same rule [`crate::capture::owned_scale`] reads a live mode with, and it
/// shares that rule's one blind spot: a `(low resolution)` 1x mode at or under
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
/// So there is nothing here to work around. The arrangement macOS restores is the
/// one last set in System Settings by whoever uses that Mac, and the agent takes
/// it as given: it reports the geometry it finds and never applies a second
/// configuration. Everything downstream follows from that — `active` on the wire
/// is whichever display the Mac currently calls main, and the configured size is
/// only ever an *initial* one (see
/// [`crate::config::Config::virtual_display_initial_size`]), because after the
/// first launch the remembered mode is what the display comes up in.
///
/// The one state this cannot undo is an arrangement remembered as *offline* — see
/// [`Offline`], which is reported rather than routed around, and
/// `docs/known-issues.md`.
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

/// The settings: one mode, listed in points, with HiDPI asked for.
fn settings(points: (u32, u32)) -> anyhow::Result<Retained<AnyObject>> {
    let mode_class = class("CGVirtualDisplayMode")?;
    let allocated: Allocated<AnyObject> = unsafe { msg_send![mode_class, alloc] };
    let mode: Retained<AnyObject> = unsafe {
        msg_send![allocated, initWithWidth: points.0, height: points.1, refreshRate: 60.0_f64]
    };
    let modes = NSArray::from_retained_slice(&[mode]);

    let class = class("CGVirtualDisplaySettings")?;
    let settings: Retained<AnyObject> = unsafe { msg_send![class, new] };
    unsafe {
        let _: () = msg_send![&*settings, setHiDPI: 1_u32];
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
}
