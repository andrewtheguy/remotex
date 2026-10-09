use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

const PREBUILT_FRONTEND: &str = "REMOTEX_PREBUILT_FRONTEND";
/// A copy of the software VP9 decoder's release archive, which the frontend's
/// build takes instead of downloading the release (frontend/wasm/vp9/fetch.ts).
const VP9_WASM_ARCHIVE: &str = "REMOTEX_VP9_WASM_ARCHIVE";

fn main() -> Result<()> {
    println!("cargo:rerun-if-env-changed={PREBUILT_FRONTEND}");

    let root = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").context("Cargo did not set CARGO_MANIFEST_DIR")?,
    );
    let output = PathBuf::from(env::var_os("OUT_DIR").context("Cargo did not set OUT_DIR")?)
        .join("frontend-dist");

    if let Some(prebuilt) = env::var_os(PREBUILT_FRONTEND) {
        let prebuilt = PathBuf::from(prebuilt);
        ensure!(
            !prebuilt.as_os_str().is_empty(),
            "{PREBUILT_FRONTEND} must name the directory containing index.html"
        );
        let prebuilt = if prebuilt.is_absolute() {
            prebuilt
        } else {
            root.join(prebuilt)
        };
        println!("cargo:rerun-if-changed={}", prebuilt.display());
        replace_dir(&prebuilt, &output).with_context(|| {
            format!(
                "failed to stage the prebuilt frontend from {}",
                prebuilt.display()
            )
        })?;
    } else {
        build_frontend(&root, &output)?;
    }

    ensure!(
        output.join("index.html").is_file(),
        "frontend build produced no {}",
        output.join("index.html").display()
    );
    Ok(())
}

fn build_frontend(root: &Path, output: &Path) -> Result<()> {
    for path in [
        "Cargo.toml",
        "frontend/src",
        "frontend/index.html",
        "frontend/package.json",
        "frontend/bun.lock",
        "frontend/tsconfig.json",
        "frontend/tsconfig.app.json",
        "frontend/tsconfig.node.json",
        "frontend/vite.config.ts",
        // The page's compositor for a passed graphics pipeline, built to
        // WebAssembly (frontend/wasm/egfx) from the crate the gateway composes
        // with. The binding's files are named one by one: its directory also
        // holds what the build writes.
        "frontend/wasm/egfx/Cargo.toml",
        "frontend/wasm/egfx/Cargo.lock",
        "frontend/wasm/egfx/.cargo/config.toml",
        "frontend/wasm/egfx/rust-toolchain.toml",
        "frontend/wasm/egfx/src",
        "crates/remotex-rdp-graphics/Cargo.toml",
        "crates/remotex-rdp-graphics/src",
        // The page's decoder for lossless sound, a module of its own
        // (frontend/wasm/flac), named file by file for the same reason.
        "frontend/wasm/flac/Cargo.toml",
        "frontend/wasm/flac/Cargo.lock",
        "frontend/wasm/flac/.cargo/config.toml",
        "frontend/wasm/flac/rust-toolchain.toml",
        "frontend/wasm/flac/src",
        // The page's software VP9 decoder, which is not built here: the release
        // of andrewtheguy/vp9-wasm the pin names, unpacked beside it by the
        // script.
        "frontend/wasm/vp9/pin.json",
        "frontend/wasm/vp9/fetch.ts",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed={VP9_WASM_ARCHIVE}");

    let frontend_dir = root.join("frontend");
    let mut bun = Command::new("bun");
    bun.args(["run", "build"]).current_dir(&frontend_dir).env("REMOTEX_FRONTEND_OUT_DIR", output);
    // The frontend's build runs Cargo for its WebAssembly modules, and that Cargo
    // must not take this one's for its own: the flags and wrappers this build was
    // given are for the gateway's target — under `cargo clippy` the wrapper *is*
    // clippy — the job server's descriptors are not passed down, and a target
    // directory shared with the build that is waiting on this script is a lock
    // neither would ever be given, so each module builds in its own directory's.
    for inherited in [
        "CARGO_BUILD_TARGET_DIR",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_MAKEFLAGS",
        "CLIPPY_ARGS",
        "CLIPPY_CONF_DIR",
        "MAKEFLAGS",
        "MFLAGS",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTDOCFLAGS",
        "RUSTFLAGS",
        // Each module is built by the toolchain its own directory names
        // (frontend/wasm/egfx/rust-toolchain.toml pins a nightly), which the one
        // this build runs under, and the compiler Cargo names to its build
        // scripts, would otherwise be chosen over.
        "CARGO",
        "RUSTC",
        "RUSTDOC",
        "RUSTUP_TOOLCHAIN",
    ] {
        bun.env_remove(inherited);
    }
    let status = bun
        .status()
        .context("failed to run `bun run build` for the frontend")?;
    ensure!(status.success(), "`bun run build` for the frontend failed");
    Ok(())
}

fn replace_dir(source: &Path, destination: &Path) -> Result<()> {
    ensure!(
        source.join("index.html").is_file(),
        "{} contains no index.html",
        source.display()
    );
    if destination.exists() {
        fs::remove_dir_all(destination)
            .with_context(|| format!("failed to remove {}", destination.display()))?;
    }
    copy_dir(source, destination)
}

fn copy_dir(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    for entry in fs::read_dir(source)
        .with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read an entry in {}", source.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
        } else {
            bail!(
                "prebuilt frontend contains unsupported entry {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}
