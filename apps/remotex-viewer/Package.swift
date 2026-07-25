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
            path: "Tests"
        ),
    ]
)
