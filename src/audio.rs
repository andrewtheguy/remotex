//! Bounded handoff from engine-produced PCM to one live encoding listener.
//!
//! The broadcast queue never blocks the engine read loop and drops its oldest
//! complete buffer when a listener falls behind. Encoding happens only while a
//! client is attached; quiet remotes emit nothing. A buffer is turned into Opus
//! packets at the rate the target's [`AudioPlan`] holds.
//!
//! The one exception is a High Performance Mac's sound, which every browser is
//! passed as it came ([`crate::vnc_apple_media::PASSED_SOUND`]): the same queue
//! then carries the Mac's own AAC-ELD units ([`AudioBridge::unit`]), and the
//! listener hands them on as packets with no encoder behind them
//! ([`AudioListener::into_passed`]).
//!
//! So is wlshare's sound, which wlshare codes itself as the session was
//! started with: Opus ([`crate::vnc_audio::PASSED_OPUS`]) at the rate the
//! plan's walk arrives at, which the queue hands back to the engine for wlshare
//! to be told ([`AudioBridge::ask_rate`]), or FLAC
//! ([`crate::vnc_audio::PASSED_FLAC`]).
//!
//! A session started with lossless sound is sent it as FLAC:
//! wlshare's FLAC frames passed, or an RDP host's PCM coded as
//! FLAC here ([`AudioListener::into_flac`]). The page decodes either in its
//! WebAssembly module, and there is no rate to walk.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use futures_util::Stream;
use log::{debug, info, warn};
use sound_opus::walk::BitrateWalk;
use tokio::sync::{broadcast, watch};

use crate::config::AudioPlan;
use crate::opus_stream::{OpusStream, OPUS_CODEC};

/// Complete PCM wave buffers retained before the oldest one is dropped.
///
/// Sixteen is about three seconds at the tested Windows host's ~186 ms buffers.
/// The client clamps scheduled lead to 300 ms, so retaining this much does not
/// mean it will play this far behind: a listener that catches up receives the
/// newest sixteen buffers still available, and the client trims any excess lead
/// that survives to playback. A remote that sends smaller buffers gets a shorter
/// retained interval from the same fixed depth.
pub const AUDIO_QUEUE_DEPTH: usize = 16;

/// Linear PCM parameters: the only kind of audio this path carries.
///
/// PCM because it is the one [RDPSND audio format](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/30a6cc00-31c4-4e15-9aa4-95a5c5074697)
/// clients and servers are both required to support, so accepting a compressed
/// RDP format would make this depend on what a particular Windows version happens
/// to offer. What the *gateway* then sends a browser is Opus.
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
    /// 1.41 Mbit/s.
    pub const fn byte_rate(self) -> u32 {
        self.sample_rate * self.block_align() as u32
    }
}

/// The single format this gateway asks an RDP server to redirect, and therefore
/// the only one a wave buffer can be in.
///
/// One format rather than a list, and that is load-bearing beyond simplicity —
/// see MS-RDPEA: RDPSND identifies a buffer's format by an index,
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
    /// The bitrate, in bits per second, a remote that codes its own Opus is to
    /// hold: what the listener's walk last arrived at, `None` until one has
    /// asked. Read by the engine, which is the side that can tell the remote.
    rate: watch::Sender<Option<i32>>,
}

impl AudioBridge {
    pub fn new() -> Self {
        Self {
            waves: broadcast::channel(AUDIO_QUEUE_DEPTH).0,
            format: watch::Sender::new(None),
            rate: watch::Sender::new(None),
        }
    }

    /// Ask the remote to code its Opus at `bps` bits per second: the walk of a
    /// passed stream's listener, whose encoder is the remote's
    /// ([`AudioListener::into_passed`]).
    pub fn ask_rate(&self, bps: i32) {
        self.rate.send_replace(Some(bps));
    }

    /// The rate last asked for, and a wake-up for each one after it. For the
    /// engine that passes the remote's own Opus.
    pub fn asked_rate(&self) -> watch::Receiver<Option<i32>> {
        self.rate.subscribe()
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

    /// Queue one already-encoded unit of a passed stream, for a listener built
    /// with [`AudioListener::into_passed`]. The same queue and the same dropping
    /// as [`Self::wave`]: a unit is one packet, and a listener that lost some is
    /// told so ([`EncodedAudio::gap`]), since the next may decode from them.
    pub fn unit(&self, unit: Vec<u8>) {
        let _ = self.waves.send(Bytes::from(unit));
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
    /// writable before any negotiation. It is worth a log
    /// line, and it is where a future "the remote is quiet" indicator would come
    /// from.
    pub fn negotiated_format(&self) -> Option<PcmFormat> {
        *self.format.borrow()
    }

    /// The next buffer already queued, or `None` if none is.
    ///
    /// Exists for the engines' tests, which assert that what arrived on the wire
    /// reached this queue as samples — the one thing a bridge with no listener
    /// otherwise keeps to itself. A lagged reader panics rather than reading as
    /// an empty queue: a test that overran the queue's depth is a test whose
    /// buffers went missing, and reporting that as "nothing arrived" would make
    /// the wrong assertion fail.
    #[cfg(test)]
    pub fn queued_wave(&mut self) -> Option<Bytes> {
        match self.waves.try_recv() {
            Ok(samples) => Some(samples),
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                None
            }
            Err(broadcast::error::TryRecvError::Lagged(dropped)) => {
                panic!("the test queue dropped {dropped} buffer(s) before this read")
            }
        }
    }

    /// Return everything the client has to be told, and a live-only stream of
    /// packet batches. The stream ends with the bridge or consumer; a format the
    /// encoder cannot carry fails here.
    pub fn into_packets(
        self,
        format: PcmFormat,
        plan: AudioPlan,
    ) -> Result<EncodedAudio<impl Stream<Item = Vec<Bytes>>>, anyhow::Error> {
        struct State {
            encoder: OpusStream,
            waves: broadcast::Receiver<Bytes>,
            /// The adaptive walk's verdict, written by whoever sends the packets;
            /// `None` on a fixed-rate plan. Read here because this is the side
            /// holding the encoder.
            signals: Option<Arc<AudioSignals>>,
            /// The bitrate the encoder is actually at, so the desired rate is
            /// applied once per change rather than re-set per buffer.
            applied_bps: i32,
            /// When this listener attached, which only the diagnostic line below
            /// reads: frames encoded against time elapsed is how a stream that is
            /// drifting from real time shows itself.
            started: tokio::time::Instant,
        }

        let (encoder, head) = OpusStream::new(format, plan.bitrate_bps)
            .with_context(|| format!("cannot carry {format:?} as opus"))?;
        let packet_frames = encoder.packet_frames();
        let signals = plan.adaptive.then(|| Arc::new(AudioSignals::new(plan.bitrate_bps)));

        let state = State {
            encoder,
            waves: self.waves,
            signals: signals.clone(),
            applied_bps: plan.bitrate_bps,
            started: tokio::time::Instant::now(),
        };
        let stream = futures_util::stream::unfold(state, |mut state| async move {
            loop {
                let samples = match state.waves.recv().await {
                    Ok(samples) => samples,
                    // Old audio was dropped while this consumer was behind.
                    // Skipping forward is the point: the alternative is a delay that
                    // never comes back. The encoder carries on — Opus packets are
                    // independently decodable, so a gap is a gap in the sound
                    // rather than a broken stream.
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        debug!("audio: listener fell behind, {dropped} buffer(s) dropped");
                        // The queue only lags when the sender stopped draining it,
                        // which is the link behind by the whole queue's depth — no
                        // send measurement needed to know shedding should be on,
                        // and the walk is told at the sender's next send.
                        if let Some(signals) = &state.signals {
                            signals.note_lag();
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                };
                if let Some(signals) = &state.signals {
                    let desired = signals.desired_bps();
                    if desired != state.applied_bps {
                        match state.encoder.set_bitrate(desired) {
                            Ok(()) => {
                                info!(
                                    "audio: opus bitrate moved to {} kbit/s (from {})",
                                    desired / 1000,
                                    state.applied_bps / 1000
                                );
                                state.applied_bps = desired;
                            }
                            // The stream is still perfectly good at the rate it
                            // already had; losing the ability to adapt is not a
                            // reason to stop the sound.
                            Err(e) => warn!("audio: could not move the opus bitrate: {e:#}"),
                        }
                    }
                    // The catch-up the operator asked for: while the link is
                    // behind, silence is the one content whose loss cannot be
                    // heard, so it is shed *before* the encoder instead of
                    // queued behind everything else. The client simply receives
                    // no packets for a while — exactly what a quiet remote
                    // already produces — and the backlog drains by that much.
                    if signals.behind() && is_silence(&samples) {
                        debug!(
                            "audio: shed {} bytes of silence so the link can catch up",
                            samples.len()
                        );
                        continue;
                    }
                }
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
                    // Empty when the buffer did not complete an Opus frame.
                    // Yielding nothing would end the stream, so keep reading
                    // instead.
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
            codec: OPUS_CODEC,
            sample_rate: crate::pcm48::SAMPLE_RATE,
            channels: format.channels,
            packet_frames,
            head,
            passthrough: false,
            signals,
            gap: Arc::default(),
            packets: stream,
        })
    }
}

impl AudioListener {
    /// Everything the client has to be told about a passed stream, and a live-only
    /// stream of its units, each already a packet: no encoder and no silence to
    /// shed, since what arrives is what the remote coded. Every unit already
    /// queued when one is read goes in the same batch.
    ///
    /// `plan` is a remote's that codes Opus at a rate it can be told
    /// (wlshare's), and `None` for a stream with no rate to move. An adaptive
    /// one gets the walk's signals, as an encoder here would: the sender
    /// reports its sends through them, and what the walk asks for goes to the
    /// remote ([`AudioBridge::ask_rate`]) in place of an encoder.
    ///
    /// Units this listener fell behind are dropped, as PCM is, but they were
    /// coded: the remote's encoder went on from them and the client's decoder
    /// never saw them. [`EncodedAudio::gap`] is raised when that happens, for
    /// the sender to tell the client before the next batch.
    pub fn into_passed(self, format: PassedFormat, plan: Option<AudioPlan>) -> EncodedAudio<impl Stream<Item = Vec<Bytes>>> {
        let signals = plan.filter(|plan| plan.adaptive).map(|plan| Arc::new(AudioSignals::new(plan.bitrate_bps)));
        let gap = Arc::new(AtomicBool::new(false));
        // The flag, and whether units were lost after the batch last yielded,
        // which is a gap before the next one rather than before that one.
        let state = (self.waves, Arc::clone(&gap), false);
        let stream = futures_util::stream::unfold(state, |(mut units, gap, mut lost)| async move {
            if lost {
                gap.store(true, Ordering::Relaxed);
                lost = false;
            }
            loop {
                match units.recv().await {
                    Ok(unit) => {
                        let mut batch = vec![unit];
                        loop {
                            match units.try_recv() {
                                Ok(unit) => batch.push(unit),
                                Err(broadcast::error::TryRecvError::Lagged(dropped)) => {
                                    debug!("audio: listener fell behind, {dropped} passed unit(s) dropped");
                                    lost = true;
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        return Some((batch, (units, gap, lost)));
                    }
                    // As for PCM, skipping forward is the point; unlike PCM,
                    // what was skipped was coded, and the client is told.
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        debug!("audio: listener fell behind, {dropped} passed unit(s) dropped");
                        gap.store(true, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        EncodedAudio {
            codec: format.codec,
            sample_rate: format.sample_rate,
            channels: format.channels,
            packet_frames: format.packet_frames,
            head: format.head.to_vec(),
            passthrough: true,
            signals,
            gap,
            packets: stream,
        }
    }
}

/// What `audioFormat` names lossless sound as. Not a WebCodecs registration's
/// string to this client: the page decodes these frames itself.
pub const FLAC_CODEC: &str = "flac";

impl AudioListener {
    /// Everything the client has to be told about `format` coded as FLAC here, and
    /// a live-only stream of its frames: twenty milliseconds each, a FLAC stream
    /// of its own as wlshare's are, so each decodes alone and a dropped buffer
    /// costs its own samples. No walk and no silence to shed: silence is a few
    /// bytes a frame already. Fails for a format FLAC does not carry.
    pub fn into_flac(self, format: PcmFormat) -> anyhow::Result<EncodedAudio<impl Stream<Item = Vec<Bytes>>>> {
        struct State {
            encoder: sound_flac::Encoder,
            waves: broadcast::Receiver<Bytes>,
            /// Samples not yet a whole block's worth, as the engine gave them.
            pending: Vec<u8>,
            /// One block as libFLAC takes it, a signed integer to a sample.
            block: Vec<i32>,
        }

        anyhow::ensure!(
            format.bits_per_sample == 16 && (1..=2).contains(&format.channels),
            "cannot carry {format:?} as FLAC: the page decodes 16-bit mono or stereo"
        );
        let stream = sound_flac::Stream {
            rate: format.sample_rate,
            channels: format.channels as u8,
            bits: 16,
            block: (format.sample_rate / 50) as u16,
        };
        let encoder = sound_flac::Encoder::new(stream).with_context(|| format!("cannot carry {format:?} as FLAC"))?;
        let block_bytes = stream.samples() * 2;

        let state = State { encoder, waves: self.waves, pending: Vec::new(), block: Vec::new() };
        let packets = futures_util::stream::unfold(state, move |mut state| async move {
            loop {
                let samples = match state.waves.recv().await {
                    Ok(samples) => samples,
                    // As for Opus: skipping forward is the point. What was held
                    // of the block before the gap goes with it, so no frame
                    // joins samples that were never neighbours.
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        debug!("audio: listener fell behind, {dropped} buffer(s) dropped");
                        state.pending.clear();
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                };
                state.pending.extend_from_slice(&samples);
                let whole = state.pending.len() / block_bytes;
                let mut frames = Vec::with_capacity(whole);
                for bytes in state.pending.chunks_exact(block_bytes) {
                    state.block.clear();
                    state.block.extend(bytes.as_chunks::<2>().0.iter().map(|&pair| i32::from(i16::from_le_bytes(pair))));
                    let mut frame = Vec::new();
                    if let Err(e) = state.encoder.encode(&state.block, &mut frame) {
                        warn!("audio: the FLAC encoder failed, ending the stream: {e}");
                        return None;
                    }
                    frames.push(Bytes::from(frame));
                }
                state.pending.drain(..whole * block_bytes);
                // Yielding nothing would end the stream, so keep reading.
                if !frames.is_empty() {
                    return Some((frames, state));
                }
            }
        });
        Ok(EncodedAudio {
            codec: FLAC_CODEC,
            sample_rate: format.sample_rate,
            channels: format.channels,
            packet_frames: u32::from(stream.block),
            head: Vec::new(),
            passthrough: false,
            signals: None,
            gap: Arc::default(),
            packets,
        })
    }
}

/// A stream a remote codes itself and the gateway passes as it came: what the
/// client configures its decoder from, since there is no encoder here to say.
#[derive(Clone, Copy, Debug)]
pub struct PassedFormat {
    /// The WebCodecs codec string naming the stream.
    pub codec: &'static str,
    pub sample_rate: u32,
    pub channels: u16,
    /// Samples per unit at [`Self::sample_rate`].
    pub packet_frames: u32,
    /// The decoder's configuration, as the codec's WebCodecs registration defines
    /// the `description`.
    pub head: &'static [u8],
}

/// Whether a wave buffer is silence: every 16-bit sample within one dither step
/// of zero. Exact zeros are what a paused player and an idle desktop actually
/// produce; the ±2 margin is for a remote that dithers its output.
///
/// The samples are read as the little-endian 16-bit PCM the rest of this path
/// already assumes ([`PCM_CD_QUALITY`], [`crate::pcm48`]).
fn is_silence(pcm: &[u8]) -> bool {
    pcm.as_chunks::<2>().0.iter().all(|&pair| i16::from_le_bytes(pair).unsigned_abs() <= 2)
}

/// A configured stream, and everything the client needs to play it.
pub struct EncodedAudio<S> {
    /// The WebCodecs codec string the client configures its decoder with: `opus`,
    /// or a passed stream's own ([`PassedFormat::codec`]).
    pub codec: &'static str,
    /// The rate the client plays at: [`crate::pcm48::SAMPLE_RATE`], because that
    /// is what the encoder resampled to, or a passed stream's own.
    pub sample_rate: u32,
    pub channels: u16,
    /// Samples per packet at [`Self::sample_rate`]: 960 for Opus. The client turns
    /// it into a packet duration, which is the one thing it cannot work out from
    /// the fields above.
    pub packet_frames: u32,
    /// The decoder's configuration: `OpusHead`, or a passed stream's
    /// [`PassedFormat::head`].
    pub head: Vec<u8>,
    /// Whether the packets are the remote's own, passed as they came, rather
    /// than coded here: what the session card says of the stream.
    pub passthrough: bool,
    /// `Some` exactly when the plan is adaptive: the sender's handle for
    /// reporting how its sends went ([`AudioWalk`] writes through it) and
    /// the encoder's source of truth for the rate it should be at. A passed
    /// stream's encoder is the remote's, which the sender tells itself.
    pub signals: Option<Arc<AudioSignals>>,
    /// Raised by a passed stream when units the remote coded were dropped
    /// before the batch it yields next. The sender takes it down and tells the
    /// client ([`crate::protocol::audio::gap`]) ahead of that batch. Never
    /// raised by a stream coded here, whose loss is before its encoder.
    pub gap: Arc<AtomicBool>,
    pub packets: S,
}

impl<S: Stream<Item = Vec<Bytes>> + Send + 'static> EncodedAudio<S> {
    /// This stream behind one type, so an encoded one and a passed one can be
    /// armed by the same pump.
    pub fn boxed(self) -> EncodedAudio<futures_util::stream::BoxStream<'static, Vec<Bytes>>> {
        EncodedAudio {
            codec: self.codec,
            sample_rate: self.sample_rate,
            channels: self.channels,
            packet_frames: self.packet_frames,
            head: self.head,
            passthrough: self.passthrough,
            signals: self.signals,
            gap: self.gap,
            packets: futures_util::StreamExt::boxed(self.packets),
        }
    }
}

/// The adaptive audio walk's shared state — written on the sending side, where
/// blocking is measurable, and read on the encoding side, which owns the
/// encoder. Two atomics rather than a channel because neither side may wait on
/// the other: the encoder reads whatever verdict is current when a wave buffer
/// arrives. The walk itself is sound-opus's ([`AudioWalk`]); this is only how
/// its word crosses between the two tasks.
#[derive(Debug)]
pub struct AudioSignals {
    /// The bitrate the walk wants the encoder at, in bits per second.
    desired_bps: AtomicI32,
    /// Whether the link is currently behind — the state that sheds silence.
    behind: AtomicBool,
    /// Whether the encoder's queue lagged since the walk last heard: evidence
    /// of the link behind that the sending side did not measure, for the
    /// walk to take up at its next send ([`AudioWalk::sent`]).
    lagged: AtomicBool,
}

impl AudioSignals {
    fn new(bitrate_bps: i32) -> Self {
        Self {
            desired_bps: AtomicI32::new(bitrate_bps),
            behind: AtomicBool::new(false),
            lagged: AtomicBool::new(false),
        }
    }

    pub fn desired_bps(&self) -> i32 {
        self.desired_bps.load(Ordering::Relaxed)
    }

    fn set_desired_bps(&self, bps: i32) {
        self.desired_bps.store(bps, Ordering::Relaxed);
    }

    pub fn behind(&self) -> bool {
        self.behind.load(Ordering::Relaxed)
    }

    fn set_behind(&self, behind: bool) {
        self.behind.store(behind, Ordering::Relaxed);
    }

    /// The encoder's queue lagged: the link is behind from now, whatever the
    /// walk last said, and the walk hears of it at the next send.
    fn note_lag(&self) {
        self.behind.store(true, Ordering::Relaxed);
        self.lagged.store(true, Ordering::Relaxed);
    }

    fn take_lagged(&self) -> bool {
        self.lagged.swap(false, Ordering::Relaxed)
    }
}

/// What the audio link will bear: sound-opus's walk (`sound_opus::walk`), the
/// one both this gateway and wlshare code from, owned by whatever task sends
/// the packets, with its verdicts published through [`AudioSignals`] to the
/// side that holds the encoder — or, for a passed stream, to the engine that
/// tells the remote ([`AudioBridge::ask_rate`]). The signal is how long the
/// send waited: the audio socket's queue is deliberately two deep
/// ([`crate::session::AUDIO_SOCKET_BUFFER`]), so a wait means the browser is
/// not draining sound as fast as the remote produces it. The ceiling is the
/// configured bitrate, the floor the crate's, and the steps, the cooldowns and
/// the hold on a refused rate are the crate's too, so a card and a log line
/// here state what the stream does and nothing adaptive is this gateway's own.
pub struct AudioWalk {
    walk: BitrateWalk,
    signals: Arc<AudioSignals>,
}

impl AudioWalk {
    /// A walk from `ceiling` bits per second, the plan's rate, already
    /// validated by [`crate::config`]; `signals` is the [`EncodedAudio`]'s,
    /// which an adaptive plan has.
    pub fn new(ceiling: i32, signals: Arc<AudioSignals>) -> Self {
        Self { walk: BitrateWalk::new(u32::try_from(ceiling).unwrap_or(0), true), signals }
    }

    /// Record how long one send blocked, publish the verdicts, and return the
    /// new bitrate when it moved (for the caller's log line). Behind is the
    /// walk's word; a queue that lagged on the encoder side is handed to the
    /// walk first, so the link stays behind for the clear second the walk
    /// asks for rather than until this send.
    pub fn sent(&mut self, blocked: Duration, now: Instant) -> Option<i32> {
        if self.signals.take_lagged() {
            self.walk.dropped();
        }
        let moved = self.walk.sent(blocked, now).map(|bps| bps as i32);
        self.signals.set_behind(self.walk.behind());
        if let Some(bps) = moved {
            self.signals.set_desired_bps(bps);
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::StreamExt as _;

    use super::*;

    /// What this format would cost on the wire, which is the whole reason it is
    /// sent as Opus.
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
            .into_packets(PCM_CD_QUALITY, crate::config::AudioPlan::default())
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

    /// The header a listener hands back: Opus at the rate it resampled to, and the
    /// `OpusHead` the client configures its decoder with.
    #[tokio::test]
    async fn the_stream_describes_itself() {
        let bridge = AudioBridge::new();
        let opus = bridge
            .take_listener()
            .into_packets(PCM_CD_QUALITY, AudioPlan::fixed())
            .expect("opus");
        assert_eq!(opus.codec, "opus");
        assert!(!opus.passthrough);
        assert_eq!(opus.sample_rate, crate::pcm48::SAMPLE_RATE);
        assert_eq!(opus.packet_frames, 960);
        assert_eq!(&opus.head[0..8], b"OpusHead");
    }

    /// A passed stream whose remote codes Opus at a rate it can be told walks
    /// as an encoder here does, and what the walk asks for is the remote's to
    /// hear: the queue hands it to whoever passes the stream. A fixed plan has
    /// no walk.
    #[tokio::test]
    async fn a_passed_opus_stream_walks_the_remotes_rate() {
        let bridge = AudioBridge::new();
        let plan = AudioPlan::default();
        let passed = bridge.take_listener().into_passed(crate::vnc_audio::PASSED_OPUS, Some(plan));
        assert_eq!((passed.codec, passed.sample_rate, passed.channels, passed.packet_frames), ("opus", 48_000, 2, 960));
        assert_eq!(&passed.head[0..8], b"OpusHead");
        assert!(passed.passthrough);
        let signals = passed.signals.expect("an adaptive plan walks");
        assert_eq!(signals.desired_bps(), plan.bitrate_bps);

        let mut asked = bridge.asked_rate();
        assert_eq!(*asked.borrow_and_update(), None, "nothing asked until a walk does");
        let mut walk = AudioWalk::new(plan.bitrate_bps, signals);
        let now = Instant::now();
        assert_eq!(walk.sent(SLOW, now), None);
        let lower = walk.sent(SLOW, now).expect("two slow sends give up rate");
        bridge.ask_rate(lower);
        assert!(asked.has_changed().unwrap());
        assert_eq!(*asked.borrow_and_update(), Some(64_000));

        let fixed = bridge.take_listener().into_passed(crate::vnc_audio::PASSED_OPUS, Some(AudioPlan::fixed()));
        assert!(fixed.signals.is_none(), "a fixed rate is named once and never walked");
    }

    /// A passed stream that fell behind the queue says so: the units it lost
    /// were coded, so the batch after them is marked as following a gap, and
    /// one that lost nothing is not.
    #[tokio::test]
    async fn a_passed_stream_that_lost_units_raises_the_gap() {
        let bridge = AudioBridge::new();
        let passed = bridge.take_listener().into_passed(crate::vnc_audio::PASSED_OPUS, None);
        let gap = Arc::clone(&passed.gap);
        let mut stream = Box::pin(passed.packets);

        bridge.unit(vec![1]);
        assert_eq!(next(&mut stream).await.unwrap(), [Bytes::from_static(&[1])]);
        assert!(!gap.load(Ordering::Relaxed), "nothing lost");

        for n in 0..AUDIO_QUEUE_DEPTH as u8 + 3 {
            bridge.unit(vec![n]);
        }
        let batch = next(&mut stream).await.unwrap();
        assert_eq!(batch.len(), AUDIO_QUEUE_DEPTH, "the queue's depth survived");
        assert_eq!(batch[0], Bytes::from_static(&[3]), "the oldest went");
        assert!(gap.load(Ordering::Relaxed), "and the batch follows a gap");
    }

    /// A passed stream is the remote's units as they came: described by the format
    /// it was handed, a packet each, batched with whatever was already queued, and
    /// with no walk behind it.
    #[tokio::test]
    async fn a_passed_stream_hands_on_the_remotes_units_untouched() {
        let bridge = AudioBridge::new();
        let passed = bridge.take_listener().into_passed(crate::vnc_apple_media::PASSED_SOUND, None);
        assert_eq!(
            (passed.codec, passed.sample_rate, passed.channels, passed.packet_frames),
            ("mp4a.40.39", 48_000, 2, 480)
        );
        assert_eq!(passed.head, [0xf8, 0xe6, 0x50, 0x00], "the Mac's AudioSpecificConfig");
        assert!(passed.signals.is_none(), "a passed stream has no bitrate to walk");
        assert!(passed.passthrough);
        let mut stream = Box::pin(passed.packets);

        bridge.unit(vec![1, 2, 3]);
        assert_eq!(next(&mut stream).await.unwrap(), [Bytes::from_static(&[1, 2, 3])]);
        bridge.unit(vec![4]);
        bridge.unit(vec![5, 6]);
        assert_eq!(
            next(&mut stream).await.unwrap(),
            [Bytes::from_static(&[4]), Bytes::from_static(&[5, 6])],
            "units already queued go together"
        );

        drop(bridge);
        assert!(next(&mut stream).await.is_none());
    }

    /// An RDP host's PCM coded as FLAC: described as what it is, one frame for
    /// every twenty milliseconds whatever the buffers' sizes, each decoding on
    /// its own to exactly the samples that went in.
    #[tokio::test]
    async fn pcm_coded_as_flac_is_whole_frames_of_the_same_samples() {
        let bridge = AudioBridge::new();
        let flac = bridge.take_listener().into_flac(PCM_CD_QUALITY).expect("flac");
        assert_eq!(
            (flac.codec, flac.sample_rate, flac.channels, flac.packet_frames),
            ("flac", 44_100, 2, 882)
        );
        assert!(flac.head.is_empty(), "a frame states its own shape");
        assert!(!flac.passthrough, "coded here, from the host's PCM");
        assert!(flac.signals.is_none(), "lossless has no bitrate to walk");
        let mut stream = Box::pin(flac.packets);

        // Two and a half blocks of a tone, in buffers that do not line up with them.
        let block = 882 * 4;
        let pcm: Vec<u8> = one_frame_of_tone().iter().copied().cycle().take(block * 5 / 2).collect();
        bridge.wave(pcm[..100].to_vec());
        bridge.wave(pcm[100..block * 2 + 8].to_vec());
        let frames = next(&mut stream).await.expect("frames");
        assert_eq!(frames.len(), 2, "two whole blocks, and the rest waits");
        bridge.wave(pcm[block * 2 + 8..].to_vec());
        bridge.wave(vec![0u8; block / 2]);
        let mut frames = [frames, next(&mut stream).await.expect("the third block")].concat();
        assert_eq!(frames.len(), 3);

        let mut decoder =
            sound_flac::Decoder::new(sound_flac::Stream { rate: 44_100, channels: 2, bits: 16, block: 882 })
                .unwrap();
        let mut samples = Vec::new();
        let mut decoded = Vec::new();
        for frame in frames.drain(..) {
            assert!(frame.len() <= usize::from(u16::MAX), "a packet's length is a u16 on the wire");
            samples.clear();
            decoder.decode(&frame, &mut samples).expect("a frame decodes on its own");
            decoded.extend(samples.iter().flat_map(|&sample| (sample as i16).to_le_bytes()));
        }
        let mut sent = pcm.clone();
        sent.extend_from_slice(&vec![0u8; block / 2]);
        assert!(decoded == sent[..block * 3], "FLAC is lossless");
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

    // ---- the adaptive walk ---------------------------------------------------

    /// One Opus packet's worth of a full-scale tone, for tests that must not
    /// look like silence.
    fn one_frame_of_tone() -> Vec<u8> {
        let frames = crate::pcm48::group_frames_in(PCM_CD_QUALITY.sample_rate)
            .expect("the negotiated rate makes whole groups");
        let mut pcm = Vec::with_capacity(frames * usize::from(PCM_CD_QUALITY.block_align()));
        for frame in 0..frames {
            let phase = (frame % 100) as f32 / 100.0 * std::f32::consts::TAU;
            let sample = (phase.sin() * 12_000.0) as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        pcm
    }

    /// The adaptive plan every walk test runs on: the default Opus rate, walked.
    fn adaptive_plan() -> AudioPlan {
        AudioPlan { bitrate_bps: 96_000, adaptive: true }
    }

    /// A send that blocked a packet's length: behind, to the walk.
    const SLOW: Duration = Duration::from_millis(20);

    #[test]
    fn silence_is_recognized_and_a_tone_is_not() {
        assert!(is_silence(&one_frame_of_pcm()));
        // A dithered silence still reads as one.
        let mut dithered = one_frame_of_pcm();
        dithered[0] = 2;
        dithered[1] = 0;
        assert!(is_silence(&dithered));
        assert!(!is_silence(&one_frame_of_tone()));
    }

    /// The walk is sound-opus's, and its word reaches the signals: the rate it
    /// arrives at, bounded by the crate's floor and the plan's ceiling, and
    /// whether the link is behind, which rises with the first slow send and
    /// clears after a second of clear ones, well before any rate returns.
    #[test]
    fn the_audio_walk_publishes_through_the_signals() {
        let signals = Arc::new(AudioSignals::new(96_000));
        let mut walk = AudioWalk::new(96_000, Arc::clone(&signals));
        let start = Instant::now();
        let cooldown = Duration::from_secs(2);
        let send = Duration::from_millis(200);

        // One slow send is not a verdict — but it does mark the link behind.
        assert_eq!(walk.sent(SLOW, start), None);
        assert!(signals.behind());
        assert_eq!(walk.sent(SLOW, start), Some(64_000));
        assert_eq!(signals.desired_bps(), 64_000);

        // Sustained blocking bottoms out on the crate's floor, never below.
        let mut at = start;
        for _ in 0..20 {
            at += cooldown;
            walk.sent(SLOW, at);
            walk.sent(SLOW, at);
        }
        assert_eq!(signals.desired_bps(), sound_opus::walk::BITRATE_FLOOR as i32);
        assert!(signals.behind());

        // A second of clear sends clears the behind flag well before any rate returns.
        for _ in 0..6 {
            at += send;
            walk.sent(Duration::ZERO, at);
        }
        assert!(!signals.behind());
        assert_eq!(signals.desired_bps(), 32_000);

        // And a long clear stretch climbs back exactly to the ceiling.
        for _ in 0..20 {
            for _ in 0..20 {
                at += send;
                walk.sent(Duration::ZERO, at);
            }
            at += cooldown;
        }
        assert_eq!(signals.desired_bps(), 96_000);
    }

    /// A lag the encoder side saw, with no slow send to show for it, is
    /// behind until a clear second has followed, not until the next send.
    #[test]
    fn a_lagged_queue_keeps_the_link_behind_for_a_clear_second() {
        let signals = Arc::new(AudioSignals::new(96_000));
        let mut walk = AudioWalk::new(96_000, Arc::clone(&signals));
        let mut at = Instant::now();
        signals.note_lag();
        assert!(signals.behind(), "shed from the moment the lag is seen");
        assert_eq!(walk.sent(Duration::ZERO, at), None);
        assert!(signals.behind(), "one clear send is not relief");
        for _ in 0..6 {
            at += Duration::from_millis(200);
            walk.sent(Duration::ZERO, at);
        }
        assert!(!signals.behind());
        assert_eq!(signals.desired_bps(), 96_000, "a lag moves no rate by itself");
    }

    /// While the link is behind, silent buffers are shed before the encoder —
    /// the stream yields nothing for them — and sound resumes the moment the
    /// source stops being silent. A fixed-rate plan sheds nothing.
    #[tokio::test]
    async fn silence_is_shed_only_while_behind() {
        let bridge = AudioBridge::new();
        let encoded = bridge
            .take_listener()
            .into_packets(PCM_CD_QUALITY, adaptive_plan())
            .expect("an adaptive opus stream");
        let signals = encoded.signals.clone().expect("an adaptive plan has signals");
        let mut stream = Box::pin(encoded.packets);

        // Not behind: silence is content like any other.
        bridge.wave(one_frame_of_pcm());
        assert_eq!(next(&mut stream).await.expect("packets").len(), 1);

        // Behind: the silent buffer is shed — nothing may reach the stream, so
        // the *next* thing it yields is the tone that follows.
        signals.set_behind(true);
        bridge.wave(one_frame_of_pcm());
        bridge.wave(one_frame_of_tone());
        let packets = next(&mut stream).await.expect("packets");
        assert_eq!(packets.len(), 1, "the shed silence must not add a packet");
        let mut decoder =
            opus::Decoder::new(crate::pcm48::SAMPLE_RATE, opus::Channels::Stereo).expect("decoder");
        let mut decoded = vec![0i16; crate::opus_stream::FRAME_FRAMES * 2];
        decoder.decode(&packets[0], &mut decoded, false).expect("decode");
        // The packet that came through is the tone, not the silence: with the
        // silent frame shed, the encoder's first packet carries signal.
        let peak = decoded.iter().map(|s| s.abs()).max().expect("samples");
        assert!(peak > 1_000, "the surviving packet should carry the tone, peak {peak}");
    }

    /// The desired rate published on the signals reaches the live encoder: the
    /// same tone costs visibly fewer bytes per packet after the walk turns the
    /// rate down.
    #[tokio::test]
    async fn a_moved_bitrate_is_applied_to_the_live_encoder() {
        let bridge = AudioBridge::new();
        let encoded = bridge
            .take_listener()
            .into_packets(PCM_CD_QUALITY, adaptive_plan())
            .expect("an adaptive opus stream");
        let signals = encoded.signals.clone().expect("signals");
        let mut stream = Box::pin(encoded.packets);

        let bytes_of = |packets: &[Bytes]| -> usize {
            packets.iter().map(|p| p.len()).sum::<usize>() / packets.len()
        };

        // A few packets at the ceiling to get past the encoder settling.
        let mut at_ceiling = 0;
        for _ in 0..5 {
            bridge.wave(one_frame_of_tone());
            at_ceiling = bytes_of(&next(&mut stream).await.expect("packets"));
        }

        signals.set_desired_bps(16_000);
        // The new rate applies from the buffer after the change is seen.
        let mut at_floor = usize::MAX;
        for _ in 0..5 {
            bridge.wave(one_frame_of_tone());
            at_floor = at_floor.min(bytes_of(&next(&mut stream).await.expect("packets")));
        }
        assert!(
            at_floor * 2 < at_ceiling,
            "16 kbit/s packets should be well under half the 96 kbit/s ones, \
             got {at_floor} against {at_ceiling}"
        );
    }
}
