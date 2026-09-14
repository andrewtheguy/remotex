use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

const PREBUILT_FRONTEND: &str = "REMOTEX_PREBUILT_FRONTEND";

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
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    let frontend_dir = root.join("frontend");
    let status = Command::new("bun")
        .args(["run", "build"])
        .current_dir(&frontend_dir)
        .env("REMOTEX_FRONTEND_OUT_DIR", output)
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
