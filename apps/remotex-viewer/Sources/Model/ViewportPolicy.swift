import Foundation

/// Maps measured windows to viewport messages.
///
/// Two questions, and keeping them apart is the whole of this type.
///
/// *May* this session resize the remote — the operator's `resize` on the target.
/// That is the gateway's answer and this client cannot argue with it.
///
/// *How* — continuously as the window changes, or only when the user asks. That is
/// this client's answer, and it is the same question on every protocol: an engine
/// that acts on one viewport report acts on all of them. `autoFollows` starts false
/// and is asked again for every connection, so connecting never reshapes a remote's
/// desktop unasked.
struct ViewportPolicy: Equatable {
    /// Whether this session may resize the remote at all. Nothing is reported
    /// while false, asked for or not.
    private(set) var allowed = false

    /// Whether the remote follows this window unasked. The user's choice, per
    /// session; every connection starts manual.
    var autoFollows = false

    /// The last size sent on this connection, for the dedupe below.
    private var lastSent: DisplayMode?

    init() {}

    /// Derive the permission: the target's `resize`, settled at connect for every
    /// protocol.
    init(resize: Bool) {
        allowed = resize
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
