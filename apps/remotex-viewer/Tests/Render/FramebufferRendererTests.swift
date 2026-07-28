import Metal
import Testing
@testable import RemotexViewer

/// Covers the upload path — where a tile lands, and what happens to one that
/// would not fit — without a window or a drawable. Only the draw itself needs
/// either, and there is no logic in it.
///
/// Disabled rather than silently skipped where there is no GPU, so an empty run
/// says so.
@MainActor
@Suite(.enabled(if: MTLCreateSystemDefaultDevice() != nil))
struct FramebufferRendererTests {
    @Test
    func aTileLandsAtItsOwnCoordinates() throws {
        let renderer = try #require(FramebufferRenderer.make())
        renderer.resize(to: DisplayMode(w: 8, h: 8))
        #expect(renderer.snapshot() == nil, "no tile has landed yet")

        // A 2x2 red block at (4, 2).
        renderer.upload([tile(x: 4, y: 2, w: 2, h: 2, blue: 0x00, green: 0x00, red: 0xFF)])

        let pixels = try #require(renderer.snapshot())
        #expect(pixels.count == 8 * 8 * 4)
        for row in 0 ..< 8 {
            for column in 0 ..< 8 {
                let offset = (row * 8 + column) * 4
                let inside = (2 ... 3).contains(row) && (4 ... 5).contains(column)
                let expected: [UInt8] = inside ? [0x00, 0x00, 0xFF, 0xFF] : [0, 0, 0, 0]
                #expect(
                    Array(pixels[offset ..< offset + 4]) == expected,
                    "row \(row) column \(column)"
                )
            }
        }
    }

    /// Tiles overwrite; there is no blending and no delta state.
    @Test
    func alaterTileOverwritesAnEarlierOne() throws {
        let renderer = try #require(FramebufferRenderer.make())
        renderer.resize(to: DisplayMode(w: 4, h: 4))
        // One batch, so this covers ordering *within* a frame as well.
        renderer.upload([
            tile(x: 0, y: 0, w: 4, h: 4, blue: 0xFF, green: 0x00, red: 0x00),
            tile(x: 0, y: 0, w: 2, h: 2, blue: 0x00, green: 0xFF, red: 0x00),
        ])

        let pixels = try #require(renderer.snapshot())
        #expect(Array(pixels[0 ..< 4]) == [0x00, 0xFF, 0x00, 0xFF], "overwritten")
        // Row 0, column 2 is outside the second tile and keeps the first.
        #expect(Array(pixels[8 ..< 12]) == [0xFF, 0x00, 0x00, 0xFF], "untouched")
    }

    /// The guard that keeps a bad tile from taking the process with it: an
    /// out-of-range `replaceRegion` is a Metal validation abort, not an error. If
    /// this regressed, the test process would die rather than fail.
    @Test
    func aTileOutsideTheFramebufferIsDroppedRatherThanAborting() throws {
        let renderer = try #require(FramebufferRenderer.make())
        renderer.resize(to: DisplayMode(w: 4, h: 4))
        renderer.upload([tile(x: 0, y: 0, w: 4, h: 4, blue: 0x11, green: 0x22, red: 0x33)])

        renderer.upload([
            tile(x: 3, y: 0, w: 4, h: 1, blue: 0, green: 0, red: 0),
            tile(x: 0, y: 3, w: 1, h: 4, blue: 0, green: 0, red: 0),
            tile(x: 9, y: 9, w: 1, h: 1, blue: 0, green: 0, red: 0),
        ])

        let pixels = try #require(renderer.snapshot())
        #expect(Array(pixels[0 ..< 4]) == [0x11, 0x22, 0x33, 0xFF], "nothing was written")
    }

    /// The gateway suppresses a redundant `resize` so an unchanged desktop keeps
    /// its pixels, but a reconnect re-announces one. Reallocating here would
    /// throw the framebuffer away for nothing.
    @Test
    func resizingToTheSameSizeKeepsThePixels() throws {
        let renderer = try #require(FramebufferRenderer.make())
        renderer.resize(to: DisplayMode(w: 4, h: 4))
        renderer.upload([tile(x: 0, y: 0, w: 4, h: 4, blue: 0x11, green: 0x22, red: 0x33)])

        renderer.resize(to: DisplayMode(w: 4, h: 4))

        let pixels = try #require(renderer.snapshot())
        #expect(Array(pixels[0 ..< 4]) == [0x11, 0x22, 0x33, 0xFF])
    }

    @Test
    func resizingToADifferentSizeStartsFromNothing() throws {
        let renderer = try #require(FramebufferRenderer.make())
        renderer.resize(to: DisplayMode(w: 4, h: 4))
        renderer.upload([tile(x: 0, y: 0, w: 4, h: 4, blue: 0x11, green: 0x22, red: 0x33)])

        renderer.resize(to: DisplayMode(w: 8, h: 8))

        #expect(renderer.size == DisplayMode(w: 8, h: 8))
        #expect(renderer.snapshot() == nil, "a new framebuffer shows black until a tile lands")
    }

    /// Clearing has to leave nothing behind: an interruption must not show pixels
    /// from a session that ended, and the gateway repaints in full on reattach.
    @Test
    func clearingDropsTheFramebuffer() throws {
        let renderer = try #require(FramebufferRenderer.make())
        renderer.resize(to: DisplayMode(w: 4, h: 4))
        renderer.upload([tile(x: 0, y: 0, w: 4, h: 4, blue: 0x11, green: 0x22, red: 0x33)])

        renderer.clear()

        #expect(renderer.size == nil)
        #expect(renderer.snapshot() == nil)
        // A tile arriving after a clear has nowhere to go, and must not crash.
        renderer.upload([tile(x: 0, y: 0, w: 4, h: 4, blue: 0, green: 0, red: 0)])
        #expect(renderer.snapshot() == nil)
    }

    /// A frame's worth of tiles costs one redraw request, however many tiles it
    /// carries.
    ///
    /// The pixels are identical either way, so nothing in `snapshot()` can see
    /// this, and a paused `MTKView` would coalesce a burst of requests anyway —
    /// but only if they all land inside one refresh interval, which is a race. The
    /// point of asking once is that it does not depend on that.
    @Test
    func aBatchCostsOneRedrawRequestHoweverManyTilesItCarries() throws {
        let renderer = try #require(FramebufferRenderer.make())
        renderer.resize(to: DisplayMode(w: 16, h: 16))
        let before = renderer.displayRequests

        renderer.upload((0 ..< 8).map { i in
            tile(x: UInt16(i * 2), y: 0, w: 2, h: 2, blue: 0x11, green: 0x22, red: 0x33)
        })

        #expect(renderer.displayRequests == before + 1)

        // And a second batch is a second request, so this is counting batches
        // rather than measuring nothing at all.
        renderer.upload([tile(x: 0, y: 4, w: 2, h: 2, blue: 0, green: 0, red: 0)])
        #expect(renderer.displayRequests == before + 2)
    }

    /// The blit has to preserve orientation: the framebuffer's top-left texel
    /// must come out at the render target's top-left.
    ///
    /// This is the shader's `uv.y` flip. It is a flip rather than a straight
    /// mapping because clip space grows upward while the texture's row 0 is the
    /// top of the desktop — and it cannot be checked any other way short of
    /// looking at the screen. Getting it wrong renders the desktop upside down,
    /// and a strip is placed by its own y, so nothing downstream could undo it.
    @Test
    func theBlitPreservesOrientation() throws {
        let renderer = try #require(FramebufferRenderer.make())
        renderer.resize(to: DisplayMode(w: 2, h: 2))
        // Distinct quadrants: top-left red, top-right green, bottom-left blue,
        // bottom-right white.
        renderer.upload([
            tile(x: 0, y: 0, w: 1, h: 1, blue: 0x00, green: 0x00, red: 0xFF),
            tile(x: 1, y: 0, w: 1, h: 1, blue: 0x00, green: 0xFF, red: 0x00),
            tile(x: 0, y: 1, w: 1, h: 1, blue: 0xFF, green: 0x00, red: 0x00),
            tile(x: 1, y: 1, w: 1, h: 1, blue: 0xFF, green: 0xFF, red: 0xFF),
        ])

        let descriptor = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .bgra8Unorm,
            width: 2,
            height: 2,
            mipmapped: false
        )
        descriptor.usage = [.renderTarget, .shaderRead]
        descriptor.storageMode = .shared
        let target = try #require(renderer.device.makeTexture(descriptor: descriptor))

        renderer.render(into: target)

        var rendered = [UInt8](repeating: 0, count: 2 * 2 * 4)
        rendered.withUnsafeMutableBytes { buffer in
            target.getBytes(
                buffer.baseAddress!,
                bytesPerRow: 2 * 4,
                from: MTLRegionMake2D(0, 0, 2, 2),
                mipmapLevel: 0
            )
        }
        // A render target's row 0 is its top row, as the framebuffer's is.
        #expect(Array(rendered[0 ..< 4]) == [0x00, 0x00, 0xFF, 0xFF], "top-left stays red")
        #expect(Array(rendered[4 ..< 8]) == [0x00, 0xFF, 0x00, 0xFF], "top-right stays green")
        #expect(Array(rendered[8 ..< 12]) == [0xFF, 0x00, 0x00, 0xFF], "bottom-left stays blue")
        #expect(Array(rendered[12 ..< 16]) == [0xFF, 0xFF, 0xFF, 0xFF], "bottom-right stays white")
    }

    private func tile(
        x: UInt16,
        y: UInt16,
        w: UInt16,
        h: UInt16,
        blue: UInt8,
        green: UInt8,
        red: UInt8
    ) -> DecodedTile {
        var bgra = [UInt8]()
        for _ in 0 ..< Int(w) * Int(h) {
            bgra.append(contentsOf: [blue, green, red, 0xFF])
        }
        return DecodedTile(x: x, y: y, w: w, h: h, bgra: bgra)
    }
}
