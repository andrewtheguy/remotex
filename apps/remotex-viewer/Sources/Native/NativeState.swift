import CoreGraphics
import Foundation

/// A remote desktop size, in the remote's own pixels.
///
/// `UInt16` because that is what `viewport` is on the wire, and the gateway
/// rejects an out-of-range value rather than clamping it — so a size that could
/// not be sent back is not representable here either.
struct DisplayMode: Equatable, Hashable, Sendable, Decodable {
    var w: UInt16
    var h: UInt16
}

/// One of the remote's displays, as the Display menu lists them.
struct DisplayChoice: Equatable, Hashable, Sendable, Decodable, Identifiable {
    var id: UInt32
    var label: String
    var detail: String
}

/// What the page says about itself, decoded from one `state` event.
///
/// Every menu title, tick and enabled state is derived from this and from nothing
/// else. It is deliberately a *report*, not a model: the client owns the session
/// and this is what it has decided, so a menu can never claim a capability the
/// thing on screen does not have.
///
/// The mirror of `NativeState` in `frontend/src/nativeHost.ts`. `Decodable` with
/// defaults throughout, so a page mid-navigation that posts a partial object leaves
/// the menus reading "nothing is connected" rather than failing to decode.
struct NativeState: Equatable, Decodable {
    /// Which screen the client is on. The menus are dead outside the desktop.
    var mode: Mode = .picker
    /// The connection lifecycle, which decides Take Over's title and presence.
    var status: Status = .connecting
    /// Whether the first frame has arrived. Keyboard capture waits for it: before
    /// it there is no desktop to type at, only a canvas that has not been sized.
    var ready = false
    /// The remote's framebuffer size, and how many of its pixels it draws per point
    /// of its own desktop — 1 for VNC, RDP and a 1x Mac, 2 for a Retina one.
    /// **Resize to Display** is the arithmetic over these two.
    var size: RemoteSize?
    /// The target's `resize`: permission to resize at all, and nothing more.
    var canResize = false
    /// The gateway's second permission, held by plain `vnc` alone: whether this
    /// remote may be handed the window's size unasked.
    var canAutoResize = false
    /// Whether the client is following the window right now. Its choice within the
    /// permission above.
    var autoResize = false
    var canClipboard = false
    var canAudio = false
    var audioEnabled = false
    var audioError: String?
    var displays: [DisplayChoice] = []
    var activeDisplayId: UInt32?
    var macKeyOverridesEnabled = true
    var macKeyOverridesActive = false
    var remoteIsMac = false

    enum Mode: String, Decodable {
        case picker
        case desktop
    }

    enum Status: String, Decodable {
        case connecting
        case connected
        case reconnecting
        /// Another client holds the one session slot (a claim answered 409).
        case busy
        /// This client was evicted by someone else's takeover (close 4001).
        case takenOver
        /// The session could not be opened and nothing is being retried.
        case failed
    }

    struct RemoteSize: Equatable, Decodable {
        var w: UInt16
        var h: UInt16
        var scale: Double
    }

    /// Spelled out because declaring `init(from:)` takes the synthesized ones with
    /// it. Each name is the field the page posts — see `NativeState` in
    /// `frontend/src/nativeHost.ts`.
    private enum CodingKeys: String, CodingKey {
        case mode, status, ready, size
        case canResize, canAutoResize, autoResize
        case canClipboard, canAudio, audioEnabled, audioError
        case displays, activeDisplayId
        case macKeyOverridesEnabled, macKeyOverridesActive, remoteIsMac
    }

    init() {}

    /// Written out rather than synthesized, because the synthesized decoder does
    /// not use a property's default value: it calls `decode` for every key and
    /// throws on the first one missing. A page part way through a navigation posts
    /// what it has, and the answer to a missing field is "nothing is connected" —
    /// not a decode failure that leaves the menus describing the session before it.
    init(from decoder: any Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        // A field that is absent *or* the wrong shape takes the fallback: this is a
        // report from a page, and a menu reading "nothing is connected" is a better
        // answer to a malformed one than no menus at all.
        func value<T: Decodable>(_ key: CodingKeys, _ fallback: T) -> T {
            ((try? values.decodeIfPresent(T.self, forKey: key)) ?? nil) ?? fallback
        }
        mode = value(.mode, Mode.picker)
        status = value(.status, Status.connecting)
        ready = value(.ready, false)
        size = try? values.decodeIfPresent(RemoteSize.self, forKey: .size)
        canResize = value(.canResize, false)
        canAutoResize = value(.canAutoResize, false)
        autoResize = value(.autoResize, false)
        canClipboard = value(.canClipboard, false)
        canAudio = value(.canAudio, false)
        audioEnabled = value(.audioEnabled, false)
        audioError = try? values.decodeIfPresent(String.self, forKey: .audioError)
        displays = value(.displays, [DisplayChoice]())
        activeDisplayId = try? values.decodeIfPresent(UInt32.self, forKey: .activeDisplayId)
        macKeyOverridesEnabled = value(.macKeyOverridesEnabled, true)
        macKeyOverridesActive = value(.macKeyOverridesActive, false)
        remoteIsMac = value(.remoteIsMac, false)
    }

    /// The framebuffer as `DisplayMode`, for the geometry.
    var remoteSize: DisplayMode? {
        size.map { DisplayMode(w: $0.w, h: $0.h) }
    }

    /// The remote's own density, never this Mac's. See `RemoteGeometry`.
    var remoteScale: CGFloat {
        guard let scale = size?.scale, scale > 0 else {
            return 1
        }
        return CGFloat(scale)
    }

    /// Keyboard capture belongs to a live desktop with something on it.
    var capturesKeyboard: Bool {
        mode == .desktop && status == .connected && ready
    }
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
    hostScale: CGFloat
) -> String {
    guard let remote else {
        return "Waiting for the Remote Desktop"
    }
    let host = densityLabel(hostScale)
    return "\(remote.w)×\(remote.h) — remote \(densityLabel(remoteScale)), this screen \(host)"
}
