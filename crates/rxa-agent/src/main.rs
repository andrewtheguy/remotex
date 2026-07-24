//! `remotex-agent` — the macOS screen-sharing agent remotex dials over `rxa`.
//!
//! Skeleton only; see `docs/mac-agent-plan.md` for the design. The work still
//! to do, in order: ScreenCaptureKit capture with native dirty rects, per-tile
//! PNG/JPEG encoding, `CGEvent` input injection, cursor shapes, and the
//! LaunchAgent packaging.
//!
//! This crate compiles on macOS only (ScreenCaptureKit + CoreGraphics) and is
//! excluded from the workspace's `default-members` for that reason. The guard
//! below turns "built on the wrong platform" into a clear compile error rather
//! than a wall of missing-symbol noise.

// A bare `#![cfg(target_os = "macos")]` would compile the crate away to
// nothing on Linux and fail at link time with "main function not found",
// which says nothing useful. Fail at compile time with the reason instead.
#[cfg(not(target_os = "macos"))]
compile_error!(
    "rxa-agent is macOS-only (ScreenCaptureKit + CoreGraphics). It is excluded \
     from the workspace's default-members; build it on a Mac with \
     `cargo build -p rxa-agent`."
);

fn main() {
    println!(
        "remotex-agent {} — rxa/{} (not implemented yet)",
        env!("CARGO_PKG_VERSION"),
        String::from_utf8_lossy(rxa_proto::PROLOGUE)
    );
}
