//! Chromium's subprocess: renderer, GPU, network, storage, alerts.
//!
//! Deliberately almost nothing. Everything about *what* it does — the `remotex://`
//! scheme, the message router's renderer side — comes out of `remotex-cef`, so the
//! browser process and this one cannot come to disagree about either.
//!
//! What is here is what only a subprocess has, and one of the three is here for a
//! linking reason rather than a design one. `remotex-cef` is a `staticlib` linked
//! into the Swift app, and a staticlib keeps every public symbol — so a public
//! entry point that touched `cef::sandbox` would leave the app with an undefined
//! `cef_sandbox_initialize` to resolve on behalf of a process it never runs.

use cef::*;

fn main() {
    let args = args::Args::new();

    // The seatbelt, before anything else runs. The browser process does not do
    // this: it passes a null `sandbox_info` and merely says `no_sandbox = 0`.
    #[cfg(target_os = "macos")]
    let _sandbox = {
        let mut sandbox = cef::sandbox::Sandbox::new();
        sandbox.initialize(args.as_main_args());
        sandbox
    };

    // Three directories up rather than `../Frameworks`: this executable is inside
    // `Contents/Frameworks/remotex Helper.app/Contents/MacOS`, and the framework
    // is beside that helper bundle rather than inside it.
    let loader =
        library_loader::LibraryLoader::new(&std::env::current_exe().unwrap_or_default(), true);
    if !loader.load() {
        return;
    }
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let mut app = remotex_cef::app::helper_app();
    execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
}
