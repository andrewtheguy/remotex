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
    /// The report from `connected` is the one that sizes a freshly started engine,
    /// and it repeats a size that was already measured — in the picker, or for the
    /// previous target. Two dedupes stood between it and the socket: the policy's,
    /// which was reset, and the queue's, which was not, because it is reset on a
    /// new socket and a target switch keeps the socket it has.
    @Test
    func connectingSendsTheMeasuredViewportEvenAfterASwitch() async throws {
        let session = try await Session.attached()
        session.model.reportViewport(DisplayMode(w: 1600, h: 1000))

        session.connect(protocolName: "vnc")
        try await session.expectViewport(w: 1600, h: 1000)

        // Switch away and back to the same size: the socket, and so the queue's
        // memo, outlive the trip to the picker.
        session.model.apply(.control(.picker))
        session.connect(protocolName: "vnc")

        try await session.expectViewport(w: 1600, h: 1000, count: 2)
    }

    /// The surface exists for the picker too — the framebuffer has to survive a
    /// trip there and back — so resizing the window while choosing a target
    /// measures something with no engine to resize. Sending it also taught the
    /// queue's dedupe the size, which then swallowed the report from `connected`.
    @Test
    func measuringInThePickerReportsNothing() async throws {
        let session = try await Session.attached()
        #expect(session.model.session.screen == .picker)

        session.model.reportViewport(DisplayMode(w: 1600, h: 1000))

        try await session.settle()
        #expect(session.viewports.isEmpty)
    }

    /// The debounce, from the other side: a window dragged across many sizes ends
    /// as one report, not one per frame.
    @Test
    func adragResizeCollapsesIntoOneReport() async throws {
        let session = try await Session.attached()
        session.connect(protocolName: "vnc")
        try await session.settle()

        for width in stride(from: 1200, through: 1600, by: 50) {
            session.model.reportViewport(DisplayMode(w: UInt16(width), h: 1000))
        }

        try await session.expectViewport(w: 1600, h: 1000)
    }

    /// rxa answers `setResolution` off a fixed list instead, so an automatic report
    /// would be refused anyway — and it is never sent.
    @Test
    func anRxaTargetReportsNothingAutomatically() async throws {
        let session = try await Session.attached()
        session.connect(protocolName: "rxa")

        session.model.reportViewport(DisplayMode(w: 1600, h: 1000))
        try await session.settle()

        #expect(session.viewports.isEmpty)
    }

    /// A model attached to a scripted socket, waiting in the picker.
    @MainActor
    private struct Session {
        let model: AppModel
        let socket: FakeWebSocketTransport

        static func attached() async throws -> Session {
            let socket = FakeWebSocketTransport(closeAfterDraining: false)
            let model = AppModel(
                defaults: UserDefaults(
                    suiteName: "ViewportReportingTests.\(UUID().uuidString)"
                )!,
                clipboard: ClipboardSynchronizer(
                    pasteboard: NSPasteboard.withUniqueName(),
                    startsPolling: false
                )
            )
            await model.beginSession(
                over: FakeGateway(claims: [.claimed("tok")], sockets: [socket])
            )
            let session = Session(model: model, socket: socket)
            // `start` returns once the claim is under way, not once the socket is
            // up, and opening one discards whatever was queued before it. Waiting
            // here is what keeps these tests measuring the dedupes rather than that
            // race.
            for _ in 0..<200 where model.session.connectionStatus != .connected {
                try await Task.sleep(for: .milliseconds(5))
            }
            #expect(model.session.connectionStatus == .connected)
            return session
        }

        func connect(protocolName: String) {
            model.apply(
                .control(
                    .connected(
                        ServerMessage.Connected(
                            name: "t",
                            protocolName: protocolName,
                            resize: true,
                            clipboard: false
                        )
                    )
                )
            )
        }

        /// The viewports the socket has actually been sent, in order.
        var viewports: [DisplayMode] {
            socket.sentFrames.compactMap { frame in
                guard
                    let data = frame.data(using: .utf8),
                    let json = try? JSONSerialization.jsonObject(with: data)
                        as? [String: Any],
                    json["type"] as? String == "viewport",
                    let w = json["w"] as? Int,
                    let h = json["h"] as? Int
                else {
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

        /// Long enough for the debounce to have fired and the queue to have drained,
        /// for the assertions that something was *not* sent.
        func settle() async throws {
            try await Task.sleep(for: .milliseconds(400))
        }
    }
}
