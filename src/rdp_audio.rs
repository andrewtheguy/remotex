//! The remote's sound, from FreeRDP's `rdpsnd` device into this gateway's queue.
//!
//! Almost nothing, and that is the news. The engine this replaced carried 800
//! lines here — a hand-written MS-RDPEA state machine over two virtual channels,
//! because IronRDP had no audio client at all. FreeRDP has `rdpsnd`, and the
//! [`freerdp`] wrapper is registered as its output *device*, so what is left is
//! the adapter between that device's callbacks and [`AudioBridge`].
//!
//! Everything downstream is protocol-agnostic and unchanged: the queue, the
//! Opus and passthrough encoders, `/ws/audio`, and the claim the socket is bound
//! to. Nothing in [`crate::audio`] knows this is RDP, and it should not start.
//!
//! ## The one rule
//!
//! **These callbacks run on FreeRDP's thread**, in place, while the desktop is
//! being decoded. That is deliberate — it is what keeps a wave buffer from
//! queueing behind a backlog of paint rectangles — and it means nothing here may
//! block. It does not: [`AudioBridge::wave`] is a broadcast send that drops its
//! oldest buffer rather than waiting, which is the same bargain the damage path
//! already makes and for the same reason. A dropped tile is repainted; a wave
//! buffer that waits is a stall in the desktop.

use std::sync::Arc;

use freerdp::{Audio, AudioFormat, AudioSink};
use log::{debug, info, warn};

use crate::audio::{AudioBridge, PCM_CD_QUALITY};

/// What this gateway asks an RDP server to redirect, in the wrapper's terms.
///
/// The same numbers as [`PCM_CD_QUALITY`], in a different crate's type, and
/// `the_two_formats_are_the_same_sound` below is what keeps them that way. They
/// are separate types on purpose: [`crate::audio`] describes what a *browser*
/// will be sent and knows nothing about RDP, while this one is what the
/// **server** was asked for. That they currently agree is a fact about this
/// engine, not a property of either.
const WANTED: AudioFormat = AudioFormat {
    channels: PCM_CD_QUALITY.channels,
    sample_rate: PCM_CD_QUALITY.sample_rate,
    bits_per_sample: PCM_CD_QUALITY.bits_per_sample,
};

/// The `Connect::audio` for a target, or `None` for one that asked for no sound.
///
/// The `Option` is already the answer: [`crate::session`] builds an
/// [`AudioBridge`] only for a target that opted in, so a `None` here is a
/// session that never registers `rdpsnd` and never asks the server for audio at
/// all — rather than one that receives it and throws it away.
pub fn connect(bridge: Option<Arc<AudioBridge>>) -> Option<Audio> {
    let bridge = bridge?;
    Some(Audio { format: WANTED, sink: Arc::new(Sink { bridge }) })
}

/// The wrapper's sink, which is this gateway's queue with a shape on it.
struct Sink {
    bridge: Arc<AudioBridge>,
}

impl AudioSink for Sink {
    /// The channel came up and the server announced what it can send.
    ///
    /// This is the moment the format is *published*, not the first wave buffer:
    /// it fires whether or not the remote ever makes a noise, so it is what
    /// separates "set up, and the desktop is quiet" from "never set up". The
    /// distinction is the whole reason [`AudioBridge::negotiated_format`] exists
    /// — it is read at attach time to say which of the two a silent stream is.
    fn negotiated(&self, accepted: usize) {
        if accepted == 0 {
            // Not fatal and not recoverable: the socket still opens and still
            // carries silence, because a client that asked for audio should not
            // lose its stream over the far end's format list. What it cannot do
            // is play anything, so the reason belongs in the log rather than
            // nowhere.
            warn!(
                "audio: the server offers no {} Hz {}-bit stereo PCM, so this session will be \
                 silent — MS-RDPEA requires that format of both ends",
                WANTED.sample_rate, WANTED.bits_per_sample
            );
            return;
        }
        self.bridge.publish_format(PCM_CD_QUALITY);
    }

    /// The device opened. Already-known information — the wrapper accepts only
    /// [`WANTED`] — so this is a log line and nothing else.
    fn opened(&self, format: AudioFormat) {
        debug!(
            "audio: rdpsnd playing {} Hz, {} channel(s), {}-bit",
            format.sample_rate, format.channels, format.bits_per_sample
        );
    }

    fn wave(&self, samples: &[u8]) {
        // One copy, and it is the only one on this path: the wrapper hands out a
        // borrow of FreeRDP's own buffer, valid for the duration of the call, and
        // the queue holds `Bytes` that every listener then shares.
        self.bridge.wave(samples.to_vec());
    }

    /// The device closed — the far end stopped redirecting, or is about to
    /// reopen.
    ///
    /// Ends nothing. An open response stays open and fills with silence until
    /// sound comes back, which is what a remote going quiet looks like and must
    /// not cost the listener its stream.
    fn closed(&self) {
        info!("audio: the remote closed its audio channel");
        self.bridge.clear_format();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two format types describe the same sound.
    ///
    /// Worth asserting rather than reading, because nothing else connects them:
    /// the wrapper refuses any format but the one it was asked for, and the
    /// encoders downstream are built from [`PCM_CD_QUALITY`]. If these drifted,
    /// the server would offer a format the device declines and the session would
    /// be silent — with the right numbers in every log line, since each half
    /// would be reporting its own.
    #[test]
    fn the_two_formats_are_the_same_sound() {
        assert_eq!(WANTED.channels, PCM_CD_QUALITY.channels);
        assert_eq!(WANTED.sample_rate, PCM_CD_QUALITY.sample_rate);
        assert_eq!(WANTED.bits_per_sample, PCM_CD_QUALITY.bits_per_sample);
        assert_eq!(WANTED.block_align(), PCM_CD_QUALITY.block_align());
        assert_eq!(WANTED.byte_rate(), PCM_CD_QUALITY.byte_rate());
    }

    /// A target that asked for no sound gets no channel, rather than a channel
    /// nothing reads.
    #[test]
    fn no_bridge_means_no_rdpsnd() {
        assert!(connect(None).is_none());
        assert!(connect(Some(Arc::new(AudioBridge::new()))).is_some());
    }

    /// The queue only reports a negotiated format once the server has one to
    /// offer — and a server with nothing acceptable leaves it unset, which is
    /// what the attach-time log reads.
    #[test]
    fn a_server_with_no_acceptable_format_publishes_none() {
        let bridge = Arc::new(AudioBridge::new());
        let sink = Sink { bridge: Arc::clone(&bridge) };
        sink.negotiated(0);
        assert_eq!(bridge.negotiated_format(), None);
        sink.negotiated(1);
        assert_eq!(bridge.negotiated_format(), Some(PCM_CD_QUALITY));
        sink.closed();
        assert_eq!(bridge.negotiated_format(), None);
    }

    /// A wave buffer reaches a listener whole.
    ///
    /// Through the passthrough codec, which is the only one that lets a test
    /// compare bytes: Opus would answer with a packet, and what is being checked
    /// here is the copy out of FreeRDP's borrowed buffer — the one place on this
    /// path where the samples could be truncated or aliased.
    #[tokio::test]
    async fn a_wave_buffer_reaches_a_listener() {
        use futures_util::StreamExt;

        let wave: Vec<u8> = (0..64u8).collect();
        let bridge = Arc::new(AudioBridge::new());
        let listener = bridge.take_listener();
        let sink = Sink { bridge: Arc::clone(&bridge) };
        sink.negotiated(1);

        let encoded = listener
            .into_packets(PCM_CD_QUALITY, crate::config::AudioCodec::Pcm)
            .expect("the negotiated format must be carryable as passthrough PCM");
        let mut packets = Box::pin(encoded.packets);

        sink.wave(&wave);
        let batch = tokio::time::timeout(std::time::Duration::from_secs(5), packets.next())
            .await
            .expect("timed out waiting for the wave buffer")
            .expect("the stream ended instead of carrying the buffer");
        assert_eq!(batch.len(), 1, "passthrough is one packet per buffer");
        assert_eq!(batch[0].as_ref(), wave.as_slice());
    }
}
