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
/// carries `UInt16` because the gateway *rejects* an
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
    /// How much room the viewer has, in the *remote's* pixels — its room in points
    /// times the density the remote draws at. Followed continuously by VNC, acted on
    /// only by request for RDP, ignored entirely by rxa.
    case viewport(w: UInt16, h: UInt16)
    /// `viewport` with no size on it: put the remote back at whatever size the far
    /// side considers its default — a target's configured `width`/`height` for RDP
    /// and VNC, the point size the agent created its display at for rxa. Gated on
    /// the same target opt-in and acted on by the same three engines.
    ///
    /// It exists for a client whose window is not a shape a desktop can usefully
    /// be, which is the browser on a phone rather than anything here — this viewer
    /// always has a real window to measure. Carried anyway because this enum is the
    /// gateway's `ClientMsg`, not a subset of it, and the wire-contract test holds
    /// the two identical: a variant this build could not express is drift waiting to
    /// be discovered at runtime.
    case defaultSize
    /// Re-announce the desktop size and repaint everything. The gateway injects
    /// one itself on reattach, so this is the manual escape hatch.
    case refresh
    case connect(target: String)
    case disconnect
    case clipboard(text: String)
    case clipboardRequest
    /// Share a different one of the remote's displays, by the `id` of an entry
    /// from the last `displays` message.
    ///
    /// The counterpart to `viewport` above, and the contrast is the point: a
    /// remote's *resolution* belongs to the machine running it, while *which of
    /// its screens to look at* is only a question for the person looking. Only
    /// rxa answers it.
    case selectDisplay(id: UInt32)
    /// The density of the screen this window is on, in hundredths — 100 for a 1x
    /// screen, 200 for a Retina one. Sent on connect and whenever the window
    /// changes screen.
    ///
    /// It changes nothing about how the remote is drawn here: the framebuffer view
    /// is laid out at the remote's own point size and AppKit rasterizes that for
    /// whichever screen it is on (see `RemoteGeometry`). This asks the remote to
    /// *have* the density that makes the result one pixel per pixel. Only rxa acts
    /// on it, and only for a display the agent made.
    case hostScale(scale: UInt16)
    /// "I lost the tiles you told me to remember." Sent when a cached tile will
    /// not decode, or when a reference names a slot this client does not hold.
    ///
    /// Deliberately not `refresh`: that is routed to the engine, which would
    /// repaint into the gateway's *unchanged* slot table, send the same references
    /// back, and miss again. This is handled by the socket's own bridge, which
    /// empties the table and asks for the repaint itself.
    case cacheReset
    /// Start or stop this attachment's audio. Reconnects require reassertion.
    case audio(enabled: Bool)

    /// The `type` tag this encodes as. Public so the wire-contract test can
    /// compare the whole set against the Rust enum.
    var tag: String {
        switch self {
        case .mouseMove: "mouseMove"
        case .mouseButton: "mouseButton"
        case .wheel: "wheel"
        case .key: "key"
        case .viewport: "viewport"
        case .defaultSize: "defaultSize"
        case .refresh: "refresh"
        case .connect: "connect"
        case .disconnect: "disconnect"
        case .clipboard: "clipboard"
        case .clipboardRequest: "clipboardRequest"
        case .selectDisplay: "selectDisplay"
        case .hostScale: "hostScale"
        case .cacheReset: "cacheReset"
        case .audio: "audio"
        }
    }

    /// Every tag this build can send, for the wire-contract test.
    static let allTags: Set<String> = [
        "mouseMove", "mouseButton", "wheel", "key", "viewport", "defaultSize",
        "refresh", "connect", "disconnect", "clipboard", "clipboardRequest",
        "selectDisplay", "hostScale", "cacheReset", "audio",
    ]

    /// Tags this build knows exist and deliberately never sends.
    ///
    /// Empty, and kept rather than deleted: `audio` was here for as long as the
    /// gateway had sound and the viewer did not, and what the entry recorded was a
    /// *decision* — the wire-contract test asks whether somebody has considered each
    /// message, and an empty set is the answer "all of them are implemented" rather
    /// than the absence of the question.
    static let unsentTags: Set<String> = []
}

extension ClientMessage: Encodable {
    private enum Key: String, CodingKey {
        case type, x, y, button, pressed, dx, dy, code, caps, w, h, target, text
        case id, scale, enabled
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
        case .viewport(let w, let h):
            try container.encode(w, forKey: .w)
            try container.encode(h, forKey: .h)
        case .connect(let target):
            try container.encode(target, forKey: .target)
        case .clipboard(let text):
            try container.encode(text, forKey: .text)
        case .selectDisplay(let id):
            try container.encode(id, forKey: .id)
        case .hostScale(let scale):
            try container.encode(scale, forKey: .scale)
        case .audio(let enabled):
            // Never defaulted on the gateway's side either: `{"type":"audio"}` with
            // nothing said about it is rejected there rather than read as one of the
            // two answers (`src/protocol.rs`).
            try container.encode(enabled, forKey: .enabled)
        case .defaultSize, .refresh, .disconnect, .clipboardRequest, .cacheReset:
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
