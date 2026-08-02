import Foundation

/// The contract between this app and the canvas page.
///
/// The TypeScript side is `frontend/src/viewer/bridge.ts`. The two are written
/// together and ship in the same bundle, so unlike the pre-`2203112` WKWebView
/// shell there is no bridge version to negotiate: that shell hosted an SPA
/// served by a gateway which could be a different release, and this page cannot
/// be.
///
/// Typed both ways rather than `[String: Any]`, so a field renamed on one side
/// is a compile error rather than a message the page silently ignores.
enum CanvasCommand: Equatable, Encodable {
    /// The remote's framebuffer size and its own pixel density, which is the
    /// pair a client needs to present the desktop — see `RemoteGeometry`.
    case resize(w: UInt16, h: UInt16, scale: Double)
    /// Drop the framebuffer, the slot table and the pointer: the session
    /// detached, switched target, or was taken over.
    case clear
    /// `ServerMessage.Cursor` forwarded verbatim, base64 PNG and all. No
    /// message at all means the remote composites its own pointer.
    case cursor(ServerMessage.Cursor)
    case audioFormat(ServerMessage.AudioFormat)
    case audioStop
    /// The app's `canSendInput`: false on the picker and whenever no target is
    /// connected.
    case input(enabled: Bool)

    private enum CodingKeys: String, CodingKey {
        case type, w, h, scale, image, hx, hy, codec, sampleRate, channels, head, enabled
    }

    func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .resize(let w, let h, let scale):
            try container.encode("resize", forKey: .type)
            try container.encode(w, forKey: .w)
            try container.encode(h, forKey: .h)
            try container.encode(scale, forKey: .scale)
        case .clear:
            try container.encode("clear", forKey: .type)
        case .cursor(let cursor):
            try container.encode("cursor", forKey: .type)
            try container.encode(cursor.image, forKey: .image)
            try container.encode(cursor.w, forKey: .w)
            try container.encode(cursor.h, forKey: .h)
            try container.encode(cursor.hx, forKey: .hx)
            try container.encode(cursor.hy, forKey: .hy)
        case .audioFormat(let format):
            try container.encode("audioFormat", forKey: .type)
            try container.encode(format.codec, forKey: .codec)
            try container.encode(format.sampleRate, forKey: .sampleRate)
            try container.encode(format.channels, forKey: .channels)
            try container.encode(format.head, forKey: .head)
        case .audioStop:
            try container.encode("audioStop", forKey: .type)
        case .input(let enabled):
            try container.encode("input", forKey: .type)
            try container.encode(enabled, forKey: .enabled)
        }
    }

    /// The JSON body of a control envelope, or nil if it will not encode.
    ///
    /// Nil rather than a throw for the same reason `ClientMessage.jsonText()`
    /// does it: the caller is a delivery path with nowhere useful to report to,
    /// and one unencodable command must not take the stream with it.
    func jsonData() -> Data? {
        try? JSONEncoder().encode(self)
    }
}

/// Page to app, over the `remotexCanvas` script message handler.
///
/// Input only, plus the two things the app cannot see for itself: whether the
/// page came up able to decode audio, and whether sound is actually playing.
enum CanvasEvent: Equatable, Decodable {
    /// The first message of every stream attachment, and every layout change
    /// after it.
    ///
    /// `room` is the page's *content box* — what is left for the desktop once
    /// its scroll bars have taken their width. This side cannot measure it: the
    /// web view's bounds include that width, so a window fitted to them is short
    /// by a scroll bar whenever one is up, which leaves the bars in place and
    /// sustaining each other. `content` is the desktop as laid out, so the two
    /// together say whether it fits and by how much it does not.
    case ready(
        secureContext: Bool,
        audioDecoder: Bool,
        room: DisplayMode,
        content: DisplayMode
    )
    /// A position in remote framebuffer pixels, already mapped and clamped by
    /// the page, which is the only side that knows where the canvas is on
    /// screen.
    case pointer(x: Int32, y: Int32)
    /// Carries no position: a press is preceded by its own `pointer`, and a
    /// release lands wherever the pointer already is — the same ordering the
    /// native surface used.
    case button(MouseButton, pressed: Bool, clicks: Int)
    case wheel(dx: Float, dy: Float, unit: WheelUnit)
    /// The page and the gateway disagree about the slot table.
    case cacheReset
    case audioState(playing: Bool, error: String?)

    private enum CodingKeys: String, CodingKey {
        case type, secureContext, audioDecoder, room, content
        case x, y, button, pressed, clicks
        case dx, dy, unit, playing, error
    }

    /// A `{w, h}` pair as the page sends it. CSS pixels, so fractional in
    /// principle; rounded here because the window they size is whole points.
    private struct Size: Decodable {
        let w: Double
        let h: Double

        var mode: DisplayMode {
            DisplayMode(
                w: UInt16(clamping: Int(w.rounded())),
                h: UInt16(clamping: Int(h.rounded()))
            )
        }
    }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let type = try container.decode(String.self, forKey: .type)
        switch type {
        case "ready":
            self = .ready(
                secureContext: try container.decode(Bool.self, forKey: .secureContext),
                audioDecoder: try container.decode(Bool.self, forKey: .audioDecoder),
                room: try container.decode(Size.self, forKey: .room).mode,
                content: try container.decode(Size.self, forKey: .content).mode
            )
        case "pointer":
            self = .pointer(
                x: try container.decode(Int32.self, forKey: .x),
                y: try container.decode(Int32.self, forKey: .y)
            )
        case "button":
            self = .button(
                try container.decode(MouseButton.self, forKey: .button),
                pressed: try container.decode(Bool.self, forKey: .pressed),
                clicks: try container.decode(Int.self, forKey: .clicks)
            )
        case "wheel":
            self = .wheel(
                dx: try container.decode(Float.self, forKey: .dx),
                dy: try container.decode(Float.self, forKey: .dy),
                unit: try container.decode(WheelUnit.self, forKey: .unit)
            )
        case "cacheReset":
            self = .cacheReset
        case "audioState":
            self = .audioState(
                playing: try container.decode(Bool.self, forKey: .playing),
                error: try container.decodeIfPresent(String.self, forKey: .error)
            )
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: container,
                debugDescription: "unknown canvas event \(type)"
            )
        }
    }

    /// Decode one script message body.
    ///
    /// The page is the only writer and it is ours, so anything unreadable here
    /// is a bug rather than input to be tolerated — but it is still dropped
    /// rather than trapped, because a `WKScriptMessageHandler` runs on the main
    /// actor with a live session behind it.
    static func decode(_ json: Data) -> CanvasEvent? {
        try? JSONDecoder().decode(CanvasEvent.self, from: json)
    }
}
