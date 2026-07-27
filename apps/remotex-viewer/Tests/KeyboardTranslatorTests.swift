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
        #expect(
            translator.translate(event(.flagsChanged, 0x37, [.command], sideMask: 0x0008)).isEmpty
        )
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
    func commandVStaysCommandVForAMacGuest() {
        var translator = KeyboardTranslator()
        #expect(
            translator.translate(
                event(.flagsChanged, 0x37, [.command], sideMask: 0x0008),
                mapCommandToControl: false
            )
                == [
                    TranslatedKeyEvent(code: "MetaLeft", pressed: true, caps: false),
                ]
        )
        #expect(
            translator.translate(
                event(.keyDown, 0x09, [.command]),
                mapCommandToControl: false
            )
                == [
                    TranslatedKeyEvent(code: "KeyV", pressed: true, caps: false),
                ]
        )
        #expect(
            translator.translate(
                event(.keyUp, 0x09, [.command]),
                mapCommandToControl: false
            )
                == [
                    TranslatedKeyEvent(code: "KeyV", pressed: false, caps: false),
                ]
        )
        #expect(
            translator.translate(
                event(.flagsChanged, 0x37, []),
                mapCommandToControl: false
            )
                == [
                    TranslatedKeyEvent(code: "MetaLeft", pressed: false, caps: false),
                ]
        )
    }

    @Test
    func commandVStaysCommandVWhenKeyboardOverridesAreDisabled() {
        var translator = KeyboardTranslator()
        #expect(
            translator.translate(
                event(.flagsChanged, 0x37, [.command], sideMask: 0x0008),
                mapCommandToControl: false
            )
                == [
                    TranslatedKeyEvent(code: "MetaLeft", pressed: true, caps: false),
                ]
        )
        #expect(
            translator.translate(
                event(.keyDown, 0x09, [.command]),
                mapCommandToControl: false
            )
                == [
                    TranslatedKeyEvent(code: "KeyV", pressed: true, caps: false),
                ]
        )
    }

    @Test
    func bareCommandTapsTheRemoteWindowsKey() {
        var translator = KeyboardTranslator()
        #expect(
            translator.translate(event(.flagsChanged, 0x37, [.command], sideMask: 0x0008)).isEmpty
        )
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
        _ = translator.translate(event(.flagsChanged, 0x37, [.command], sideMask: 0x0008))
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

    /// The chord above ends with Command released, so the next bare tap is a fresh
    /// one. The flag that suppresses the synthetic tap during a chord was only
    /// cleared on the path that does *not* forward a Command release, so the tap
    /// after any forwarded chord was swallowed — one Windows key press lost per
    /// Command chord, recovered only by the tap after it.
    @Test
    func aBareCommandTapStillWorksAfterAForwardedChord() {
        var translator = KeyboardTranslator()
        _ = translator.translate(event(.flagsChanged, 0x37, [.command], sideMask: 0x0008))
        _ = translator.translate(event(.keyDown, 0x0C, [.command]))
        _ = translator.translate(event(.keyUp, 0x0C, [.command]))
        _ = translator.translate(event(.flagsChanged, 0x37, []))

        #expect(
            translator.translate(event(.flagsChanged, 0x37, [.command], sideMask: 0x0008)).isEmpty
        )
        #expect(
            translator.translate(event(.flagsChanged, 0x37, []))
                == [
                    TranslatedKeyEvent(code: "MetaLeft", pressed: true, caps: false),
                    TranslatedKeyEvent(code: "MetaLeft", pressed: false, caps: false),
                ]
        )
    }

    @Test
    func releasingOneModifierKeepsOnlyTheOtherSidePressed() {
        let pairs: [
            (
                independent: NSEvent.ModifierFlags,
                leftCode: UInt16,
                rightCode: UInt16,
                leftName: String,
                rightName: String,
                leftMask: UInt,
                rightMask: UInt,
                mapCommandToControl: Bool
            )
        ] = [
            (.shift, 0x38, 0x3C, "ShiftLeft", "ShiftRight", 0x0002, 0x0004, true),
            (.control, 0x3B, 0x3E, "ControlLeft", "ControlRight", 0x0001, 0x2000, true),
            (.option, 0x3A, 0x3D, "AltLeft", "AltRight", 0x0020, 0x0040, true),
            (.command, 0x37, 0x36, "MetaLeft", "MetaRight", 0x0008, 0x0010, false),
        ]

        for pair in pairs {
            var translator = KeyboardTranslator()
            #expect(
                translator.translate(
                    event(.flagsChanged, pair.leftCode, pair.independent, sideMask: pair.leftMask),
                    mapCommandToControl: pair.mapCommandToControl
                )
                    == [
                        TranslatedKeyEvent(code: pair.leftName, pressed: true, caps: false),
                    ]
            )
            #expect(
                translator.translate(
                    event(
                        .flagsChanged,
                        pair.rightCode,
                        pair.independent,
                        sideMask: pair.leftMask | pair.rightMask
                    ),
                    mapCommandToControl: pair.mapCommandToControl
                )
                    == [
                        TranslatedKeyEvent(code: pair.rightName, pressed: true, caps: false),
                    ]
            )
            #expect(
                translator.translate(
                    event(.flagsChanged, pair.leftCode, pair.independent, sideMask: pair.rightMask),
                    mapCommandToControl: pair.mapCommandToControl
                )
                    == [
                        TranslatedKeyEvent(code: pair.leftName, pressed: false, caps: false),
                    ]
            )
            #expect(
                translator.translate(
                    event(.flagsChanged, pair.rightCode, [], sideMask: 0),
                    mapCommandToControl: pair.mapCommandToControl
                )
                    == [
                        TranslatedKeyEvent(code: pair.rightName, pressed: false, caps: false),
                    ]
            )
        }
    }

    private func event(
        _ type: NSEvent.EventType,
        _ keyCode: UInt16,
        _ modifiers: NSEvent.ModifierFlags,
        sideMask: UInt = 0
    ) -> NSEvent {
        NSEvent.keyEvent(
            with: type,
            location: .zero,
            modifierFlags: NSEvent.ModifierFlags(rawValue: modifiers.rawValue | sideMask),
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
