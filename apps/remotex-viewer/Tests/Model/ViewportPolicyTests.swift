import Foundation
import Testing
@testable import RemotexViewer

struct ViewportPolicyTests {
    /// Permission is the gateway's answer; the mode is this client's. A target the
    /// operator opted in starts the same way for every protocol — allowed, and
    /// manual.
    @Test
    func anOptedInTargetStartsAllowedAndManual() {
        let policy = ViewportPolicy(resize: true)
        #expect(policy.allowed)
        #expect(!policy.autoFollows)
    }

    /// The operator's veto, and it is the whole of it: a target without `resize`
    /// sends nothing, asked for or not. The engine would drop the request, and
    /// neither client offers a control that would make one.
    @Test
    func aTargetWithoutResizeSendsNothingEitherWay() {
        var policy = ViewportPolicy(resize: false)
        #expect(!policy.allowed)
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: false) == nil)
        #expect(
            policy.report(DisplayMode(w: 1280, h: 800), manual: true) == nil,
            "not even on request"
        )
        policy.autoFollows = true
        #expect(
            policy.report(DisplayMode(w: 1280, h: 800), manual: false) == nil,
            "and the mode cannot grant what the operator withheld"
        )
    }

    /// Manual is the default and it means exactly one thing: measuring sends
    /// nothing, asking sends. What used to be RDP's rule alone is now every
    /// target's starting point.
    @Test
    func manualReportsOnlyWhatWasAskedFor() {
        var policy = ViewportPolicy(resize: true)
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: false) == nil)
        #expect(
            policy.report(DisplayMode(w: 1280, h: 800), manual: true)
                == .viewport(w: 1280, h: 800)
        )
    }

    /// And auto means the window drives it, on the same target the case above left
    /// silent — the difference is the client's choice and nothing else.
    @Test
    func autoReportsWhatTheWindowMeasured() {
        var policy = ViewportPolicy(resize: true)
        policy.autoFollows = true
        #expect(
            policy.report(DisplayMode(w: 1280, h: 800), manual: false)
                == .viewport(w: 1280, h: 800)
        )
    }

    /// Dragging a window edge measures a new size every frame. Without this, an
    /// auto-following remote would be told to resize on each one.
    @Test
    func anUnchangedSizeIsNotReportedTwice() {
        var policy = ViewportPolicy(resize: true)
        policy.autoFollows = true
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: false) != nil)
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: false) == nil)
        #expect(policy.report(DisplayMode(w: 1281, h: 800), manual: false) != nil)
        #expect(policy.report(DisplayMode(w: 1281, h: 800), manual: false) == nil)
    }

    /// The size the gateway knew about went away with the previous socket, so the
    /// first report on a new one has to go out even when it has not changed.
    @Test
    func aNewConnectionReportsEvenTheSameSizeAgain() {
        var policy = ViewportPolicy(resize: true)
        let size = DisplayMode(w: 1280, h: 800)
        #expect(policy.report(size, manual: true) != nil)
        #expect(policy.report(size, manual: true) == nil)

        policy.resetForNewConnection()

        #expect(policy.report(size, manual: true) == .viewport(w: 1280, h: 800))
    }

    /// The dedupe applies to a requested report too, so pressing "Resize to
    /// Window" twice at the same size is one message.
    @Test
    func aRepeatedManualRequestIsAlsoDeduped() {
        var policy = ViewportPolicy(resize: true)
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: true) != nil)
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: true) == nil)
    }
}
