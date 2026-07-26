import AppKit

/// The scroll view's document: black margin, with the framebuffer laid into it.
///
/// Three views rather than one (scroll → surface → framebuffer) so the
/// framebuffer can stay *exactly* the remote's size — one texel per device
/// pixel, no scaling — while this absorbs a window larger than the remote as
/// margin. Sizing the document to `max(remote, visible)` is also what retires
/// the web canvas's ±1px snap-to-viewport hack: the margin case is expressed in
/// geometry instead of a tolerance against phantom scrollbars.
///
/// Input lands here rather than on the framebuffer so the tracking area covers
/// the margin too, and coordinates are mapped through the framebuffer's frame.
final class RemoteSurfaceView: NSView {
    /// Flipped so y grows downward, as it does in the framebuffer and in the DOM
    /// coordinates the protocol inherited.
    override var isFlipped: Bool { true }
    override var acceptsFirstResponder: Bool { true }

    /// So the first click into an inactive window reaches the remote instead of
    /// being spent activating the app.
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    private let framebuffer: FramebufferView
    private weak var model: AppModel?
    private var trackingArea: NSTrackingArea?
    private var cursor = RemoteCursor.hidden

    init(model: AppModel, renderer: FramebufferRenderer) {
        self.model = model
        framebuffer = FramebufferView(renderer: renderer)
        super.init(frame: .zero)
        addSubview(framebuffer)
        postsFrameChangedNotifications = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("RemoteSurfaceView is not loaded from a nib")
    }

    /// The remote's size, or nil while there is nothing to show. Set by the host
    /// when the model's `remoteSize` changes.
    var remoteSize: DisplayMode? {
        didSet {
            guard remoteSize != oldValue else {
                return
            }
            needsLayout = true
        }
    }

    override func layout() {
        super.layout()
        guard let remoteSize else {
            framebuffer.frame = .zero
            framebuffer.isHidden = true
            return
        }
        framebuffer.isHidden = false
        let scale = window?.backingScaleFactor ?? 1
        let size = RemoteGeometry.pointSize(of: remoteSize, backingScale: scale)
        // Centred in whatever margin there is, so a remote smaller than the window
        // does not sit in a corner.
        framebuffer.frame = CGRect(
            x: max(0, (bounds.width - size.width) / 2).rounded(),
            y: max(0, (bounds.height - size.height) / 2).rounded(),
            width: size.width,
            height: size.height
        )
    }

    /// The window moved to a display with a different scale, so the point size
    /// the framebuffer occupies changed even though the remote did not.
    func backingScaleChanged() {
        needsLayout = true
        model?.reportViewport(measuredViewport())
    }

    /// The room available for the remote desktop, in device pixels.
    ///
    /// Measured from the scroll view's visible area rather than from this view,
    /// whose own size is at least the remote's — using this view's bounds would
    /// report the desktop's current size back as the space available for it and
    /// never shrink.
    func measuredViewport() -> DisplayMode {
        let visible = enclosingScrollView?.contentView.bounds.size ?? bounds.size
        return RemoteGeometry.viewport(
            clip: visible,
            backingScale: window?.backingScaleFactor ?? 1
        )
    }

    /// The remote pixel under a point in this view's coordinates, or nil when
    /// there is no framebuffer to be over.
    func remotePoint(for point: CGPoint) -> (x: Int32, y: Int32)? {
        guard let remoteSize, !framebuffer.frame.isEmpty else {
            return nil
        }
        return RemoteGeometry.remotePoint(
            CGPoint(
                x: point.x - framebuffer.frame.minX,
                y: point.y - framebuffer.frame.minY
            ),
            in: framebuffer.frame.size,
            remote: remoteSize
        )
    }

    // MARK: - Pointer

    /// `.mouseMoved` needs a tracking area at all, and inside a scroll view it
    /// has to be `.inVisibleRect` or the rect goes stale on every scroll.
    ///
    /// `.activeInKeyWindow` rather than `.activeAlways`: pointer motion should not
    /// reach the remote while another app is in front.
    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let trackingArea {
            removeTrackingArea(trackingArea)
        }
        let area = NSTrackingArea(
            rect: .zero,
            options: [.mouseMoved, .cursorUpdate, .activeInKeyWindow, .inVisibleRect],
            owner: self,
            userInfo: nil
        )
        addTrackingArea(area)
        trackingArea = area
    }

    /// The remote's pointer shape wins over the margin's ordinary arrow, but only
    /// over the framebuffer — a hidden pointer in the black margin would be a
    /// pointer the user cannot find.
    override func resetCursorRects() {
        guard !framebuffer.frame.isEmpty else {
            return
        }
        addCursorRect(framebuffer.frame, cursor: cursor)
    }

    func apply(cursor next: NSCursor) {
        cursor = next
        window?.invalidateCursorRects(for: self)
    }

    // `mouseMoved` is *not* delivered while a button is down, so all three drag
    // variants are needed or dragging a window on the remote stops the moment the
    // button goes down.
    override func mouseMoved(with event: NSEvent) {
        report(motion: event)
    }

    override func mouseDragged(with event: NSEvent) {
        report(motion: event)
    }

    override func rightMouseDragged(with event: NSEvent) {
        report(motion: event)
    }

    override func otherMouseDragged(with event: NSEvent) {
        report(motion: event)
    }

    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        report(button: event, pressed: true)
    }

    override func mouseUp(with event: NSEvent) {
        report(button: event, pressed: false)
    }

    override func rightMouseDown(with event: NSEvent) {
        // Deliberately not calling super, which would open a context menu instead
        // of letting the press through. `menu(for:)` refuses one as well.
        window?.makeFirstResponder(self)
        report(button: event, pressed: true)
    }

    override func rightMouseUp(with event: NSEvent) {
        report(button: event, pressed: false)
    }

    override func otherMouseDown(with event: NSEvent) {
        report(button: event, pressed: true)
    }

    override func otherMouseUp(with event: NSEvent) {
        report(button: event, pressed: false)
    }

    override func menu(for event: NSEvent) -> NSMenu? {
        nil
    }

    override func scrollWheel(with event: NSEvent) {
        guard let model, model.session.canCaptureKeyboard else {
            return
        }
        guard let delta = WheelMapping.delta(
            scrollingDeltaX: event.scrollingDeltaX,
            scrollingDeltaY: event.scrollingDeltaY,
            hasPreciseScrollingDeltas: event.hasPreciseScrollingDeltas
        ) else {
            return
        }
        model.sendWheel(dx: delta.dx, dy: delta.dy)
    }

    private func report(motion event: NSEvent) {
        guard let model,
              let point = remotePoint(for: convert(event.locationInWindow, from: nil))
        else {
            return
        }
        model.sendPointer(x: point.x, y: point.y)
    }

    private func report(button event: NSEvent, pressed: Bool) {
        guard let model,
              let button = WheelMapping.button(forEventNumber: event.buttonNumber)
        else {
            return
        }
        // The position first, so a click lands where the pointer is even if no
        // motion was reported since the last one (a tap without a move).
        if pressed, let point = remotePoint(for: convert(event.locationInWindow, from: nil)) {
            model.sendPointer(x: point.x, y: point.y)
        }
        model.sendMouseButton(button, pressed: pressed)
    }
}
