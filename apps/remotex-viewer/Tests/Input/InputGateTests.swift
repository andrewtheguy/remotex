import AppKit
import Foundation
import Testing
@testable import RemotexViewer

/// `canSendInput`, checked at the socket rather than at the flag: there is one gate
/// and five paths through it, so a gate that was added to one of them and missed on
/// another has to fail here.
///
/// The gate is a live desktop and nothing else. Whether a target *accepts* what is
/// sent is the target's own question, decided where it is configured — this client
/// has no mode for holding input back.
@MainActor
struct InputGateTests {
    /// A live desktop: every one of the five paths reaches the socket. The other
    /// tests here assert emptiness, so this is what says the harness sends anything
    /// at all.
    @Test
    func everyPathReachesALiveDesktop() async throws {
        let session = try await Self.interactive()
        let model = session.model
        session.pasteboard.clearContents()
        session.pasteboard.setString("local text", forType: .string)

        model.sendPointer(x: 10, y: 20)
        model.sendWheel(dx: 0, dy: 3)
        model.sendMouseButton(.left, pressed: true, clicks: 1)
        model.sendKey(code: "KeyA", pressed: true, caps: false)
        model.clipboard.pushLocalClipboard(force: true)
        try await session.settle()

        #expect(session.sent(ofType: "mouseMove").count == 1)
        #expect(session.sent(ofType: "wheel").count == 1)
        #expect(session.sent(ofType: "mouseButton").count == 1)
        #expect(session.sent(ofType: "key").count == 1)
        #expect(session.sent(ofType: "clipboard").count == 1)
    }

    /// Off the desktop, nothing the user does reaches the remote — the picker is over
    /// the surface, and a stray event under it belongs to no session. Every path
    /// again, because the gate is what is being tested and not any one caller.
    @Test
    func nothingReachesTheRemoteOffTheDesktop() async throws {
        let session = try await Self.interactive()
        let model = session.model
        session.pasteboard.clearContents()
        session.pasteboard.setString("local text", forType: .string)
        model.apply(.control(.picker))
        try await session.settle()
        let already = session.sent(ofType: "clipboard").count

        model.sendPointer(x: 10, y: 20)
        model.sendWheel(dx: 0, dy: 3)
        model.sendKey(code: "KeyA", pressed: true, caps: false)
        model.clipboard.pushLocalClipboard(force: true)
        try await session.settle()

        #expect(session.sent(ofType: "mouseMove").isEmpty)
        #expect(session.sent(ofType: "wheel").isEmpty)
        #expect(session.sent(ofType: "key").isEmpty)
        #expect(session.sent(ofType: "clipboard").count == already)
        #expect(!model.clipboard.isEnabled, "and the panel cannot ask for one")
    }

    /// A modifier left down on the remote is this client's worst failure mode, and
    /// the keyboard convention changing under a held key closes the path that would
    /// have released it — so the release is part of the change.
    @Test
    func whatWasHeldIsReleasedWhenTheConventionChanges() async throws {
        let session = try await Self.interactive()
        let model = session.model
        model.sendKey(code: "ShiftLeft", pressed: true, caps: false)
        model.sendMouseButton(.left, pressed: true, clicks: 1)
        try await session.settle()

        model.macOSKeyboardOverridesEnabled.toggle()
        try await session.settle()

        let keys = session.sent(ofType: "key")
        #expect(keys.count == 2)
        #expect(keys.last?["code"] as? String == "ShiftLeft")
        #expect(keys.last?["pressed"] as? Bool == false)
        let buttons = session.sent(ofType: "mouseButton")
        #expect(buttons.count == 2)
        #expect(buttons.last?["pressed"] as? Bool == false)
    }

    /// The exception is for a button this client recorded, and *held* is the part that
    /// has to be checked rather than assumed: off the desktop, a release for a button
    /// the remote was never told about is the one input event that would otherwise get
    /// past a closed gate.
    @Test
    func anUnrecordedReleaseStaysBehindTheClosedGate() async throws {
        let session = try await Self.interactive()
        let model = session.model
        model.sendMouseButton(.left, pressed: true, clicks: 1)
        model.apply(.control(.picker))
        model.sendMouseButton(.left, pressed: false, clicks: 1)
        try await session.settle()
        #expect(
            session.sent(ofType: "mouseButton").count == 2,
            "the press, and the release the exception let through for it"
        )

        // The same button again, now that nothing is recorded as held.
        model.sendMouseButton(.left, pressed: false, clicks: 1)
        try await session.settle()

        #expect(session.sent(ofType: "mouseButton").count == 2, "and nothing after it")
    }

    /// The same exception, doing its job. Off the desktop with nothing having
    /// released for us, a button recorded as held still has to be able to come up —
    /// a modifier or a button left down on the remote is this client's worst
    /// failure, and closing the path outright would be how that happens.
    @Test
    func aHeldButtonStillComesUpOffTheDesktop() async throws {
        let session = try await Self.interactive()
        let model = session.model
        model.sendMouseButton(.left, pressed: true, clicks: 1)
        model.apply(.control(.picker))
        try await session.settle()

        model.sendMouseButton(.left, pressed: false, clicks: 1)
        try await session.settle()

        let buttons = session.sent(ofType: "mouseButton")
        #expect(buttons.count == 2)
        #expect(buttons.last?["pressed"] as? Bool == false)
    }

    /// Capture follows the same gate one step further in. It is a local event monitor
    /// that swallows every Command chord the system delivers to this app — Quit
    /// included — so while it is up this Mac has no shortcuts of its own, and leaving
    /// the desktop is what hands them back.
    @Test
    func capturingStopsWithTheDesktop() async throws {
        let session = try await Self.interactive()
        session.model.apply(.control(.resize(w: 800, h: 600, scale: 1)))
        #expect(session.model.canCaptureKeyboardNow)

        session.model.apply(.control(.picker))

        #expect(!session.model.canCaptureKeyboardNow)
        #expect(!session.model.canSendInput)
    }

    /// A target arrives driveable. There is nothing to answer for it beforehand, and
    /// nothing carried over from the session before it.
    @Test
    func aFreshTargetIsDriveable() async throws {
        let session = try await AttachedSession.attached(suite: "InputGateTests")
        #expect(session.model.session.screen == .picker)
        #expect(!session.model.canSendInput, "not from the picker")

        session.connect(protocolName: "vnc", clipboard: true)

        #expect(session.model.canSendInput)
        #expect(session.model.clipboard.isEnabled)
    }

    /// Attached and on the desktop of a target that offers a clipboard, which is
    /// every path the gate covers available at once.
    private static func interactive() async throws -> AttachedSession {
        let session = try await AttachedSession.attached(suite: "InputGateTests")
        session.connect(protocolName: "vnc", clipboard: true)
        #expect(session.model.canSendInput)
        #expect(session.model.clipboard.isEnabled)
        return session
    }
}
