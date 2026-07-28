//! The Mac's general pasteboard, read and written on demand.
//!
//! Used by two callers with nothing else in common: the settings dialog's Copy
//! button, for this Mac's public key, and the clipboard bridge, where the
//! gateway asks for the pasteboard text
//! ([`rxa_proto::msg::GatewayMsg::ClipboardRequest`]) or hands over text to put
//! on it.
//!
//! **Contents are read as rarely as possible.** AppKit has no change
//! notification for `NSPasteboard` — unlike iOS, where `UIPasteboard` posts
//! one — so [`change_count`] is the only signal there is, and the watcher in
//! [`crate::session`] polls it. That poll is the cheap half: `changeCount` is a
//! counter, and reading it is not a pasteboard access. Only when it moves does
//! anything call [`read`], so the Mac's contents are touched once per actual
//! copy rather than once per tick.
//!
//! That distinction matters because macOS 15.4+ governs programmatic content
//! reads with [`access_behavior`]: the general pasteboard asks the user by
//! default, and only after the first alert does the app appear in System
//! Settings where "Always Allow" can be chosen. See
//! `docs/mac-agent-architecture.md`.
//!
//! All functions are plain synchronous calls that drop the `NSPasteboard`
//! before returning, so no non-`Send` Objective-C object is ever held across an
//! await in the session task. `NSPasteboard` is thread-safe and needs no main
//! thread marker, so the session task can call these directly.

use log::{info, warn};
use objc2_app_kit::{NSPasteboard, NSPasteboardAccessBehavior, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// The pasteboard's change counter. Bumped by every write, by any app.
///
/// Reading this is *not* a content read: it never trips a paste alert and
/// costs nothing worth measuring, which is what makes polling it acceptable
/// where polling [`read`] would not be.
pub fn change_count() -> isize {
    NSPasteboard::generalPasteboard().changeCount()
}

/// Whether macOS will let this app read the pasteboard without bothering the
/// user, as far as it will say. macOS 15.4+, which is the project's floor.
///
/// Read-only by design — an app cannot grant itself paste access. This exists
/// so the menu bar can explain a symptom the user would otherwise have to
/// guess at: clipboard sync that prompts on every copy, or silently returns
/// nothing.
pub fn access_behavior() -> NSPasteboardAccessBehavior {
    NSPasteboard::generalPasteboard().accessBehavior()
}

/// A short human-readable form of [`access_behavior`] for the menu bar, or
/// `None` when there is nothing worth saying (the user has already allowed it,
/// or has not yet been asked).
pub fn access_warning() -> Option<&'static str> {
    match access_behavior() {
        // Never asked yet: the first read will prompt, and only then does the
        // app appear in System Settings. Nothing actionable to say in advance.
        NSPasteboardAccessBehavior::Default => None,
        NSPasteboardAccessBehavior::AlwaysAllow => None,
        NSPasteboardAccessBehavior::Ask => {
            Some("Paste access: asks each time — set to Allow in System Settings")
        }
        NSPasteboardAccessBehavior::AlwaysDeny => {
            Some("Paste access: denied — clipboard reads return nothing")
        }
        // The enum is `NSInteger`-backed, so a future macOS can add a case.
        _ => None,
    }
}

/// The pasteboard's current text, or `None` when it holds no string at all
/// (an image, a file promise, or nothing).
pub fn read() -> Option<String> {
    let pasteboard = NSPasteboard::generalPasteboard();
    let text = unsafe { pasteboard.stringForType(NSPasteboardTypeString) };
    text.map(|s| s.to_string())
}

/// Replace the pasteboard's contents with `text`. Returns whether it took —
/// `setString:forType:` can refuse, and the caller decides whether that is
/// worth reporting.
///
/// Bumps [`change_count`], so a caller that is also watching must re-baseline
/// afterwards or it will read its own write straight back out.
pub fn write(text: &str) -> bool {
    let pasteboard = NSPasteboard::generalPasteboard();
    // Required before writing: without it the new item is merged into whatever
    // types are already on the pasteboard.
    pasteboard.clearContents();
    let wrote =
        unsafe { pasteboard.setString_forType(&NSString::from_str(text), NSPasteboardTypeString) };
    if wrote {
        info!("pasteboard: wrote {} bytes", text.len());
    } else {
        warn!("pasteboard: refused a {}-byte write", text.len());
    }
    wrote
}
