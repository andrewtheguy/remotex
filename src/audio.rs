//! Remote audio: the queue between an engine that receives sound and the HTTP
//! response that plays it, and the WAV framing that response needs.
//!
//! See docs/remote-audio.md. Audio deliberately does not travel on the desktop
//! WebSocket: the browser already has a streaming audio client, so the gateway
//! hands it an ordinary live HTTP response and leaves buffering, decoding and
//! playback to `<audio>`. That is why this module knows nothing about
//! [`crate::protocol`] — no wire record, no version bump, no decoder or jitter
//! buffer in any client.
//!
//! Nothing here is RDP-specific either, though RDPSND is its only producer today
//! (see [`crate::rdp_audio`]).
//!
//! ## Why a broadcast channel
//!
//! [`AudioBridge::wave`] is called from inside the RDP read loop, so the queue
//! has to be one that cannot block it and cannot grow a delay. `broadcast` is
//! exactly that, and each of its properties answers a requirement:
//!
//! - `send` never awaits, so the read loop never waits for a consumer;
//! - a full ring drops the **oldest** buffer, so a slow consumer loses old audio
//!   rather than accumulating latency;
//! - a consumer that fell behind is told (`Lagged`) and skips forward, which is
//!   the same choice made again on the reading side;
//! - with no receiver attached, `send` simply fails, which is how an
//!   audio-enabled target discards sound while nobody is listening.
//!
//! There is at most one consumer, because there is one session (see CLAUDE.md).
//! That is enforced in [`AudioBridge::take_listener`] rather than assumed.

use std::convert::Infallible;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::Stream;
use log::{debug, info, warn};
use tokio::sync::{broadcast, oneshot, watch};

/// How many wave buffers the queue holds before the oldest are dropped.
///
/// A Windows server sends a few KiB per buffer — roughly 20–25 ms of CD-quality
/// stereo — so this is on the order of a second and a half. Enough to ride out a
/// scheduling hiccup in the response task, and short enough that a consumer that
/// stopped reading cannot put a noticeable delay between the desktop and its
/// sound.
pub const AUDIO_QUEUE_DEPTH: usize = 64;

/// Linear PCM parameters: the only kind of audio this path carries.
///
/// PCM because it is the one [RDPSND audio format](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/30a6cc00-31c4-4e15-9aa4-95a5c5074697)
/// clients and servers are both required to support, so accepting a compressed
/// one would make this depend on what a particular Windows version happens to
/// offer. It is also what makes the HTTP side cheap: wrapping PCM needs a
/// header, not an encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
}

impl PcmFormat {
    /// Bytes in one sample across every channel (WAV `nBlockAlign`).
    pub const fn block_align(self) -> u16 {
        self.channels * (self.bits_per_sample / 8)
    }

    /// Bytes a second of this format occupies (WAV `nAvgBytesPerSec`).
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

/// Bytes in the canonical WAV header written before the first buffer.
pub const WAV_HEADER_LEN: usize = 44;

/// The RIFF and `data` chunk sizes of a stream whose length is not known.
///
/// A live response has no end, so neither field can be filled in. `0xFFFFFFFF`
/// is the convention for that (rather than 0, which some readers take
/// literally and treat as an empty file).
const WAV_UNKNOWN_SIZE: u32 = u32::MAX;

/// The 44-byte RIFF/WAVE header for `format`, with both size fields left
/// unknown.
pub fn wav_header(format: PcmFormat) -> [u8; WAV_HEADER_LEN] {
    let mut header = [0u8; WAV_HEADER_LEN];
    let mut put = |at: usize, bytes: &[u8]| header[at..at + bytes.len()].copy_from_slice(bytes);

    put(0, b"RIFF");
    put(4, &WAV_UNKNOWN_SIZE.to_le_bytes());
    put(8, b"WAVE");

    put(12, b"fmt ");
    put(16, &16u32.to_le_bytes()); // the PCM fmt chunk is 16 bytes
    put(20, &1u16.to_le_bytes()); // WAVE_FORMAT_PCM
    put(22, &format.channels.to_le_bytes());
    put(24, &format.sample_rate.to_le_bytes());
    put(28, &format.byte_rate().to_le_bytes());
    put(32, &format.block_align().to_le_bytes());
    put(34, &format.bits_per_sample.to_le_bytes());

    put(36, b"data");
    put(40, &WAV_UNKNOWN_SIZE.to_le_bytes());
    header
}

/// The seam between an engine receiving redirected audio and the HTTP response
/// playing it. Created and owned by the session slot, so its lifetime is the
/// engine's: dropping it ends any response reading from it.
#[derive(Debug)]
pub struct AudioBridge {
    waves: broadcast::Sender<Vec<u8>>,
    /// The negotiated format, `None` until the engine's audio channel has come
    /// up. The endpoint waits on this before writing a header it would otherwise
    /// be guessing at.
    format: watch::Sender<Option<PcmFormat>>,
    /// Ends the current listener's response. Held here rather than derived from
    /// the engine's lifetime because two of the cases that must end a response
    /// leave the engine running: a session takeover, and a second request by the
    /// same owner (which replaces the first rather than sharing the stream).
    listener: Mutex<Option<oneshot::Sender<()>>>,
    /// Which MS-RDPEA transport is filling this queue — see
    /// [`Self::claim_transport`].
    transport: Mutex<Option<&'static str>>,
}

impl AudioBridge {
    pub fn new() -> Self {
        Self {
            waves: broadcast::channel(AUDIO_QUEUE_DEPTH).0,
            format: watch::Sender::new(None),
            listener: Mutex::new(None),
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

    /// Forget the negotiated format: the far side closed the audio channel.
    pub fn clear_format(&self) {
        self.format.send_replace(None);
    }

    /// Queue one buffer. Never blocks and never fails visibly: a full queue
    /// drops its oldest buffer and no listener drops this one.
    pub fn wave(&self, samples: Vec<u8>) {
        let _ = self.waves.send(samples);
    }

    /// Attach the one listener, ending whichever response held the slot before.
    pub fn take_listener(&self) -> AudioListener {
        let (stop_tx, stop) = oneshot::channel();
        // Assigning drops the previous sender, and that drop is what ends the
        // previous response.
        *self.listener.lock().unwrap() = Some(stop_tx);
        AudioListener {
            waves: self.waves.subscribe(),
            format: self.format.subscribe(),
            stop,
        }
    }

    /// End the current listener's response without touching the engine. What a
    /// session takeover does: the desktop carries on for the new browser, but
    /// the previous browser's audio belongs to the claim it no longer holds.
    pub fn stop_listener(&self) {
        if self.listener.lock().unwrap().take().is_some() {
            debug!("audio: ending the current listener's response");
        }
    }
}

impl Default for AudioBridge {
    fn default() -> Self {
        Self::new()
    }
}

/// One HTTP response's read side of an [`AudioBridge`].
pub struct AudioListener {
    waves: broadcast::Receiver<Vec<u8>>,
    format: watch::Receiver<Option<PcmFormat>>,
    stop: oneshot::Receiver<()>,
}

impl AudioListener {
    /// Wait up to `timeout` for the negotiated format. `None` means the audio
    /// channel never came up, which the endpoint reports rather than answering
    /// with a header it invented.
    pub async fn await_format(&mut self, timeout: Duration) -> Option<PcmFormat> {
        // Read out of the guard rather than matching on it, so nothing borrowed
        // from the watch is alive across the await below.
        let current = *self.format.borrow_and_update();
        if current.is_some() {
            return current;
        }
        tokio::time::timeout(timeout, async {
            while self.format.changed().await.is_ok() {
                let current = *self.format.borrow_and_update();
                if current.is_some() {
                    return current;
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
    }

    /// The response body: one WAV header for `format`, then each buffer as it
    /// arrives.
    ///
    /// Open-ended by construction — there is no recording and no seekable
    /// history, so a listener starts at live audio and receives only what
    /// arrives after it attached. It ends when the engine's bridge is dropped
    /// (the target changed, disconnected, or the engine died), when the slot
    /// ends this listener (a takeover, or a second request), or when the
    /// consumer stops reading and this stream is dropped with it.
    pub fn into_stream(self, format: PcmFormat) -> impl Stream<Item = Result<Vec<u8>, Infallible>> {
        struct State {
            header: Option<Vec<u8>>,
            waves: broadcast::Receiver<Vec<u8>>,
            stop: oneshot::Receiver<()>,
        }

        let state = State {
            header: Some(wav_header(format).to_vec()),
            waves: self.waves,
            stop: self.stop,
        };
        futures_util::stream::unfold(state, |mut state| async move {
            if let Some(header) = state.header.take() {
                return Some((Ok(header), state));
            }
            loop {
                tokio::select! {
                    // Resolves on the value *or* on the sender being dropped,
                    // and both mean the same thing here.
                    _ = &mut state.stop => return None,
                    wave = state.waves.recv() => match wave {
                        Ok(samples) => return Some((Ok(samples), state)),
                        // Old audio was dropped while this consumer was behind.
                        // Skipping forward is the point: the alternative is a
                        // delay that never comes back.
                        Err(broadcast::error::RecvError::Lagged(dropped)) => {
                            debug!("audio: listener fell behind, {dropped} buffer(s) dropped");
                        }
                        Err(broadcast::error::RecvError::Closed) => return None,
                    },
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;

    use super::*;

    /// The header is the whole of the "encoder", so its bytes are pinned rather
    /// than recomputed by the test the same way the code computes them.
    #[test]
    fn the_wav_header_is_a_cd_quality_riff_stream_of_unknown_length() {
        let header = wav_header(PCM_CD_QUALITY);
        assert_eq!(
            header,
            [
                b'R', b'I', b'F', b'F', //
                0xff, 0xff, 0xff, 0xff, // length unknown: this response has no end
                b'W', b'A', b'V', b'E', //
                b'f', b'm', b't', b' ', //
                16, 0, 0, 0, // PCM fmt chunk size
                1, 0, // WAVE_FORMAT_PCM
                2, 0, // stereo
                0x44, 0xac, 0, 0, // 44100 Hz
                0x10, 0xb1, 2, 0, // 176400 bytes a second
                4, 0, // 4 bytes a sample frame
                16, 0, // 16 bits a sample
                b'd', b'a', b't', b'a', //
                0xff, 0xff, 0xff, 0xff, // and neither does its data chunk
            ]
        );
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
        let header = wav_header(telephone);
        assert_eq!(&header[24..28], &8_000u32.to_le_bytes());
        assert_eq!(&header[28..32], &8_000u32.to_le_bytes());
    }

    async fn next(stream: &mut (impl Stream<Item = Result<Vec<u8>, Infallible>> + Unpin)) -> Option<Vec<u8>> {
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("timed out waiting for the audio stream")
            .map(|chunk| chunk.unwrap())
    }

    #[tokio::test]
    async fn a_listener_gets_the_header_then_only_what_arrives_after_it_attached() {
        let bridge = AudioBridge::new();
        bridge.publish_format(PCM_CD_QUALITY);
        // Discarded: nobody was listening, and there is no history to replay.
        bridge.wave(vec![1, 1, 1, 1]);

        let mut listener = bridge.take_listener();
        assert_eq!(
            listener.await_format(Duration::from_secs(5)).await,
            Some(PCM_CD_QUALITY),
            "the format was published before the listener attached"
        );
        let mut stream = Box::pin(listener.into_stream(PCM_CD_QUALITY));
        assert_eq!(
            next(&mut stream).await.unwrap(),
            wav_header(PCM_CD_QUALITY).to_vec()
        );

        bridge.wave(vec![2, 2, 2, 2]);
        assert_eq!(next(&mut stream).await.unwrap(), vec![2, 2, 2, 2]);
    }

    #[tokio::test]
    async fn a_format_published_after_the_listener_attached_still_reaches_it() {
        let bridge = AudioBridge::new();
        let mut listener = bridge.take_listener();
        bridge.publish_format(PCM_CD_QUALITY);
        assert_eq!(
            listener.await_format(Duration::from_secs(5)).await,
            Some(PCM_CD_QUALITY)
        );
    }

    /// The 503 case: a target configured for audio whose channel never came up.
    #[tokio::test]
    async fn waiting_for_a_format_that_never_arrives_times_out() {
        tokio::time::pause();
        let bridge = AudioBridge::new();
        let mut listener = bridge.take_listener();
        assert_eq!(listener.await_format(Duration::from_secs(5)).await, None);
    }

    #[tokio::test]
    async fn dropping_the_bridge_ends_the_response() {
        let bridge = AudioBridge::new();
        let listener = bridge.take_listener();
        let mut stream = Box::pin(listener.into_stream(PCM_CD_QUALITY));
        next(&mut stream).await.unwrap(); // the header

        drop(bridge);
        assert!(next(&mut stream).await.is_none());
    }

    /// A takeover, and the one lifecycle case the engine's own lifetime cannot
    /// express: the desktop carries on, the audio response does not.
    #[tokio::test]
    async fn stopping_the_listener_ends_the_response_but_not_the_bridge() {
        let bridge = AudioBridge::new();
        let listener = bridge.take_listener();
        let mut stream = Box::pin(listener.into_stream(PCM_CD_QUALITY));
        next(&mut stream).await.unwrap();

        bridge.stop_listener();
        assert!(next(&mut stream).await.is_none());

        // The bridge is still usable, which is what a takeover needs.
        let replacement = bridge.take_listener();
        let mut stream = Box::pin(replacement.into_stream(PCM_CD_QUALITY));
        next(&mut stream).await.unwrap();
        bridge.wave(vec![7]);
        assert_eq!(next(&mut stream).await.unwrap(), vec![7]);
    }

    #[tokio::test]
    async fn a_second_listener_replaces_the_first() {
        let bridge = AudioBridge::new();
        let first = bridge.take_listener();
        let mut first = Box::pin(first.into_stream(PCM_CD_QUALITY));
        next(&mut first).await.unwrap();

        let second = bridge.take_listener();
        let mut second = Box::pin(second.into_stream(PCM_CD_QUALITY));
        assert!(
            next(&mut first).await.is_none(),
            "the first response should have ended"
        );
        next(&mut second).await.unwrap();
        bridge.wave(vec![9]);
        assert_eq!(next(&mut second).await.unwrap(), vec![9]);
    }

    /// The backpressure rule: the producer is never held up, and what gives way
    /// is old audio. A queue that blocked here would be blocking the RDP read
    /// loop, and one that grew would be building a permanent delay.
    #[tokio::test]
    async fn an_unread_queue_drops_its_oldest_buffers_instead_of_blocking() {
        let bridge = AudioBridge::new();
        let listener = bridge.take_listener();
        let mut stream = Box::pin(listener.into_stream(PCM_CD_QUALITY));
        next(&mut stream).await.unwrap();

        // Twice the depth, none of it read: every one of these returns at once.
        for i in 0..AUDIO_QUEUE_DEPTH * 2 {
            bridge.wave(vec![i as u8]);
        }
        // The reader is told it fell behind (swallowed inside the stream) and
        // resumes at the oldest buffer still held, not at the first one sent.
        let resumed = next(&mut stream).await.unwrap();
        assert_eq!(resumed, vec![AUDIO_QUEUE_DEPTH as u8]);
    }
}
