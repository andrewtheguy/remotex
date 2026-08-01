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
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    if env::var("CI").is_ok_and(|value| value == "true") {
        return;
    }

    let frontend_dir = Path::new(&env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("frontend");
    let status = Command::new("bun")
        .args(["run", "build"])
        .current_dir(frontend_dir)
        .status()
        .expect("failed to run `bun run build` for the frontend");

    assert!(status.success(), "`bun run build` for the frontend failed");
}
