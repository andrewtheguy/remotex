import AppKit
import Foundation
import Testing

@testable import RemotexViewer

/// What density actually reaches the socket, driven through a whole session.
///
/// The agent reconfigures a display of its own from these, so a wrong number is
/// not cosmetic: it is a 2x desktop on a 1x screen, or four times the framebuffer
/// for a picture the client immediately halves.
@MainActor
struct HostScaleReportingTests {
    /// The bug the reader exists for: what is sent on `connected` has to be this
    /// screen, read then, not whatever a notification happened to push earlier.
    ///
    /// `viewDidChangeBackingProperties` is a hint that something moved, not a
    /// record of where it stopped, and it may not have fired at all by the time a
    /// session starts. While the density was a value cached from that
    /// notification, a session that never changed screen sent the default instead
    /// of the truth — a 1x desktop asked for from a Retina window, and the
    /// mirror-image fault reported from the other side: a 2x desktop on a 1x
    /// screen, correctable only by moving the window to another display and back,
    /// which is just two more notifications.
    ///
    /// Fails against that cache: with nothing reported before `connect`, the
    /// cached value is still its default and 100 goes out in place of 200.
    @Test
    func connectingReportsThisScreenWithoutWaitingForAMove() async throws {
        let session = try await Self.attached()
        session.model.hostScaleReader = { 2 }

        session.connect(protocolName: "rxa")
        try await session.expectHostScale(200)
    }

    /// Moving between screens re-reports, and the value comes from the reader
    /// rather than from the notification.
    @Test
    func movingBetweenScreensReportsTheNewOne() async throws {
        let session = try await Self.attached()
        var scale: CGFloat = 2
        session.model.hostScaleReader = { scale }

        session.connect(protocolName: "rxa")
        try await session.expectHostScale(200)

        scale = 1
        session.model.reportHostScale()
        try await session.expectHostScale(100, count: 2)
    }

    /// Unchanged density sends nothing: acting on it is a WindowServer reconfigure
    /// at the other end, which relays every window on that desktop.
    @Test
    func anUnchangedDensityIsNotResent() async throws {
        let session = try await Self.attached()
        session.model.hostScaleReader = { 2 }

        session.connect(protocolName: "rxa")
        try await session.expectHostScale(200)

        session.model.reportHostScale()
        try await session.settle()
        #expect(session.hostScales == [200], "a repeat of the same density")
    }

    /// Switching the shared display re-reports even though the number has not
    /// changed. The agent applies this to whichever display it is sharing *now*,
    /// so a switch onto one it made would otherwise leave it at whatever density
    /// macOS had remembered against it.
    @Test
    func switchingTheSharedDisplayReportsAgain() async throws {
        let session = try await Self.attached()
        session.model.hostScaleReader = { 2 }

        session.connect(protocolName: "rxa")
        try await session.expectHostScale(200)

        session.model.apply(.control(.displays(active: 7, displays: [])))
        try await session.expectHostScale(200, count: 2)

        // The same display again is not a switch.
        session.model.apply(.control(.displays(active: 7, displays: [])))
        try await session.settle()
        #expect(session.hostScales == [200, 200])
    }

    /// Measuring in the picker has nothing to report to: there is no engine, and
    /// teaching the dedupe a value there would swallow the report from
    /// `connected`, which is the one that matters.
    @Test
    func reportingInThePickerSendsNothing() async throws {
        let session = try await Self.attached()
        session.model.hostScaleReader = { 2 }

        session.model.reportHostScale()
        try await session.settle()
        #expect(session.hostScales.isEmpty)
    }

    private static func attached() async throws -> AttachedSession {
        try await AttachedSession.attached(suite: "HostScaleReportingTests")
    }
}

/// The density half of what an attached session can be asked about, alongside the
/// viewport one.
@MainActor
extension AttachedSession {
    /// The densities the socket has actually been sent, in order.
    var hostScales: [Int] {
        sent(ofType: "hostScale").compactMap { $0["scale"] as? Int }
    }

    func expectHostScale(_ scale: Int, count: Int = 1) async throws {
        for _ in 0..<200 {
            if hostScales.count >= count {
                break
            }
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(hostScales.count == count)
        #expect(hostScales.last == scale)
    }
}
