import Foundation

/// Maps measured windows to viewport messages.
///
/// Two questions, and keeping them apart is the whole of this type.
///
/// *May* this session resize the remote — the operator's `resize` on the target.
/// That is the gateway's answer and this client cannot argue with it. It also
/// answers a second one: whether the window may drive the size *unasked*, which is
/// plain `vnc` alone. RDP and Apple Screen Sharing take a resize the user asked
/// for and not a stream of them (see docs/known-issues.md), so on those the window
/// never reports and "Resize to Window" still does.
///
/// *How* — continuously as the window changes, or only when the user asks. That is
/// this client's answer, within what the above allows. `autoFollows` starts false
/// and is asked again for every connection, so connecting never reshapes a remote's
/// desktop unasked.
struct ViewportPolicy: Equatable {
    /// Whether this session may resize the remote at all. Nothing is reported
    /// while false, asked for or not.
    private(set) var allowed = false

    /// Whether this session may be put in auto mode at all. The gateway's, not the
    /// user's, and the reason `autoFollows` is not simply settable.
    private(set) var autoAllowed = false

    /// Whether the remote follows this window unasked. The user's choice, per
    /// session; every connection starts manual, and it cannot be turned on where
    /// the gateway did not allow the mode.
    private(set) var autoFollows = false

    /// The last size sent on this connection, for the dedupe below.
    private var lastSent: DisplayMode?

    init() {}

    /// Derive both permissions from the connect status, settled there for every
    /// protocol.
    init(resize: Bool, autoResize: Bool) {
        allowed = resize
        autoAllowed = resize && autoResize
    }

    /// Set the mode, and report whether it took. Turning it *on* is refused where
    /// the gateway did not allow the mode — this is the single gate, so the menu
    /// item, the remembered default and the connect-time seed cannot each need
    /// their own. Turning it off is never refused.
    @discardableResult
    mutating func setAutoFollows(_ enabled: Bool) -> Bool {
        guard enabled else {
            autoFollows = false
            return true
        }
        guard autoAllowed else {
            return false
        }
        autoFollows = true
        return true
    }

    /// The message to send for a measured window, or nil for none.
    ///
    /// `manual` marks a report the user asked for — "Resize to Window", or
    /// switching auto on — which is the only kind that goes out in manual mode.
    mutating func report(_ size: DisplayMode, manual: Bool) -> ClientMessage? {
        guard allowed else {
            return nil
        }
        guard manual || autoFollows else {
            return nil
        }
        // Deduped so dragging a window edge does not send one report per frame to
        // an engine that acts on every one of them.
        guard lastSent != size else {
            return nil
        }
        lastSent = size
        return .viewport(w: size.w, h: size.h)
    }

    /// Forget the per-connection dedupe. Display switches retain it.
    mutating func resetForNewConnection() {
        lastSent = nil
    }
}
