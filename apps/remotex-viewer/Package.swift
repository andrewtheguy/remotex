// swift-tools-version: 6.2

import PackageDescription

let package = Package(
    name: "remotex-viewer",
    platforms: [
        .macOS(.v15),
    ],
    products: [
        .executable(name: "remotex-viewer", targets: ["RemotexViewer"]),
    ],
    targets: [
        .executableTarget(
            name: "RemotexViewer",
            path: "Sources"
        ),
        .testTarget(
            name: "RemotexViewerTests",
            dependencies: ["RemotexViewer"],
            path: "Tests",
            // Opus audio fixtures (`Fixtures/opus`), produced by the gateway's own
            // encoder (`write_swift_opus_fixtures` in src/opus_stream.rs) because
            // macOS can encode Opus but must not for these tests. Tile payloads are
            // encoded at runtime through ImageIO, so they need no fixture.
            resources: [.copy("Fixtures")]
        ),
    ]
)
