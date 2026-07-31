import Foundation
import Testing
@testable import RemotexViewer

/// The literals here are copied verbatim out of
/// `control_messages_encode_to_tagged_camelcase_text` and
/// `oversized_clipboard_text_is_refused_with_its_size` in src/protocol.rs. That
/// Rust test asserts the gateway emits exactly these strings; this one asserts
/// the viewer reads them. Between the two, neither side can drift alone.
struct ServerMessageTests {
    @Test
    func resizeAndErrorDecode() throws {
        #expect(
            try ServerMessage.decode(#"{"type":"resize","w":1280,"h":800,"scale":1.0}"#)
                == .resize(w: 1280, h: 800, scale: 1)
        )
        #expect(
            try ServerMessage.decode(#"{"type":"error","message":"boom"}"#)
                == .error(message: "boom")
        )
        #expect(try ServerMessage.decode(#"{"type":"picker"}"#) == .picker)
    }

    @Test
    func connectedCarriesTheProtocolNameAroundTheSwiftKeyword() throws {
        let message = try ServerMessage.decode(
            #"""
            {"type":"connected","name":"mac","protocol":"vnc","resize":false,\#
            "clipboard":true,"audio":false}
            """#
        )
        #expect(
            message == .connected(
                ServerMessage.Connected(
                    name: "mac",
                    protocolName: "vnc",
                    resize: false,
                    clipboard: true,
                    audio: false
                )
            )
        )
    }

    @Test
    func remoteOsDecodesBothWays() throws {
        #expect(
            try ServerMessage.decode(#"{"type":"remoteOs","macos":true}"#)
                == .remoteOs(macos: true)
        )
        #expect(
            try ServerMessage.decode(#"{"type":"remoteOs","macos":false}"#)
                == .remoteOs(macos: false)
        )
    }

    /// `virtual` is a Swift keyword-adjacent name the struct spells `isVirtual`,
    /// so the coding key is the one thing here that can silently drift.
    @Test
    func displaysDecodeIncludingTheVirtualFlag() throws {
        let decoded = try ServerMessage.decode(
            #"""
            {"type":"displays","active":9,"displays":[\
            {"id":7,"label":"Display 1","detail":"1920×1080 at 1x","main":true,"virtual":false},\
            {"id":9,"label":"Virtual display","detail":"3200×2000 at 2x","main":false,"virtual":true}]}
            """#
                .replacingOccurrences(of: "\\\n", with: "")
        )
        #expect(
            decoded
                == .displays(
                    active: 9,
                    displays: [
                        .init(
                            id: 7,
                            label: "Display 1",
                            detail: "1920×1080 at 1x",
                            main: true,
                            isVirtual: false
                        ),
                        .init(
                            id: 9,
                            label: "Virtual display",
                            detail: "3200×2000 at 2x",
                            main: false,
                            isVirtual: true
                        ),
                    ]
                )
        )
    }

    /// A Mac can have every screen unplugged, so an empty list is a state to
    /// carry rather than one to treat as drift.
    @Test
    func anEmptyDisplayListDecodes() throws {
        #expect(
            try ServerMessage.decode(#"{"type":"displays","active":0,"displays":[]}"#)
                == .displays(active: 0, displays: [])
        )
    }

    @Test
    func cursorCarriesAnImageOrTheRemoteHidingIt() throws {
        #expect(
            try ServerMessage.decode(
                #"{"type":"cursor","image":"iVBORw0K","w":16,"h":24,"hx":2,"hy":3}"#
            ) == .cursor(
                ServerMessage.Cursor(image: "iVBORw0K", w: 16, h: 24, hx: 2, hy: 3)
            )
        )
        // A hidden pointer: null image, and every dimension zeroed with it.
        #expect(
            try ServerMessage.decode(
                #"{"type":"cursor","image":null,"w":0,"h":0,"hx":0,"hy":0}"#
            ) == .cursor(ServerMessage.Cursor(image: nil, w: 0, h: 0, hx: 0, hy: 0))
        )
    }

    /// Both nullable fields are serialized as explicit `null` rather than
    /// omitted, and each null is meaningful: no observed change time, and not
    /// oversized. Decoding must not confuse either with a zero.
    @Test
    func clipboardDecodesItsNullableFields() throws {
        #expect(
            try ServerMessage.decode(
                #"{"type":"clipboard","text":"hi \"there\"","changedAtMs":1721234567890,"requested":false,"oversizedBytes":null}"#
            ) == .clipboard(
                ServerMessage.Clipboard(
                    text: #"hi "there""#,
                    changedAtMs: 1_721_234_567_890,
                    requested: false,
                    oversizedBytes: nil
                )
            )
        )
        #expect(
            try ServerMessage.decode(
                #"{"type":"clipboard","text":"","changedAtMs":null,"requested":true,"oversizedBytes":null}"#
            ) == .clipboard(
                ServerMessage.Clipboard(
                    text: "",
                    changedAtMs: nil,
                    requested: true,
                    oversizedBytes: nil
                )
            )
        )
    }

    /// Empty text alone means "the remote has copied nothing", so the size is
    /// the only thing keeping that apart from a clipboard too large to transfer.
    @Test
    func anOversizedClipboardKeepsItsSizeAlongsideEmptyText() throws {
        let message = try ServerMessage.decode(
            #"{"type":"clipboard","text":"","changedAtMs":42,"requested":false,"oversizedBytes":209715200}"#
        )
        #expect(
            message == .clipboard(
                ServerMessage.Clipboard(
                    text: "",
                    changedAtMs: 42,
                    requested: false,
                    oversizedBytes: 209_715_200
                )
            )
        )
    }

    /// A gateway that grows a control message must not break an older viewer:
    /// the frame becomes a value the receive loop can step over, not an error.
    @Test
    func anUnknownTypeBecomesAValueRatherThanAnError() throws {
        #expect(
            try ServerMessage.decode(#"{"type":"somethingNew","extra":[1,2,3]}"#)
                == .unsupported(type: "somethingNew")
        )
    }

    /// The exact bytes `src/protocol.rs` pins for this message, `head` included: the
    /// base64 there decodes to the 19-byte `OpusHead` an Opus stream begins with, and
    /// the pre-skip inside it is what the decoder needs (see `OpusDecoder`).
    @Test
    func audioFormatDecodesItsHeadFromBase64() throws {
        let message = try ServerMessage.decode(#"""
        {"type":"audioFormat","codec":"opus","sampleRate":48000,"channels":2,\#
        "head":"T3B1c0hlYWQBAjgBRKwAAAAAAA=="}
        """#)
        guard case .audioFormat(let format) = message else {
            Issue.record("expected audioFormat, got \(message)")
            return
        }
        #expect(format.codec == "opus")
        #expect(format.sampleRate == 48_000)
        #expect(format.channels == 2)
        #expect(format.head.count == 19)
        #expect(format.head.prefix(8) == Data("OpusHead".utf8))
        // Bytes 10–11, little-endian: the encoder's own delay, which the gateway builds
        // rather than stubs.
        #expect(UInt16(format.head[10]) | (UInt16(format.head[11]) << 8) == 312)
    }

    /// A tag this build *does* know, arriving with a payload it cannot read, is
    /// real drift. Reported as such and dropped, rather than silently ignored.
    @Test
    func aKnownTypeWithABadPayloadIsReportedAsMalformed() {
        // A `head` that is not base64 is caught here, where the tag is still known,
        // rather than reaching a decoder as an empty `Data` and being reported as "this
        // Mac cannot play Opus".
        expectMalformed(
            #"""
            {"type":"audioFormat","codec":"opus","sampleRate":48000,"channels":2,\#
            "head":"not base64 !!"}
            """#,
            type: "audioFormat"
        )
        expectMalformed(
            #"{"type":"audioFormat","codec":"opus","sampleRate":48000,"channels":2}"#,
            type: "audioFormat"
        )
        expectMalformed(#"{"type":"resize","w":1280,"scale":1.0}"#, type: "resize")
        // Including a size with no density: the two are only useful together, and
        // guessing 1x for a Retina Mac would draw its desktop at twice its size.
        expectMalformed(#"{"type":"resize","w":1280,"h":800}"#, type: "resize")
        expectMalformed(#"{"type":"connected","name":"mac"}"#, type: "connected")
        expectMalformed(#"{"type":"clipboard","text":"hi"}"#, type: "clipboard")
        // The gateway's w/h are u16. A wider value could not have been sent by
        // one, and is not something to silently truncate.
        expectMalformed(#"{"type":"resize","w":70000,"h":10,"scale":1.0}"#, type: "resize")
    }

    @Test
    func aFrameThatIsNotATaggedObjectIsUntagged() {
        for text in [#"[1,2,3]"#, #""hello""#, #"{"nope":1}"#, #"{"type":7}"#, "not json"] {
            #expect(throws: ServerMessageError.untagged) {
                try ServerMessage.decode(text)
            }
        }
    }

    private func expectMalformed(
        _ text: String,
        type: String,
        sourceLocation: SourceLocation = #_sourceLocation
    ) {
        do {
            let message = try ServerMessage.decode(text)
            Issue.record("expected malformed, decoded \(message)", sourceLocation: sourceLocation)
        } catch let error as ServerMessageError {
            guard case .malformed(let reported, _) = error else {
                Issue.record("expected malformed, got \(error)", sourceLocation: sourceLocation)
                return
            }
            #expect(reported == type, sourceLocation: sourceLocation)
        } catch {
            Issue.record("expected malformed, got \(error)", sourceLocation: sourceLocation)
        }
    }
}
