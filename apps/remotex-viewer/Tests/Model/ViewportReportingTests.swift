import AppKit
import Foundation
import Testing
@testable import RemotexViewer

/// What actually reaches the socket when the window is measured, driven through a
/// whole session over a scripted transport.
///
/// `ViewportPolicy` already covers which protocols want a report. These cover the
/// wiring around it — three components and a screen — which is where both of the
/// "it never resized to the window" bugs lived.
@MainActor
struct ViewportReportingTests {
    /// Connecting reports nothing: manual is where every session starts, and a
    /// remote that was already the size it wanted has not been asked to change.
    @Test
    func connectingReportsNothing() async throws {
        let session = try await Self.attached()
        session.model.reportViewport(DisplayMode(w: 1600, h: 1000))

        session.connect(protocolName: "vnc")

        try await session.settle()
        #expect(session.viewports.isEmpty)
    }

    /// Switching auto on is what sizes the engine now, and it repeats a size that
    /// was already measured — in the picker, or for the previous target. Two dedupes
    /// stand between it and the socket: the policy's, reset on `connected`, and the
    /// queue's, which needs resetting there too because it is reset on a new socket
    /// and a target switch keeps the socket it has.
    @Test
    func autoResizeSendsTheMeasuredViewportEvenAfterASwitch() async throws {
        let session = try await Self.attached()
        session.model.reportViewport(DisplayMode(w: 1600, h: 1000))

        session.connect(protocolName: "vnc")
        session.model.setAutoResize(true)
        try await session.expectViewport(w: 1600, h: 1000)

        // Switch away and back to the same size: the socket, and so the queue's
        // memo, outlive the trip to the picker — and the mode does not, so it has
        // to be asked for again.
        session.model.apply(.control(.picker))
        session.connect(protocolName: "vnc")
        #expect(!session.model.autoResizes, "the mode belongs to the session that ended")
        session.model.setAutoResize(true)

        try await session.expectViewport(w: 1600, h: 1000, count: 2)
    }

    /// The surface exists for the picker too — the framebuffer has to survive a
    /// trip there and back — so resizing the window while choosing a target
    /// measures something with no engine to resize. Sending it also taught the
    /// queue's dedupe the size, which then swallowed the report from `connected`.
    @Test
    func measuringInThePickerReportsNothing() async throws {
        let session = try await Self.attached()
        #expect(session.model.session.screen == .picker)

        session.model.reportViewport(DisplayMode(w: 1600, h: 1000))

        try await session.settle()
        #expect(session.viewports.isEmpty)
    }

    /// The debounce, from the other side: a window dragged across many sizes ends
    /// as one report, not one per frame.
    @Test
    func aDragResizeCollapsesIntoOneReport() async throws {
        let session = try await Self.attached()
        session.connect(protocolName: "vnc")
        session.model.setAutoResize(true)
        try await session.settle()

        for width in stride(from: 1200, through: 1600, by: 50) {
            session.model.reportViewport(DisplayMode(w: UInt16(width), h: 1000))
        }

        try await session.expectViewport(w: 1600, h: 1000)
    }

    /// A Mac's screens are set on that Mac, so nothing an rxa session measures
    /// reaches the socket while a real screen is shared — and in manual mode, which
    /// is where it starts, nothing does even once the agent's own display is up.
    @Test
    func anRxaTargetReportsNothingUntilBothHalvesAgree() async throws {
        let session = try await Self.attached()
        session.connect(protocolName: "rxa")

        session.model.reportViewport(DisplayMode(w: 1600, h: 1000))
        try await session.settle()
        #expect(session.viewports.isEmpty)

        // The agent's own display arrives: that raises the three menu items, and by
        // itself sends nothing — manual is still manual.
        session.model.apply(.control(.displays(active: 9, displays: Self.displays)))
        session.model.reportViewport(DisplayMode(w: 1440, h: 900))
        try await session.settle()
        #expect(session.viewports.isEmpty)
        #expect(session.model.canAutoResize, "and now it may be asked for")

        // Asked for, it follows the window like any other allowed target: a display
        // made to be looked at from here has nobody sitting at it.
        session.model.setAutoResize(true)
        try await session.expectViewport(w: 1440, h: 900)
        session.model.reportViewport(DisplayMode(w: 1280, h: 800))
        try await session.expectViewport(w: 1280, h: 800, count: 2)

        // And a switch back to one of the Mac's own screens stops it, mode or no
        // mode: the permission is the display's, not the session's.
        session.model.apply(.control(.displays(active: 7, displays: Self.displays)))
        session.model.reportViewport(DisplayMode(w: 1600, h: 1000))
        try await session.settle()
        #expect(session.viewports.count == 2)
    }

    /// And the whole path a press of "Resize to Window" takes: three components
    /// and a screen, which is where both of the "it never resized" bugs lived.
    /// The display list is what unlocks it, and switching to a real screen locks
    /// it again — with the automatic report in between still swallowed.
    @Test
    func askingResizesAnAgentMadeDisplayAndNothingElse() async throws {
        let session = try await Self.attached()
        session.connect(protocolName: "rxa", resize: true)
        session.model.apply(.control(.displays(active: 9, displays: Self.displays)))

        session.model.reportViewport(DisplayMode(w: 1440, h: 900))
        try await session.settle()
        #expect(session.viewports.isEmpty, "measuring is not asking")

        session.model.resizeToWindow()
        try await session.expectViewport(w: 1440, h: 900)

        // Onto one of the Mac's own screens: the item greys out, and a press that
        // somehow got through would still send nothing.
        session.model.apply(.control(.displays(active: 7, displays: Self.displays)))
        #expect(!session.model.session.canResize)
        session.model.resizeToWindow()
        try await session.settle()
        #expect(session.viewports.count == 1)
    }

    /// The fake Mac's two screens: one somebody is sitting at, and the one the
    /// agent made to be looked at from here.
    private static let displays: [ServerMessage.DisplayInfo] = [
        .init(id: 7, label: "Display 1", detail: "1920×1080 at 1x", main: true, isVirtual: false),
        .init(
            id: 9,
            label: "Virtual display",
            detail: "3200×2000 at 2x",
            main: false,
            isVirtual: true
        ),
    ]

    private static func attached() async throws -> AttachedSession {
        try await AttachedSession.attached(suite: "ViewportReportingTests")
    }
}

/// The viewport half of what an attached session can be asked about. On the shared
/// harness rather than in it, because nothing else cares how a `viewport` frame is
/// spelled.
@MainActor
extension AttachedSession {
    /// The viewports the socket has actually been sent, in order.
    var viewports: [DisplayMode] {
        sent(ofType: "viewport").compactMap { frame in
            guard let w = frame["w"] as? Int, let h = frame["h"] as? Int else {
                return nil
            }
            return DisplayMode(w: UInt16(w), h: UInt16(h))
        }
    }

    /// Polled rather than slept on past the 250ms debounce, so a report that
    /// arrives promptly is not waited out.
    func expectViewport(w: UInt16, h: UInt16, count: Int = 1) async throws {
        for _ in 0..<200 {
            if viewports.count >= count {
                break
            }
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(viewports.count == count)
        #expect(viewports.last == DisplayMode(w: w, h: h))
    }
}
