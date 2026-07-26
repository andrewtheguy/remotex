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
/// - **rxa** ignores viewport reports outright. A Mac's virtual display takes
///   only sizes off a fixed list, so it answers `displayModes` and
///   `setResolution` instead.
struct ViewportPolicy: Equatable {
    /// Suppress automatic reports; only an explicit request gets through. RDP and
    /// rxa with `resize`.
    var manualOnly = false
    /// Send nothing at all. rxa, which acts on `setResolution` instead.
    var ignoresViewport = false

    /// The last size sent on this connection, for the dedupe below.
    private var lastSent: DisplayMode?

    init(manualOnly: Bool = false, ignoresViewport: Bool = false) {
        self.manualOnly = manualOnly
        self.ignoresViewport = ignoresViewport
    }

    /// Derive the policy from a `connected` message.
    init(protocolName: String, resize: Bool) {
        // rxa never wants one, whether or not it can resize.
        ignoresViewport = protocolName == "rxa"
        manualOnly = (protocolName == "rdp" || protocolName == "rxa") && resize
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
    mutating func resetForNewConnection() {
        lastSent = nil
    }
}
