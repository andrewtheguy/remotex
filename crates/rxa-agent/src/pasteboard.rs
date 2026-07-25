//! The Mac's general pasteboard, read and written on demand.
//!
//! Used by two callers with nothing else in common: the menu bar's "Copy
//! pre-shared key", and the clipboard bridge, where the gateway asks for the
//! pasteboard text ([`rxa_proto::msg::GatewayMsg::ClipboardRequest`]) or hands
//! over text to put on it.
//!
//! **Only on request.** There is no poller here, and that is deliberate:
//! watching the pasteboard means reading its *contents* on a timer, and recent
//! macOS surfaces every programmatic content read to the user as a paste
//! notification. One read per button press is both quieter and cheaper than a
//! timer that usually finds nothing changed.
//!
//! Both functions are plain synchronous calls that drop the `NSPasteboard`
//! before returning, so no non-`Send` Objective-C object is ever held across an
//! await in the session task. `NSPasteboard` is thread-safe and needs no main
//! thread marker, so the session task can call these directly.

use log::{info, warn};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

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
