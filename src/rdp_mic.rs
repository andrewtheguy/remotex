//! The browser's microphone, into FreeRDP's MS-RDPEAI endpoint.
//!
//! The mirror of [`crate::rdp_camera`], and just as small for the same reason:
//! the wrapper implements the channel — version, formats, open, the data PDUs —
//! and what is left here is the adapter between its two surfaces and
//! [`MicBridge`]'s two surfaces. Samples go straight through in both name and
//! fact: the browser captured the PCM, the Windows host reads it, and this file
//! never looks inside a buffer.
//!
//! ## The one rule
//!
//! **The event callbacks run on FreeRDP's threads** and must not block —
//! [`MicBridge::signal`] is an unbounded send, which cannot. The control
//! direction has no such constraint: `sample` is called from the mic socket's
//! task, and the wrapper writes without waiting on the session.

use std::sync::Arc;

use log::info;

use crate::mic::{MicBridge, MicControl, MicFormat, MicSignal};

/// The `Connect::microphone` for a target, or `None` for one that asked for none.
///
/// The `Option` is already the answer, exactly as for the camera:
/// [`crate::session`] builds a [`MicBridge`] only for a target that opted in, so
/// a `None` here is a session that never registers `audin` and never offers the
/// server a microphone at all.
pub fn connect(bridge: Option<Arc<MicBridge>>) -> Option<freerdp::Microphone> {
    let bridge = bridge?;
    let mic = freerdp::Microphone::new(Arc::new(Events { bridge: Arc::clone(&bridge) }));
    bridge.set_control(Arc::new(Control { mic: mic.clone() }));
    Some(mic)
}

/// The wrapper's events, which are this gateway's signals with a shape on them.
struct Events {
    bridge: Arc<MicBridge>,
}

impl freerdp::MicEvents for Events {
    /// The host opened the audio-input channel: microphone redirection is on
    /// offer. A log line rather than a signal — the browser has nothing to do
    /// with it, but "the host never negotiated" is the line that answers why an
    /// enabled microphone stayed silent, and policy can turn the channel off
    /// server-side.
    fn negotiated(&self, version: u32) {
        info!("mic: the host negotiated MS-RDPEAI version {version}");
    }

    fn opened(&self, format: freerdp::MicFormat) {
        self.bridge.signal(MicSignal::Open(MicFormat {
            channels: format.channels,
            sample_rate: format.sample_rate,
        }));
    }

    fn closed(&self) {
        self.bridge.signal(MicSignal::Close);
    }
}

/// The bridge's control, which is the wrapper's microphone with a shape on it.
struct Control {
    mic: freerdp::Microphone,
}

impl MicControl for Control {
    fn sample(&self, pcm: &[u8]) -> bool {
        self.mic.sample(pcm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::error::TryRecvError;

    /// A target that asked for no microphone gets no channel, rather than a
    /// channel nothing feeds.
    #[test]
    fn no_bridge_means_no_audin() {
        assert!(connect(None).is_none());
        assert!(connect(Some(Arc::new(MicBridge::new()))).is_some());
    }

    /// The wrapper's decisions come out of the bridge as signals, with the format
    /// carried through field for field.
    #[test]
    fn the_hosts_decisions_become_signals() {
        use freerdp::MicEvents as _;

        let bridge = Arc::new(MicBridge::new());
        let mut signals = bridge.subscribe();
        let events = Events { bridge: Arc::clone(&bridge) };

        events.opened(freerdp::MicFormat { channels: 2, sample_rate: 44_100, bits_per_sample: 16 });
        events.closed();

        assert_eq!(
            signals.try_recv(),
            Ok(MicSignal::Open(MicFormat { channels: 2, sample_rate: 44_100 }))
        );
        assert_eq!(signals.try_recv(), Ok(MicSignal::Close));
        assert_eq!(signals.try_recv(), Err(TryRecvError::Empty));
    }

    /// With no host streaming, a sample is refused rather than queued for ever:
    /// the wrapper's endpoint accepts samples only between open and close.
    #[test]
    fn a_sample_before_the_host_opens_is_refused() {
        let bridge = Arc::new(MicBridge::new());
        let mic = connect(Some(Arc::clone(&bridge)));
        assert!(mic.is_some());
        assert!(!bridge.sample(&[0, 0, 0, 0]));
    }
}
