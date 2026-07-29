import Foundation

/// Maps measured windows to viewport messages.
///
/// VNC may follow continuously; RDP is request-only; RXA is request-only while
/// sharing an agent-created display. The pure policy keeps those differences
/// out of socket and UI code.
struct ViewportPolicy: Equatable {
    /// Suppress automatic reports; only an explicit request gets through. RDP
    /// with `resize`, and rxa with `resize` while sharing an agent-made display.
    var manualOnly = false
    /// Send nothing at all. Every rxa target until — and unless — the display
    /// being shared turns out to be one the agent made.
    var ignoresViewport = false

    /// Whether the remote continuously follows the window. Stored separately
    /// because the two gating flags do not distinguish resize-disabled targets.
    private(set) var followsWindow = false

    /// The target's `resize`, for an rxa target only.
    ///
    /// Held rather than acted on: it is the operator's half of the permission and
    /// grants nothing by itself, because what the agent can resize is a display it
    /// *made*. `sharing(virtualDisplay:)` supplies the other half. Left false for
    /// RDP and VNC, whose behaviour a display list must not be able to touch.
    private var rxaResizeAllowed = false

    /// The last size sent on this connection, for the dedupe below.
    private var lastSent: DisplayMode?

    init(manualOnly: Bool = false, ignoresViewport: Bool = false) {
        self.manualOnly = manualOnly
        self.ignoresViewport = ignoresViewport
    }

    /// Derive initial policy. RXA stays disabled until `displays` identifies an
    /// owned display.
    init(protocolName: String, resize: Bool) {
        ignoresViewport = protocolName == "rxa"
        manualOnly = protocolName == "rdp" && resize
        followsWindow = protocolName == "vnc" && resize
        rxaResizeAllowed = protocolName == "rxa" && resize
    }

    /// Enable request-only RXA resizing only for an owned display. No-op for
    /// other protocols.
    mutating func sharing(virtualDisplay isVirtual: Bool) {
        guard rxaResizeAllowed else {
            return
        }
        ignoresViewport = !isVirtual
        manualOnly = isVirtual
    }

    /// The message to send for a measured window, or nil for none.
    ///
    /// `manual` marks a report the user asked for ("Resize to Window"), which is
    /// the only kind that gets past `manualOnly`.
    mutating func report(_ size: DisplayMode, manual: Bool) -> ClientMessage? {
        guard !ignoresViewport else {
            return nil
        }
        guard manual || !manualOnly else {
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
