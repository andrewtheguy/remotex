use std::env;
use std::fs::File;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;

/// A file this script leaves in `frontend/dist` so Cargo notices the bundle is
/// gone. Cargo backdates a build script's "last build" to before the script
/// started, so every file Vite writes looks newer and would rerun this script
/// on each build; the marker is set to the Unix epoch instead. It is missing
/// exactly when `frontend/dist` has been deleted, which Cargo counts as changed.
const MARKER: &str = "frontend/dist/.embedded";

fn main() {
    println!("cargo:rerun-if-env-changed=CI");
    for path in [
        "frontend/src",
        "frontend/index.html",
        "frontend/package.json",
        "frontend/bun.lock",
        "frontend/tsconfig.json",
        "frontend/tsconfig.app.json",
        "frontend/tsconfig.node.json",
        "frontend/vite.config.ts",
        MARKER,
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let frontend_dir = root.join("frontend");

    // CI builds the frontend once and hands every builder the same `frontend/dist`;
    // a local build makes its own.
    if !env::var("CI").is_ok_and(|value| value == "true") {
        let status = Command::new("bun")
            .args(["run", "build"])
            .current_dir(&frontend_dir)
            .status()
            .expect("failed to run `bun run build` for the frontend");
        assert!(status.success(), "`bun run build` for the frontend failed");
    }

    // src/assets.rs compiles the bundle into the binary, so its absence is a build
    // error here rather than a gateway that serves 404s.
    assert!(
        frontend_dir.join("dist").join("index.html").is_file(),
        "frontend/dist/index.html is missing: the frontend is compiled into the gateway, \
         so build it first with `bun run build` in frontend/"
    );

    File::create(root.join(MARKER))
        .and_then(|marker| marker.set_modified(SystemTime::UNIX_EPOCH))
        .expect("failed to write the frontend/dist marker");
}
