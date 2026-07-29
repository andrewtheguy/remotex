//! Remote audio: the queue between an engine that receives sound and the HTTP
//! response that plays it. The response's bytes are Ogg/Opus, framed by
//! [`crate::opus_stream`].
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

use crate::opus_stream::OggOpus;

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
/// RDP format would make this depend on what a particular Windows version happens
/// to offer. What the *gateway* then sends a browser is a separate question, and
/// the answer is Opus: PCM is the right thing to ask a server for and the wrong
/// thing to put on a network.
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
    /// the number Opus exists to shrink: 176 400 for the format below.
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

    /// The response body: the Ogg header pages for `format`, then Opus packets as
    /// the PCM to fill them arrives.
    ///
    /// Open-ended by construction — there is no recording and no seekable
    /// history, so a listener starts at live audio and receives only what
    /// arrives after it attached. It ends when the engine's bridge is dropped
    /// (the target changed, disconnected, or the engine died), when the slot
    /// ends this listener (a takeover, or a second request), or when the
    /// consumer stops reading and this stream is dropped with it.
    ///
    /// A wave buffer usually yields several pages and sometimes none — the
    /// encoder cuts 20 ms frames out of whatever sizes RDP sends, and holds the
    /// remainder (see [`crate::opus_stream`]).
    pub fn into_stream(self, format: PcmFormat) -> impl Stream<Item = Result<Vec<u8>, Infallible>> {
        struct State {
            /// `None` once the encoder has failed or refused the format; the
            /// stream then ends, which is what the endpoint's caller already
            /// handles for a remote that went away.
            encoder: Option<OggOpus>,
            header: Option<Vec<u8>>,
            waves: broadcast::Receiver<Vec<u8>>,
            stop: oneshot::Receiver<()>,
        }

        let (encoder, header) = match OggOpus::new(format) {
            Ok((encoder, header)) => (Some(encoder), Some(header)),
            Err(e) => {
                // Reported here rather than as an HTTP status: by this point the
                // 200 has been sent, and a media element shows a load failure
                // either way. The log is the only place this can be seen.
                warn!("audio: cannot encode {format:?} as opus, no audio will be sent: {e}");
                (None, None)
            }
        };

        let state = State {
            encoder,
            header,
            waves: self.waves,
            stop: self.stop,
        };
        futures_util::stream::unfold(state, |mut state| async move {
            if let Some(header) = state.header.take() {
                return Some((Ok(header), state));
            }
            let encoder = state.encoder.as_mut()?;
            loop {
                let samples = tokio::select! {
                    // Resolves on the value *or* on the sender being dropped,
                    // and both mean the same thing here.
                    _ = &mut state.stop => return None,
                    wave = state.waves.recv() => match wave {
                        Ok(samples) => samples,
                        // Old audio was dropped while this consumer was behind.
                        // Skipping forward is the point: the alternative is a
                        // delay that never comes back. The encoder carries on:
                        // Opus frames are independent, so a gap is a gap in the
                        // sound rather than a broken stream.
                        Err(broadcast::error::RecvError::Lagged(dropped)) => {
                            debug!("audio: listener fell behind, {dropped} buffer(s) dropped");
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => return None,
                    },
                };
                match encoder.push(&samples) {
                    // Empty when the buffer did not complete a 20 ms frame. Yielding
                    // nothing would end the stream, so keep reading instead.
                    Ok(pages) if pages.is_empty() => continue,
                    Ok(pages) => return Some((Ok(pages), state)),
                    Err(e) => {
                        warn!("audio: the opus encoder failed, ending the stream: {e}");
                        return None;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;

    use super::*;
    use crate::opus_stream::FRAME_FRAMES;

    /// What this format costs on the wire, which is the whole reason the response
    /// is Opus and not this.
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
        let frames = FRAME_FRAMES * PCM_CD_QUALITY.sample_rate as usize
            / crate::opus_stream::OPUS_SAMPLE_RATE as usize;
        vec![0u8; frames * usize::from(PCM_CD_QUALITY.block_align())]
    }

    /// Ogg pages in a chunk, counted by their capture pattern.
    fn page_count(chunk: &[u8]) -> usize {
        chunk.windows(4).filter(|w| *w == b"OggS").count()
    }

    async fn next(stream: &mut (impl Stream<Item = Result<Vec<u8>, Infallible>> + Unpin)) -> Option<Vec<u8>> {
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("timed out waiting for the audio stream")
            .map(|chunk| chunk.unwrap())
    }

    /// The header pages, asserted to be exactly that. Every test wants them out
    /// of the way, and none of them should pass if audio arrived first.
    async fn expect_headers(
        stream: &mut (impl Stream<Item = Result<Vec<u8>, Infallible>> + Unpin),
    ) {
        let headers = next(stream).await.expect("the ogg header pages");
        assert_eq!(&headers[0..4], b"OggS");
        assert_eq!(page_count(&headers), 2, "OpusHead and OpusTags");
    }

    #[tokio::test]
    async fn a_listener_gets_the_headers_then_only_what_arrives_after_it_attached() {
        let bridge = AudioBridge::new();
        bridge.publish_format(PCM_CD_QUALITY);
        // Discarded: nobody was listening, and there is no history to replay.
        bridge.wave(one_frame_of_pcm());

        let mut listener = bridge.take_listener();
        assert_eq!(
            listener.await_format(Duration::from_secs(5)).await,
            Some(PCM_CD_QUALITY),
            "the format was published before the listener attached"
        );
        let mut stream = Box::pin(listener.into_stream(PCM_CD_QUALITY));
        expect_headers(&mut stream).await;

        bridge.wave(one_frame_of_pcm());
        let audio = next(&mut stream).await.unwrap();
        assert_eq!(page_count(&audio), 1, "one frame in, one page out");
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
        expect_headers(&mut stream).await;

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
        expect_headers(&mut stream).await;

        bridge.stop_listener();
        assert!(next(&mut stream).await.is_none());

        // The bridge is still usable, which is what a takeover needs — and the
        // replacement gets its own headers, without which its Opus packets would
        // arrive with nothing to configure a decoder.
        let replacement = bridge.take_listener();
        let mut stream = Box::pin(replacement.into_stream(PCM_CD_QUALITY));
        expect_headers(&mut stream).await;
        bridge.wave(one_frame_of_pcm());
        assert_eq!(page_count(&next(&mut stream).await.unwrap()), 1);
    }

    #[tokio::test]
    async fn a_second_listener_replaces_the_first() {
        let bridge = AudioBridge::new();
        let first = bridge.take_listener();
        let mut first = Box::pin(first.into_stream(PCM_CD_QUALITY));
        expect_headers(&mut first).await;

        let second = bridge.take_listener();
        let mut second = Box::pin(second.into_stream(PCM_CD_QUALITY));
        assert!(
            next(&mut first).await.is_none(),
            "the first response should have ended"
        );
        expect_headers(&mut second).await;
        bridge.wave(one_frame_of_pcm());
        assert_eq!(page_count(&next(&mut second).await.unwrap()), 1);
    }

    /// The backpressure rule: the producer is never held up, and what gives way
    /// is old audio. A queue that blocked here would be blocking the RDP read
    /// loop, and one that grew would be building a permanent delay.
    ///
    /// Read straight off the queue rather than through [`AudioListener::into_stream`],
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
        let mut survived: Vec<Vec<u8>> = Vec::new();
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
