import MetalKit

/// The `MTKView` the framebuffer is blitted into. Deliberately dumb: the
/// renderer owns the texture and decides when to redraw.
///
/// Sized to the remote's own point size while its drawable stays the remote's
/// pixel size, so this is also where the remote is scaled to the host: the layer
/// resamples a framebuffer-sized drawable into whatever the window's display makes
/// of those points. One texel per device pixel only when the two densities agree.
final class FramebufferView: MTKView {
    /// Called with this window's backing scale whenever it changes, so the remote
    /// can be asked to match it. Presentation does not depend on it — see
    /// `viewDidChangeBackingProperties`.
    var onBackingScaleChange: ((CGFloat) -> Void)?

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
    /// `backingScaleFactor` without any resize notification, and the layer has to
    /// be told or it keeps rasterizing for the display it left.
    ///
    /// Nothing else *presented* follows from it: neither the drawable (which tracks
    /// the remote) nor this view's point size (the remote's own — see
    /// `RemoteGeometry`) depends on the host's density, so the desktop keeps its
    /// physical size across the move and the `contentsScale` line is the whole of
    /// switching density with it — the layer now rasterizes at the new screen's
    /// scale, magnifying a 1x remote onto a Retina display and halving a Retina
    /// remote onto a 1x one.
    ///
    /// The new scale is also handed upwards, which is a different question: not how
    /// to draw what arrives, but what to ask the remote to send. A display the agent
    /// made can change its own density to match, turning a resampled picture into
    /// one pixel per pixel.
    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        let scale = window?.backingScaleFactor ?? 1
        layer?.contentsScale = scale
        onBackingScaleChange?(scale)
    }
}
