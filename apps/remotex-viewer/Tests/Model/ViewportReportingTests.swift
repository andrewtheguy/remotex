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
        // memo, outlive the trip to the picker. Turning auto on remembered it, so
        // the reconnect re-applies the mode on its own and re-sends the measured
        // viewport — that same size gets out only because the memo was reset on
        // `connected`.
        session.model.apply(.control(.picker))
        session.connect(protocolName: "vnc")
        #expect(session.model.autoResizes, "the remembered default is re-applied on connect")

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

    /// A press of "Resize to Window" reaches the socket, and measuring alone does
    /// not — three components and a screen, which is where both of the "it never
    /// resized" bugs lived.
    @Test
    func askingResizesTheWindowAndMeasuringDoesNot() async throws {
        let session = try await Self.attached()
        session.connect(protocolName: "vnc", resize: true)

        session.model.reportViewport(DisplayMode(w: 1440, h: 900))
        try await session.settle()
        #expect(session.viewports.isEmpty, "measuring is not asking")

        session.model.resizeToWindow()
        try await session.expectViewport(w: 1440, h: 900)
    }

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
