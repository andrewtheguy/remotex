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

    /// A Mac's resolution is set on that Mac — in System Settings, whether the
    /// screen is one of its own or one the agent created — so a viewport report
    /// means nothing to it, including one the user asked for.
    @Test
    func rxaIgnoresViewportsEntirely() {
        for resize in [true, false] {
            var policy = ViewportPolicy(protocolName: "rxa", resize: resize)
            #expect(policy.ignoresViewport)
            #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: false) == nil)
            #expect(
                policy.report(DisplayMode(w: 1280, h: 800), manual: true) == nil,
                "not even on request"
            )
        }
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
