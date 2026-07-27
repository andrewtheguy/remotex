import Foundation

enum ViewerScreen: String {
    /// Choose a gateway and confirm it can be spoken to. Its own step ahead of
    /// the credentials, because "can this address be reached, and does it speak a
    /// protocol this build knows" is a separate question from "who are you" — and
    /// it is answered when the user asks for it, not as a side effect of signing
    /// in.
    case server
    case login
    case picker
    case desktop
}

enum ViewerConnectionStatus: String {
    case connecting
    case connected
    case reconnecting
    /// Another client holds the one session slot (a claim answered 409).
    case busy
    /// This client was evicted by someone else's takeover (close 4001).
    case takenOver
}

/// One entry of the remote's resolution menu, in the remote's own pixels.
///
/// `UInt16` because that is what `setResolution` is on the wire, and the gateway
/// rejects an out-of-range value rather than clamping it — so a mode that could
/// not be sent back is not representable here either.
struct DisplayMode: Equatable, Identifiable, Hashable, Sendable, Decodable {
    var w: UInt16
    var h: UInt16

    var id: String { "\(w)x\(h)" }
    var label: String { "\(w) × \(h)" }
}

/// What the viewer knows about the session it is attached to.
///
/// Every field here used to be computed in JavaScript and shipped over the host
/// bridge. It is now derived from the gateway's own control messages, which is
/// why the derivations are spelled out where they happen (`AppModel.handle`).
struct ViewerSessionState: Equatable {
    var screen = ViewerScreen.server
    var connectionStatus: ViewerConnectionStatus?
    var connectedTarget: String?
    /// `"rdp"`, `"vnc"`, or `"rxa"`, from `connected`. Decides the resize
    /// behaviour; nothing else branches on it.
    var protocolName: String?
    /// Whether the remote runs macOS, as the gateway's engine discovered it.
    /// Decides only whether a local Command shortcut stays Command or becomes
    /// remote Control.
    var remoteIsMac = false
    /// The resolutions the remote offers. Empty for every target without a
    /// menu — only a Mac agent on a virtual display fills this in.
    var displayModes: [DisplayMode] = []
    /// The remote's current size in framebuffer pixels, from the last `resize`.
    /// Nil before the first one, which is the "waiting for the remote desktop"
    /// state.
    var remoteSize: DisplayMode?
    /// How many of those pixels the remote draws per point of its own desktop: 1
    /// for VNC, RDP and a 1x Mac, 2 for a Retina one. Everything the desktop is
    /// presented at is derived from it rather than from this Mac's display, which
    /// is what keeps the remote the same physical size on either — see
    /// `RemoteGeometry`.
    var remoteScale: CGFloat = 1
    /// Whether to offer "Resize to Window": RDP only. VNC follows the viewport
    /// on its own, and rxa answers a resolution menu instead.
    var canResize = false
    /// Whether to *suppress* automatic viewport reports. True for RDP (a resize
    /// forces an expensive Deactivation-Reactivation) and for rxa (which ignores
    /// viewport reports entirely).
    var manualResize = false
    var canClipboard = false
    /// The target a connect is waiting on, so the picker can show progress until
    /// the gateway answers with `connected` — or with an error.
    var pendingTarget: String?
    /// The last engine error, shown against the picker. Not a dead end: the
    /// socket stays up and the session returns to the picker.
    var connectError: String?

    /// Keyboard capture belongs to a live desktop and nothing else. Computed
    /// rather than stored: it was only ever a function of these two.
    var canCaptureKeyboard: Bool {
        screen == .desktop && connectionStatus == .connected
    }
}
