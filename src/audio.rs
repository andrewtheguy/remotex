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
/// A Windows server sends a few KiB per buffer — roughly 20–25 ms of CD-quality
/// stereo — so this is on the order of a second and a half. Enough to ride out a
/// scheduling hiccup in the response task, and short enough that a consumer that
/// stopped reading cannot put a noticeable delay between the desktop and its
/// sound.
pub const AUDIO_QUEUE_DEPTH: usize = 64;

/// One Opus frame as a duration: 20 ms, derived rather than written down.
const FRAME: Duration =
    Duration::from_nanos(FRAME_FRAMES as u64 * 1_000_000_000 / OPUS_SAMPLE_RATE as u64);

/// How often the keepalive asks whether the stream owes any silence. Nothing is
/// emitted unless the arithmetic in [`silence_owed`] says so, so this is a polling
/// interval and not a cadence.
const SILENCE_CHECK: Duration = Duration::from_millis(200);

/// How far ahead of real time the keepalive keeps the stream, in frames — 400 ms.
///
/// Paid once, at the start, and then maintained: without it the element would sit
/// exactly at the live edge, playing each page out as fast as it arrives and
/// re-stalling every time it catches up. The cost is that opening the panel while
/// the remote is already playing inserts 400 ms of silence in front of the sound,
/// which is indistinguishable from the buffering a media element does anyway.
const SILENCE_LEAD_FRAMES: u64 = 20;

/// Most silence one check may emit: 1 s. Filling a hole should arrive as a top-up
/// rather than a flood.
const SILENCE_CATCHUP_FRAMES: u64 = 50;

/// Past this much owed — 5 s — nothing was reading this stream at all: a consumer
/// that stopped polling, or a laptop that slept. Paying that back in silence would
/// only delay the next real audio behind it, so the dead period is skipped instead.
const SILENCE_RESYNC_FRAMES: u64 = 250;

/// How many frames of silence the stream owes, given how long it has been open and
/// how much it has encoded.
///
/// Anchored to the clock rather than to "nothing has arrived for N ms", and that is
/// the whole trick: real audio counts towards `encoded` too, so a remote delivering
/// at real time is owed nothing and never has filler mixed into its sound, while a
/// remote that stalls and then delivers its backlog is briefly *ahead* rather than
/// permanently late. A rule that padded after every quiet interval would add that
/// delay on each hiccup and never give it back.
///
/// `skipped` records frames deliberately not paid back, and is why a resync is
/// permanent rather than re-owed on the next check.
fn silence_owed(elapsed: Duration, encoded: u64, skipped: &mut u64) -> usize {
    let target = (elapsed.as_nanos() / FRAME.as_nanos()) as u64 + SILENCE_LEAD_FRAMES;
    let owed = target.saturating_sub(encoded + *skipped);
    if owed > SILENCE_RESYNC_FRAMES {
        *skipped += owed;
        return 0;
    }
    owed.min(SILENCE_CATCHUP_FRAMES) as usize
}

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
            /// When this response opened, which is what the keepalive measures
            /// against. `tokio`'s clock rather than the standard library's, so
            /// `tokio::time::pause` can drive it in tests.
            started: tokio::time::Instant,
            /// Frames the keepalive decided not to pay back — see [`silence_owed`].
            skipped: u64,
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
            started: tokio::time::Instant::now(),
            skipped: 0,
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
                        Ok(samples) => encoder.push(&samples),
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
                    // The keepalive. A fresh timer each iteration, so an arriving
                    // buffer resets it — which costs nothing, since how much
                    // silence is owed comes from the clock and not from this.
                    _ = tokio::time::sleep(SILENCE_CHECK) => {
                        match silence_owed(
                            state.started.elapsed(),
                            encoder.frames_encoded(),
                            &mut state.skipped,
                        ) {
                            0 => continue,
                            owed => encoder.push_silence(owed),
                        }
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

    /// The keepalive's arithmetic, tested as arithmetic: at this scale the rules are
    /// easier to state in numbers than to observe through an encoder.
    #[test]
    fn the_keepalive_pays_the_lead_once_and_then_tracks_real_time() {
        let mut skipped = 0;
        // Nothing encoded yet: the lead plus the elapsed frames.
        assert_eq!(silence_owed(FRAME * 10, 0, &mut skipped), 30);
        // Having paid it, a stream keeping up owes nothing — including one whose
        // frames are real audio rather than silence.
        assert_eq!(silence_owed(FRAME * 10, 30, &mut skipped), 0);
        assert_eq!(silence_owed(FRAME * 100, 120, &mut skipped), 0);
        // A remote briefly ahead of the clock (a stall, then its backlog) is not
        // owed negative silence.
        assert_eq!(silence_owed(FRAME * 100, 500, &mut skipped), 0);
        assert_eq!(skipped, 0, "nothing here was a dead period");
    }

    #[test]
    fn the_keepalive_tops_up_in_bounded_steps_and_skips_a_dead_period() {
        let mut skipped = 0;
        // A 3 s hole is filled a second at a time rather than in one flood.
        assert_eq!(
            silence_owed(FRAME * 150, 0, &mut skipped),
            SILENCE_CATCHUP_FRAMES as usize
        );
        assert_eq!(skipped, 0);

        // A minute of not being polled is skipped, permanently: the next check is
        // owed only the time that passed since it, not the minute.
        let mut skipped = 0;
        assert_eq!(silence_owed(FRAME * 3_000, 0, &mut skipped), 0);
        assert_eq!(skipped, 3_000 + SILENCE_LEAD_FRAMES);
        assert_eq!(silence_owed(FRAME * 3_010, 0, &mut skipped), 10);
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
