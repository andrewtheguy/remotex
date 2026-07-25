import AppKit
import Testing
@testable import RemotexViewer

struct KeyboardTranslatorTests {
    @Test
    func mapsMacVirtualKeycodesToCanonicalDOMCodes() {
        #expect(KeyboardTranslator.domCode(for: 0x00) == "KeyA")
        #expect(KeyboardTranslator.domCode(for: 0x09) == "KeyV")
        #expect(KeyboardTranslator.domCode(for: 0x60) == "F5")
        #expect(KeyboardTranslator.domCode(for: 0x67) == "F11")
        #expect(KeyboardTranslator.domCode(for: 0x7E) == "ArrowUp")
    }

    @Test
    func unknownMacKeycodesAreNotInvented() {
        #expect(KeyboardTranslator.domCode(for: 0xFFFF) == nil)
    }

    @Test
    func commandVBecomesControlVWithoutAWindowsKey() {
        var translator = KeyboardTranslator()
        #expect(translator.translate(event(.flagsChanged, 0x37, [.command])).isEmpty)
        #expect(
            translator.translate(event(.keyDown, 0x09, [.command]))
                == [
                    TranslatedKeyEvent(code: "ControlLeft", pressed: true, caps: false),
                    TranslatedKeyEvent(code: "KeyV", pressed: true, caps: false),
                ]
        )
        #expect(
            translator.translate(event(.keyUp, 0x09, [.command]))
                == [
                    TranslatedKeyEvent(code: "KeyV", pressed: false, caps: false),
                    TranslatedKeyEvent(code: "ControlLeft", pressed: false, caps: false),
                ]
        )
        #expect(translator.translate(event(.flagsChanged, 0x37, [])).isEmpty)
    }

    @Test
    func bareCommandTapsTheRemoteWindowsKey() {
        var translator = KeyboardTranslator()
        #expect(translator.translate(event(.flagsChanged, 0x37, [.command])).isEmpty)
        #expect(
            translator.translate(event(.flagsChanged, 0x37, []))
                == [
                    TranslatedKeyEvent(code: "MetaLeft", pressed: true, caps: false),
                    TranslatedKeyEvent(code: "MetaLeft", pressed: false, caps: false),
                ]
        )
    }

    @Test
    func commandQIsForwardedAsAWindowsChordInsteadOfQuittingLocally() {
        var translator = KeyboardTranslator()
        _ = translator.translate(event(.flagsChanged, 0x37, [.command]))
        #expect(
            translator.translate(event(.keyDown, 0x0C, [.command]))
                == [
                    TranslatedKeyEvent(code: "MetaLeft", pressed: true, caps: false),
                    TranslatedKeyEvent(code: "KeyQ", pressed: true, caps: false),
                ]
        )
        #expect(
            translator.translate(event(.keyUp, 0x0C, [.command]))
                == [
                    TranslatedKeyEvent(code: "KeyQ", pressed: false, caps: false),
                ]
        )
        #expect(
            translator.translate(event(.flagsChanged, 0x37, []))
                == [
                    TranslatedKeyEvent(code: "MetaLeft", pressed: false, caps: false),
                ]
        )
    }

    private func event(
        _ type: NSEvent.EventType,
        _ keyCode: UInt16,
        _ modifiers: NSEvent.ModifierFlags
    ) -> NSEvent {
        NSEvent.keyEvent(
            with: type,
            location: .zero,
            modifierFlags: modifiers,
            timestamp: 0,
            windowNumber: 0,
            context: nil,
            characters: "",
            charactersIgnoringModifiers: "",
            isARepeat: false,
            keyCode: keyCode
        )!
    }
}
