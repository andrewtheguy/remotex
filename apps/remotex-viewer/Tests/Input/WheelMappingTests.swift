import AppKit
import Testing
@testable import RemotexViewer

struct WheelMappingTests {
    /// AppKit is positive-up, DOM `deltaY` is positive-down. Getting this wrong
    /// inverts scrolling on every target at once, and it cannot be seen in any
    /// output — only felt.
    @Test
    func bothAxesAreInverted() throws {
        let delta = try #require(
            WheelMapping.delta(
                scrollingDeltaX: 3,
                scrollingDeltaY: 5,
                hasPreciseScrollingDeltas: true
            )
        )
        #expect(delta.dx == -3)
        #expect(delta.dy == -5)

        let other = try #require(
            WheelMapping.delta(
                scrollingDeltaX: -2,
                scrollingDeltaY: -7,
                hasPreciseScrollingDeltas: true
            )
        )
        #expect(other.dx == 2)
        #expect(other.dy == 7)
    }

    /// A trackpad's deltas are point-like, and travel as pixels. Unscaled, so the
    /// agent can spend them as the pixels they are — scaling them into some
    /// common unit is what made a glide arrive as a series of jumps.
    @Test
    func preciseDeltasTravelAsPixels() throws {
        let delta = try #require(
            WheelMapping.delta(
                scrollingDeltaX: 0,
                scrollingDeltaY: 1.5,
                hasPreciseScrollingDeltas: true
            )
        )
        #expect(delta.dy == -1.5)
        #expect(delta.unit == .pixel)
    }

    /// A notched wheel reports *lines*, and says so rather than being converted.
    /// The remote cannot tell the two apart afterwards: AppKit's one line and a
    /// trackpad's one point are the same number and nothing like the same scroll.
    @Test
    func lineDeltasTravelAsLines() throws {
        let delta = try #require(
            WheelMapping.delta(
                scrollingDeltaX: 0,
                scrollingDeltaY: 1,
                hasPreciseScrollingDeltas: false
            )
        )
        #expect(delta.dy == -1)
        #expect(delta.unit == .line)

        let triple = try #require(
            WheelMapping.delta(
                scrollingDeltaX: 0,
                scrollingDeltaY: -3,
                hasPreciseScrollingDeltas: false
            )
        )
        #expect(triple.dy == 3)
        #expect(triple.unit == .line)
    }

    @Test
    func aStillWheelSendsNothing() {
        #expect(
            WheelMapping.delta(
                scrollingDeltaX: 0,
                scrollingDeltaY: 0,
                hasPreciseScrollingDeltas: true
            ) == nil
        )
        #expect(
            WheelMapping.delta(
                scrollingDeltaX: 0,
                scrollingDeltaY: 0,
                hasPreciseScrollingDeltas: false
            ) == nil
        )
    }

    /// `JSONEncoder` refuses a non-finite Float, and that throw would escape into
    /// the send loop and take every message queued behind it — so one bad scroll
    /// event would end the session, not just itself.
    @Test
    func nonFiniteDeltasAreRefused() {
        for value in [CGFloat.nan, .infinity, -.infinity] {
            #expect(
                WheelMapping.delta(
                    scrollingDeltaX: value,
                    scrollingDeltaY: 1,
                    hasPreciseScrollingDeltas: true
                ) == nil,
                "dx \(value)"
            )
            #expect(
                WheelMapping.delta(
                    scrollingDeltaX: 1,
                    scrollingDeltaY: value,
                    hasPreciseScrollingDeltas: true
                ) == nil,
                "dy \(value)"
            )
        }
    }

    /// Mirrors `mouseButtonFromEvent` in the web client. AppKit numbers right
    /// before centre where the DOM does the opposite, and both agree that 3 and 4
    /// are the side buttons.
    @Test
    func everyNamedButtonMapsAndNothingElseDoes() {
        #expect(WheelMapping.button(forEventNumber: 0) == .left)
        #expect(WheelMapping.button(forEventNumber: 1) == .right)
        #expect(WheelMapping.button(forEventNumber: 2) == .middle)
        #expect(WheelMapping.button(forEventNumber: 3) == .back)
        #expect(WheelMapping.button(forEventNumber: 4) == .forward)
        for number in [5, 31, -1] {
            #expect(WheelMapping.button(forEventNumber: number) == nil, "button \(number)")
        }
    }
}
