import Foundation
import Testing

@testable import RemotexViewer

/// The Display menu's readout, which exists to make a failed density visible.
///
/// It is a diagnostic, so the property worth pinning is not the wording but that
/// the two densities are reported *separately*. A readout that folded them into
/// one number, or showed only the resolution, would say nothing about the failure
/// it was added for: a density the remote was asked for and quietly dropped, which
/// no message reports and which otherwise reads as a desktop that merely looks
/// soft or comes back half the size it was asked for.
struct DisplaySummaryTests {
    @Test
    func aDensityReadsAsAWholeNumberWhereItIsOne() {
        #expect(densityLabel(1) == "1x")
        #expect(densityLabel(2) == "2x")
        // Not every screen is integral, and rounding one to 2x would be a lie in
        // exactly the place someone is looking for one.
        #expect(densityLabel(1.5) == "1.5x")
        #expect(densityLabel(1.25) == "1.25x")
    }

    @Test
    func theSummaryReportsTheRemoteAndThisScreenApart() {
        let summary = displaySummary(
            remote: DisplayMode(w: 2560, h: 1600),
            remoteScale: 2,
            hostScale: 200
        )
        #expect(summary.contains("2560×1600"))
        #expect(summary.contains("remote 2x"))
        #expect(summary.contains("this screen 2x"))
    }

    /// The case the readout is for: the remote never took the density this screen
    /// asked for, so the two numbers disagree and the summary has to show both
    /// rather than reconcile them.
    @Test
    func aDensityTheRemoteNeverAppliedShowsAsADisagreement() {
        let summary = displaySummary(
            remote: DisplayMode(w: 1280, h: 800),
            remoteScale: 1,
            hostScale: 200
        )
        #expect(summary.contains("remote 1x"))
        #expect(summary.contains("this screen 2x"))
    }

    /// Before the first `resize` there is no resolution to report, and a
    /// placeholder reading 0×0 would be a worse answer than saying so.
    @Test
    func noRemoteSizeYetSaysSoRatherThanShowingZeroes() {
        let summary = displaySummary(remote: nil, remoteScale: 1, hostScale: 200)
        #expect(!summary.contains("0×0"))
        #expect(summary.contains("Waiting"))
    }
}
