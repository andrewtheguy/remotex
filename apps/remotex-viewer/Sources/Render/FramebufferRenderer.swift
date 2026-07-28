import Metal
import MetalKit
import OSLog

/// Holds the remote framebuffer as one Metal texture and blits it to the view.
///
/// The texture is exactly the remote's size and so is the drawable, so nothing is
/// scaled in here — a remote is presented at its own point size and the layer
/// resamples the drawable for the display it is on (see `FramebufferView`). Tiles
/// are written into the texture with `replaceRegion`; there is no delta state to
/// keep, since every tile overwrites its rectangle outright.
@MainActor
final class FramebufferRenderer: NSObject, MTKViewDelegate {
    let device: any MTLDevice

    private let queue: any MTLCommandQueue
    private let pipeline: any MTLRenderPipelineState
    private let sampler: any MTLSamplerState
    private let log = Logger(subsystem: "dev.remotex.viewer", category: "render")

    private var texture: (any MTLTexture)?
    /// False until the first tile lands, so a freshly allocated texture shows
    /// black rather than whatever the allocation happened to contain. Cheaper
    /// than zero-filling `w * h * 4` bytes, and it covers "no texture yet" and
    /// "back at the picker" with the same branch.
    private var hasContent = false
    private weak var view: MTKView?

    /// Nil when there is no Metal device, or the shader will not compile.
    ///
    /// A factory rather than a failable initializer because `init?()` cannot
    /// override `NSObject.init()`. Nil is a real outcome to handle rather than
    /// trap on: `MTLCreateSystemDefaultDevice` can return nil, and a headless
    /// process is one of the contexts where it does.
    static func make() -> FramebufferRenderer? {
        guard let device = MTLCreateSystemDefaultDevice(),
              let queue = device.makeCommandQueue()
        else {
            return nil
        }
        let descriptor = MTLRenderPipelineDescriptor()
        do {
            let library = try device.makeLibrary(source: RemoteShaders.source, options: nil)
            descriptor.vertexFunction = library.makeFunction(name: RemoteShaders.vertexFunction)
            descriptor.fragmentFunction = library.makeFunction(name: RemoteShaders.fragmentFunction)
        } catch {
            return nil
        }
        descriptor.colorAttachments[0].pixelFormat = .bgra8Unorm
        guard let pipeline = try? device.makeRenderPipelineState(descriptor: descriptor) else {
            return nil
        }
        // Nearest, not linear: the remote's pixels are the content, and a blurred
        // desktop reads as a broken one. The web canvas used
        // `image-rendering: pixelated` for the same reason.
        let samplerDescriptor = MTLSamplerDescriptor()
        samplerDescriptor.minFilter = .nearest
        samplerDescriptor.magFilter = .nearest
        samplerDescriptor.sAddressMode = .clampToEdge
        samplerDescriptor.tAddressMode = .clampToEdge
        guard let sampler = device.makeSamplerState(descriptor: samplerDescriptor) else {
            return nil
        }
        return FramebufferRenderer(
            device: device,
            queue: queue,
            pipeline: pipeline,
            sampler: sampler
        )
    }

    private init(
        device: any MTLDevice,
        queue: any MTLCommandQueue,
        pipeline: any MTLRenderPipelineState,
        sampler: any MTLSamplerState
    ) {
        self.device = device
        self.queue = queue
        self.pipeline = pipeline
        self.sampler = sampler
        super.init()
    }

    func attach(view: MTKView) {
        self.view = view
    }

    /// The framebuffer's current size, or nil before the first `resize`.
    private(set) var size: DisplayMode?

    /// Adopt a new remote size.
    ///
    /// A no-op when the size already matches: a `resize` that repeats the size
    /// already in use would otherwise throw away a perfectly good framebuffer,
    /// and a reconnect to an unchanged desktop re-announces one.
    func resize(to next: DisplayMode) {
        guard size != next else {
            return
        }
        size = next
        hasContent = false
        texture = makeTexture(next)
        view?.drawableSize = CGSize(width: Int(next.w), height: Int(next.h))
        requestDisplay()
    }

    /// Drop everything on screen. Cheap to obey: the gateway always repaints in
    /// full when a client (re)attaches, so there is nothing to preserve across an
    /// interruption, and stale pixels from a session that ended are worse than
    /// black.
    func clear() {
        size = nil
        texture = nil
        hasContent = false
        requestDisplay()
    }

    /// Write one frame's tiles into the framebuffer and ask for a single redraw.
    ///
    /// The redraw is requested once for the whole batch, not once per tile. An
    /// `MTKView` would coalesce a burst either way — it is paused and redraws on
    /// demand — but "either way" depends on the burst arriving inside one refresh
    /// interval, which is a race. Asking once makes one repaint one draw by
    /// construction.
    ///
    /// Tiles are applied in order, because a later one overwriting an earlier one
    /// is how the wire expresses a region being painted twice.
    func upload(_ tiles: [DecodedTile]) {
        for tile in tiles {
            write(tile)
        }
        requestDisplay()
    }

    private func write(_ tile: DecodedTile) {
        guard let texture else {
            return
        }
        // Not defensive style: an out-of-range `replaceRegion` is a Metal
        // validation abort, which takes the process with it. Ordering makes a
        // tile from before a shrink impossible in theory, and two comparisons are
        // a cheap way not to depend on that.
        guard Int(tile.x) + Int(tile.w) <= texture.width,
              Int(tile.y) + Int(tile.h) <= texture.height
        else {
            log.warning(
                """
                tile \(tile.w, privacy: .public)x\(tile.h, privacy: .public) at \
                \(tile.x, privacy: .public),\(tile.y, privacy: .public) does not fit \
                \(texture.width, privacy: .public)x\(texture.height, privacy: .public)
                """
            )
            return
        }
        tile.bgra.withUnsafeBytes { bytes in
            guard let base = bytes.baseAddress else {
                return
            }
            texture.replace(
                region: MTLRegionMake2D(Int(tile.x), Int(tile.y), Int(tile.w), Int(tile.h)),
                mipmapLevel: 0,
                withBytes: base,
                bytesPerRow: Int(tile.w) * 4
            )
        }
        hasContent = true
    }

    /// Ask for a redraw of the whole view. No dirty-rect bookkeeping and no timer:
    /// the view is paused, so this is the only thing that draws.
    ///
    /// The count is a test seam. Whether a batch cost one redraw or twenty is
    /// invisible to `snapshot()` — the pixels are identical either way — and there
    /// is no view at all in a headless test, so the request itself is the only
    /// thing left to observe.
    private(set) var displayRequests = 0

    private func requestDisplay() {
        displayRequests += 1
        view?.setNeedsDisplay(view?.bounds ?? .zero)
    }

    /// The framebuffer's current pixels, BGRA and top-down, or nil when there is
    /// nothing to read.
    ///
    /// Free to do because the texture is `.shared`: this is a copy out of the
    /// same allocation the GPU samples, with no blit and no synchronisation. It
    /// exists so the upload path — the bounds guard and `replaceRegion`'s
    /// offsets — can be checked without a window or a drawable.
    func snapshot() -> [UInt8]? {
        guard let texture, hasContent else {
            return nil
        }
        var pixels = [UInt8](repeating: 0, count: texture.width * texture.height * 4)
        pixels.withUnsafeMutableBytes { buffer in
            guard let base = buffer.baseAddress else {
                return
            }
            texture.getBytes(
                base,
                bytesPerRow: texture.width * 4,
                from: MTLRegionMake2D(0, 0, texture.width, texture.height),
                mipmapLevel: 0
            )
        }
        return pixels
    }

    // MARK: - MTKViewDelegate

    func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {
        // Nothing: the drawable is pinned to the remote's size, not the view's.
    }

    func draw(in view: MTKView) {
        guard let drawable = view.currentDrawable,
              let descriptor = view.currentRenderPassDescriptor,
              let buffer = queue.makeCommandBuffer()
        else {
            return
        }
        encode(into: descriptor, on: buffer)
        // `presentsWithTransaction` is set on the view, which changes the present
        // protocol: the drawable is presented here rather than by the command
        // buffer, after it is scheduled. Using `buffer.present(drawable)` under
        // that flag is what makes the framebuffer lag behind a scroll.
        buffer.commit()
        buffer.waitUntilScheduled()
        drawable.present()
    }

    /// Draw the framebuffer into `target` and wait for it.
    ///
    /// The same encode the view's draw uses, against a texture instead of a
    /// drawable — which is what makes the shader's UV orientation checkable
    /// without a window. Worth being able to check: a strip is placed into the
    /// texture by its own y, so an inverted sampler cannot be corrected anywhere
    /// downstream.
    func render(into target: any MTLTexture) {
        let descriptor = MTLRenderPassDescriptor()
        descriptor.colorAttachments[0].texture = target
        descriptor.colorAttachments[0].loadAction = .clear
        descriptor.colorAttachments[0].storeAction = .store
        descriptor.colorAttachments[0].clearColor = MTLClearColorMake(0, 0, 0, 1)
        guard let buffer = queue.makeCommandBuffer() else {
            return
        }
        encode(into: descriptor, on: buffer)
        buffer.commit()
        buffer.waitUntilCompleted()
    }

    private func encode(
        into descriptor: MTLRenderPassDescriptor,
        on buffer: any MTLCommandBuffer
    ) {
        guard let texture, hasContent else {
            // The pass's clear alone. Still encoded, so the target is defined.
            buffer.makeRenderCommandEncoder(descriptor: descriptor)?.endEncoding()
            return
        }
        guard let encoder = buffer.makeRenderCommandEncoder(descriptor: descriptor) else {
            return
        }
        encoder.setRenderPipelineState(pipeline)
        encoder.setFragmentTexture(texture, index: 0)
        encoder.setFragmentSamplerState(sampler, index: 0)
        encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        encoder.endEncoding()
    }

    private func makeTexture(_ size: DisplayMode) -> (any MTLTexture)? {
        let descriptor = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .bgra8Unorm,
            width: Int(size.w),
            height: Int(size.h),
            mipmapped: false
        )
        descriptor.usage = [.shaderRead]
        // Shared: on unified memory `replaceRegion` is then a memcpy straight
        // into the allocation the GPU reads, with no staging blit.
        descriptor.storageMode = .shared
        guard let texture = device.makeTexture(descriptor: descriptor) else {
            log.error(
                "could not allocate a \(size.w, privacy: .public)x\(size.h, privacy: .public) framebuffer"
            )
            return nil
        }
        return texture
    }
}
