import Foundation
import Testing
@testable import RemotexViewer

/// The density the page reports, which is the one number in `state` that is
/// arithmetic rather than a flag — and so the one that can be nonsense.
struct NativeStateScaleTests {
    /// The values a real remote reports, untouched.
    @Test
    func ordinaryDensitiesSurvive() {
        #expect(scale(1) == 1)
        #expect(scale(2) == 2)
        #expect(scale(1.5) == 1.5)
        #expect(densityLabel(scale(2)) == "2x")
        #expect(densityLabel(scale(1.5)) == "1.5x")
    }

    /// A density that is not one falls back rather than propagating: everything
    /// downstream divides by this, and zero or a negative is not a document size.
    @Test
    func anonsensicalDensityFallsBackToOne() {
        #expect(scale(0) == 1)
        #expect(scale(-2) == 1)
        #expect(scale(.nan) == 1)
        #expect(scale(.infinity) == 1)
    }

    /// The one that mattered: `densityLabel` converts a whole density with
    /// `Int(_:)`, which **traps** rather than overflowing. `1e300` is ordinary JSON
    /// and a page is what sends it, so before the clamp a malformed report took the
    /// app down instead of drawing a silly menu.
    @Test
    func ahugeDensityIsClampedRatherThanTrapping() {
        #expect(scale(1e300) == CGFloat(NativeState.maximumScale))
        #expect(scale(Double(Int.max)) == CGFloat(NativeState.maximumScale))
        // Reached through the label too, which is where the trap was.
        #expect(densityLabel(scale(1e300)) == "64x")
        // And directly, because this function is free and a caller could hand it
        // anything: it answers rather than crashing.
        #expect(densityLabel(.infinity) == "?x")
        #expect(densityLabel(.nan) == "?x")
        #expect(densityLabel(1e300) == "?x")
    }

    /// The whole line, since that is what the menu shows.
    @Test
    func thesummaryLineSurvivesANonsenseDensity() {
        #expect(
            displaySummary(remote: DisplayMode(w: 1920, h: 1080), remoteScale: scale(1e300), hostScale: 2)
                == "1920×1080 — remote 64x, this screen 2x"
        )
    }

    /// Read the way the app reads it: through a decoded report.
    private func scale(_ reported: Double) -> CGFloat {
        var state = NativeState()
        state.size = NativeState.RemoteSize(w: 1920, h: 1080, scale: reported)
        return state.remoteScale
    }
}
