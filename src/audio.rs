//! Bounded handoff from engine-produced PCM to one live encoding listener.
//!
//! The broadcast queue never blocks the engine read loop and drops its oldest
//! complete buffer when a listener falls behind. Encoding happens only while a
//! client is attached; quiet remotes emit nothing. What a buffer is turned into
//! is the target's ([`AudioCodec`]) — Opus packets, or the same bytes back out
//! again — and everything else here is the same either way.

use std::sync::Mutex;

use bytes::Bytes;
use futures_util::Stream;
use log::{debug, info, warn};
use tokio::sync::{broadcast, watch};

use crate::config::AudioCodec;
use crate::opus_stream::{OpusStream, OPUS_CODEC};
use crate::pcm_stream::{PcmStream, PCM_CODEC};

/// Complete PCM wave buffers retained before the oldest one is dropped.
///
/// Sixteen is about three seconds at the tested Windows host's ~186 ms buffers.
/// The client discards backlog past its own 1.5-second ceiling on arrival, so
/// retaining much more than that ceiling cannot be heard — it can only be
/// delivered across a link that is already behind and then thrown away. The
/// margin above the ceiling is for a remote that sends smaller buffers, where
/// sixteen of them absorb a shorter stall.
pub const AUDIO_QUEUE_DEPTH: usize = 16;

/// Linear PCM parameters: the only kind of audio this path carries.
///
/// PCM because it is the one [RDPSND audio format](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/30a6cc00-31c4-4e15-9aa4-95a5c5074697)
/// clients and servers are both required to support, so accepting a compressed
/// RDP format would make this depend on what a particular Windows version happens
/// to offer. What the *gateway* then sends a browser is a separate question, and
/// the target's [`AudioCodec`] answers it: Opus for anything that leaves the
/// building, or these same bytes for a link fast enough not to care.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
}

impl PcmFormat {
    /// Bytes in one sample across every channel — RDPSND's `nBlockAlign`.
    pub const fn block_align(self) -> u16 {
        self.channels * (self.bits_per_sample / 8)
    }

    /// Bytes a second of this format occupies — RDPSND's `nAvgBytesPerSec`, and
    /// the number Opus exists to shrink: 176 400 for the format below, which is
    /// 1.41 Mbit/s and exactly what `audio_codec = "pcm"` puts on the wire.
    pub const fn byte_rate(self) -> u32 {
        self.sample_rate * self.block_align() as u32
    }
}

/// The single format this gateway asks an RDP server to redirect, and therefore
/// the only one a wave buffer can be in.
///
/// One format rather than a list, and that is load-bearing beyond simplicity —
/// see [`crate::rdp_audio`]: RDPSND identifies a buffer's format by an index,
/// and with one advertised format the index can only mean this.
pub const PCM_CD_QUALITY: PcmFormat = PcmFormat {
    channels: 2,
    sample_rate: 44_100,
    bits_per_sample: 16,
};

/// The seam between an engine receiving PCM and the session encoding it.
#[derive(Debug)]
pub struct AudioBridge {
    /// [`Bytes`] rather than `Vec<u8>`: a broadcast receiver clones the slot it
    /// reads, and cloning `Bytes` bumps a refcount where cloning a `Vec` copies
    /// the whole wave buffer per listener per buffer.
    waves: broadcast::Sender<Bytes>,
    /// The negotiated format, `None` until the engine's audio channel has come up
    /// and again once it closes. Nothing depends on it — the response opens either
    /// way — so it is a record rather than a gate: what the log says about whether
    /// the remote's audio is set up, and what an indicator would read one day.
    format: watch::Sender<Option<PcmFormat>>,
    /// Which MS-RDPEA transport is filling this queue — see
    /// [`Self::claim_transport`].
    transport: Mutex<Option<&'static str>>,
}

impl AudioBridge {
    pub fn new() -> Self {
        Self {
            waves: broadcast::channel(AUDIO_QUEUE_DEPTH).0,
            format: watch::Sender::new(None),
            transport: Mutex::new(None),
        }
    }

    /// Claim this bridge for one transport, refusing a second claimant.
    ///
    /// MS-RDPEA carries audio over *either* the static `rdpsnd` channel or the
    /// dynamic `AUDIO_PLAYBACK_DVC`, and remotex registers both because which one
    /// a server uses is the server's choice (see [`crate::rdp_audio`]). Nothing in
    /// the protocol says a server may drive both at once, and none does — but if
    /// one ever did, both would push buffers into this one queue and the result
    /// would be interleaved noise with no error anywhere to explain it. First
    /// claim wins, so that misbehaviour costs a log line instead of a mystery.
    pub fn claim_transport(&self, name: &'static str) -> bool {
        let mut held = self.transport.lock().unwrap();
        match *held {
            Some(owner) if owner != name => {
                warn!("audio: ignoring {name}, this session's audio already arrives over {owner}");
                false
            }
            _ => {
                *held = Some(name);
                true
            }
        }
    }

    /// Release the transport claim: that channel closed, and another may take it.
    pub fn release_transport(&self, name: &'static str) {
        let mut held = self.transport.lock().unwrap();
        if *held == Some(name) {
            *held = None;
        }
    }

    /// Announce the negotiated format — the endpoint's cue that audio is
    /// actually set up rather than merely configured.
    pub fn publish_format(&self, format: PcmFormat) {
        // `send_replace`, not `send`: a format announced while nobody is
        // listening is exactly the normal case, and `send` treats no receivers
        // as an error.
        if self.format.send_replace(Some(format)) != Some(format) {
            info!(
                "audio: negotiated {} Hz, {} channel(s), {}-bit PCM",
                format.sample_rate, format.channels, format.bits_per_sample
            );
        }
    }

    /// The format the remote's audio channel has agreed to, or `None` while it has
    /// not come up. Nothing branches on it — see the field's own note — so this
    /// exists for the log line at attach time.
    pub fn negotiated_format(&self) -> Option<PcmFormat> {
        *self.format.borrow()
    }

    /// Forget the negotiated format: the far side closed the audio channel.
    ///
    /// Ends nothing. An open response stays open and fills with silence until the
    /// channel comes back — which is what a remote going quiet for a while looks
    /// like, and it must not cost the listener its stream.
    pub fn clear_format(&self) {
        self.format.send_replace(None);
    }

    /// Queue one buffer. Never blocks and never fails visibly: a full queue
    /// drops its oldest buffer and no listener drops this one.
    ///
    /// Takes the `Vec` an engine already owns; wrapping it in [`Bytes`] here is
    /// free, and everything downstream shares it instead of copying it.
    pub fn wave(&self, samples: Vec<u8>) {
        let _ = self.waves.send(Bytes::from(samples));
    }

    /// How many listeners are reading this queue.
    ///
    /// Exists for [`crate::session`]'s tests, which assert on it rather than on a
    /// socket going quiet: stopping audio aborts a task, so the observable fact is
    /// that its subscription went away.
    #[cfg(test)]
    pub fn listener_count(&self) -> usize {
        self.waves.receiver_count()
    }

    /// Subscribe from the live edge. The attachment owns listener cancellation.
    pub fn take_listener(&self) -> AudioListener {
        AudioListener {
            waves: self.waves.subscribe(),
            format: self.format.subscribe(),
        }
    }
}

impl Default for AudioBridge {
    fn default() -> Self {
        Self::new()
    }
}

/// One client's read side of an [`AudioBridge`].
pub struct AudioListener {
    waves: broadcast::Receiver<Bytes>,
    format: watch::Receiver<Option<PcmFormat>>,
}

impl AudioListener {
    /// The format the remote's audio channel has agreed to, or `None` while it has
    /// not come up (or has closed again).
    ///
    /// Read, never waited for: nothing about the response depends on the answer,
    /// because the gateway advertises exactly one format and so the header is
    /// writable before any negotiation (see [`crate::rdp_audio`]). It is worth a log
    /// line, and it is where a future "the remote is quiet" indicator would come
    /// from.
    pub fn negotiated_format(&self) -> Option<PcmFormat> {
        *self.format.borrow()
    }

    /// Return everything the client has to be told, and a live-only stream of
    /// packet batches. The stream ends with the bridge or consumer; a format this
    /// codec cannot carry fails here.
    pub fn into_packets(
        self,
        format: PcmFormat,
        codec: AudioCodec,
    ) -> Result<EncodedAudio<impl Stream<Item = Vec<Bytes>>>, anyhow::Error> {
        struct State {
            encoder: PacketEncoder,
            waves: broadcast::Receiver<Bytes>,
            /// When this listener attached, which only the diagnostic line below
            /// reads: frames encoded against time elapsed is how a stream that is
            /// drifting from real time shows itself.
            started: tokio::time::Instant,
        }

        let (encoder, head) = PacketEncoder::new(format, codec)
            .map_err(|e| anyhow::anyhow!("cannot carry {format:?} as {}: {e}", codec.name()))?;
        let name = encoder.codec_name();
        let sample_rate = encoder.sample_rate();
        let packet_frames = encoder.packet_frames();

        let state = State {
            encoder,
            waves: self.waves,
            started: tokio::time::Instant::now(),
        };
        let stream = futures_util::stream::unfold(state, |mut state| async move {
            loop {
                let samples = match state.waves.recv().await {
                    Ok(samples) => samples,
                    // Old audio was dropped while this consumer was behind.
                    // Skipping forward is the point: the alternative is a delay that
                    // never comes back. The encoder carries on — packets on both
                    // codecs are independently decodable, so a gap is a gap in the
                    // sound rather than a broken stream.
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        debug!("audio: listener fell behind, {dropped} buffer(s) dropped");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                };
                // Four numbers, and between them they say whether this gateway is
                // adding delay: the buffer size and the queue depth locate a backlog,
                // and encoded frames against elapsed time say whether the stream is
                // running ahead of real time or behind it. It was written to answer a
                // report of a couple of seconds of lag, and it answered it — no
                // backlog, ratio 0.9996 — which is why it stays: the next such report
                // deserves the same evidence rather than a fresh round of theories.
                debug!(
                    "audio: wave {} bytes, {} queued, {} frames encoded in {} ms",
                    samples.len(),
                    state.waves.len(),
                    state.encoder.frames_encoded(),
                    state.started.elapsed().as_millis(),
                );
                match state.encoder.push(&samples) {
                    // Empty when the buffer did not complete a packet — a partial
                    // Opus frame, or a part-frame of PCM. Yielding nothing would
                    // end the stream, so keep reading instead.
                    Ok(packets) if packets.is_empty() => continue,
                    Ok(packets) => return Some((packets, state)),
                    Err(e) => {
                        warn!("audio: the audio encoder failed, ending the stream: {e}");
                        return None;
                    }
                }
            }
        });
        Ok(EncodedAudio {
            codec: name,
            sample_rate,
            channels: format.channels,
            packet_frames,
            head,
            packets: stream,
        })
    }
}

/// A configured stream, and everything the client needs to play it.
pub struct EncodedAudio<S> {
    /// What this is: `opus`, or [`crate::pcm_stream::PCM_CODEC`]. Only the first
    /// of those is a WebCodecs codec string, which is the client's cue that the
    /// second needs no decoder.
    pub codec: &'static str,
    /// The rate the client plays at: [`crate::pcm48::SAMPLE_RATE`] for Opus,
    /// because that is what it was resampled to, and the *remote's* own rate for
    /// passthrough, because nothing resampled it.
    pub sample_rate: u32,
    pub channels: u16,
    /// Samples per packet at [`Self::sample_rate`]: 960 for Opus, and 0 for
    /// passthrough, whose packets are whatever size the remote sent and therefore
    /// describe their own length. The client turns it into a packet duration,
    /// which is the one thing it cannot work out from the fields above.
    pub packet_frames: u32,
    /// `OpusHead`, or empty when there is no decoder to configure.
    pub head: Vec<u8>,
    pub packets: S,
}

/// The two ways a wave buffer reaches the wire, as one thing the pump can hold.
///
/// An enum rather than a trait object: there are two of them, both live in this
/// crate, and the dispatch is one `match` per wave buffer.
enum PacketEncoder {
    /// Boxed because the two are wildly different sizes — an Opus encoder carries
    /// a resampler and its scratch buffers, passthrough carries a `Vec` — and an
    /// enum as big as its largest variant would put half a kilobyte on the stack
    /// to hold neither. One allocation per audio subscription.
    Opus(Box<OpusStream>),
    Pcm(PcmStream),
}

impl PacketEncoder {
    fn new(format: PcmFormat, codec: AudioCodec) -> Result<(Self, Vec<u8>), anyhow::Error> {
        Ok(match codec {
            AudioCodec::Opus => {
                let (stream, head) = OpusStream::new(format)?;
                (Self::Opus(Box::new(stream)), head)
            }
            AudioCodec::Pcm => {
                let (stream, head) = PcmStream::new(format)?;
                (Self::Pcm(stream), head)
            }
        })
    }

    fn codec_name(&self) -> &'static str {
        match self {
            Self::Opus(_) => OPUS_CODEC,
            Self::Pcm(_) => PCM_CODEC,
        }
    }

    fn sample_rate(&self) -> u32 {
        match self {
            Self::Opus(_) => crate::pcm48::SAMPLE_RATE,
            Self::Pcm(stream) => stream.sample_rate(),
        }
    }

    fn packet_frames(&self) -> u32 {
        match self {
            Self::Opus(stream) => stream.packet_frames(),
            Self::Pcm(stream) => stream.packet_frames(),
        }
    }

    fn frames_encoded(&self) -> u64 {
        match self {
            Self::Opus(stream) => stream.frames_encoded(),
            Self::Pcm(stream) => stream.frames_encoded(),
        }
    }

    fn push(&mut self, pcm: &Bytes) -> Result<Vec<Bytes>, anyhow::Error> {
        match self {
            Self::Opus(stream) => stream.push(pcm),
            Self::Pcm(stream) => stream.push(pcm),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::StreamExt as _;

    use super::*;

    /// What this format costs on the wire, which is the whole reason the default
    /// is Opus and not this — and, at `audio_codec = "pcm"`, exactly the bill.
    #[test]
    fn the_negotiated_format_is_cd_quality_pcm() {
        assert_eq!(PCM_CD_QUALITY.block_align(), 4);
        assert_eq!(PCM_CD_QUALITY.byte_rate(), 176_400);
    }

    /// Eight-bit mono is not a format this path negotiates, but the derived
    /// fields must not be hard-coded for the one that is.
    #[test]
    fn the_derived_rates_follow_the_format() {
        let telephone = PcmFormat {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 8,
        };
        assert_eq!(telephone.block_align(), 1);
        assert_eq!(telephone.byte_rate(), 8_000);
    }

    /// One Opus packet's worth of silent PCM at the negotiated format.
    ///
    /// Every test here has to hand over whole frames: a buffer too small to
    /// complete one is held by the encoder, so a stream fed scraps yields nothing
    /// and a `next()` on it would wait forever rather than fail.
    fn one_frame_of_pcm() -> Vec<u8> {
        let frames = crate::pcm48::group_frames_in(PCM_CD_QUALITY.sample_rate)
            .expect("the negotiated rate makes whole groups");
        vec![0u8; frames * usize::from(PCM_CD_QUALITY.block_align())]
    }

    /// The packet stream alone, with the header asserted to be one — every test
    /// here is about the queue rather than the encoder, and none of them should
    /// pass if a listener came back without a way to decode it.
    fn packets_of(listener: AudioListener) -> impl Stream<Item = Vec<Bytes>> {
        let encoded = listener
            .into_packets(PCM_CD_QUALITY, AudioCodec::default())
            .expect("the negotiated format must be encodable");
        assert_eq!(&encoded.head[0..8], b"OpusHead");
        encoded.packets
    }

    async fn next(stream: &mut (impl Stream<Item = Vec<Bytes>> + Unpin)) -> Option<Vec<Bytes>> {
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("timed out waiting for the audio stream")
    }

    #[tokio::test]
    async fn a_listener_gets_only_what_arrives_after_it_attached() {
        let bridge = AudioBridge::new();
        bridge.publish_format(PCM_CD_QUALITY);
        // Discarded: nobody was listening, and there is no history to replay.
        bridge.wave(one_frame_of_pcm());

        let listener = bridge.take_listener();
        assert_eq!(
            listener.negotiated_format(),
            Some(PCM_CD_QUALITY),
            "the format was published before the listener attached"
        );
        let mut stream = Box::pin(packets_of(listener));

        bridge.wave(one_frame_of_pcm());
        let packets = next(&mut stream).await.expect("a packet for the buffer");
        assert_eq!(packets.len(), 1, "one frame in, one packet out");
    }

    /// The watch, not a snapshot: a listener attached before the channel came up
    /// still sees it, which is what a log line at attach time would otherwise miss.
    #[tokio::test]
    async fn a_format_published_after_the_listener_attached_still_reaches_it() {
        let bridge = AudioBridge::new();
        let listener = bridge.take_listener();
        assert_eq!(listener.negotiated_format(), None);
        bridge.publish_format(PCM_CD_QUALITY);
        assert_eq!(listener.negotiated_format(), Some(PCM_CD_QUALITY));
        // Closing the channel is not the end of anything; it only stops being a
        // record of a live negotiation.
        bridge.clear_format();
        assert_eq!(listener.negotiated_format(), None);
    }

    /// A quiet remote now costs **nothing**, which is the keepalive's deletion
    /// stated as a property rather than an absence. Paused time auto-advances
    /// whenever the runtime has nothing to do, so this would see any timer that
    /// still fired: three seconds pass with no buffer sent, and the stream yields
    /// nothing at all rather than filler.
    #[tokio::test(start_paused = true)]
    async fn a_quiet_remote_produces_no_packets_at_all() {
        let bridge = AudioBridge::new();
        let listener = bridge.take_listener();
        assert_eq!(listener.negotiated_format(), None);
        let mut stream = Box::pin(packets_of(listener));

        assert!(
            tokio::time::timeout(Duration::from_secs(3), stream.next())
                .await
                .is_err(),
            "nothing was sent, so nothing should have been encoded"
        );

        // And the stream is still live: silence was a gap, not an end.
        bridge.wave(one_frame_of_pcm());
        assert_eq!(next(&mut stream).await.expect("audio after the gap").len(), 1);
    }

    /// The remote going quiet and coming back, which is the sequence this exists
    /// for: one stream throughout, sound resuming with no help from the client.
    #[tokio::test(start_paused = true)]
    async fn audio_that_stops_and_starts_again_stays_one_stream() {
        let bridge = AudioBridge::new();
        bridge.publish_format(PCM_CD_QUALITY);
        let listener = bridge.take_listener();
        let mut stream = Box::pin(packets_of(listener));

        bridge.wave(one_frame_of_pcm());
        assert_eq!(next(&mut stream).await.unwrap().len(), 1);

        // The channel closes, as a real host's does when nothing is playing, and it
        // comes back — without the listener having reattached.
        bridge.clear_format();
        tokio::time::advance(Duration::from_secs(30)).await;
        bridge.publish_format(PCM_CD_QUALITY);
        bridge.wave(one_frame_of_pcm());
        assert_eq!(
            next(&mut stream).await.unwrap().len(),
            1,
            "the same stream should carry the audio that came back"
        );
    }

    #[tokio::test]
    async fn dropping_the_bridge_ends_the_stream() {
        let bridge = AudioBridge::new();
        let listener = bridge.take_listener();
        let mut stream = Box::pin(packets_of(listener));

        drop(bridge);
        assert!(next(&mut stream).await.is_none());
    }

    /// A second listener does **not** end the first, and that is a property this
    /// module deliberately stopped having: a `oneshot` used to live in the bridge so
    /// a takeover could cut the previous browser's stream. Ending a listener is now
    /// [`crate::session`]'s job — it owns the task doing the reading — which is where
    /// `a_takeover_ends_the_audio_while_the_engine_carries_on` and the enable/disable
    /// tests moved to. What is left here is the queue's own promise: subscribing
    /// takes nothing away from anyone.
    #[tokio::test]
    async fn a_second_listener_leaves_the_first_alone() {
        let bridge = AudioBridge::new();
        let mut first = Box::pin(packets_of(bridge.take_listener()));
        let mut second = Box::pin(packets_of(bridge.take_listener()));

        bridge.wave(one_frame_of_pcm());
        assert_eq!(next(&mut first).await.unwrap().len(), 1);
        assert_eq!(next(&mut second).await.unwrap().len(), 1);
    }

    /// The header the two options hand back, side by side. Everything else in
    /// this module is codec-blind, so this is the one place the difference is
    /// visible: passthrough announces the remote's own rate and no decoder
    /// configuration at all, which is what tells a client to skip WebCodecs.
    #[tokio::test]
    async fn the_two_options_describe_themselves_differently() {
        let bridge = AudioBridge::new();
        let opus = bridge
            .take_listener()
            .into_packets(PCM_CD_QUALITY, AudioCodec::Opus)
            .expect("opus");
        assert_eq!(opus.codec, "opus");
        assert_eq!(opus.sample_rate, crate::pcm48::SAMPLE_RATE);
        assert_eq!(opus.packet_frames, 960);
        assert_eq!(&opus.head[0..8], b"OpusHead");

        let pcm = bridge
            .take_listener()
            .into_packets(PCM_CD_QUALITY, AudioCodec::Pcm)
            .expect("passthrough");
        assert_eq!(pcm.codec, "pcm-s16le");
        assert_eq!(pcm.sample_rate, PCM_CD_QUALITY.sample_rate, "not resampled");
        assert_eq!(pcm.packet_frames, 0, "each packet's length is its own");
        assert!(pcm.head.is_empty(), "there is nothing to configure");

        // And the bytes really are the bytes: one buffer in, the same buffer out.
        let mut stream = Box::pin(pcm.packets);
        let wave: Vec<u8> = (0..64u8).collect();
        bridge.wave(wave.clone());
        assert_eq!(next(&mut stream).await.expect("a packet"), vec![wave]);
    }

    /// The backpressure rule: the producer is never held up, and what gives way
    /// is old audio. A queue that blocked here would be blocking the RDP read
    /// loop, and one that grew would be building a permanent delay.
    ///
    /// Read straight off the queue rather than through [`AudioListener::into_packets`],
    /// for two reasons: what is under test is the queue's overflow rule, and the
    /// encoder in between makes buffers unidentifiable, so there would be no way
    /// to say *which* audio survived. Draining the stream instead is also not
    /// available — ending the bridge to terminate the drain ends the response
    /// immediately, by design.
    #[test]
    fn an_unread_queue_drops_its_oldest_buffers_instead_of_blocking() {
        let bridge = AudioBridge::new();
        let mut listener = bridge.take_listener();

        // Twice the depth, none of it read: every one of these returns at once.
        let sent = AUDIO_QUEUE_DEPTH * 2;
        for i in 0..sent {
            bridge.wave(vec![i as u8]);
        }

        let mut lagged = None;
        let mut survived: Vec<Bytes> = Vec::new();
        loop {
            match listener.waves.try_recv() {
                Ok(buffer) => survived.push(buffer),
                // Reported once, before the oldest surviving buffer.
                Err(broadcast::error::TryRecvError::Lagged(dropped)) => lagged = Some(dropped),
                Err(_) => break,
            }
        }

        assert_eq!(
            lagged,
            Some((sent - AUDIO_QUEUE_DEPTH) as u64),
            "the reader should be told exactly how much it missed"
        );
        assert_eq!(survived.len(), AUDIO_QUEUE_DEPTH, "the queue holds its depth");
        assert_eq!(
            survived[0],
            vec![AUDIO_QUEUE_DEPTH as u8],
            "it resumes at the oldest buffer still held, not the first one sent"
        );
    }
}
