//! The protocol-engine seam (docs/architecture.md).
//!
//! Every engine exposes the same contract: an async
//! `run(config, input_rx, frame_tx)` that connects to the target, consumes
//! browser input as [`ClientMsg`], emits the uniform [`ServerMsg`] stream
//! (resize, tiles, error), and returns when the session ends. That shared
//! signature *is* the `Session` seam — with two engines and no dynamic
//! dispatch, a `match` here beats a trait object (which IronRDP's non-`Send`
//! futures could not implement cleanly anyway).

use log::warn;
use tokio::sync::mpsc;

use crate::config::{Protocol, TargetConfig};
use crate::protocol::{ClientMsg, ServerMsg};
use crate::{rdp, vnc};

/// Spawn the protocol engine for `target` on its own thread.
///
/// The engine runs on a dedicated thread with a current-thread runtime:
/// IronRDP's `read_pdu` future is not `Send`-general (it holds a
/// `&dyn PduHint` across await), so it can't live on the shared multi-thread
/// runtime via `tokio::spawn`; a current-thread runtime imposes no `Send`
/// bound. The VNC engine doesn't need this, but sharing the one spawn path
/// keeps the seam uniform. The session ends when the browser goes away
/// (`input_rx` closed) or the remote host disconnects.
///
/// Scalability: this costs one OS thread + one current-thread runtime per
/// connection — fine here, since multi session is permanently out of scope
/// (single user, one active session at a time; see CLAUDE.md).
pub fn spawn(
    target: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                warn!("session: failed to build engine runtime: {e}");
                return;
            }
        };
        match target.protocol {
            Protocol::Rdp => rt.block_on(rdp::run(target, input_rx, frame_tx)),
            Protocol::Vnc => rt.block_on(vnc::run(target, input_rx, frame_tx)),
        }
    });
}
