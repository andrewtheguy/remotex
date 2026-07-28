import Foundation
import Testing
@testable import RemotexViewer

struct ViewportPolicyTests {
    /// VNC is the only engine that can resize cheaply, so it is the only one
    /// followed continuously.
    @Test
    func vncFollowsTheWindow() {
        var policy = ViewportPolicy(protocolName: "vnc", resize: true)
        #expect(!policy.manualOnly)
        #expect(!policy.ignoresViewport)
        #expect(
            policy.report(DisplayMode(w: 1280, h: 800), manual: false)
                == .viewport(w: 1280, h: 800)
        )
    }

    /// An RDP resize forces a Deactivation-Reactivation — expensive and visible —
    /// so it happens only when the user asks for it.
    @Test
    func rdpOnlyResizesOnRequest() {
        var policy = ViewportPolicy(protocolName: "rdp", resize: true)
        #expect(policy.manualOnly)

        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: false) == nil)
        #expect(
            policy.report(DisplayMode(w: 1280, h: 800), manual: true)
                == .viewport(w: 1280, h: 800)
        )
    }

    /// A Mac's own screen is set on that Mac, in System Settings, so a viewport
    /// report means nothing to a target the operator did not opt in — and the
    /// display's half of the permission cannot stand in for the target's.
    @Test
    func rxaWithoutResizeIgnoresViewportsEntirely() {
        var policy = ViewportPolicy(protocolName: "rxa", resize: false)
        #expect(policy.ignoresViewport)
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: false) == nil)
        #expect(
            policy.report(DisplayMode(w: 1280, h: 800), manual: true) == nil,
            "not even on request"
        )

        policy.sharing(virtualDisplay: true)
        #expect(
            policy.report(DisplayMode(w: 1280, h: 800), manual: true) == nil,
            "an agent-made display does not grant what the target withheld"
        )
    }

    /// With `resize` the target allows it, but only a display the agent *made*
    /// can act on it — so the control appears when that display is shared and
    /// disappears again on a switch to one of the Mac's own screens.
    @Test
    func rxaResizesOnlyTheDisplayTheAgentMade() {
        var policy = ViewportPolicy(protocolName: "rxa", resize: true)
        #expect(
            policy.ignoresViewport,
            "silent until a displays says which screen is being shared"
        )
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: true) == nil)

        policy.sharing(virtualDisplay: true)
        #expect(policy.manualOnly)
        #expect(
            policy.report(DisplayMode(w: 1440, h: 900), manual: false) == nil,
            "still on request only: this display is not dragged around by a window"
        )
        #expect(
            policy.report(DisplayMode(w: 1440, h: 900), manual: true)
                == .viewport(w: 1440, h: 900)
        )

        policy.sharing(virtualDisplay: false)
        #expect(policy.ignoresViewport)
        #expect(
            policy.report(DisplayMode(w: 1600, h: 1000), manual: true) == nil,
            "the Mac's own panel is not this window's to resize"
        )
    }

    /// The display list is an rxa message and must stay one. Neither of the other
    /// two protocols has a display the client may resize, so a gateway that sent
    /// them a list must not be able to change how they behave.
    @Test
    func aDisplayListCannotChangeRdpOrVnc() {
        for (name, resize) in [("rdp", true), ("rdp", false), ("vnc", false)] {
            var policy = ViewportPolicy(protocolName: name, resize: resize)
            let before = policy
            policy.sharing(virtualDisplay: true)
            #expect(policy == before, "\(name) resize=\(resize)")
            policy.sharing(virtualDisplay: false)
            #expect(policy == before, "\(name) resize=\(resize)")
        }
    }

    /// The dedupe is about the display, not about the connection, so switching
    /// away and back does not re-send: that display is still exactly the size it
    /// was left at.
    @Test
    func theDedupeSurvivesADisplaySwitch() {
        var policy = ViewportPolicy(protocolName: "rxa", resize: true)
        policy.sharing(virtualDisplay: true)
        #expect(
            policy.report(DisplayMode(w: 1440, h: 900), manual: true)
                == .viewport(w: 1440, h: 900)
        )
        policy.sharing(virtualDisplay: false)
        policy.sharing(virtualDisplay: true)
        #expect(policy.report(DisplayMode(w: 1440, h: 900), manual: true) == nil)
    }

    /// A target that cannot resize is still reported to — the engine ignores it,
    /// which is what the web client does too, and suppressing it here would mean
    /// two rules where one suffices.
    @Test
    func aTargetWithoutResizeIsStillFollowed() {
        var policy = ViewportPolicy(protocolName: "vnc", resize: false)
        #expect(!policy.manualOnly)
        #expect(
            policy.report(DisplayMode(w: 800, h: 600), manual: false)
                == .viewport(w: 800, h: 600)
        )
    }

    /// Dragging a window edge measures a new size every frame. Without this, VNC
    /// would be told to resize on each one.
    @Test
    func anUnchangedSizeIsNotReportedTwice() {
        var policy = ViewportPolicy(protocolName: "vnc", resize: true)
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: false) != nil)
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: false) == nil)
        #expect(policy.report(DisplayMode(w: 1281, h: 800), manual: false) != nil)
        #expect(policy.report(DisplayMode(w: 1281, h: 800), manual: false) == nil)
    }

    /// The size the gateway knew about went away with the previous socket, so the
    /// first report on a new one has to go out even when it has not changed.
    @Test
    func aNewConnectionReportsEvenTheSameSizeAgain() {
        var policy = ViewportPolicy(protocolName: "vnc", resize: true)
        let size = DisplayMode(w: 1280, h: 800)
        #expect(policy.report(size, manual: false) != nil)
        #expect(policy.report(size, manual: false) == nil)

        policy.resetForNewConnection()

        #expect(policy.report(size, manual: false) == .viewport(w: 1280, h: 800))
    }

    /// The dedupe applies to a requested report too, so pressing "Resize to
    /// Window" twice at the same size is one message.
    @Test
    func aRepeatedManualRequestIsAlsoDeduped() {
        var policy = ViewportPolicy(protocolName: "rdp", resize: true)
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: true) != nil)
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: true) == nil)
    }
}
