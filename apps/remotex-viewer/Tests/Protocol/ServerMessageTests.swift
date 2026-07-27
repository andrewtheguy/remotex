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
            try ServerMessage.decode(#"{"type":"resize","w":1280,"h":800}"#)
                == .resize(w: 1280, h: 800)
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
            #"{"type":"connected","name":"mac","protocol":"rxa","resize":false,"clipboard":true}"#
        )
        #expect(
            message == .connected(
                ServerMessage.Connected(
                    name: "mac",
                    protocolName: "rxa",
                    resize: false,
                    clipboard: true
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

    @Test
    func displayModesDecodeInOrderAndEmpty() throws {
        #expect(
            try ServerMessage.decode(
                #"{"type":"displayModes","modes":[{"w":1290,"h":830},{"w":1024,"h":768}]}"#
            ) == .displayModes(modes: [
                DisplayMode(w: 1290, h: 830),
                DisplayMode(w: 1024, h: 768),
            ])
        )
        // An empty list is how "there is nothing to offer" travels, so it must
        // decode to an empty menu rather than failing.
        #expect(
            try ServerMessage.decode(#"{"type":"displayModes","modes":[]}"#)
                == .displayModes(modes: [])
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

    /// A tag this build *does* know, arriving with a payload it cannot read, is
    /// real drift. Reported as such and dropped, rather than silently ignored.
    @Test
    func aKnownTypeWithABadPayloadIsReportedAsMalformed() {
        expectMalformed(#"{"type":"resize","w":1280}"#, type: "resize")
        expectMalformed(#"{"type":"connected","name":"mac"}"#, type: "connected")
        expectMalformed(#"{"type":"clipboard","text":"hi"}"#, type: "clipboard")
        // The gateway's w/h are u16. A wider value could not have been sent by
        // one, and is not something to silently truncate.
        expectMalformed(#"{"type":"resize","w":70000,"h":10}"#, type: "resize")
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
