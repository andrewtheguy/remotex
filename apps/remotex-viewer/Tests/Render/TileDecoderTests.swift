import CoreGraphics
import Foundation
import ImageIO
import Testing
import UniformTypeIdentifiers
@testable import RemotexViewer

/// CoreGraphics only, so this runs headless — a Metal device is not always
/// available under `swift test`, and none of the pixel work needs one.
///
/// Payloads are encoded at runtime rather than checked in as fixtures: the point
/// is that whatever ImageIO produces round-trips, and a fixture would freeze one
/// encoder's choices (bit depth, colour type, palette) as if they were the
/// contract.
struct TileDecoderTests {
    /// The tile's own rows, top-down, is what `replaceRegion` means by row 0.
    /// A quadrant image catches both a vertical flip and a channel swap, which
    /// a uniform one cannot.
    @Test
    func aPNGDecodesToTopDownBGRA() async throws {
        let red: Pixel = (255, 0, 0)
        let green: Pixel = (0, 255, 0)
        let blue: Pixel = (0, 0, 255)
        let white: Pixel = (255, 255, 255)
        let png = try encode(
            image(width: 2, height: 2, pixels: [red, green, blue, white]),
            as: .png
        )

        let tile = try #require(
            await TileDecoder().decode(
                TileFrame(format: .png, slot: BatchFrame.noSlot, x: 3, y: 5, w: 2, h: 2, payload: png)
            )
        )
        #expect(tile.x == 3)
        #expect(tile.y == 5)
        #expect(tile.bgra.count == 2 * 2 * 4)
        // Buffer index 0 is the image's top-left pixel: row 0 is the top row,
        // and `bgr` reads it back out of the B,G,R,X byte order.
        #expect(bgr(tile, 0) == red, "top-left")
        #expect(bgr(tile, 1) == green, "top-right")
        #expect(bgr(tile, 2) == blue, "bottom-left")
        #expect(bgr(tile, 3) == white, "bottom-right")
    }

    /// JPEG decodes as three-channel RGB where PNG may be RGBA, so the same
    /// context path has to normalize both. Lossy, hence bands and a tolerance
    /// rather than exact pixels — row order is what matters here.
    @Test
    func aJPEGDecodesToTopDownBGRA() async throws {
        let width = 16
        let height = 16
        let pixels: [Pixel] = (0 ..< width * height).map { index in
            index < width * height / 2 ? (255, 0, 0) : (0, 0, 255)
        }
        let jpeg = try encode(
            image(width: width, height: height, pixels: pixels),
            as: .jpeg
        )

        let tile = try #require(
            await TileDecoder().decode(
                TileFrame(
                    format: .jpeg, slot: BatchFrame.noSlot,
                    x: 0,
                    y: 0,
                    w: UInt16(width),
                    h: UInt16(height),
                    payload: jpeg
                )
            )
        )
        #expect(tile.bgra.count == width * height * 4)
        let top = bgr(tile, width * 2 + width / 2)
        let bottom = bgr(tile, width * (height - 3) + width / 2)
        #expect(top.r > 200 && top.b < 55, "the top band should be red, got \(top)")
        #expect(bottom.b > 200 && bottom.r < 55, "the bottom band should be blue, got \(bottom)")
    }

    /// A grayscale PNG has one channel and an RGBA one has four; both have to
    /// come out as the same four-byte BGRA the texture upload expects.
    @Test
    func grayscaleAndAlphaPNGsBothNormalizeToFourBytesPerPixel() async throws {
        let decoder = TileDecoder()
        for (name, data) in try [
            ("grayscale", encode(grayscaleImage(width: 4, height: 4), as: .png)),
            ("rgba", encode(alphaImage(width: 4, height: 4), as: .png)),
        ] {
            let tile = try #require(
                await decoder.decode(
                    TileFrame(format: .png, slot: BatchFrame.noSlot, x: 0, y: 0, w: 4, h: 4, payload: data)
                ),
                "\(name) should decode"
            )
            #expect(tile.bgra.count == 4 * 4 * 4, "\(name) should be BGRA")
        }
    }

    /// The header decides placement, so a payload of another size is dropped.
    /// Scaling it into the rectangle instead would spread one bad tile over the
    /// screen, and a dropped tile costs one repaint.
    @Test
    func aPayloadDisagreeingWithTheHeaderIsDropped() async throws {
        let png = try encode(
            image(width: 4, height: 4, pixels: Array(repeating: (1, 2, 3), count: 16)),
            as: .png
        )
        let decoder = TileDecoder()
        #expect(
            await decoder.decode(
                TileFrame(format: .png, slot: BatchFrame.noSlot, x: 0, y: 0, w: 8, h: 8, payload: png)
            ) == nil
        )
        #expect(
            await decoder.decode(
                TileFrame(format: .png, slot: BatchFrame.noSlot, x: 0, y: 0, w: 4, h: 2, payload: png)
            ) == nil
        )
    }

    @Test
    func junkAndEmptyPayloadsAreDropped() async {
        let decoder = TileDecoder()
        #expect(
            await decoder.decode(
                TileFrame(format: .png, slot: BatchFrame.noSlot, x: 0, y: 0, w: 4, h: 4, payload: Data())
            ) == nil
        )
        #expect(
            await decoder.decode(
                TileFrame(
                    format: .png, slot: BatchFrame.noSlot,
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 4,
                    payload: Data([0x00, 0x01, 0x02, 0x03])
                )
            ) == nil
        )
    }

    /// A zero-sized tile has no pixels to place, and a zero `bytesPerRow` would
    /// fail the context anyway.
    @Test
    func aZeroSizedTileIsDropped() async throws {
        let png = try encode(
            image(width: 1, height: 1, pixels: [(9, 9, 9)]),
            as: .png
        )
        let decoder = TileDecoder()
        #expect(
            await decoder.decode(
                TileFrame(format: .png, slot: BatchFrame.noSlot, x: 0, y: 0, w: 0, h: 4, payload: png)
            ) == nil
        )
        #expect(
            await decoder.decode(
                TileFrame(format: .png, slot: BatchFrame.noSlot, x: 0, y: 0, w: 4, h: 0, payload: png)
            ) == nil
        )
    }

    // MARK: - Fixtures

    private typealias Pixel = (r: UInt8, g: UInt8, b: UInt8)

    private func bgr(_ tile: DecodedTile, _ index: Int) -> Pixel {
        (r: tile.bgra[index * 4 + 2], g: tile.bgra[index * 4 + 1], b: tile.bgra[index * 4])
    }

    /// `pixels` in reading order: left to right, top row first.
    private func image(width: Int, height: Int, pixels: [Pixel]) throws -> CGImage {
        var bytes = [UInt8]()
        bytes.reserveCapacity(width * height * 4)
        for pixel in pixels {
            bytes.append(contentsOf: [pixel.r, pixel.g, pixel.b, 0xFF])
        }
        return try image(
            width: width,
            height: height,
            bytes: bytes,
            bitsPerPixel: 32,
            space: CGColorSpaceCreateDeviceRGB(),
            info: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipLast.rawValue)
        )
    }

    private func grayscaleImage(width: Int, height: Int) throws -> CGImage {
        let bytes = (0 ..< width * height).map { UInt8($0 * 8) }
        return try image(
            width: width,
            height: height,
            bytes: bytes,
            bitsPerPixel: 8,
            space: CGColorSpaceCreateDeviceGray(),
            info: CGBitmapInfo(rawValue: CGImageAlphaInfo.none.rawValue)
        )
    }

    private func alphaImage(width: Int, height: Int) throws -> CGImage {
        var bytes = [UInt8]()
        for index in 0 ..< width * height {
            bytes.append(contentsOf: [UInt8(index * 4), 0x40, 0x80, 0x80])
        }
        return try image(
            width: width,
            height: height,
            bytes: bytes,
            bitsPerPixel: 32,
            space: CGColorSpaceCreateDeviceRGB(),
            info: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue)
        )
    }

    private func image(
        width: Int,
        height: Int,
        bytes: [UInt8],
        bitsPerPixel: Int,
        space: CGColorSpace,
        info: CGBitmapInfo
    ) throws -> CGImage {
        let provider = try #require(CGDataProvider(data: Data(bytes) as CFData))
        return try #require(
            CGImage(
                width: width,
                height: height,
                bitsPerComponent: 8,
                bitsPerPixel: bitsPerPixel,
                bytesPerRow: width * bitsPerPixel / 8,
                space: space,
                bitmapInfo: info,
                provider: provider,
                decode: nil,
                shouldInterpolate: false,
                intent: .defaultIntent
            )
        )
    }

    private func encode(_ image: CGImage, as type: UTType) throws -> Data {
        let data = NSMutableData()
        let destination = try #require(
            CGImageDestinationCreateWithData(data, type.identifier as CFString, 1, nil)
        )
        CGImageDestinationAddImage(destination, image, nil)
        #expect(CGImageDestinationFinalize(destination))
        return data as Data
    }
}
