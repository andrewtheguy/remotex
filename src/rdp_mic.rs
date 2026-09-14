//! The browser's microphone, into the RDP client's MS-RDPEAI channel.
//!
//! The microphone's counterpart of [`crate::rdp_camera`]: the channel — the version, the
//! format offered, the host's opens and the packets cut to its size — is [`rdpeai`], run
//! on the RDP client's own thread, and what is left here is the adapter between its two
//! surfaces and [`MicBridge`]'s. The host's decisions become [`MicSignal`]s; the PCM the
//! bridge decodes becomes [`MicrophoneFeed`] calls.
//!
//! [`rdpeai`]: crate::rdp_client::proto::rdpeai

use std::sync::Arc;

use crate::mic::{MicBridge, MicControl, MicFormat, MicSignal};
use crate::rdp_client::proto::rdpeai::Format;
use crate::rdp_client::{MicrophoneFeed, MicrophoneSink};

/// The `Connect::microphone` for a target that carries one: a sink that turns the host's
/// decisions into the bridge's signals.
pub fn sink(bridge: Arc<MicBridge>) -> Box<dyn MicrophoneSink> {
    Box::new(Signals(bridge))
}

/// Make the session's microphone feed the bridge's control, so the mic socket's audio
/// reaches the host.
pub fn attach(bridge: &MicBridge, feed: MicrophoneFeed) {
    bridge.set_control(Arc::new(Control(feed)));
}

/// Every call is an unbounded send, so the client's thread never waits here.
struct Signals(Arc<MicBridge>);

impl MicrophoneSink for Signals {
    // Said in the client's log; the browser has nothing to do until the host records.
    fn negotiated(&self, _version: u32) {}

    fn opened(&self, format: Format) {
        self.0.signal(MicSignal::Open(MicFormat { channels: format.channels, sample_rate: format.sample_rate }));
    }

    fn closed(&self) {
        self.0.signal(MicSignal::Close);
    }
}

struct Control(MicrophoneFeed);

impl MicControl for Control {
    // The host's recording device is the channel's, there for the whole session.
    fn plug(&self) {}

    // The device stays and simply hears nothing more; the next socket starts a fresh
    // stream.
    fn unplug(&self) {
        self.0.flush();
    }

    fn sample(&self, pcm: Vec<u8>) -> bool {
        self.0.sample(pcm)
    }

    fn reset(&self) {
        self.0.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::error::TryRecvError;

    #[test]
    fn the_hosts_decisions_become_signals() {
        let bridge = Arc::new(MicBridge::new());
        let mut signals = bridge.subscribe();
        let sink = sink(Arc::clone(&bridge));
        sink.negotiated(2);
        sink.opened(Format { channels: 1, sample_rate: 16_000 });
        sink.closed();
        assert_eq!(
            signals.try_recv(),
            Ok(MicSignal::Open(MicFormat { channels: 1, sample_rate: 16_000 }))
        );
        assert_eq!(signals.try_recv(), Ok(MicSignal::Close));
        assert_eq!(signals.try_recv(), Err(TryRecvError::Empty));
    }
}
