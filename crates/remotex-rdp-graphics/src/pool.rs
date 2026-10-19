//! Where the decoding that runs side by side runs: rayon's global pool, unless
//! whoever runs this crate names a pool of its own ([`set`]). The gateway names
//! none. The page's module names its own, so that it can stop it: a page's
//! threads are workers, and iOS frees nothing of a worker ended while its thread
//! waits for work, until the tab is closed (`frontend/wasm/egfx`).

use std::sync::{Arc, RwLock};

use rayon::ThreadPool;

static POOL: RwLock<Option<Arc<ThreadPool>>> = RwLock::new(None);

/// Name the pool the decoding runs on from here on, or none for rayon's global
/// one. A pool no longer named is dropped once nothing runs on it, which stops
/// its threads.
pub fn set(pool: Option<ThreadPool>) {
    *POOL.write().expect("no thread panics holding the pool") = pool.map(Arc::new);
}

/// Run `op` in the pool named, where what it runs side by side is spread over
/// that pool's threads, or here, on rayon's global one.
pub(crate) fn install<R: Send>(op: impl FnOnce() -> R + Send) -> R {
    let pool = POOL.read().expect("no thread panics holding the pool").clone();
    match pool {
        Some(pool) => pool.install(op),
        None => op(),
    }
}
