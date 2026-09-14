use std::env;
use std::path::Path;
use std::process::Command;

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
        // Not `frontend/dist`: Cargo stamps "last build" before this script runs,
        // so a directory the script rewrites always looks newer and every build
        // would run Vite again.
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    let frontend_dir = Path::new(&env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("frontend");

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
}
