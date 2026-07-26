import Foundation
import Testing
@testable import RemotexViewer

/// The gateway deserializes these with serde's internally-tagged representation,
/// which buffers the object and so accepts any field order. Assertions therefore
/// compare the parsed object rather than the literal text — pinning byte-for-byte
/// output would fail on a Foundation key-ordering change that the wire does not
/// care about.
struct ClientMessageTests {
    @Test
    func inputEventsEncodeToTheirTaggedShape() throws {
        try expectEncoding(
            .mouseMove(x: 5, y: 6),
            ["type": "mouseMove", "x": 5, "y": 6]
        )
        try expectEncoding(
            .mouseButton(button: .right, pressed: true),
            ["type": "mouseButton", "button": "right", "pressed": true]
        )
        try expectEncoding(
            .wheel(dx: 0, dy: -2.5),
            ["type": "wheel", "dx": 0, "dy": -2.5]
        )
        try expectEncoding(
            .key(code: "KeyA", pressed: false, caps: true),
            ["type": "key", "code": "KeyA", "pressed": false, "caps": true]
        )
    }

    @Test
    func sessionControlAndClipboardEncodeToTheirTaggedShape() throws {
        try expectEncoding(.refresh, ["type": "refresh"])
        try expectEncoding(.disconnect, ["type": "disconnect"])
        try expectEncoding(.clipboardRequest, ["type": "clipboardRequest"])
        try expectEncoding(.connect(target: "mac"), ["type": "connect", "target": "mac"])
        try expectEncoding(
            .clipboard(text: "héllo"),
            ["type": "clipboard", "text": "héllo"]
        )
    }

    /// `viewport` and `setResolution` share a payload shape but not a meaning —
    /// see ClientMsg in src/protocol.rs — so both tags must survive encoding.
    @Test
    func viewportAndSetResolutionKeepTheirOwnTags() throws {
        try expectEncoding(
            .viewport(w: 1280, h: 800),
            ["type": "viewport", "w": 1280, "h": 800]
        )
        try expectEncoding(
            .setResolution(w: 1280, h: 800),
            ["type": "setResolution", "w": 1280, "h": 800]
        )
    }

    /// The gateway's `w`/`h` are u16 and it rejects anything wider rather than
    /// clamping, so the type carries the ceiling and the extremes must survive.
    @Test
    func sizesAtTheUInt16BoundaryEncodeIntact() throws {
        try expectEncoding(
            .viewport(w: 65535, h: 1),
            ["type": "viewport", "w": 65535, "h": 1]
        )
    }

    /// JSONEncoder refuses a non-finite Float. A throw escaping the send loop
    /// would take every message queued behind this one, so the encode answers
    /// nil and the caller drops one wheel event instead.
    @Test
    func aNonFiniteWheelDeltaDoesNotProduceAFrame() {
        #expect(ClientMessage.wheel(dx: .infinity, dy: 0).jsonText() == nil)
        #expect(ClientMessage.wheel(dx: 0, dy: .nan).jsonText() == nil)
        #expect(ClientMessage.wheel(dx: -.infinity, dy: 3).jsonText() == nil)
    }

    @Test
    func everyCaseReportsADistinctTag() {
        let messages: [ClientMessage] = [
            .mouseMove(x: 0, y: 0),
            .mouseButton(button: .left, pressed: true),
            .wheel(dx: 0, dy: 0),
            .key(code: "KeyA", pressed: true, caps: false),
            .viewport(w: 1, h: 1),
            .setResolution(w: 1, h: 1),
            .refresh,
            .connect(target: "t"),
            .disconnect,
            .clipboard(text: ""),
            .clipboardRequest,
        ]
        #expect(Set(messages.map(\.tag)) == ClientMessage.allTags)
        #expect(messages.count == ClientMessage.allTags.count)
    }

    private func expectEncoding(
        _ message: ClientMessage,
        _ expected: [String: Any],
        sourceLocation: SourceLocation = #_sourceLocation
    ) throws {
        let text = try #require(message.jsonText(), sourceLocation: sourceLocation)
        let parsed = try JSONSerialization.jsonObject(with: Data(text.utf8))
        let object = try #require(parsed as? [String: Any], sourceLocation: sourceLocation)
        #expect(
            NSDictionary(dictionary: object) == NSDictionary(dictionary: expected),
            "\(text)",
            sourceLocation: sourceLocation
        )
    }
}
