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
//!
//! ## Why the response never goes quiet
//!
//! A remote is quiet most of the time, and the tested Windows host does not merely
//! stop sending buffers when nothing is playing — it never opens the audio channel
//! at all until something does, and closes it again afterwards. So "no audio yet"
//! and "no audio ever" look identical from here, and an endpoint that waited to
//! find out answered `503` to a perfectly good session whose desktop happened to be
//! silent. A media element does not retry, so that `503` was final.
//!
//! Instead the response opens immediately and **keeps flowing**: while no buffers
//! arrive, [`AudioListener::into_stream`] encodes silence, so the element stays
//! playing and real audio simply replaces the silence when the remote starts — and
//! again after it stops and starts. One element, one `play()`, from the click that
//! opened the panel; nothing to re-load, no second autoplay attempt for a browser
//! policy to refuse, and no restart delay when sound returns.
//!
//! That silence trickles rather than keeping pace with the clock, which is the part
//! that is easy to get wrong — see [`SILENCE_TRICKLE_FRAMES`]. A media element never
//! skips forward, so a keepalive that matched real time would make start-up
//! buffering and every hiccup permanent lag.
//!
//! FreeRDP makes the same call one layer down: its `rdpsnd_recv_close_pdu` only
//! logs, deliberately leaving the local audio device open, and reopens it on the
//! next wave. A server closing the channel is not a teardown.

use std::convert::Infallible;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::Stream;
use log::{debug, info, warn};
use tokio::sync::{broadcast, oneshot, watch};

use crate::opus_stream::{FRAME_FRAMES, OPUS_SAMPLE_RATE, OggOpus};

/// How many wave buffers the queue holds before the oldest are dropped.
///
/// This is deep: the tested host sends **32 KiB per buffer, 186 ms** of CD-quality
/// stereo, so 64 of them is **11.8 seconds** — not the "second and a half" this
/// comment claimed while it assumed a few KiB a buffer. Worth knowing before relying
/// on the drop rule to bound latency, because it does not: a consumer that ran one
/// buffer per second slow would be eleven seconds behind before anything was
/// dropped.
///
/// It has never come to that. Measured against the live target, 299 consecutive
/// buffers arrived with the queue at **zero** every time — the host paces itself to
/// real time (one buffer every ~189 ms) and the encode side keeps up with room to
/// spare. So the depth is doing nothing but absorbing a scheduling hiccup, which is
/// what it is for, and shrinking it would only make a drop more likely without
/// making anything faster.
pub const AUDIO_QUEUE_DEPTH: usize = 64;

/// One Opus frame as a duration: 20 ms, derived rather than written down.
const FRAME: Duration =
    Duration::from_nanos(FRAME_FRAMES as u64 * 1_000_000_000 / OPUS_SAMPLE_RATE as u64);

/// How long the keepalive lets the stream go without sending anything.
const SILENCE_CHECK: Duration = Duration::from_millis(500);

/// How long since the last wave buffer before the remote counts as quiet.
///
/// The interval above is not enough on its own. A live host leaves ~185 ms between
/// buffers, but it is a desktop and not a clock: one late delivery longer than the
/// check would put 100 ms of silence into the middle of real audio, which is a
/// stutter, and the listener then carries that 100 ms as lag. Eight times the
/// cadence is comfortably past any hiccup and still quick enough that a browser is
/// never left more than two seconds with nothing at all.
const SILENCE_GRACE: Duration = Duration::from_millis(1_500);

/// How much silence it sends when it does fire: 100 ms, against that 500 ms wait.
///
/// **Deliberately slower than real time, and that is the whole point of the number.**
/// A keepalive that kept pace with the clock — which is what this was first — keeps
/// the element alive but makes every stall permanent: a media element resumes where
/// it stopped and never skips forward, so whatever it fell behind by during start-up
/// buffering or one hiccup, it stays behind by, and a session accumulates seconds of
/// lag with nothing to shed them.
///
/// At a fifth of real time, a quiet remote is instead when a listener catches back
/// up: the element plays out its buffer at 1x while receiving 0.2x, so a second of
/// lag is gone after about a second and a quarter of quiet. It arrives at the sound
/// starved rather than ahead, which is what playing at the live edge means.
const SILENCE_TRICKLE: Duration = Duration::from_millis(100);

/// The same in Opus frames, which is the unit the encoder cuts silence in.
const SILENCE_TRICKLE_FRAMES: usize =
    (SILENCE_TRICKLE.as_nanos() / FRAME.as_nanos()) as usize;

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
    /// The negotiated format, `None` until the engine's audio channel has come up
    /// and again once it closes. Nothing depends on it — the response opens either
    /// way — so it is a record rather than a gate: what the log says about whether
    /// the remote's audio is set up, and what an indicator would read one day.
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
    ///
    /// What it does *not* do is go quiet when the remote does: gaps are filled with
    /// encoded silence, so the element keeps playing and resumes on its own when
    /// sound comes back. See this module's header.
    pub fn into_stream(self, format: PcmFormat) -> impl Stream<Item = Result<Vec<u8>, Infallible>> {
        struct State {
            /// `None` once the encoder has failed or refused the format; the
            /// stream then ends, which is what the endpoint's caller already
            /// handles for a remote that went away.
            encoder: Option<OggOpus>,
            header: Option<Vec<u8>>,
            waves: broadcast::Receiver<Vec<u8>>,
            stop: oneshot::Receiver<()>,
            /// When a wave buffer last arrived, so the keepalive can tell a quiet
            /// remote from a late delivery. `tokio`'s clock rather than the standard
            /// library's, so `tokio::time::pause` can drive it in tests.
            last_wave: tokio::time::Instant,
            /// When the response opened, which only the diagnostic line below reads:
            /// frames encoded against time elapsed is how a stream that is drifting
            /// from real time shows itself.
            started: tokio::time::Instant,
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
            last_wave: tokio::time::Instant::now(),
            started: tokio::time::Instant::now(),
        };
        futures_util::stream::unfold(state, |mut state| async move {
            if let Some(header) = state.header.take() {
                return Some((Ok(header), state));
            }
            let encoder = state.encoder.as_mut()?;
            loop {
                let encoded = tokio::select! {
                    // Resolves on the value *or* on the sender being dropped,
                    // and both mean the same thing here.
                    _ = &mut state.stop => return None,
                    wave = state.waves.recv() => match wave {
                        Ok(samples) => {
                            let now = tokio::time::Instant::now();
                            // Four numbers, and between them they say whether this
                            // gateway is adding delay: the buffer size and the queue
                            // depth locate a backlog, and encoded frames against
                            // elapsed time say whether the stream is running ahead of
                            // real time or behind it. It was written to answer a
                            // report of a couple of seconds of lag, and it answered it
                            // — no backlog, ratio 0.9996 — which is why it stays:
                            // the next such report deserves the same evidence rather
                            // than a fresh round of theories.
                            debug!(
                                "audio: wave {} bytes, {} queued, {} frames encoded in {} ms",
                                samples.len(),
                                state.waves.len(),
                                encoder.frames_encoded(),
                                state.started.elapsed().as_millis(),
                            );
                            state.last_wave = now;
                            encoder.push(&samples)
                        }
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
                    // The keepalive, which fills a remote that has gone quiet and
                    // nothing else: a late buffer is waited for rather than papered
                    // over. What it hands across is less than the interval's worth,
                    // so a quiet remote drains the listener's backlog instead of
                    // holding it.
                    _ = tokio::time::sleep(SILENCE_CHECK) => {
                        if state.last_wave.elapsed() < SILENCE_GRACE {
                            continue;
                        }
                        encoder.push_silence(SILENCE_TRICKLE_FRAMES)
                    }
                };
                match encoded {
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

        let listener = bridge.take_listener();
        assert_eq!(
            listener.negotiated_format(),
            Some(PCM_CD_QUALITY),
            "the format was published before the listener attached"
        );
        let mut stream = Box::pin(listener.into_stream(PCM_CD_QUALITY));
        expect_headers(&mut stream).await;

        bridge.wave(one_frame_of_pcm());
        let audio = next(&mut stream).await.unwrap();
        assert_eq!(page_count(&audio), 1, "one frame in, one page out");
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

    /// What used to be the `503`: a target configured for audio whose channel never
    /// came up. Nothing is negotiated, nothing is published, and the response is
    /// still a playable stream — the point of the whole keepalive.
    #[tokio::test(start_paused = true)]
    async fn a_listener_that_negotiated_nothing_still_gets_a_playable_stream() {
        let bridge = AudioBridge::new();
        let listener = bridge.take_listener();
        assert_eq!(listener.negotiated_format(), None);
        let mut stream = Box::pin(listener.into_stream(PCM_CD_QUALITY));
        expect_headers(&mut stream).await;

        // Paused time auto-advances to the keepalive's timer, so this is arithmetic
        // rather than a race: nothing has been sent, and pages arrive anyway.
        for _ in 0..3 {
            let silence = next(&mut stream).await.expect("the stream must stay open");
            assert_eq!(page_count(&silence), 1, "a batch of silence shares one page");
        }
    }

    /// The remote going quiet and coming back, which is the sequence this exists
    /// for: one stream throughout, sound resuming with no help from the client.
    #[tokio::test(start_paused = true)]
    async fn audio_that_stops_and_starts_again_stays_one_stream() {
        let bridge = AudioBridge::new();
        bridge.publish_format(PCM_CD_QUALITY);
        let listener = bridge.take_listener();
        let mut stream = Box::pin(listener.into_stream(PCM_CD_QUALITY));
        expect_headers(&mut stream).await;

        bridge.wave(one_frame_of_pcm());
        assert_eq!(page_count(&next(&mut stream).await.unwrap()), 1);

        // The channel closes, as a real host's does when nothing is playing.
        bridge.clear_format();
        for _ in 0..2 {
            assert_eq!(page_count(&next(&mut stream).await.unwrap()), 1, "silence");
        }

        // And it comes back, without the listener having reattached.
        bridge.publish_format(PCM_CD_QUALITY);
        bridge.wave(one_frame_of_pcm());
        assert_eq!(
            page_count(&next(&mut stream).await.unwrap()),
            1,
            "the same stream should carry the audio that came back"
        );
    }

    /// Real audio must not have filler mixed into it. A buffer already on the queue
    /// wins over the keepalive's timer, so while buffers keep arriving faster than
    /// [`SILENCE_CHECK`] — a live remote sends one every ~185 ms — the check never
    /// fires and one frame in is one page out.
    #[tokio::test(start_paused = true)]
    async fn a_buffer_arriving_before_the_next_check_is_not_padded() {
        let bridge = AudioBridge::new();
        let listener = bridge.take_listener();
        let mut stream = Box::pin(listener.into_stream(PCM_CD_QUALITY));
        expect_headers(&mut stream).await;

        tokio::time::advance(SILENCE_CHECK / 2).await;
        bridge.wave(one_frame_of_pcm());
        assert_eq!(page_count(&next(&mut stream).await.unwrap()), 1);
    }

    /// The two properties the keepalive's numbers have to keep, both of which were
    /// learnt by getting them wrong. Pinned here because they are invisible in the
    /// arm that uses them: it is two constants and a `sleep`.
    #[test]
    fn the_keepalive_trickles_rather_than_keeps_pace() {
        // Slower than real time, or a listener that fell behind stays behind: it
        // plays out at 1x and can only catch up while receiving less than that.
        assert!(
            SILENCE_TRICKLE * 2 < SILENCE_CHECK,
            "silence must arrive well under real time, \
             got {SILENCE_TRICKLE:?} per {SILENCE_CHECK:?}"
        );
        // And the frame count has to be that duration, not a number beside it.
        assert_eq!(FRAME * SILENCE_TRICKLE_FRAMES as u32, SILENCE_TRICKLE);
        // And it must not be able to land inside real audio: a host sends a buffer
        // every ~185 ms, and one arriving late must be waited for rather than filled
        // in, since the filler becomes lag the listener then carries.
        assert!(
            SILENCE_GRACE > Duration::from_millis(185) * 4,
            "the grace has to clear a late delivery, not just the usual cadence"
        );
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
