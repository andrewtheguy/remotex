import Foundation

/// Decides whether a measured window becomes a `viewport` message.
///
/// Its own type because this is the one place three protocols disagree, and left
/// inline it would rot into `if protocolName ==` checks spread across the model.
/// Pure, so all three behaviours are testable without a socket.
///
/// The three, as `frontend/src/useRemoteDesktop.ts` picks between them:
///
/// - **VNC** follows the window continuously. It is the only engine that can
///   resize cheaply.
/// - **RDP** resizes only when the user asks, because a resize forces a
///   Deactivation-Reactivation — an expensive, visible renegotiation.
/// - **rxa** resizes only when the user asks, and only a display the *agent
///   made* for the purpose. A Mac's own panel is set on that Mac, in System
///   Settings, and no message asks it to change — so which of the two is being
///   shared decides whether the request may be made at all, and that is not
///   known until a `displays` arrives.
struct ViewportPolicy: Equatable {
    /// Suppress automatic reports; only an explicit request gets through. RDP
    /// with `resize`, and rxa with `resize` while sharing an agent-made display.
    var manualOnly = false
    /// Send nothing at all. Every rxa target until — and unless — the display
    /// being shared turns out to be one the agent made.
    var ignoresViewport = false

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

    /// Derive the policy from a `connected` message.
    ///
    /// The whole decision for VNC and RDP. For rxa it is only half of one: the
    /// session starts out ignoring viewports and may gain a manual report later,
    /// when a `displays` says which screen is being shared. That is the one thing
    /// about this type a `connected` no longer settles, and it is why the update
    /// below exists rather than a second initializer.
    init(protocolName: String, resize: Bool) {
        ignoresViewport = protocolName == "rxa"
        manualOnly = protocolName == "rdp" && resize
        rxaResizeAllowed = protocolName == "rxa" && resize
    }

    /// A `displays` arrived: `isVirtual` says whether the display now being
    /// shared is one the agent made rather than one of the Mac's own screens.
    ///
    /// Only an agent-made display can be resized from here — a real panel's mode
    /// is changed on the Mac, by whoever is sitting at it — so this is what turns
    /// "Resize to Window" on for an rxa session, and off again the instant the
    /// user picks a real screen from the Display menu. It stays request-only
    /// either way: even a display made to be looked at from here is not dragged
    /// around by this window.
    ///
    /// A no-op for RDP and VNC, and deliberately so rather than by their never
    /// sending a display list: a gateway that somehow sent one must not be able to
    /// change how those two behave.
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

    /// Forget the dedupe. Called on every `connected`: the size the gateway knew
    /// about went away with the previous socket, so the first report on a new one
    /// has to go out even if it matches.
    ///
    /// Deliberately *not* called on a display switch. The memo means "the display
    /// being resized is already this size", and switching away and back leaves
    /// that display at exactly the size it was left at — so clearing it would buy
    /// a redundant round trip and no correctness.
    mutating func resetForNewConnection() {
        lastSent = nil
    }
}
