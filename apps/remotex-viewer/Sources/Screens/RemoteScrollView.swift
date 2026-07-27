import AppKit

/// The scroll view the remote desktop lives in, with scrollbars of our own.
///
/// The reference is Microsoft Remote Desktop, which is what this was measured
/// against: a desktop that fits shows no bar at all; one that does not shows a bar
/// on that axis *beside* the picture rather than over it; the bar stays put while
/// the other one is dragged; and the square where two bars would meet is surround,
/// not a raised box.
///
/// `NSScroller` gives none of that reliably. An overlay scroller — the style a
/// trackpad Mac prefers — is drawn only while a scroll is reaching the scroll view,
/// and none ever does here: `RemoteSurfaceView.scrollWheel` sends every wheel event
/// to the remote, which is the point of it. Forcing the legacy style instead brings
/// back a white corner piece with no API to remove, and AppKit's own autohide logic
/// decides visibility from measurements it re-derives mid-scroll. The bars below are
/// three rules of our own — needed, laid out, drawn — and nothing else.
final class RemoteScrollView: NSScrollView {
    /// Called after every layout, which is the only reliable signal that the room
    /// for the document changed.
    ///
    /// A frame notification is not enough: AppKit applies the window's title bar as
    /// a top `contentInset` partway through the first layout, and with no
    /// `NSScroller` in the view tree that inset moves nothing that posts one — the
    /// first viewport report would keep carrying the title bar, which is a Linux
    /// guest's taskbar below the fold for the rest of the session. `tile` runs for
    /// it, and for everything else that changes the room.
    var onTile: (() -> Void)?

    private let horizontal = RemoteScrollbar(orientation: .horizontal)
    private let vertical = RemoteScrollbar(orientation: .vertical)

    override init(frame: NSRect) {
        super.init(frame: frame)
        // Ours are the only bars: AppKit's are switched off rather than restyled.
        hasHorizontalScroller = false
        hasVerticalScroller = false
        autohidesScrollers = false
        contentView = CenteringClipView()
        for bar in [horizontal, vertical] {
            bar.isHidden = true
            bar.onScroll = { [weak self] fraction in
                self?.scroll(bar.orientation, to: fraction)
            }
            addSubview(bar)
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("RemoteScrollView is not loaded from a nib")
    }

    /// Whether each bar is up, for tests and for anything that needs to know how
    /// much room the document really has.
    var visibleBars: (horizontal: Bool, vertical: Bool) {
        (!horizontal.isHidden, !vertical.isHidden)
    }

    /// The thickness a bar takes out of the room it sits beside. Deliberately not
    /// `NSScroller.scrollerWidth`, which follows a style we no longer use.
    static let barThickness: CGFloat = 12

    override func tile() {
        super.tile()
        layOutBars()
        onTile?()
    }

    override func reflectScrolledClipView(_ clipView: NSClipView) {
        super.reflectScrolledClipView(clipView)
        refreshBars()
    }

    /// Decide which bars are needed, give them the room, and leave the rest to the
    /// clip view.
    ///
    /// The two decisions are not independent — a horizontal bar takes height, which
    /// can be what makes a vertical one necessary — so the pair is settled first and
    /// the clip is inset by both at once. Deciding one at a time is what made
    /// AppKit's version flip: each bar changed the measurement the other was judged
    /// against.
    private func layOutBars() {
        let document = documentView?.frame.size ?? .zero
        let full = CGSize(
            width: max(0, frame.width - contentInsets.left - contentInsets.right),
            height: max(0, frame.height - contentInsets.top - contentInsets.bottom)
        )
        let thickness = Self.barThickness
        var needsHorizontal = document.width > full.width + 0.5
        var needsVertical = document.height > full.height + 0.5
        if needsHorizontal, document.height > full.height - thickness + 0.5 {
            needsVertical = true
        }
        if needsVertical, document.width > full.width - thickness + 0.5 {
            needsHorizontal = true
        }

        let room = CGSize(
            width: full.width - (needsVertical ? thickness : 0),
            height: full.height - (needsHorizontal ? thickness : 0)
        )
        let origin = CGPoint(x: contentInsets.left, y: contentInsets.top)
        contentView.frame = CGRect(origin: origin, size: room)

        horizontal.isHidden = !needsHorizontal
        vertical.isHidden = !needsVertical
        // Each bar spans only its own axis, so the corner between them stays the
        // scroll view's background — the box the reference client never shows.
        horizontal.frame = CGRect(
            x: origin.x,
            y: origin.y + room.height,
            width: room.width,
            height: thickness
        )
        vertical.frame = CGRect(
            x: origin.x + room.width,
            y: origin.y,
            width: thickness,
            height: room.height
        )
        refreshBars()
    }

    /// Push the clip's position into the bars: where the knob sits, and how much of
    /// the document is on screen.
    private func refreshBars() {
        let document = documentView?.frame.size ?? .zero
        let visible = contentView.bounds.size
        horizontal.update(
            visible: visible.width,
            document: document.width,
            offset: contentView.bounds.origin.x
        )
        vertical.update(
            visible: visible.height,
            document: document.height,
            offset: contentView.bounds.origin.y
        )
    }

    /// A bar was dragged to `fraction` of its travel.
    private func scroll(_ orientation: NSUserInterfaceLayoutOrientation, to fraction: CGFloat) {
        let document = documentView?.frame.size ?? .zero
        let visible = contentView.bounds.size
        var origin = contentView.bounds.origin
        switch orientation {
        case .horizontal:
            origin.x = (document.width - visible.width) * fraction
        default:
            origin.y = (document.height - visible.height) * fraction
        }
        contentView.scroll(to: origin)
        reflectScrolledClipView(contentView)
    }
}

/// One scrollbar: a track with a knob, dragged with the pointer.
///
/// Deliberately plain. It exists to show that there is more desktop than window and
/// to let the rest be reached — the same job the reference client's does — so it has
/// no arrows, no autohide, and no behaviour that depends on where the *other* bar is.
final class RemoteScrollbar: NSView {
    let orientation: NSUserInterfaceLayoutOrientation
    /// Called with the fraction of the travel the knob was dragged to, 0...1.
    var onScroll: ((CGFloat) -> Void)?

    /// How much of the document is on screen, 0...1. A full one hides nothing, but
    /// the bar is not shown in that case anyway.
    private var proportion: CGFloat = 1
    /// Where the visible part starts, as a fraction of the scrollable travel.
    private var value: CGFloat = 0
    /// Where in the knob the pointer took hold, so a drag does not jump.
    private var grab: CGFloat?

    /// The knob never gets thinner than this, however long the document is: a
    /// one-pixel knob is one nobody can grab.
    private static let minimumKnob: CGFloat = 24
    private static let inset: CGFloat = 3

    init(orientation: NSUserInterfaceLayoutOrientation) {
        self.orientation = orientation
        super.init(frame: .zero)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("RemoteScrollbar is not loaded from a nib")
    }

    override var isFlipped: Bool { true }

    func update(visible: CGFloat, document: CGFloat, offset: CGFloat) {
        guard document > 0 else {
            proportion = 1
            value = 0
            return
        }
        proportion = min(1, visible / document)
        let travel = max(0, document - visible)
        value = travel > 0 ? min(1, max(0, offset / travel)) : 0
        needsDisplay = true
    }

    override func draw(_ dirtyRect: NSRect) {
        // A track dark enough to read as part of the surround, and a knob light
        // enough to find against it.
        NSColor(white: 0.16, alpha: 1).setFill()
        bounds.fill()
        let knob = knobRect()
        guard !knob.isEmpty else {
            return
        }
        NSColor(white: grab == nil ? 0.55 : 0.75, alpha: 1).setFill()
        let radius = min(knob.width, knob.height) / 2
        NSBezierPath(roundedRect: knob, xRadius: radius, yRadius: radius).fill()
    }

    private var length: CGFloat {
        orientation == .horizontal ? bounds.width : bounds.height
    }

    private func knobRect() -> NSRect {
        let inset = Self.inset
        let track = length - inset * 2
        guard track > 0 else {
            return .zero
        }
        let knob = max(Self.minimumKnob, track * proportion)
        guard knob < track else {
            return .zero // the whole document is on screen
        }
        let start = inset + (track - knob) * value
        return orientation == .horizontal
            ? NSRect(x: start, y: inset, width: knob, height: bounds.height - inset * 2)
            : NSRect(x: inset, y: start, width: bounds.width - inset * 2, height: knob)
    }

    override func mouseDown(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        let knob = knobRect()
        guard !knob.isEmpty else {
            return
        }
        let along = orientation == .horizontal ? point.x : point.y
        let knobStart = orientation == .horizontal ? knob.minX : knob.minY
        let knobLength = orientation == .horizontal ? knob.width : knob.height
        // A click off the knob jumps to it first, then drags from its middle, which
        // is what makes clicking the track behave like grabbing the bar there.
        grab = (along >= knobStart && along <= knobStart + knobLength)
            ? along - knobStart
            : knobLength / 2
        report(at: along)
    }

    override func mouseDragged(with event: NSEvent) {
        guard grab != nil else {
            return
        }
        let point = convert(event.locationInWindow, from: nil)
        report(at: orientation == .horizontal ? point.x : point.y)
    }

    override func mouseUp(with event: NSEvent) {
        grab = nil
        needsDisplay = true
    }

    // MARK: - Accessibility
    //
    // AppKit's scrollers were in the accessibility tree for free; a plain view is
    // not, so without this the desktop has no scroll bar as far as VoiceOver or an
    // AX script is concerned. Role, orientation and position are what a scroll bar
    // is asked for, and the position is settable so it can be driven the way an
    // `NSScroller` can. None of it touches layout.

    /// Hidden means *not needed* here, and a bar that is not needed must not be in
    /// the tree: claiming to be an element unconditionally put two scroll bars on a
    /// desktop that fit its window perfectly, both reading zero and neither able to
    /// scroll anything.
    override func isAccessibilityElement() -> Bool { !isHidden }

    override func accessibilityRole() -> NSAccessibility.Role? { .scrollBar }

    override func accessibilityOrientation() -> NSAccessibilityOrientation {
        orientation == .horizontal ? .horizontal : .vertical
    }

    /// Where the visible part starts in the travel, 0...1 — the same thing an
    /// `NSScroller` reports, so anything that knows how to read one reads this.
    override func accessibilityValue() -> Any? { value }

    override func setAccessibilityValue(_ accessibilityValue: Any?) {
        guard let fraction = (accessibilityValue as? NSNumber)?.doubleValue else {
            return
        }
        value = CGFloat(min(1, max(0, fraction)))
        needsDisplay = true
        onScroll?(value)
    }

    private func report(at along: CGFloat) {
        guard let grab else {
            return
        }
        let inset = Self.inset
        let track = length - inset * 2
        let knob = max(Self.minimumKnob, track * proportion)
        let travel = track - knob
        guard travel > 0 else {
            return
        }
        value = min(1, max(0, (along - grab - inset) / travel))
        needsDisplay = true
        onScroll?(value)
    }
}

/// The clip view that keeps a desktop smaller than the window in the middle of it.
///
/// The document is exactly the remote's size — deliberately, so that a desktop which
/// overflows one axis needs a bar on that axis only. A document padded out to the
/// window instead (which this did before) is a document that overflows the *other*
/// axis the moment the first bar takes its thickness.
///
/// Padding was doing one useful thing, though — centring — so that is done here,
/// which is where AppKit expects it: `constrainBoundsRect` is asked where the
/// document may sit, and a document with room to spare is offered the middle.
final class CenteringClipView: NSClipView {
    override func constrainBoundsRect(_ proposedBounds: NSRect) -> NSRect {
        var rect = super.constrainBoundsRect(proposedBounds)
        guard let documentView else {
            return rect
        }
        if rect.width > documentView.frame.width {
            rect.origin.x = (documentView.frame.width - rect.width) / 2
        }
        if rect.height > documentView.frame.height {
            rect.origin.y = (documentView.frame.height - rect.height) / 2
        }
        return rect
    }
}
