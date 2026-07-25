import Testing
@testable import RemotexViewer

struct NativePressedKeysTests {
    @Test
    func startupFocusChangesDoNotRequestReleaseAll() {
        var keys = NativePressedKeys()
        let first = keys.takeForRelease()
        let second = keys.takeForRelease()
        #expect(!first)
        #expect(!second)
    }

    @Test
    func heldKeysRequestExactlyOneReleaseAll() {
        var keys = NativePressedKeys()
        keys.record(code: "MetaLeft", pressed: true)
        keys.record(code: "KeyV", pressed: true)
        let first = keys.takeForRelease()
        let second = keys.takeForRelease()
        #expect(first)
        #expect(!second)
    }

    @Test
    func normallyReleasedKeysDoNotRequestReleaseAll() {
        var keys = NativePressedKeys()
        keys.record(code: "KeyV", pressed: true)
        keys.record(code: "KeyV", pressed: false)
        let requested = keys.takeForRelease()
        #expect(!requested)
    }
}
