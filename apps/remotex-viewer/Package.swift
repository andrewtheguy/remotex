// swift-tools-version: 6.2

import Foundation
import PackageDescription

// Where `libremotex_cef.a` is looked for.
//
// A directory of its own rather than `target/release`, populated by
// `packaging/macos-viewer/build-viewer-app.sh`, because the manifest cannot know
// which cargo profile the build asked for — naming both would link whichever
// happened to be lying around, which for a `--debug` build beside a stale release
// one is the wrong library and no error to say so.
//
// Derived from `#filePath` rather than written relative, because a relative
// linker flag resolves against wherever `swift build` was invoked, and this must
// not depend on that.
let repositoryRoot = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()  // apps/remotex-viewer
    .deletingLastPathComponent()  // apps
    .deletingLastPathComponent()  // <repo>
    .path

let package = Package(
    name: "remotex-viewer",
    platforms: [
        .macOS(.v15),
    ],
    products: [
        .executable(name: "remotex-viewer", targets: ["RemotexViewer"]),
    ],
    targets: [
        // The Chromium the app shows the client in. A system library, so the
        // crate's own header is the one declaration of the seam rather than a copy
        // of it that could drift.
        .systemLibrary(name: "CRemotexCEF", path: "Sources/CRemotexCEF"),
        .executableTarget(
            name: "RemotexViewer",
            dependencies: ["CRemotexCEF"],
            path: "Sources",
            exclude: ["CRemotexCEF"],
            linkerSettings: [
                .unsafeFlags([
                    "-L\(repositoryRoot)/target/cef-link",
                    // Where `libcef_sandbox.dylib` is found at *runtime*: beside
                    // the framework, inside this app's own bundle. The staged copy
                    // is restamped `@rpath/…` by `stage-cef.sh`, which is what
                    // makes this the answer rather than the working directory.
                    "-Xlinker", "-rpath",
                    "-Xlinker",
                    "@executable_path/../Frameworks/Chromium Embedded Framework.framework/Libraries",
                ]),
                .linkedLibrary("remotex_cef"),
                // The seatbelt. Linked but never called: entering it is
                // `remotex-cef-helper`'s job, and this app resolves the symbols
                // only because a Rust staticlib keeps every public symbol of its
                // dependency graph, `cef::sandbox` among them.
                .linkedLibrary("cef_sandbox"),
                // `libcef_dll_wrapper`, which the crate links, is C++ — so the
                // C++ runtime has to come in with it. Nothing in this package is
                // C++; this is entirely on behalf of what is inside that archive.
                .linkedLibrary("c++"),
            ]
        ),
        // No resources: the Opus fixtures went with the decoder that read them.
        // Decoding is the canvas page's now (WebCodecs), so what this target can
        // still check about audio is the *subscription* — `AudioControlTests` —
        // and that needs no bytes.
        //
        // Nothing here brings Chromium up, and nothing can: CEF wants to be inside
        // an app bundle with the framework in `Contents/Frameworks`, and a test
        // process is neither. The FFI therefore sits behind `CommandSink`, which
        // the tests satisfy with fakes.
        .testTarget(
            name: "RemotexViewerTests",
            dependencies: ["RemotexViewer"],
            path: "Tests",
            linkerSettings: [
                // A test bundle has no `Contents/Frameworks` to resolve
                // `libcef_sandbox.dylib` against, so it is pointed at the staging
                // directory instead. Here rather than on the executable target so
                // that the shipped binary carries no absolute path from whichever
                // machine built it — nothing in these tests loads Chromium, but
                // dyld still resolves the library at load.
                .unsafeFlags([
                    "-Xlinker", "-rpath",
                    "-Xlinker", "\(repositoryRoot)/target/cef-link",
                ])
            ]
        ),
    ]
)
