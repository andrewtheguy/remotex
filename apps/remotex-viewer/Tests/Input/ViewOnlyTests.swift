import AppKit
import Foundation
import Testing
@testable import RemotexViewer

/// View only, checked at the socket rather than at the flag: the mode's whole
/// promise is about what does and does not reach the remote, so a gate that was
/// added to one of the five paths and missed on another has to fail here.
@MainActor
struct ViewOnlyTests {
    @Test
    func nothingTheUserDoesReachesTheRemote() async throws {
        let session = try await Self.interactive()
        let model = session.model
        session.pasteboard.clearContents()
        session.pasteboard.setString("local text", forType: .string)

        model.isViewOnly = true
        model.sendPointer(x: 10, y: 20)
        model.sendWheel(dx: 0, dy: 3)
        model.sendMouseButton(.left, pressed: true)
        model.sendKey(code: "KeyA", pressed: true, caps: false)
        model.clipboard.pushLocalClipboard(force: true)
        try await session.settle()

        #expect(session.sent(ofType: "mouseMove").isEmpty)
        #expect(session.sent(ofType: "wheel").isEmpty)
        #expect(session.sent(ofType: "mouseButton").isEmpty)
        #expect(session.sent(ofType: "key").isEmpty)
        #expect(session.sent(ofType: "clipboard").isEmpty)
        #expect(!model.clipboard.isEnabled, "and the panel cannot ask for one")
    }

    /// The other half of the one above: with the toggle off, every one of those
    /// paths does reach the socket — so the assertions there are about the mode and
    /// not about a harness that never sends anything.
    @Test
    func everyPathReachesTheRemoteWithTheToggleOff() async throws {
        let session = try await Self.interactive()
        let model = session.model
        session.pasteboard.clearContents()
        session.pasteboard.setString("local text", forType: .string)

        model.sendPointer(x: 10, y: 20)
        model.sendWheel(dx: 0, dy: 3)
        model.sendMouseButton(.left, pressed: true)
        model.sendKey(code: "KeyA", pressed: true, caps: false)
        model.clipboard.pushLocalClipboard(force: true)
        try await session.settle()

        #expect(session.sent(ofType: "mouseMove").count == 1)
        #expect(session.sent(ofType: "wheel").count == 1)
        #expect(session.sent(ofType: "mouseButton").count == 1)
        #expect(session.sent(ofType: "key").count == 1)
        #expect(session.sent(ofType: "clipboard").count == 1)
    }

    /// A modifier left down on the remote is this client's worst failure mode, and
    /// switching view only on closes the paths that would have released it — so the
    /// release has to be sent as part of the switch.
    @Test
    func whatWasHeldIsReleasedAsTheToggleGoesOn() async throws {
        let session = try await Self.interactive()
        let model = session.model
        model.sendKey(code: "ShiftLeft", pressed: true, caps: false)
        model.sendMouseButton(.left, pressed: true)
        try await session.settle()

        model.isViewOnly = true
        try await session.settle()

        let keys = session.sent(ofType: "key")
        #expect(keys.count == 2)
        #expect(keys.last?["code"] as? String == "ShiftLeft")
        #expect(keys.last?["pressed"] as? Bool == false)
        let buttons = session.sent(ofType: "mouseButton")
        #expect(buttons.count == 2)
        #expect(buttons.last?["pressed"] as? Bool == false)
    }

    /// Capture itself is suspended, which is the point of the mode: the monitor
    /// swallows every Command chord bar Quit, Close and Settings, so while it is up
    /// this Mac has no shortcuts of its own. Handing input back is not a side effect
    /// of view only — it is what view only is for.
    @Test
    func capturingIsSuspendedSoTheChordsComeBack() async throws {
        let session = try await Self.interactive()
        session.model.apply(.control(.resize(w: 800, h: 600, scale: 1)))
        #expect(session.model.canCaptureKeyboardNow)

        session.model.isViewOnly = true

        #expect(!session.model.canCaptureKeyboardNow)
        #expect(!session.model.canSendInput)

        session.model.isViewOnly = false
        #expect(session.model.canCaptureKeyboardNow, "and resumed on the way back")
    }

    /// The mode outlives a target switch — it says how the viewer is being used,
    /// not anything about what it is attached to — and the clipboard stays off with
    /// it rather than coming back up under the next target.
    @Test
    func theModeSurvivesATargetSwitch() async throws {
        let session = try await Self.interactive()
        session.model.isViewOnly = true

        session.model.apply(.control(.picker))
        session.connect(protocolName: "vnc", clipboard: true)

        #expect(session.model.isViewOnly)
        #expect(!session.model.canSendInput)
        #expect(!session.model.clipboard.isEnabled)
    }

    /// Attached and on the desktop of a target that offers a clipboard, which is
    /// every path this mode closes available at once.
    private static func interactive() async throws -> AttachedSession {
        let session = try await AttachedSession.attached(suite: "ViewOnlyTests")
        session.connect(protocolName: "vnc", clipboard: true)
        #expect(session.model.canSendInput)
        #expect(session.model.clipboard.isEnabled)
        return session
    }
}
