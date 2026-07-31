import Foundation

enum ViewerScreen: String {
    /// Which gateway to use: the one in this bundle, or one at an address.
    ///
    /// The landing screen, and the reason there is one at all. The embedded gateway
    /// is the right answer for a Mac on the same network as its targets and the wrong
    /// one when the link to them is slow — there the gateway belongs near the target
    /// and this app should be talking to *it*. Only the user knows which case they
    /// are in, so the app asks instead of deciding.
    case home
    /// Credentials for a remote gateway. Never reached for the embedded one, which
    /// has no login to offer (`no_login_handler` in src/server.rs).
    case login
    /// Starting the gateway in this bundle, or explaining why it did not start.
    ///
    /// The embedded branch's own screen, and it asks nothing: the token the gateway
    /// prints is the whole of the authentication. So this is a progress indicator
    /// that occasionally becomes an error message.
    case launching
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

/// A remote desktop size, in the remote's own pixels.
///
/// `UInt16` because that is what `viewport` is on the wire, and the gateway
/// rejects an out-of-range value rather than clamping it — so a size that could
/// not be sent back is not representable here either.
struct DisplayMode: Equatable, Hashable, Sendable, Decodable {
    var w: UInt16
    var h: UInt16
}

/// A density as a menu reads it: `2x`, `1x`, `1.5x` for the fractional screens
/// that exist.
func densityLabel(_ scale: CGFloat) -> String {
    let rounded = (scale * 100).rounded() / 100
    // A whole number without the `.0`, which is what almost every screen is.
    if rounded == rounded.rounded() {
        return "\(Int(rounded))x"
    }
    return "\(rounded)x"
}

/// The line the Display menu shows: what the remote is drawing, and what this
/// Mac's screen is.
///
/// Both densities, always, because one that failed to apply is otherwise
/// invisible. Both engines that match a client's density report the outcome only
/// as a `resize`, and a request the remote quietly dropped produces no message at
/// all: the desktop simply looks soft, or half the size it was asked for, with
/// nothing saying which end disagreed. Two numbers that ought to match and don't
/// is the whole diagnostic — which is why this is not just the resolution.
///
/// `nil` before the first `resize`, which is the "waiting for the remote desktop"
/// state: a placeholder reading 0x0 would be a worse answer than saying so.
func displaySummary(
    remote: DisplayMode?,
    remoteScale: CGFloat,
    hostScale: UInt16
) -> String {
    guard let remote else {
        return "Waiting for the Remote Desktop"
    }
    let host = densityLabel(CGFloat(hostScale) / 100)
    return "\(remote.w)×\(remote.h) — remote \(densityLabel(remoteScale)), this screen \(host)"
}

/// What the viewer knows about the session it is attached to.
///
/// Every field here is derived from the gateway's own control messages, which is
/// why the derivations are spelled out where they happen (`AppModel.handle`).
struct ViewerSessionState: Equatable {
    var screen = ViewerScreen.home
    var connectionStatus: ViewerConnectionStatus?
    var connectedTarget: String?
    /// `"rdp"` or `"vnc"`, from `connected`. Carried to mirror the wire; nothing
    /// branches on it.
    var protocolName: String?
    /// Whether the remote runs macOS, as the gateway's engine discovered it.
    /// Decides only whether a local Command shortcut stays Command or becomes
    /// remote Control.
    var remoteIsMac = false
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
    /// Whether this session may resize the remote at all: the target's `resize`,
    /// settled at connect for every protocol. Permission only — all three resize
    /// items in the View menu are dead without it, and none of them is decided by
    /// it alone.
    var canResize = false
    /// Whether the remote follows this window's size unasked. The client's choice
    /// rather than a fact about the protocol, and not remembered: every connection
    /// starts manual. Mirrored off `ViewportPolicy` for the same reason as
    /// `canResize` — the policy is not observed, and three menu items read this.
    var autoResize = false
    var canClipboard = false
    /// Whether this target carries sound at all — an RDP target configured for it. Not
    /// whether any is playing: see `ServerMessage.Connected.audio`.
    var canAudio = false
    /// The remote's displays and the one it is sharing, from the last
    /// `displays`. Empty for every engine that cannot offer a choice, which is
    /// what leaves the Display menu with nothing in it.
    ///
    /// Never written on a click. The checkmark follows `activeDisplayID`, which
    /// only the remote sets, so a selection it refused leaves the menu agreeing
    /// with what is on screen.
    var displays: [ServerMessage.DisplayInfo] = []
    var activeDisplayID: UInt32?
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
