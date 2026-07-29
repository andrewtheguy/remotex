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
        // The tile cache's only recovery path, and the one message whose *shape*
        // matters more than its content: the gateway routes it by tag alone, and a
        // tag it cannot parse leaves the socket emitting references into a cache
        // this client has already emptied, forever.
        try expectEncoding(.cacheReset, ["type": "cacheReset"])
    }

    /// The only way this client asks for a remote size — see ClientMsg in
    /// src/protocol.rs, where it is also the only one.
    @Test
    func viewportEncodesItsSize() throws {
        try expectEncoding(
            .viewport(w: 1280, h: 800),
            ["type": "viewport", "w": 1280, "h": 800]
        )
    }

    /// The other display message, and the opposite of `viewport`: this one asks
    /// which screen to look at, which is the only display decision a client gets
    /// to make. The id is opaque and uses the full u32 range.
    @Test
    func selectDisplayCarriesAnOpaqueId() throws {
        try expectEncoding(
            .selectDisplay(id: 2),
            ["type": "selectDisplay", "id": 2]
        )
        try expectEncoding(
            .selectDisplay(id: UInt32.max),
            ["type": "selectDisplay", "id": 4_294_967_295]
        )
    }

    /// Hundredths of a backing scale, matching the density a `resize` reports in
    /// the other direction — so the two are comparable without either end knowing
    /// how the other spells 2x.
    @Test
    func hostScaleTravelsAsHundredths() throws {
        try expectEncoding(.hostScale(scale: 100), ["type": "hostScale", "scale": 100])
        try expectEncoding(.hostScale(scale: 200), ["type": "hostScale", "scale": 200])
        // A screen with a fractional ratio is ordinary and must survive intact
        // rather than being rounded to one of the two common cases here.
        try expectEncoding(.hostScale(scale: 150), ["type": "hostScale", "scale": 150])
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

    /// Both answers explicitly, because the gateway refuses `{"type":"audio"}` with no
    /// `enabled` rather than defaulting it — so an encoder that dropped the field for
    /// `false` would look like it worked and mute nothing.
    @Test
    func audioCarriesBothAnswers() throws {
        try expectEncoding(.audio(enabled: true), ["type": "audio", "enabled": true])
        try expectEncoding(.audio(enabled: false), ["type": "audio", "enabled": false])
    }

    @Test
    func everyCaseReportsADistinctTag() {
        let messages: [ClientMessage] = [
            .mouseMove(x: 0, y: 0),
            .mouseButton(button: .left, pressed: true),
            .wheel(dx: 0, dy: 0),
            .key(code: "KeyA", pressed: true, caps: false),
            .viewport(w: 1, h: 1),
            .defaultSize,
            .refresh,
            .connect(target: "t"),
            .disconnect,
            .clipboard(text: ""),
            .clipboardRequest,
            .selectDisplay(id: 1),
            .hostScale(scale: 200),
            .cacheReset,
            .audio(enabled: true),
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
