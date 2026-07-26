import MetalKit

/// The `MTKView` the framebuffer is blitted into. Deliberately dumb: the
/// renderer owns the texture and decides when to redraw.
///
/// Sized to the remote in points while its drawable is sized to the remote in
/// pixels, so the blit is one texel per device pixel with no scaling.
final class FramebufferView: MTKView {
    init(renderer: FramebufferRenderer) {
        super.init(frame: .zero, device: renderer.device)
        // On demand, not on a clock: a burst of strips coalesces into one draw at
        // the next refresh, and an idle desktop costs nothing.
        isPaused = true
        enableSetNeedsDisplay = true
        // The drawable follows the *remote* size, which the renderer sets.
        autoResizeDrawable = false
        framebufferOnly = true
        colorPixelFormat = .bgra8Unorm
        clearColor = MTLClearColorMake(0, 0, 0, 1)
        // Keeps the framebuffer in step with the scroll view around it instead of
        // trailing it. Note the renderer's `draw` presents by hand because of it.
        presentsWithTransaction = true
        layer?.isOpaque = true
        delegate = renderer
        renderer.attach(view: self)
    }

    @available(*, unavailable)
    required init(coder: NSCoder) {
        fatalError("FramebufferView is not loaded from a nib")
    }

    /// Moving the window between displays of different scale changes
    /// `backingScaleFactor` without any resize notification. The drawable is
    /// unaffected — it tracks the remote, not the screen — but the point size the
    /// surface lays this out at is not, so the surface is asked to re-measure.
    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        layer?.contentsScale = window?.backingScaleFactor ?? 1
        (superview as? RemoteSurfaceView)?.backingScaleChanged()
    }
}
