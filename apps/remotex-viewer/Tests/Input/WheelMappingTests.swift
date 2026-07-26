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

    /// Trackpad deltas are point-like, which is what a browser reports for the
    /// same gesture, so they go through untouched and the agent's rounding keeps
    /// a flick smooth.
    @Test
    func preciseDeltasPassThroughUnscaled() throws {
        let delta = try #require(
            WheelMapping.delta(
                scrollingDeltaX: 0,
                scrollingDeltaY: 1.5,
                hasPreciseScrollingDeltas: true
            )
        )
        #expect(delta.dy == -1.5)
    }

    /// A notched wheel reports *lines*, not points. One line has to arrive as one
    /// hundred, because that is what the agent divides by to get one line back —
    /// send AppKit's 1 and a wheel click barely moves anything.
    @Test
    func lineDeltasScaleToTheProtocolsUnits() throws {
        let delta = try #require(
            WheelMapping.delta(
                scrollingDeltaX: 0,
                scrollingDeltaY: 1,
                hasPreciseScrollingDeltas: false
            )
        )
        #expect(delta.dy == -100)

        let triple = try #require(
            WheelMapping.delta(
                scrollingDeltaX: 0,
                scrollingDeltaY: -3,
                hasPreciseScrollingDeltas: false
            )
        )
        #expect(triple.dy == 300)
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

    /// Mirrors `mouseButtonFromEvent` in the web client, which names nothing past
    /// the third button — a five-button mouse's extra buttons are not forwarded
    /// rather than being forwarded as something wrong.
    @Test
    func onlyTheThreeNamedButtonsMap() {
        #expect(WheelMapping.button(forEventNumber: 0) == .left)
        #expect(WheelMapping.button(forEventNumber: 1) == .right)
        #expect(WheelMapping.button(forEventNumber: 2) == .middle)
        for number in [3, 4, 31, -1] {
            #expect(WheelMapping.button(forEventNumber: number) == nil, "button \(number)")
        }
    }
}
