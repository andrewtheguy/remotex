import Foundation
import Testing
@testable import RemotexViewer

/// The JSON both sides of the bridge agree on.
///
/// Asserted on the encoded bytes rather than on a round trip through this same
/// type, which would agree with itself whatever it spelled: the reader is
/// `frontend/src/viewer/bridge.ts`, and it is the only thing these names have to
/// match.
struct CanvasCommandTests {
    private func fields(_ command: CanvasCommand) throws -> [String: Any] {
        let data = try #require(command.jsonData())
        return try #require(JSONSerialization.jsonObject(with: data) as? [String: Any])
    }

    @Test
    func aResizeCarriesBothNumbersTheLayoutNeeds() throws {
        // The bitmap is `w`x`h` and the CSS box is those divided by `scale`, so a
        // command missing either presents the desktop at the wrong size — see
        // `desktopCanvasGeometry`.
        let json = try fields(.resize(w: 3840, h: 2160, scale: 2))
        #expect(json["type"] as? String == "resize")
        #expect(json["w"] as? Int == 3840)
        #expect(json["h"] as? Int == 2160)
        #expect(json["scale"] as? Double == 2)
    }

    @Test
    func theCommandsWithNoPayloadAreStillTagged() throws {
        #expect(try fields(.clear)["type"] as? String == "clear")
        #expect(try fields(.audioStop)["type"] as? String == "audioStop")
        let input = try fields(.input(enabled: true))
        #expect(input["type"] as? String == "input")
        #expect(input["enabled"] as? Bool == true)
    }

    /// The cursor's fields are `ServerMsg::Cursor`'s, forwarded rather than
    /// converted: the hotspot is in cursor *pixels* at both ends, and the page
    /// scales it by what the desktop is actually drawn at.
    @Test
    func aCursorIsForwardedFieldForField() throws {
        let json = try fields(
            .cursor(
                ServerMessage.Cursor(image: "iVBORw0KGgo=", w: 24, h: 24, hx: 4, hy: 6)
            )
        )
        #expect(json["type"] as? String == "cursor")
        #expect(json["image"] as? String == "iVBORw0KGgo=")
        #expect(json["w"] as? Int == 24)
        #expect(json["h"] as? Int == 24)
        #expect(json["hx"] as? Int == 4)
        #expect(json["hy"] as? Int == 6)
    }

    /// A hidden pointer is a `cursor` whose image is null — distinct from no
    /// cursor message at all, which means the remote is drawing its own. The page
    /// draws a fallback arrow for the first and nothing for the second, so a
    /// null that encoded as an absent key would put a pointer back on RDP.
    @Test
    func aHiddenPointerKeepsItsNullImage() throws {
        let json = try fields(
            .cursor(ServerMessage.Cursor(image: nil, w: 0, h: 0, hx: 0, hy: 0))
        )
        #expect(json["image"] is NSNull)
    }

    /// `OpusHead` reaches the page as base64, which is what `decodeAudioHead`
    /// reads and what the control message carried in the first place.
    @Test
    func anAudioFormatCarriesItsHeadAsBase64() throws {
        let head = try #require(Data(base64Encoded: "T3B1c0hlYWQBAjgBRKwAAAAAAA=="))
        let json = try fields(
            .audioFormat(
                ServerMessage.AudioFormat(
                    codec: "opus",
                    sampleRate: 48_000,
                    channels: 2,
                    head: head
                )
            )
        )
        #expect(json["codec"] as? String == "opus")
        #expect(json["sampleRate"] as? Double == 48_000)
        #expect(json["channels"] as? Int == 2)
        #expect(json["head"] as? String == "T3B1c0hlYWQBAjgBRKwAAAAAAA==")
    }

    // MARK: - Events

    private func event(_ json: String) -> CanvasEvent? {
        CanvasEvent.decode(Data(json.utf8))
    }

    @Test
    func everyEventThePageSendsIsRead() throws {
        #expect(
            event(
                #"{"type":"ready","secureContext":true,"audioDecoder":false,"#
                    + #""room":{"w":1265,"h":785},"content":{"w":1280,"h":800}}"#
            )
                == .ready(
                    secureContext: true,
                    audioDecoder: false,
                    room: DisplayMode(w: 1265, h: 785),
                    content: DisplayMode(w: 1280, h: 800)
                )
        )
        #expect(
            event(#"{"type":"pointer","x":12,"y":-3}"#) == .pointer(x: 12, y: -3)
        )
        #expect(
            event(#"{"type":"button","button":"right","pressed":true,"clicks":2}"#)
                == .button(.right, pressed: true, clicks: 2)
        )
        #expect(
            event(#"{"type":"wheel","dx":0.5,"dy":-2.25,"unit":"line"}"#)
                == .wheel(dx: 0.5, dy: -2.25, unit: .line)
        )
        #expect(event(#"{"type":"cacheReset"}"#) == .cacheReset)
        #expect(
            event(#"{"type":"audioState","playing":false,"error":"no decoder"}"#)
                == .audioState(playing: false, error: "no decoder")
        )
    }

    /// A null error is the ordinary case — sound started, or stopped because it
    /// was asked to — and must not read as a failure.
    @Test
    func anAudioStateWithoutAnErrorIsNotAFailure() throws {
        #expect(
            event(#"{"type":"audioState","playing":true,"error":null}"#)
                == .audioState(playing: true, error: nil)
        )
    }

    /// The page is ours and the only writer, so anything unreadable is a bug —
    /// but it is dropped rather than trapped, because this decodes on the main
    /// actor with a live session behind it.
    /// The room is CSS pixels and may be fractional; the window it sizes is
    /// whole points, so it is rounded rather than truncated — a room reported as
    /// 1279.6 is a desktop that fits, and flooring it would take a point away
    /// and put the scroll bars back.
    @Test
    func aFractionalRoomIsRoundedRatherThanTruncated() throws {
        let decoded = event(
            #"{"type":"ready","secureContext":true,"audioDecoder":true,"#
                + #""room":{"w":1279.6,"h":799.5},"content":{"w":1280,"h":800}}"#
        )
        #expect(
            decoded
                == .ready(
                    secureContext: true,
                    audioDecoder: true,
                    room: DisplayMode(w: 1280, h: 800),
                    content: DisplayMode(w: 1280, h: 800)
                )
        )
    }

    @Test
    func anythingElseIsDroppedRatherThanTrapped() {
        #expect(event(#"{"type":"nonsense"}"#) == nil)
        #expect(event(#"{"type":"pointer","x":12}"#) == nil, "a missing field")
        #expect(event(#"{"type":"button","button":"pinky","pressed":true,"clicks":1}"#) == nil)
        #expect(event("not json at all") == nil)
    }
}
