import AppKit

/// `NSEvent` scroll deltas, in the units the gateway's `wheel` message expects.
///
/// Pure and separate because neither of the two conversions here can be checked
/// by looking at the screen — an inverted axis and a wrong unit both just feel
/// wrong — and getting the sign wrong inverts scrolling on every target at once.
enum WheelMapping {
    /// What one notched wheel click is worth.
    ///
    /// The protocol carries DOM `deltaY`, where a wheel click is ~100. The Mac
    /// agent divides by exactly that (`WHEEL_DIVISOR` in
    /// `crates/rxa-agent/src/input.rs`) to get scroll lines, and RDP and VNC read
    /// only the sign, so one click has to arrive as one hundred rather than as
    /// AppKit's one line.
    static let unitsPerLine: CGFloat = 100

    /// Nil when there is nothing to send.
    static func delta(
        scrollingDeltaX: CGFloat,
        scrollingDeltaY: CGFloat,
        hasPreciseScrollingDeltas: Bool
    ) -> (dx: Float, dy: Float)? {
        // Precise deltas (trackpad, Magic Mouse) are point-like, which is what a
        // browser reports for the same gesture, so they pass through. Non-precise
        // deltas are in lines.
        let scale = hasPreciseScrollingDeltas ? 1 : unitsPerLine
        // AppKit is positive-up; DOM `deltaY` is positive-down. The RDP engine and
        // the Mac agent both document this inversion from the far side.
        //
        // `isDirectionInvertedFromDevice` is deliberately not consulted:
        // `scrollingDelta*` already reflects the user's natural-scroll setting,
        // and that is what they expect the remote to do.
        let dx = -scrollingDeltaX * scale
        let dy = -scrollingDeltaY * scale
        guard dx.isFinite, dy.isFinite, dx != 0 || dy != 0 else {
            // Non-finite matters as much as zero: `JSONEncoder` refuses a
            // non-finite Float, and that throw would escape into the send loop and
            // take every message queued behind it.
            return nil
        }
        return (dx: Float(dx), dy: Float(dy))
    }

    /// The protocol button for a mouse event, or nil for one it has no name for.
    /// Mirrors `mouseButtonFromEvent` in `frontend/src/protocol.ts`, which returns
    /// null for anything past the third button.
    static func button(forEventNumber number: Int) -> MouseButton? {
        switch number {
        case 0: .left
        case 1: .right
        case 2: .middle
        default: nil
        }
    }
}
