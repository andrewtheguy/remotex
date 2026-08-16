//! The seam between the one microphone socket and an engine redirecting a mic.
//!
//! The camera's twin, one direction over: like the camera the media is the
//! browser's and funnels in to the remote, but where the camera carries encoded
//! H.264 this carries **raw PCM** — the browser's `AudioContext` captured it, the
//! Windows host reads it as its own microphone, and the gateway moves bytes
//! without owning a codec, the same bargain `audio_codec = "pcm"` strikes in the
//! other direction. Outbound go the host's decisions — open, with the format it
//! chose, and close — which are the only things a capturing browser cannot know,
//! since MS-RDPEAI lets the *host* pick the sample rate and channel count.
//!
//! Nothing here names an engine. The engine side registers a [`MicControl`] and
//! publishes [`MicSignal`]s; RDP's adapter is [`crate::rdp_mic`], and it is the
//! only implementor, because MS-RDPEAI is the one microphone channel any of the
//! gateway's protocols has.

use std::sync::{Arc, Mutex};

use log::debug;
use tokio::sync::mpsc;

/// The PCM format the host chose for the microphone, on its way to the mic
/// socket. Always signed 16-bit little-endian, so only the two numbers the
/// browser needs to configure its capture travel here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MicFormat {
    pub channels: u16,
    pub sample_rate: u32,
}

/// A decision of the remote's, on its way to the mic socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicSignal {
    /// The host opened the stream — an application on it opened the microphone —
    /// and will read samples in this format from now on. The browser starts
    /// capturing at `format`'s rate and channel count.
    Open(MicFormat),
    /// The host closed the stream. Capture can stop; another [`MicSignal::Open`]
    /// may follow if the host reopens.
    Close,
}

/// The engine half a mic socket drives: feed it PCM.
///
/// Every method may be called from any thread and must not block — `sample` runs
/// once per captured buffer on the socket task. Unlike the camera there is no
/// plug/unplug: MS-RDPEAI has no device layer, so the microphone is present for
/// as long as the channel is registered and this only ever carries samples.
pub trait MicControl: Send + Sync {
    /// One buffer of PCM, in the format last opened. Returns whether it was sent
    /// — a refusal means no stream is open, and the caller can drop the buffer,
    /// since a microphone sample is worthless late.
    fn sample(&self, pcm: &[u8]) -> bool;
}

/// The seam itself: one per engine that carries a microphone, created by
/// [`crate::session`] alongside the audio and camera bridges and handed to both
/// halves.
///
/// Like the camera bridge there is no queue in it — samples go straight through
/// to the engine's control — so what it holds is just the two registrations: the
/// engine's control and the socket's signal sender, either of which can come and
/// go while the other stays.
///
/// The one piece of state it does keep is whether the host currently has the
/// stream open, and in what format. The host opens the microphone once, during
/// the RDP handshake, when an application on it is capturing — but the mic socket
/// opens seconds later, so without a latch the socket that attaches after the open
/// would wait for ever for an event that already fired. The bridge therefore
/// remembers the last open, under the same lock as the sender, and replays it to a
/// socket that subscribes while the host is open. A [`MicSignal::Close`] clears it,
/// so a socket that attaches after the host closed replays nothing.
#[derive(Default)]
pub struct MicBridge {
    control: Mutex<Option<Arc<dyn MicControl>>>,
    downstream: Mutex<Downstream>,
}

/// The socket-facing half of a [`MicBridge`], behind one lock so that a subscribe
/// and a signal cannot race over the open latch.
#[derive(Default)]
struct Downstream {
    /// The current mic socket's signal sender, if one is attached.
    sender: Option<mpsc::UnboundedSender<MicSignal>>,
    /// The format the host last opened with, or `None` if the host has not opened
    /// or has since closed. Replayed to a socket that subscribes while it is set.
    open: Option<MicFormat>,
}

impl std::fmt::Debug for MicBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicBridge").finish_non_exhaustive()
    }
}

impl MicBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// The engine registers its control as it starts. There is exactly one engine
    /// per bridge, so a second registration is a replaced engine — which cannot
    /// happen today, and would be harmless if it did.
    pub fn set_control(&self, control: Arc<dyn MicControl>) {
        *self.control.lock().expect("mic control lock") = Some(control);
    }

    /// The socket subscribes for the host's decisions, replacing any earlier
    /// subscriber: signals go to the one live mic socket, and a superseded
    /// socket's receiver simply ends. If the host already has the stream open, the
    /// new subscriber is handed that open at once, so a socket attaching after the
    /// host opened still learns to start capturing.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<MicSignal> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut downstream = self.downstream.lock().expect("mic signal lock");
        if let Some(format) = downstream.open {
            // Unbounded send onto a receiver we still hold: cannot fail.
            let _ = tx.send(MicSignal::Open(format));
        }
        downstream.sender = Some(tx);
        rx
    }

    /// Publish one of the host's decisions. Called on an engine thread and never
    /// blocks: the channel is unbounded and these are rare — an open, a close —
    /// not per-buffer traffic. The decision also updates the open latch, so a
    /// socket that subscribes later replays the host's current state.
    pub fn signal(&self, signal: MicSignal) {
        let mut downstream = self.downstream.lock().expect("mic signal lock");
        downstream.open = match signal {
            MicSignal::Open(format) => Some(format),
            MicSignal::Close => None,
        };
        if let Some(tx) = downstream.sender.as_ref()
            && tx.send(signal).is_err()
        {
            debug!("mic: a signal arrived with no socket to hear it: {signal:?}");
        }
    }

    /// Hand one buffer of PCM to the engine, if one is listening.
    pub fn sample(&self, pcm: &[u8]) -> bool {
        match self.control() {
            Some(control) => control.sample(pcm),
            None => false,
        }
    }

    fn control(&self) -> Option<Arc<dyn MicControl>> {
        self.control.lock().expect("mic control lock").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Recorder {
        samples: AtomicUsize,
    }

    impl MicControl for Recorder {
        fn sample(&self, _pcm: &[u8]) -> bool {
            self.samples.fetch_add(1, Ordering::Relaxed);
            true
        }
    }

    const FORMAT: MicFormat = MicFormat { channels: 1, sample_rate: 16_000 };

    /// A bridge with no engine refuses samples rather than losing them silently
    /// mid-claim: the socket sees the refusal and can tell the browser.
    #[test]
    fn a_bridge_with_no_engine_accepts_nothing() {
        let bridge = MicBridge::new();
        assert!(!bridge.sample(&[1, 2, 3, 4]));
    }

    #[test]
    fn the_engines_control_receives_the_sockets_traffic() {
        let bridge = MicBridge::new();
        let recorder = Arc::new(Recorder::default());
        bridge.set_control(recorder.clone());
        assert!(bridge.sample(&[1, 2]));
        assert_eq!(recorder.samples.load(Ordering::Relaxed), 1);
    }

    /// A second subscriber supersedes the first, the same way a second camera or
    /// audio socket does: signals follow the live socket.
    #[tokio::test]
    async fn a_second_subscriber_supersedes_the_first() {
        let bridge = MicBridge::new();
        let mut first = bridge.subscribe();
        let mut second = bridge.subscribe();
        bridge.signal(MicSignal::Open(FORMAT));
        assert_eq!(second.recv().await, Some(MicSignal::Open(FORMAT)));
        assert_eq!(first.recv().await, None);
    }

    /// The host opens the microphone during the RDP handshake, before the mic
    /// socket connects. A socket that subscribes after that open must still learn
    /// it — the open is latched and replayed — or it waits for ever for an event
    /// that already fired.
    #[tokio::test]
    async fn a_late_subscriber_learns_the_open() {
        let bridge = MicBridge::new();
        bridge.signal(MicSignal::Open(FORMAT));
        let mut rx = bridge.subscribe();
        assert_eq!(rx.recv().await, Some(MicSignal::Open(FORMAT)));
    }

    /// A close clears the latch, so a socket that attaches after the host closed
    /// replays nothing rather than a stale open.
    #[tokio::test]
    async fn a_close_clears_the_open_latch() {
        let bridge = MicBridge::new();
        bridge.signal(MicSignal::Open(FORMAT));
        bridge.signal(MicSignal::Close);
        let mut rx = bridge.subscribe();
        assert_eq!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    }
}
