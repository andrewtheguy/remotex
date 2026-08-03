//! Chromium's subprocess: renderer, GPU, network, storage, alerts.
//!
//! Deliberately almost nothing. Everything it does — the `remotex://` scheme, the
//! message router's renderer side — lives in `remotex-cef`, so the browser process
//! and this one cannot come to disagree about either. What is here is what only a
//! subprocess has: the seatbelt, and the framework loaded from three directories
//! up rather than from `../Frameworks`.

fn main() {
    remotex_cef::run_helper_process();
}
