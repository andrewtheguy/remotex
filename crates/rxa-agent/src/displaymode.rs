//! Changing the shared display's resolution, for virtual displays only.
//!
//! ## Why this is gated so hard
//!
//! A Mac has one console session, so "resize the desktop" means "change the
//! resolution of a real panel" — rearranging the screen of whoever is sitting at
//! it, from a browser, possibly on the other side of the world. That is never
//! wanted, and there is no undo. The guard in [`resizable`] is what makes the
//! feature safe to expose at all: it must be a paravirtual display *and* the
//! machine must be a VM guest before a single mode switch is attempted.
//!
//! ## What a virtual display looks like
//!
//! Measured on an Apple Virtualization guest (UTM) against a real Mac:
//!
//! | | VM display | real panel |
//! |---|---|---|
//! | `CGDisplayVendorNumber` | `0` | `0x610` (Apple) |
//! | `CGDisplayModelNumber` | `0` | `0xa050` |
//! | `CGDisplayIsBuiltin` | false | true |
//! | EDID in IORegistry | absent | present |
//!
//! Vendor and model are both zero because there is no EDID to read them from,
//! which is exactly the property a paravirtual framebuffer has and a real panel
//! does not. `hw.model` (`VirtualMac2,1`) and `kern.hv_vmm_present` then confirm
//! the machine itself.
//!
//! ## What the guest can and cannot do
//!
//! The guest can switch between the modes the virtual display advertises, and
//! that works whether or not the host has "dynamic resolution" turned on — it is
//! plain `CGDisplaySetDisplayMode`, not a host feature. What the guest *cannot*
//! do is ask for an arbitrary size: `1200x700` lands on the nearest advertised
//! mode. Arbitrary sizes only ever come from the host resizing its window, and
//! Virtualization.framework has no guest→host channel to request one. Hence a
//! list the browser picks from rather than a viewport the agent follows.
//!
//! ## The wedge
//!
//! `CGCompleteDisplayConfiguration` can hang forever on a VM whose display stack
//! has gotten stuck — spinning at ~40% CPU, with only a reboot to clear it. This
//! module therefore reports errors instead of retrying indefinitely, and callers
//! run [`apply`] somewhere a hang cannot take the session with it (see
//! `session.rs`).

use std::ptr::NonNull;

use log::{debug, info, warn};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGBeginDisplayConfiguration, CGCancelDisplayConfiguration, CGCompleteDisplayConfiguration,
    CGConfigureDisplayWithDisplayMode, CGConfigureOption, CGDisplayCopyAllDisplayModes,
    CGDisplayCopyDisplayMode, CGDisplayIsBuiltin, CGDisplayModelNumber, CGDisplayMode,
    CGDisplayVendorNumber, CGError,
};

#[link(name = "System", kind = "dylib")]
unsafe extern "C" {
    /// `sysctlbyname(3)`. Declared here rather than pulling in a libc
    /// dependency for two reads, matching how `capture.rs` declares the
    /// CoreGraphics permission calls it needs.
    fn sysctlbyname(
        name: *const std::ffi::c_char,
        oldp: *mut std::ffi::c_void,
        oldlenp: *mut usize,
        newp: *mut std::ffi::c_void,
        newlen: usize,
    ) -> i32;
}

/// One resolution the display will accept, in captured pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u16,
    pub height: u16,
}

impl Mode {
    fn area(self) -> u32 {
        u32::from(self.width) * u32::from(self.height)
    }
}

/// Whether `display` may have its resolution changed on request.
///
/// Both halves are required: the display must have no EDID identity (a
/// paravirtual framebuffer) *and* the machine must be a VM guest. Either alone
/// would be enough in practice, but the cost of a false positive here is
/// rearranging someone's real monitor, and the cost of a false negative is a
/// missing menu — so both.
pub fn resizable(display: u32) -> bool {
    let vendor = CGDisplayVendorNumber(display);
    let model = CGDisplayModelNumber(display);
    let builtin = CGDisplayIsBuiltin(display);
    let paravirtual = vendor == 0 && model == 0 && !builtin;
    let vm = in_vm();
    // Logged with the numbers behind the verdict: "why is there no resolution
    // list?" is otherwise an unanswerable question from a log.
    info!(
        "displaymode: display {display} resizable={} (vendor={vendor:#x} model={model:#x} \
         builtin={builtin} vm={vm})",
        paravirtual && vm
    );
    paravirtual && vm
}

/// Whether this machine is a virtual machine guest.
fn in_vm() -> bool {
    if sysctl_string("hw.model").is_some_and(|m| m.starts_with("VirtualMac")) {
        return true;
    }
    // Covers guests that are not Apple Virtualization (and future model names).
    // Absent on some hosts, which reads as "not a VM" — the right default.
    sysctl_u32("kern.hv_vmm_present") == Some(1)
}

/// The resolutions the display will accept, largest first, one entry per size.
///
/// Re-read on every call rather than cached: a host-driven resize regenerates
/// the list (a VM window drag turned `800x500 … 1280x800` into
/// `800x514 … 1290x830`), so a cached list is wrong the moment it matters.
pub fn modes(display: u32) -> Vec<Mode> {
    let mut sizes: Vec<Mode> = ranked(display).into_iter().map(|c| c.mode).collect();
    // `ranked` keeps every encoding of a size because `apply` tries them in
    // turn; the browser is offered each size once.
    sizes.dedup();
    sizes
}

/// Switch `display` to `w`x`h`, or to the closest mode it advertises.
///
/// Returns the size actually applied, which the caller should not need — the
/// capture stream reports the new surface size on its own — but which makes the
/// log line say what happened.
///
/// May block for a long time; see the wedge note in the module docs.
pub fn apply(display: u32, w: u16, h: u16) -> anyhow::Result<Mode> {
    let candidates = ranked(display);
    anyhow::ensure!(!candidates.is_empty(), "display {display} advertises no usable modes");

    let want = Mode { width: w, height: h };
    let chosen = choose(&candidates, want);
    let target = candidates[chosen].mode;
    if let Some(current) = current(display)
        && current == target
    {
        debug!("displaymode: display {display} is already {}x{}", target.width, target.height);
        return Ok(target);
    }

    // Walk from the chosen candidate through the remaining same-size ones. The
    // mode list contains duplicates of each resolution that differ only in
    // pixel encoding, and the display rejects some of them outright (a 64-bit
    // HDR variant fails with kCGErrorRangeCheck), so "this size does not work"
    // is only true once every variant of it has been refused.
    let mut last_err = None;
    for candidate in candidates[chosen..].iter().filter(|c| c.mode == target) {
        match set(display, &candidate.cg) {
            Ok(()) => {
                info!(
                    "displaymode: display {display} set to {}x{} (asked for {w}x{h})",
                    target.width, target.height
                );
                return Ok(target);
            }
            Err(e) => {
                warn!("displaymode: {}x{} rejected: {e}", target.width, target.height);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no applicable mode for {w}x{h}")))
}

/// A candidate mode: the size the browser sees, plus the CoreGraphics mode to
/// hand back to the display configuration.
struct Candidate {
    mode: Mode,
    cg: CFRetained<CGDisplayMode>,
}

/// Pick the index of the best candidate for `want`.
///
/// Exact match first; otherwise the largest mode that fits inside the request,
/// so a browser asking for something between two modes gets the one that will
/// not need scrollbars; otherwise the smallest mode there is. `candidates` must
/// be non-empty and sorted largest first.
fn choose(candidates: &[Candidate], want: Mode) -> usize {
    if let Some(i) = candidates.iter().position(|c| c.mode == want) {
        return i;
    }
    candidates
        .iter()
        .position(|c| c.mode.width <= want.width && c.mode.height <= want.height)
        .unwrap_or(candidates.len() - 1)
}

/// Every usable mode, ordered largest first.
///
/// One size can appear several times — the same resolution is advertised in
/// more than one pixel encoding, and some of those the display refuses — so all
/// of them are kept and [`apply`] tries them in turn.
fn ranked(display: u32) -> Vec<Candidate> {
    // `None` options rather than kCGDisplayShowDuplicateLowResolutionModes: the
    // extra entries that flag adds are scaled duplicates of sizes already in
    // the list, and offering the same resolution twice in the browser's menu
    // helps nobody.
    let Some(list) = (unsafe { CGDisplayCopyAllDisplayModes(display, None) }) else {
        warn!("displaymode: cannot list modes for display {display}");
        return Vec::new();
    };

    let mut candidates = Vec::with_capacity(list.count() as usize);
    for i in 0..list.count() {
        // SAFETY: the array came from CGDisplayCopyAllDisplayModes, so every
        // element is a CGDisplayMode, and it is not mutated while we read it.
        let Some(ptr) = NonNull::new(unsafe { list.value_at_index(i) }.cast_mut()) else {
            continue;
        };
        let mode: CFRetained<CGDisplayMode> = unsafe { CFRetained::retain(ptr.cast()) };

        // Modes the display advertises but will not run a desktop on (the VM
        // lists 640x400 and 640x480 this way) are not offerable.
        if !CGDisplayMode::is_usable_for_desktop_gui(Some(&mode)) {
            continue;
        }
        let (Ok(width), Ok(height)) = (
            u16::try_from(CGDisplayMode::pixel_width(Some(&mode))),
            u16::try_from(CGDisplayMode::pixel_height(Some(&mode))),
        ) else {
            continue;
        };
        if width == 0 || height == 0 {
            continue;
        }
        candidates.push(Candidate {
            mode: Mode { width, height },
            cg: mode,
        });
    }

    // Largest first; within one size, the highest refresh rate first. Which
    // encoding of a size the display will actually accept is not knowable from
    // here — CoreGraphics stopped exposing pixel depth — so `apply` finds out
    // by trying.
    candidates.sort_by(|a, b| {
        b.mode.area().cmp(&a.mode.area()).then_with(|| {
            b.mode.width.cmp(&a.mode.width).then_with(|| {
                CGDisplayMode::refresh_rate(Some(&b.cg))
                    .total_cmp(&CGDisplayMode::refresh_rate(Some(&a.cg)))
            })
        })
    });
    candidates
}

/// The display's current mode, if it has one. `None` during a reconfigure —
/// CoreGraphics briefly reports no mode at all while the host resizes a virtual
/// display.
fn current(display: u32) -> Option<Mode> {
    let mode = CGDisplayCopyDisplayMode(display)?;
    Some(Mode {
        width: u16::try_from(CGDisplayMode::pixel_width(Some(&mode))).ok()?,
        height: u16::try_from(CGDisplayMode::pixel_height(Some(&mode))).ok()?,
    })
}

/// Run one begin/configure/complete cycle.
///
/// `ForSession` rather than `Permanently`: the resolution a browser asked for is
/// this session's business, and writing it into the machine's display
/// preferences would outlive the session that wanted it.
fn set(display: u32, mode: &CGDisplayMode) -> anyhow::Result<()> {
    let mut config = std::ptr::null_mut();
    let err = unsafe { CGBeginDisplayConfiguration(&mut config) };
    check(err, "CGBeginDisplayConfiguration")?;

    let err = unsafe { CGConfigureDisplayWithDisplayMode(config, display, Some(mode), None) };
    if let Err(e) = check(err, "CGConfigureDisplayWithDisplayMode") {
        // Drop the half-built configuration rather than leaking it — the next
        // attempt begins a fresh one.
        unsafe { CGCancelDisplayConfiguration(config) };
        return Err(e);
    }

    let err = unsafe { CGCompleteDisplayConfiguration(config, CGConfigureOption::ForSession) };
    check(err, "CGCompleteDisplayConfiguration")
}

fn check(err: CGError, what: &str) -> anyhow::Result<()> {
    if err == CGError::Success {
        Ok(())
    } else {
        Err(anyhow::anyhow!("{what} failed with CGError {}", err.0))
    }
}

/// Read a string sysctl, or `None` if it does not exist.
fn sysctl_string(name: &str) -> Option<String> {
    let c_name = std::ffi::CString::new(name).ok()?;
    let mut len = 0usize;
    let rc = unsafe {
        sysctlbyname(
            c_name.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    let rc = unsafe {
        sysctlbyname(
            c_name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(len);
    // The value is NUL-terminated; everything from the first NUL is padding.
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

/// Read an integer sysctl, or `None` if it does not exist.
fn sysctl_u32(name: &str) -> Option<u32> {
    let c_name = std::ffi::CString::new(name).ok()?;
    let mut value = 0u32;
    let mut len = std::mem::size_of::<u32>();
    let rc = unsafe {
        sysctlbyname(
            c_name.as_ptr(),
            std::ptr::from_mut(&mut value).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<u32>()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `choose` is the whole of the "predetermined supported resolution"
    // promise: whatever the browser sends, the applied size is one the display
    // advertised. Exercised against a synthetic list because the real one comes
    // from a display this test host may not have.
    fn sizes(list: &[(u16, u16)]) -> Vec<Mode> {
        list.iter()
            .map(|&(width, height)| Mode { width, height })
            .collect()
    }

    // choose() needs Candidates, which own CoreGraphics objects; the selection
    // logic itself only reads `mode`, so mirror it over plain sizes.
    fn choose_size(list: &[Mode], want: Mode) -> Mode {
        if let Some(m) = list.iter().find(|&&m| m == want) {
            return *m;
        }
        *list
            .iter()
            .find(|m| m.width <= want.width && m.height <= want.height)
            .unwrap_or(list.last().unwrap())
    }

    /// What the test VM's virtual display advertised after the host resized it,
    /// in the order [`ranked`] produces (largest area first).
    const VM_LIST: [(u16, u16); 7] = [
        (1280, 960),
        (1290, 830),
        (1280, 824),
        (1024, 768),
        (1024, 658),
        (800, 600),
        (800, 514),
    ];

    #[test]
    fn an_advertised_size_is_taken_exactly() {
        let list = sizes(&VM_LIST);
        for &mode in &list {
            assert_eq!(choose_size(&list, mode), mode);
        }
    }

    #[test]
    fn a_size_between_modes_falls_to_the_largest_that_fits() {
        let list = sizes(&VM_LIST);
        // The measured case: asking the VM for 1200x700 landed on 1024x658 —
        // the largest advertised mode fitting inside the request. 1024x768 is
        // larger but 68 pixels too tall.
        assert_eq!(
            choose_size(&list, Mode { width: 1200, height: 700 }),
            Mode { width: 1024, height: 658 }
        );
        // Between two modes of the same width, height decides.
        assert_eq!(
            choose_size(&list, Mode { width: 1285, height: 900 }),
            Mode { width: 1280, height: 824 },
            "1290x830 is wider than 1285 and 1280x960 is taller than 900"
        );
    }

    #[test]
    fn a_request_smaller_than_every_mode_falls_to_the_smallest() {
        let list = sizes(&VM_LIST);
        assert_eq!(
            choose_size(&list, Mode { width: 320, height: 200 }),
            Mode { width: 800, height: 514 }
        );
    }

    // The guard's two halves are read from the machine this runs on, so the
    // only assertion that holds everywhere is that they agree with each other:
    // a physical Mac must never be reported resizable.
    #[test]
    fn a_builtin_display_is_never_resizable() {
        let main = objc2_core_graphics::CGMainDisplayID();
        if CGDisplayIsBuiltin(main) {
            assert!(!resizable(main), "a builtin panel must never be resizable");
        }
    }

    #[test]
    fn hw_model_is_readable_and_kern_nonsense_is_not() {
        assert!(
            sysctl_string("hw.model").is_some_and(|m| !m.is_empty()),
            "every Mac has hw.model"
        );
        assert_eq!(sysctl_string("kern.no.such.sysctl"), None);
        assert_eq!(sysctl_u32("kern.no.such.sysctl"), None);
    }
}
