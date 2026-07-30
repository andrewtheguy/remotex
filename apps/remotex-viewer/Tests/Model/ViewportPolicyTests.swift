import Foundation
import Testing
@testable import RemotexViewer

struct ViewportPolicyTests {
    /// Permission is the gateway's answer and it varies by protocol; the mode is
    /// this client's and it does not. So every target the operator opted in starts
    /// the same way — allowed, and manual.
    @Test
    func everyOptedInTargetStartsAllowedAndManual() {
        for name in ["rdp", "vnc"] {
            let policy = ViewportPolicy(protocolName: name, resize: true)
            #expect(policy.allowed, "\(name)")
            #expect(!policy.autoFollows, "\(name)")
        }
    }

    /// The operator's veto, and it is the whole of it: a target without `resize`
    /// sends nothing, asked for or not. The engine would drop the request, and
    /// neither client offers a control that would make one.
    @Test
    func aTargetWithoutResizeSendsNothingEitherWay() {
        for name in ["rdp", "vnc", "rxa"] {
            var policy = ViewportPolicy(protocolName: name, resize: false)
            #expect(!policy.allowed, "\(name)")
            #expect(
                policy.report(DisplayMode(w: 1280, h: 800), manual: false) == nil,
                "\(name)"
            )
            #expect(
                policy.report(DisplayMode(w: 1280, h: 800), manual: true) == nil,
                "not even on request: \(name)"
            )
            policy.autoFollows = true
            #expect(
                policy.report(DisplayMode(w: 1280, h: 800), manual: false) == nil,
                "and the mode cannot grant what the operator withheld: \(name)"
            )
        }
    }

    /// Manual is the default and it means exactly one thing: measuring sends
    /// nothing, asking sends. What used to be RDP's rule alone is now every
    /// target's starting point.
    @Test
    func manualReportsOnlyWhatWasAskedFor() {
        var policy = ViewportPolicy(protocolName: "rdp", resize: true)
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
        var policy = ViewportPolicy(protocolName: "rdp", resize: true)
        policy.autoFollows = true
        #expect(
            policy.report(DisplayMode(w: 1280, h: 800), manual: false)
                == .viewport(w: 1280, h: 800)
        )
    }

    /// A Mac's own screen is set on that Mac, in System Settings, so `resize` on an
    /// rxa target grants nothing until a display list says the shared display is
    /// one the agent made — and a switch back to a real screen takes it away again,
    /// in either mode.
    @Test
    func rxaIsAllowedOnlyTheDisplayTheAgentMade() {
        var policy = ViewportPolicy(protocolName: "rxa", resize: true)
        #expect(
            !policy.allowed,
            "silent until a displays says which screen is being shared"
        )
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: true) == nil)

        policy.sharing(virtualDisplay: true)
        #expect(policy.allowed)
        #expect(
            policy.report(DisplayMode(w: 1440, h: 900), manual: true)
                == .viewport(w: 1440, h: 900)
        )

        policy.autoFollows = true
        #expect(
            policy.report(DisplayMode(w: 1600, h: 1000), manual: false)
                == .viewport(w: 1600, h: 1000),
            "a display made to be looked at from here may follow the window, if asked"
        )

        policy.sharing(virtualDisplay: false)
        #expect(!policy.allowed)
        #expect(
            policy.report(DisplayMode(w: 1280, h: 800), manual: false) == nil,
            "the Mac's own panel is not this window's to resize, mode or no mode"
        )
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: true) == nil)
    }

    /// The display list is an rxa message and must stay one. Neither of the other
    /// two protocols has a display the client may resize, so a gateway that sent
    /// them a list must not be able to change what they allow.
    @Test
    func aDisplayListCannotChangeRdpOrVnc() {
        for (name, resize) in [
            ("rdp", true), ("rdp", false), ("vnc", true), ("vnc", false),
        ] {
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

    /// Dragging a window edge measures a new size every frame. Without this, an
    /// auto-following remote would be told to resize on each one.
    @Test
    func anUnchangedSizeIsNotReportedTwice() {
        var policy = ViewportPolicy(protocolName: "vnc", resize: true)
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
        var policy = ViewportPolicy(protocolName: "vnc", resize: true)
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
        var policy = ViewportPolicy(protocolName: "rdp", resize: true)
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: true) != nil)
        #expect(policy.report(DisplayMode(w: 1280, h: 800), manual: true) == nil)
    }
}
