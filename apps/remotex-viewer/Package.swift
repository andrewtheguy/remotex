// swift-tools-version: 6.2

import PackageDescription

let package = Package(
    name: "remotex-viewer",
    platforms: [
        .macOS(.v26),
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
            // Real WebP tile payloads, produced by the gateway's own encoder
            // (`write_swift_webp_fixtures` in src/protocol.rs). Checked in because
            // ImageIO can read WebP but cannot write it, so these tests cannot
            // encode their own the way they used to.
            resources: [.copy("Fixtures")]
        ),
    ]
)
