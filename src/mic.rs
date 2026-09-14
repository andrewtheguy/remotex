//! The seam between the one microphone socket and an engine redirecting a microphone.
//!
//! The browser's microphone is speech, wanted now and then, so it travels as cheaply as
//! speech can: the browser encodes it as low-bitrate mono Opus, and the gateway decodes it
//! here into the 16-bit PCM the host asked to record in. The bridge is where that happens,
//! so an engine sees only PCM in its own format. Outbound go the host's decisions — it
//! started recording, in this format, or stopped — which are the only things a capturing
//! browser cannot know, and which say when it is worth encoding at all.
//!
//! Nothing here names an engine. The engine side registers a [`MicControl`] and publishes
//! [`MicSignal`]s; RDP's adapter is [`crate::rdp_mic`], over MS-RDPEAI, and a generic VNC
//! target's is [`crate::vnc_mic`], over wlshare's microphone extension.

use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use log::{debug, warn};
use rubato::audioadapter_buffers::direct::{SequentialSliceOfSlices, SequentialSliceOfVecs};
use rubato::{Fft, FixedSync, Resampler};
use tokio::sync::mpsc;

/// The rate the browser's Opus is decoded at: Opus's own.
const DECODE_RATE: u32 = 48_000;
/// Decoded frames resampled at a time: 20 ms at [`DECODE_RATE`], which every rate the
/// engine accepts divides into a whole number of output frames.
const GROUP: usize = 960;
/// The longest packet Opus has, 120 ms, in decoded frames.
const MAX_PACKET_FRAMES: usize = 5760;

/// The 16-bit PCM an application on the host is recording in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MicFormat {
    pub channels: u16,
    pub sample_rate: u32,
}

/// The fastest rate a host may record in. Real ones stop at 48 kHz; the bound keeps a
/// malformed format from sizing the resampler's buffers.
const MAX_RATE: u32 = 192_000;

impl MicFormat {
    /// Whether the bridge can produce this format: mono or stereo, at a rate no faster
    /// than [`MAX_RATE`] that is a whole number of frames in 20 ms.
    pub fn producible(&self) -> anyhow::Result<()> {
        anyhow::ensure!(matches!(self.channels, 1 | 2), "{} channels", self.channels);
        anyhow::ensure!(
            (1..=MAX_RATE).contains(&self.sample_rate)
                && (GROUP as u64 * u64::from(self.sample_rate)).is_multiple_of(u64::from(DECODE_RATE)),
            "{} Hz is not a rate of at most {MAX_RATE} Hz with a whole number of frames in 20 ms",
            self.sample_rate
        );
        Ok(())
    }
}

/// A decision of the remote's, on its way to the mic socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicSignal {
    /// The host started recording in this format, so the browser should send.
    Open(MicFormat),
    /// The host stopped recording. Another [`MicSignal::Open`] may follow.
    Close,
}

/// The engine half a mic socket drives: feed it PCM.
///
/// May be called from any thread and must not block: it runs once per decoded group on
/// the socket task.
pub trait MicControl: Send + Sync {
    /// The browser enabled its microphone: a mic socket attached. An engine whose remote
    /// has a recording device for the whole session has nothing to do; one that lends
    /// the remote a device makes it now.
    fn plug(&self);
    /// The browser's microphone is gone: its socket closed. What the engine holds of it
    /// is dropped, and a lent device is taken back.
    fn unplug(&self);
    /// Interleaved 16-bit little-endian PCM in the format last opened. Returns whether it
    /// was taken.
    fn sample(&self, pcm: Vec<u8>) -> bool;
    /// Drop PCM taken but not yet sent: what follows, if anything, is another stream.
    fn reset(&self);
}

/// The seam itself: one per engine that carries a microphone, created by
/// [`crate::session`] and handed to both halves.
///
/// Besides the two registrations — the engine's control and the socket's signal sender —
/// it keeps whether the host is recording, and in what. A Windows host opens its recording
/// device during the RDP handshake, seconds before any mic socket connects, so the last
/// open is latched under the same lock as the sender and replayed to a socket that
/// subscribes while it stands; a close clears it.
///
/// It keeps, too, whether a mic socket is attached. The browser can enable its microphone
/// while the engine is still connecting, before any control is registered, so the plug is
/// remembered and told to the control as it registers.
#[derive(Default)]
pub struct MicBridge {
    upstream: Mutex<Upstream>,
    downstream: Mutex<Downstream>,
    decoder: Mutex<Option<Decoder>>,
}

#[derive(Default)]
struct Upstream {
    control: Option<Arc<dyn MicControl>>,
    plugged: bool,
}

#[derive(Default)]
struct Downstream {
    sender: Option<mpsc::UnboundedSender<MicSignal>>,
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

    /// The engine registers its control as it starts, and hears of a mic socket that
    /// attached before it did.
    pub fn set_control(&self, control: Arc<dyn MicControl>) {
        let mut upstream = self.upstream.lock().expect("mic control lock");
        if upstream.plugged {
            control.plug();
        }
        upstream.control = Some(control);
    }

    /// The socket subscribes for the host's decisions, replacing any earlier subscriber,
    /// and is handed the standing open at once if the host is recording.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<MicSignal> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut downstream = self.downstream.lock().expect("mic signal lock");
        if let Some(format) = downstream.open {
            // Unbounded, onto a receiver still held here: cannot fail.
            let _ = tx.send(MicSignal::Open(format));
        }
        downstream.sender = Some(tx);
        rx
    }

    /// Publish one of the host's decisions. Called on an engine thread, and never blocks.
    pub fn signal(&self, signal: MicSignal) {
        {
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
        // The next recording starts from a fresh encoder in the browser, so it gets a fresh
        // decoder here, and none of this one's audio.
        if signal == MicSignal::Close {
            self.reset();
        }
    }

    /// One Opus packet from the browser: decoded, resampled to the host's rate, and handed
    /// to the engine a group at a time. Dropped while the host is not recording or no
    /// engine listens.
    pub fn packet(&self, opus: &[u8]) {
        let Some(format) = self.downstream.lock().expect("mic signal lock").open else {
            return;
        };
        let Some(control) = self.control() else {
            return;
        };
        let mut decoder = self.decoder.lock().expect("mic decoder lock");
        // Built for the format the host opened, and rebuilt when it opens another: the
        // decoder's state and the resampler's history belong to one stream.
        if decoder.as_ref().is_none_or(|d| d.format != format) {
            match Decoder::new(format) {
                Ok(built) => *decoder = Some(built),
                Err(e) => {
                    warn!("mic: cannot produce {format:?}: {e:#}");
                    return;
                }
            }
        }
        let decoder = decoder.as_mut().expect("built above");
        match decoder.push(opus) {
            Ok(groups) => {
                for pcm in groups {
                    control.sample(pcm);
                }
            }
            Err(e) => debug!("mic: dropping a packet: {e:#}"),
        }
    }

    /// A mic socket attached: the engine is told the browser's microphone is there, now or
    /// as it registers. Told under the lock, so a plug and an unplug reach the engine in
    /// the order they were made.
    pub fn plug(&self) {
        let mut upstream = self.upstream.lock().expect("mic control lock");
        upstream.plugged = true;
        if let Some(control) = upstream.control.as_ref() {
            control.plug();
        }
    }

    /// The mic socket went away: the next stream starts from a fresh decoder, and the
    /// engine drops what it has not sent and lets the microphone go.
    pub fn unplug(&self) {
        *self.decoder.lock().expect("mic decoder lock") = None;
        let mut upstream = self.upstream.lock().expect("mic control lock");
        upstream.plugged = false;
        if let Some(control) = upstream.control.as_ref() {
            control.unplug();
        }
    }

    /// The host stopped recording: the next stream starts fresh, and what the engine
    /// has not sent yet of this one is dropped.
    pub fn reset(&self) {
        *self.decoder.lock().expect("mic decoder lock") = None;
        if let Some(control) = self.control() {
            control.reset();
        }
    }

    fn control(&self) -> Option<Arc<dyn MicControl>> {
        self.upstream.lock().expect("mic control lock").control.clone()
    }
}

/// Opus in, the host's PCM out.
struct Decoder {
    format: MicFormat,
    opus: opus::Decoder,
    /// `None` when the host records at [`DECODE_RATE`].
    resampler: Option<Fft<f32>>,
    /// Decoded mono waiting for a whole group.
    pending: Vec<f32>,
    /// Scratch: one packet decoded, and one group resampled.
    decoded: Vec<f32>,
    resampled: Vec<Vec<f32>>,
}

impl Decoder {
    fn new(format: MicFormat) -> anyhow::Result<Self> {
        format.producible()?;
        let group_out = GROUP * format.sample_rate as usize / DECODE_RATE as usize;
        let resampler = (format.sample_rate != DECODE_RATE)
            .then(|| {
                Fft::<f32>::new(DECODE_RATE as usize, format.sample_rate as usize, GROUP, 1, FixedSync::Input)
                    .with_context(|| format!("build a 48 kHz -> {} Hz resampler", format.sample_rate))
            })
            .transpose()?;
        let opus = opus::Decoder::new(DECODE_RATE, opus::Channels::Mono).context("create the opus decoder")?;
        // The resampler asks for room past the group's exact size, and says how much of
        // it one call filled.
        let room = resampler.as_ref().map_or(group_out, Resampler::output_frames_max);
        Ok(Self {
            format,
            opus,
            resampler,
            pending: Vec::with_capacity(GROUP * 2),
            decoded: vec![0.0; MAX_PACKET_FRAMES],
            resampled: vec![vec![0.0; room]],
        })
    }

    /// Decode one packet and return every whole group it completed, as PCM bytes.
    fn push(&mut self, packet: &[u8]) -> anyhow::Result<Vec<Vec<u8>>> {
        let frames = self.opus.decode_float(packet, &mut self.decoded, false).context("decode opus")?;
        self.pending.extend_from_slice(&self.decoded[..frames]);
        let groups = self.whole_groups();
        if groups.is_err() {
            // A group that failed and those behind it are not carried into the next packet.
            self.pending.clear();
        }
        groups
    }

    /// Take every whole group out of `pending`, as PCM bytes.
    fn whole_groups(&mut self) -> anyhow::Result<Vec<Vec<u8>>> {
        let mut groups = Vec::new();
        let mut taken = 0;
        while self.pending.len() - taken >= GROUP {
            let group = &self.pending[taken..taken + GROUP];
            let samples: &[f32] = match &mut self.resampler {
                Some(resampler) => {
                    let slices = [group];
                    let input = SequentialSliceOfSlices::new(&slices, 1, GROUP)
                        .map_err(|e| anyhow::anyhow!("wrap the resampler input: {e}"))?;
                    let room = self.resampled[0].len();
                    let mut output = SequentialSliceOfVecs::new_mut(&mut self.resampled, 1, room)
                        .map_err(|e| anyhow::anyhow!("wrap the resampler output: {e}"))?;
                    let (_, written) = resampler
                        .process_into_buffer(&input, &mut output, None)
                        .map_err(|e| anyhow::anyhow!("resample: {e}"))?;
                    &self.resampled[0][..written]
                }
                None => group,
            };
            groups.push(interleave(samples, self.format.channels));
            taken += GROUP;
        }
        self.pending.drain(..taken);
        Ok(groups)
    }
}

/// Mono `f32` as interleaved 16-bit little-endian PCM, the one channel copied to each.
fn interleave(samples: &[f32], channels: u16) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(samples.len() * 2 * usize::from(channels));
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        for _ in 0..channels {
            pcm.extend_from_slice(&value.to_le_bytes());
        }
    }
    pcm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        buffers: Mutex<Vec<Vec<u8>>>,
        resets: Mutex<usize>,
        plugs: Mutex<usize>,
    }

    impl MicControl for Recorder {
        fn plug(&self) {
            *self.plugs.lock().unwrap() += 1;
        }

        fn unplug(&self) {
            *self.resets.lock().unwrap() += 1;
        }

        fn sample(&self, pcm: Vec<u8>) -> bool {
            self.buffers.lock().unwrap().push(pcm);
            true
        }

        fn reset(&self) {
            *self.resets.lock().unwrap() += 1;
        }
    }

    const MONO_16K: MicFormat = MicFormat { channels: 1, sample_rate: 16_000 };

    /// Speech-shaped Opus from a real encoder: a 440 Hz tone, mono, 16 kbit/s, 60 ms
    /// packets — what the browser sends.
    fn browser_packets(count: usize) -> Vec<Vec<u8>> {
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        encoder.set_bitrate(opus::Bitrate::Bits(16_000)).unwrap();
        let frame: Vec<f32> =
            (0..2880).map(|n| (n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5).collect();
        (0..count).map(|_| encoder.encode_vec_float(&frame, 4000).unwrap()).collect()
    }

    /// The decoder on its own: every 60 ms packet is three 20 ms groups. A resampled
    /// group is not always the ratio's exact size — the first is short by the
    /// resampler's settling — but the stream keeps real time, within that settling.
    #[test]
    fn packets_decode_into_groups_that_keep_real_time() {
        let mut decoder = Decoder::new(MONO_16K).unwrap();
        let mut groups = 0;
        let mut bytes = 0;
        for packet in browser_packets(20) {
            for group in decoder.push(&packet).unwrap() {
                groups += 1;
                bytes += group.len();
            }
        }
        assert_eq!(groups, 60);
        let exact = 60 * 640;
        assert!(bytes <= exact && bytes + 640 >= exact, "{bytes} bytes for {exact}");
    }

    /// A browser that enables its microphone while the engine is still connecting has
    /// plugged it by the time the engine registers; one that let it go again has not.
    #[test]
    fn a_plug_made_before_the_engine_registers_reaches_it() {
        let bridge = MicBridge::new();
        bridge.plug();
        let recorder = Arc::new(Recorder::default());
        bridge.set_control(recorder.clone());
        assert_eq!(*recorder.plugs.lock().unwrap(), 1);

        let bridge = MicBridge::new();
        bridge.plug();
        bridge.unplug();
        let recorder = Arc::new(Recorder::default());
        bridge.set_control(recorder.clone());
        assert_eq!(*recorder.plugs.lock().unwrap(), 0);
    }

    #[test]
    fn nothing_is_decoded_until_the_host_records() {
        let bridge = MicBridge::new();
        let recorder = Arc::new(Recorder::default());
        bridge.set_control(recorder.clone());
        bridge.packet(&browser_packets(1)[0]);
        assert!(recorder.buffers.lock().unwrap().is_empty());
    }

    /// A 60 ms packet is three 20 ms groups at the host's rate — 320 frames at 16 kHz, and
    /// at 44.1 kHz stereo 882 frames of four bytes — within the resampler's settling.
    #[test]
    fn packets_become_the_hosts_pcm() {
        for (format, bytes) in [
            (MONO_16K, 640),
            (MicFormat { channels: 2, sample_rate: 44_100 }, 882 * 4),
            (MicFormat { channels: 1, sample_rate: 48_000 }, 1920),
        ] {
            let bridge = MicBridge::new();
            let recorder = Arc::new(Recorder::default());
            bridge.set_control(recorder.clone());
            bridge.signal(MicSignal::Open(format));
            for packet in browser_packets(5) {
                bridge.packet(&packet);
            }
            let buffers = recorder.buffers.lock().unwrap();
            assert_eq!(buffers.len(), 15, "{format:?}");
            let total: usize = buffers.iter().map(Vec::len).sum();
            assert!(total <= 15 * bytes && total + bytes >= 15 * bytes, "{format:?}: {total} bytes");
            // Past the codec's and resampler's settling, the tone is there.
            let loud = buffers[10]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|s| i16::from_le_bytes(*s).unsigned_abs())
                .max()
                .unwrap();
            assert!(loud > 8_000, "{format:?}: peak {loud}");
        }
    }

    #[test]
    fn garbage_is_dropped_and_the_stream_goes_on() {
        let bridge = MicBridge::new();
        let recorder = Arc::new(Recorder::default());
        bridge.set_control(recorder.clone());
        bridge.signal(MicSignal::Open(MONO_16K));
        bridge.packet(&[0xFF, 0xFF, 0xFF]);
        bridge.packet(&browser_packets(1)[0]);
        assert_eq!(recorder.buffers.lock().unwrap().len(), 3);
    }

    /// A close drops the engine's unsent audio, and a reopen in the same format decodes
    /// from a fresh decoder: the first packet of the new recording comes out exactly as it
    /// does from a bridge that never heard the old one.
    #[test]
    fn a_close_ends_the_stream() {
        let packets = browser_packets(3);
        let fresh = MicBridge::new();
        let expected = Arc::new(Recorder::default());
        fresh.set_control(expected.clone());
        fresh.signal(MicSignal::Open(MONO_16K));
        fresh.packet(&packets[2]);

        let bridge = MicBridge::new();
        let recorder = Arc::new(Recorder::default());
        bridge.set_control(recorder.clone());
        bridge.signal(MicSignal::Open(MONO_16K));
        bridge.packet(&packets[0]);
        bridge.packet(&packets[1]);
        bridge.signal(MicSignal::Close);
        assert_eq!(*recorder.resets.lock().unwrap(), 1);
        recorder.buffers.lock().unwrap().clear();
        bridge.signal(MicSignal::Open(MONO_16K));
        bridge.packet(&packets[2]);
        assert_eq!(*recorder.buffers.lock().unwrap(), *expected.buffers.lock().unwrap());
    }

    #[tokio::test]
    async fn a_late_subscriber_learns_the_open_and_a_close_clears_it() {
        let bridge = MicBridge::new();
        bridge.signal(MicSignal::Open(MONO_16K));
        let mut rx = bridge.subscribe();
        assert_eq!(rx.recv().await, Some(MicSignal::Open(MONO_16K)));
        bridge.signal(MicSignal::Close);
        assert_eq!(rx.recv().await, Some(MicSignal::Close));
        let mut late = bridge.subscribe();
        assert!(late.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_second_subscriber_supersedes_the_first() {
        let bridge = MicBridge::new();
        let mut first = bridge.subscribe();
        let mut second = bridge.subscribe();
        bridge.signal(MicSignal::Close);
        assert_eq!(second.recv().await, Some(MicSignal::Close));
        assert_eq!(first.recv().await, None);
    }

    #[test]
    fn a_format_the_engine_would_never_open_is_refused() {
        assert!(Decoder::new(MicFormat { channels: 1, sample_rate: 11_025 }).is_err());
        assert!(Decoder::new(MicFormat { channels: 3, sample_rate: 16_000 }).is_err());
        // Divides 20 ms evenly, but would size the resampler in gigabytes.
        assert!(Decoder::new(MicFormat { channels: 1, sample_rate: 4_294_967_250 }).is_err());
        assert!(Decoder::new(MicFormat { channels: 1, sample_rate: 0 }).is_err());
        assert!(MicFormat { channels: 2, sample_rate: 192_000 }.producible().is_ok());
    }
}
