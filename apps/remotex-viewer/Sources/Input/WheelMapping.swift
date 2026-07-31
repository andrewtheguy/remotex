import AppKit

/// `NSEvent` scroll deltas, in the units the gateway's `wheel` message expects.
///
/// Pure and separate because neither of the two conversions here can be checked
/// by looking at the screen — an inverted axis and a wrong unit both just feel
/// wrong — and getting the sign wrong inverts scrolling on every target at once.
enum WheelMapping {
    /// Nil when there is nothing to send.
    ///
    /// The deltas travel in their own units rather than being converted to some
    /// common one, because the remote cannot recover the difference afterwards:
    /// a trackpad's three points and a wheel's three lines are the same number
    /// and an order of magnitude apart in effect.
    static func delta(
        scrollingDeltaX: CGFloat,
        scrollingDeltaY: CGFloat,
        hasPreciseScrollingDeltas: Bool
    ) -> (dx: Float, dy: Float, unit: WheelUnit)? {
        // Precise deltas (trackpad, Magic Mouse) are point-like, which is what a
        // browser reports as `DOM_DELTA_PIXEL` for the same gesture. Non-precise
        // deltas are the notches of a wheel, which is `DOM_DELTA_LINE`.
        let unit: WheelUnit = hasPreciseScrollingDeltas ? .pixel : .line
        // AppKit is positive-up; DOM `deltaY` is positive-down. The RDP engine and
        // the Mac agent both document this inversion from the far side.
        //
        // `isDirectionInvertedFromDevice` is deliberately not consulted:
        // `scrollingDelta*` already reflects the user's natural-scroll setting,
        // and that is what they expect the remote to do.
        let dx = -scrollingDeltaX
        let dy = -scrollingDeltaY
        guard dx.isFinite, dy.isFinite, dx != 0 || dy != 0 else {
            // Non-finite matters as much as zero: `JSONEncoder` refuses a
            // non-finite Float, and that throw would escape into the send loop and
            // take every message queued behind it.
            return nil
        }
        return (dx: Float(dx), dy: Float(dy), unit: unit)
    }

    /// The protocol button for a mouse event, or nil for one it has no name for.
    /// Mirrors `mouseButtonFromEvent` in `frontend/src/protocol.ts`.
    ///
    /// AppKit and the DOM disagree in the middle — AppKit numbers right before
    /// centre — and agree again at the two side buttons, which macOS calls 3 and
    /// 4 as well.
    static func button(forEventNumber number: Int) -> MouseButton? {
        switch number {
        case 0: .left
        case 1: .right
        case 2: .middle
        case 3: .back
        case 4: .forward
        default: nil
        }
    }
}
