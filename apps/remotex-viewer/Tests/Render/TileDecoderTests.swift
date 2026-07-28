import CoreGraphics
import Foundation
import ImageIO
import Testing
@testable import RemotexViewer

/// CoreGraphics only, so this runs headless — a Metal device is not always
/// available under `swift test`, and none of the pixel work needs one.
///
/// Payloads come from `Tests/Fixtures` (see `webpFixture`). They used to be encoded
/// here at runtime, deliberately, so that no encoder's choices got frozen into a
/// test — but ImageIO cannot *write* WebP, only read it, so that is no longer
/// possible. What the fixtures freeze instead is the output of the encoder that
/// ships, which is at least the payload a real session carries.
struct TileDecoderTests {
    /// The tile's own rows, top-down, is what `replaceRegion` means by row 0.
    ///
    /// The fixture's top half is red and its bottom half blue, so a vertical flip
    /// and a channel swap both show up here — a uniform image would catch neither.
    @Test
    func aTileDecodesToTopDownBGRA() async throws {
        let payload = try webpFixture("topdown-8x8")

        let tile = try #require(
            await TileDecoder().decode(
                TileFrame(
                    format: .webp, slot: BatchFrame.noSlot, x: 3, y: 5, w: 8, h: 8, payload: payload
                )
            )
        )
        #expect(tile.x == 3)
        #expect(tile.y == 5)
        #expect(tile.bgra.count == 8 * 8 * 4)
        // Buffer index 0 is the image's top-left pixel: row 0 is the top row, and
        // `bgr` reads it back out of the B,G,R,X byte order. Lossless, so these are
        // exact rather than within a tolerance.
        #expect(bgr(tile, 0) == (255, 0, 0), "top-left")
        #expect(bgr(tile, 8 * 3 + 7) == (255, 0, 0), "end of the last red row")
        #expect(bgr(tile, 8 * 4) == (0, 0, 255), "start of the first blue row")
        #expect(bgr(tile, 8 * 8 - 1) == (0, 0, 255), "bottom-right")
    }

    /// WebP decodes as three channels, or four when its bitstream carries alpha,
    /// and ImageIO picks. Both have to come out as the same four-byte BGRA the
    /// texture upload expects.
    ///
    /// Nothing in production encodes the alpha case — both ends encode from packed
    /// RGB888 — which is exactly why it is a fixture: the normalisation would
    /// otherwise be untested until something upstream started carrying alpha.
    @Test
    func opaqueAndAlphaTilesBothNormalizeToFourBytesPerPixel() async throws {
        let decoder = TileDecoder()
        for name in ["opaque-4x4", "alpha-4x4"] {
            let tile = try #require(
                await decoder.decode(
                    TileFrame(
                        format: .webp, slot: BatchFrame.noSlot, x: 0, y: 0, w: 4, h: 4,
                        payload: try webpFixture(name)
                    )
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
        let payload = try webpFixture("opaque-4x4")
        let decoder = TileDecoder()
        #expect(
            await decoder.decode(
                TileFrame(
                    format: .webp, slot: BatchFrame.noSlot, x: 0, y: 0, w: 8, h: 8, payload: payload
                )
            ) == nil
        )
        #expect(
            await decoder.decode(
                TileFrame(
                    format: .webp, slot: BatchFrame.noSlot, x: 0, y: 0, w: 4, h: 2, payload: payload
                )
            ) == nil
        )
    }

    @Test
    func junkAndEmptyPayloadsAreDropped() async {
        let decoder = TileDecoder()
        #expect(
            await decoder.decode(
                TileFrame(
                    format: .webp, slot: BatchFrame.noSlot, x: 0, y: 0, w: 4, h: 4, payload: Data()
                )
            ) == nil
        )
        #expect(
            await decoder.decode(
                TileFrame(
                    format: .webp, slot: BatchFrame.noSlot,
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 4,
                    payload: Data([0x00, 0x01, 0x02, 0x03])
                )
            ) == nil
        )
        // A truncated WebP: the container header is intact, so this is rejected by
        // the decode rather than by a magic-byte check.
        #expect(
            await decoder.decode(
                TileFrame(
                    format: .webp, slot: BatchFrame.noSlot,
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 4,
                    payload: Data("RIFF\u{0}\u{0}\u{0}\u{0}WEBPVP8L".utf8)
                )
            ) == nil
        )
    }

    /// A zero-sized tile has no pixels to place, and a zero `bytesPerRow` would
    /// fail the context anyway.
    @Test
    func aZeroSizedTileIsDropped() async throws {
        let payload = try webpFixture("opaque-4x4")
        let decoder = TileDecoder()
        #expect(
            await decoder.decode(
                TileFrame(
                    format: .webp, slot: BatchFrame.noSlot, x: 0, y: 0, w: 0, h: 4, payload: payload
                )
            ) == nil
        )
        #expect(
            await decoder.decode(
                TileFrame(
                    format: .webp, slot: BatchFrame.noSlot, x: 0, y: 0, w: 4, h: 0, payload: payload
                )
            ) == nil
        )
    }

    /// Every fixture is what its name says, checked from the bytes on disk.
    ///
    /// This is the guard the runtime encoding used to provide for free. Without it a
    /// fixture that was regenerated wrongly — or silently replaced by a PNG, which
    /// `TileDecoder` would happily decode, since it identifies a payload from its
    /// own container and never reads `format` — would leave every test above
    /// passing while testing the wrong codec.
    @Test
    func everyFixtureIsAWebPOfTheSizeItsNameClaims() throws {
        for (name, side) in [
            ("solid-2x2-11", 2), ("solid-2x2-22", 2), ("solid-2x2-ff", 2),
            ("topdown-8x8", 8), ("opaque-4x4", 4), ("alpha-4x4", 4),
        ] {
            let data = try webpFixture(name)
            #expect(data.prefix(4) == Data("RIFF".utf8), "\(name) is not a RIFF container")
            #expect(data.dropFirst(8).prefix(4) == Data("WEBP".utf8), "\(name) is not a WebP")
            let source = try #require(CGImageSourceCreateWithData(data as CFData, nil))
            #expect(
                CGImageSourceGetType(source) as String? == "org.webmproject.webp",
                "\(name) is not identified as WebP by ImageIO"
            )
            let image = try #require(CGImageSourceCreateImageAtIndex(source, 0, nil))
            #expect(image.width == side && image.height == side, "\(name) is the wrong size")
        }
    }

    // MARK: - Fixtures

    private typealias Pixel = (r: UInt8, g: UInt8, b: UInt8)

    private func bgr(_ tile: DecodedTile, _ index: Int) -> Pixel {
        let base = index * 4
        return (tile.bgra[base + 2], tile.bgra[base + 1], tile.bgra[base])
    }
}
