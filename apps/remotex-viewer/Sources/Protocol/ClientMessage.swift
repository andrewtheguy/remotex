import Foundation

enum MouseButton: String, Sendable, Codable, CaseIterable {
    case left
    case middle
    case right
}

/// Viewer -> gateway input and session control: `ClientMsg` in
/// `src/protocol.rs`. One JSON text frame per value, internally tagged on
/// `type` with the payload fields as siblings.
///
/// The integer widths are the Rust ones, not the convenient ones. `viewport`
/// and `setResolution` carry `UInt16` because the gateway *rejects* an
/// out-of-range value at deserialization rather than clamping it, and a
/// rejected frame is only logged — so the symptom of getting this wrong is
/// "resize silently stopped working". Making the payload `UInt16` means an
/// unsendable frame cannot be built; clamping happens once, deliberately, where
/// a viewport is measured.
enum ClientMessage: Sendable, Equatable {
    case mouseMove(x: Int32, y: Int32)
    case mouseButton(button: MouseButton, pressed: Bool)
    case wheel(dx: Float, dy: Float)
    /// `code` is a DOM `KeyboardEvent.code`; see `src/keymap.rs` for the set the
    /// gateway maps. `caps` is the CapsLock *lock* state, which VNC cannot
    /// observe on its own — false on a synthetic send, which expresses case with
    /// an explicit `ShiftLeft` instead.
    case key(code: String, pressed: Bool, caps: Bool)
    /// How much room the viewer has, in device pixels. Followed continuously by
    /// VNC, acted on only by request for RDP, ignored entirely by rxa.
    case viewport(w: UInt16, h: UInt16)
    /// The user's pick from a `displayModes` menu. Not a spelling of `viewport`:
    /// a Mac's virtual display takes nothing but sizes off a fixed list.
    case setResolution(w: UInt16, h: UInt16)
    /// Re-announce the desktop size and repaint everything. The gateway injects
    /// one itself on reattach, so this is the manual escape hatch.
    case refresh
    case connect(target: String)
    case disconnect
    case clipboard(text: String)
    case clipboardRequest

    /// The `type` tag this encodes as. Public so the wire-contract test can
    /// compare the whole set against the Rust enum.
    var tag: String {
        switch self {
        case .mouseMove: "mouseMove"
        case .mouseButton: "mouseButton"
        case .wheel: "wheel"
        case .key: "key"
        case .viewport: "viewport"
        case .setResolution: "setResolution"
        case .refresh: "refresh"
        case .connect: "connect"
        case .disconnect: "disconnect"
        case .clipboard: "clipboard"
        case .clipboardRequest: "clipboardRequest"
        }
    }

    /// Every tag this build can send, for the wire-contract test.
    static let allTags: Set<String> = [
        "mouseMove", "mouseButton", "wheel", "key", "viewport", "setResolution",
        "refresh", "connect", "disconnect", "clipboard", "clipboardRequest",
    ]
}

extension ClientMessage: Encodable {
    private enum Key: String, CodingKey {
        case type, x, y, button, pressed, dx, dy, code, caps, w, h, target, text
    }

    func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: Key.self)
        try container.encode(tag, forKey: .type)
        switch self {
        case .mouseMove(let x, let y):
            try container.encode(x, forKey: .x)
            try container.encode(y, forKey: .y)
        case .mouseButton(let button, let pressed):
            try container.encode(button, forKey: .button)
            try container.encode(pressed, forKey: .pressed)
        case .wheel(let dx, let dy):
            try container.encode(dx, forKey: .dx)
            try container.encode(dy, forKey: .dy)
        case .key(let code, let pressed, let caps):
            try container.encode(code, forKey: .code)
            try container.encode(pressed, forKey: .pressed)
            try container.encode(caps, forKey: .caps)
        case .viewport(let w, let h), .setResolution(let w, let h):
            try container.encode(w, forKey: .w)
            try container.encode(h, forKey: .h)
        case .connect(let target):
            try container.encode(target, forKey: .target)
        case .clipboard(let text):
            try container.encode(text, forKey: .text)
        case .refresh, .disconnect, .clipboardRequest:
            break
        }
    }

    /// The text frame to send, or nil if it could not be built.
    ///
    /// Optional rather than throwing because the only way this fails is a
    /// non-finite wheel delta, which `JSONEncoder` refuses — and a throw
    /// escaping the send loop would take every later message down with it. The
    /// deltas are filtered before they get here; this is the backstop.
    func jsonText() -> String? {
        guard let data = try? JSONEncoder().encode(self) else {
            return nil
        }
        return String(decoding: data, as: UTF8.self)
    }
}
