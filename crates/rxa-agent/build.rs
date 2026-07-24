//! Adds the OS Swift runtime to the binary's runpath.
//!
//! `screencapturekit` compiles its Swift bridge with `swift build --triple
//! arm64-apple-macosx` — no version on the triple. Without a minimum OS
//! version Swift assumes an ancient deployment target and links the
//! *back-deployment* concurrency library, `@rpath/libswift_Concurrency.dylib`,
//! rather than the one macOS 12+ ships. Nothing then adds a matching runpath,
//! so the agent builds fine and dies at startup with:
//!
//! ```text
//! dyld: Library not loaded: @rpath/libswift_Concurrency.dylib
//! ```
//!
//! `/usr/lib/swift` is where the OS keeps it (in the dyld shared cache), and it
//! is present on every macOS this agent supports — so this is a runpath entry,
//! not a dependency on Xcode being installed on the target machine.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
