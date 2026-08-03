//! Print where CEF actually is, for `packaging/macos-viewer/stage-cef.sh`.
//!
//! The framework has to be copied into the bundle, so the packaging needs a path
//! — and there are two places it can be. `CEF_PATH` names one if it is set;
//! otherwise `cef-dll-sys` downloads and unpacks its own copy under `OUT_DIR`,
//! which is a hashed path no script should be reconstructing. Asking the crate is
//! the only answer that is right in both cases.

fn main() {
    match cef::sys::get_cef_dir() {
        Some(dir) => println!("{}", dir.display()),
        None => {
            eprintln!("cef-dll-sys could not find CEF; set CEF_PATH or let it download one");
            std::process::exit(1);
        }
    }
}
