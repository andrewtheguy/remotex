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
        // No resources: the Opus fixtures went with the decoder that read them.
        // Decoding is the canvas page's now (WebCodecs), so what this target can
        // still check about audio is the *subscription* — `AudioControlTests` —
        // and that needs no bytes.
        .testTarget(
            name: "RemotexViewerTests",
            dependencies: ["RemotexViewer"],
            path: "Tests"
        ),
    ]
)
