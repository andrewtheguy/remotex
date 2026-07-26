import Foundation
import Testing
@testable import RemotexViewer

struct PressedInputTests {
    /// Nothing is held at startup, so a focus notification that arrives before
    /// any input must not send releases for keys nobody pressed.
    @Test
    func nothingHeldReleasesNothing() {
        var pressed = PressedInput()
        #expect(pressed.isEmpty)
        #expect(pressed.takeReleaseMessages().isEmpty)
    }

    /// The protocol has no release-everything message — this is where the old
    /// bridge's single `releaseKeys` command becomes one release per code.
    @Test
    func heldKeysEachProduceTheirOwnRelease() {
        var pressed = PressedInput()
        pressed.record(code: "ControlLeft", pressed: true)
        pressed.record(code: "KeyC", pressed: true)

        #expect(
            pressed.takeReleaseMessages() == [
                .key(code: "ControlLeft", pressed: false, caps: false),
                .key(code: "KeyC", pressed: false, caps: false),
            ]
        )
        #expect(pressed.isEmpty)
    }

    /// `caps` is false on release: the gateway lets go of the keysym it recorded
    /// at press time, so the lock state is not consulted.
    @Test
    func releasesCarryNoCapsLockState() {
        var pressed = PressedInput()
        pressed.record(code: "KeyA", pressed: true)
        guard case .key(_, _, let caps) = pressed.takeReleaseMessages().first else {
            Issue.record("expected a key release")
            return
        }
        #expect(caps == false)
    }

    @Test
    func aKeyReleasedNormallyIsNotReleasedAgain() {
        var pressed = PressedInput()
        pressed.record(code: "KeyA", pressed: true)
        pressed.record(code: "KeyA", pressed: false)
        #expect(pressed.isEmpty)
        #expect(pressed.takeReleaseMessages().isEmpty)
    }

    /// Two focus notifications for one event must not send two rounds of
    /// releases, which on a remote that re-presses on repeat would be visible.
    @Test
    func takingReleasesTwiceOnlySendsThemOnce() {
        var pressed = PressedInput()
        pressed.record(code: "ShiftLeft", pressed: true)
        #expect(pressed.takeReleaseMessages().count == 1)
        #expect(pressed.takeReleaseMessages().isEmpty)
    }

    /// A button held when the window loses key status has to be let go too — the
    /// remote would otherwise be left mid-drag.
    @Test
    func heldButtonsAreReleasedAlongsideKeys() {
        var pressed = PressedInput()
        pressed.record(code: "KeyA", pressed: true)
        pressed.record(button: .left, pressed: true)
        pressed.record(button: .right, pressed: true)

        #expect(
            pressed.takeReleaseMessages() == [
                .key(code: "KeyA", pressed: false, caps: false),
                .mouseButton(button: .left, pressed: false),
                .mouseButton(button: .right, pressed: false),
            ]
        )
    }

    @Test
    func aButtonReleasedNormallyIsNotReleasedAgain() {
        var pressed = PressedInput()
        pressed.record(button: .middle, pressed: true)
        pressed.record(button: .middle, pressed: false)
        #expect(pressed.takeReleaseMessages().isEmpty)
    }
}
