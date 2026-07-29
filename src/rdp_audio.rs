//! FreeRDP audio redirection into the bounded PCM bridge.
//!
//! Register both static `rdpsnd` and dynamic `AUDIO_PLAYBACK_DVC`; the server
//! chooses between them, and the static form also depends on `rdpdr` registered
//! by the session. Callbacks advertise one PCM format and only copy complete
//! buffers, keeping resampling and Opus encoding off the RDP read loop.

use std::sync::Arc;

use ironrdp::core::{Decode as _, Encode, EncodeResult, ReadCursor, WriteCursor, impl_as_any};
use ironrdp::dvc::{DvcClientProcessor, DvcEncode, DvcMessage, DvcProcessor};
use ironrdp::pdu::{PduResult, decode_err};
use ironrdp::rdpsnd::client::RdpsndClientHandler;
use ironrdp::rdpsnd::pdu::{
    AudioFormat, AudioFormatFlags, ClientAudioFormatPdu, ClientAudioOutputPdu, PitchPdu,
    QualityMode, QualityModePdu, ServerAudioOutputPdu, TrainingConfirmPdu, Version, VolumePdu,
    WaveConfirmPdu, WaveFormat,
};
use log::{debug, info, warn};

use crate::audio::{AudioBridge, PCM_CD_QUALITY, PcmFormat};

/// The MS-RDPEA transport names, as they identify themselves in the log and in
/// [`AudioBridge::claim_transport`].
const STATIC_TRANSPORT: &str = "the static rdpsnd channel";
const DYNAMIC_TRANSPORT: &str = "AUDIO_PLAYBACK_DVC";

/// The [`RdpsndClientHandler`] IronRDP drives. Holds no audio state of its own.
#[derive(Debug)]
pub struct Handler {
    bridge: Arc<AudioBridge>,
    /// The one format we advertise, as MS-RDPEA spells it. Owned because the
    /// trait hands the list out by reference.
    formats: Vec<AudioFormat>,
    /// Whether a buffer has arrived yet, so the proof that redirection is
    /// actually working is logged once instead of every 20 milliseconds.
    seen_wave: bool,
}

impl Handler {
    pub fn new(bridge: Arc<AudioBridge>) -> Self {
        Self {
            bridge,
            formats: vec![audio_format(PCM_CD_QUALITY)],
            seen_wave: false,
        }
    }
}

/// Our [`PcmFormat`] as an MS-RDPEA `AUDIO_FORMAT`.
///
/// Every field has to match the server's advertisement exactly — `AudioFormat`
/// compares by value, and the negotiated set is the intersection — so the two
/// derived rates are computed rather than left at defaults, and `data` is `None`
/// because PCM carries no extra format bytes (`cbSize` 0).
fn audio_format(format: PcmFormat) -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: format.channels,
        n_samples_per_sec: format.sample_rate,
        n_avg_bytes_per_sec: format.byte_rate(),
        n_block_align: format.block_align(),
        bits_per_sample: format.bits_per_sample,
        data: None,
    }
}

impl RdpsndClientHandler for Handler {
    /// Being asked for our formats *is* the negotiation: the server has sent its
    /// own list, and `Rdpsnd` is about to answer with the intersection. So this is
    /// where "the audio channel came up" is recorded, even though it reads as a
    /// getter — there is no other callback for it. Nothing waits on that record
    /// (a subscription succeeds either way); it is what the log reports, and it is
    /// published here rather than on the first wave buffer because a host that
    /// negotiates and then plays nothing is a different thing to know about than a
    /// host that never negotiated.
    fn get_formats(&self) -> &[AudioFormat] {
        if self.bridge.claim_transport(STATIC_TRANSPORT) {
            self.bridge.publish_format(PCM_CD_QUALITY);
        }
        &self.formats
    }

    /// `format_no` can only be 0 — see this module's header — so it is checked
    /// rather than used. A non-zero index would mean the library's index space
    /// changed under us, and playing the buffer anyway would be playing noise.
    fn wave(&mut self, format_no: usize, _ts: u32, data: std::borrow::Cow<'_, [u8]>) {
        if format_no != 0 {
            debug!("rdp: ignoring an audio buffer in unadvertised format {format_no}");
            return;
        }
        if !self.bridge.claim_transport(STATIC_TRANSPORT) {
            return;
        }
        if !self.seen_wave {
            self.seen_wave = true;
            info!(
                "rdp: the remote is redirecting audio over the static channel \
                 ({} bytes in the first buffer)",
                data.len()
            );
        }
        self.bridge.wave(data.into_owned());
    }

    // Neither capability is advertised (`get_flags` defaults to empty and
    // `Rdpsnd` only adds ALIVE), so a conformant server sends neither. Ignored
    // rather than applied: the browser's own volume control is the one the person
    // listening can reach, and pitch is not something this path has any use for.
    fn set_volume(&mut self, volume: VolumePdu) {
        debug!("rdp: ignoring a remote audio volume change: {volume:?}");
    }

    fn set_pitch(&mut self, pitch: PitchPdu) {
        debug!("rdp: ignoring a remote audio pitch change: {pitch:?}");
    }

    /// The remote closed the audio channel, or is about to renegotiate it.
    ///
    /// A live host does this whenever nothing is playing on it, so this is the
    /// ordinary quiet case and not a fault: an open response stays open and fills
    /// with silence, and the next buffer resumes it. FreeRDP's `rdpsnd` handles a
    /// close the same way, by leaving its audio device open and reopening it on the
    /// next wave. All that is given up here is the record that the channel is up.
    fn close(&mut self) {
        debug!("rdp: the remote closed the static audio channel");
        self.seen_wave = false;
        self.bridge.clear_format();
        self.bridge.release_transport(STATIC_TRANSPORT);
    }
}

/// Audio over the dynamic virtual channel — the transport a current Windows host
/// actually uses, and the one IronRDP has no client for.
///
/// This is the same MS-RDPEA conversation `Rdpsnd` runs on the static channel, but
/// its state machine cannot be reused: it is written against `SvcProcessor` and
/// answers in `SvcMessage`s, where a dynamic channel answers in [`DvcMessage`]s.
/// So the sequence is spelled out again here over the same public PDU types.
#[derive(Debug)]
pub struct AudioPlaybackDvc {
    bridge: Arc<AudioBridge>,
    state: State,
    /// The MS-RDPEA version the server announced, which decides whether it will
    /// accept a quality-mode PDU.
    version: Option<Version>,
    seen_wave: bool,
}

/// Where the channel is in MS-RDPEA's opening sequence. The server speaks first
/// in each step, so this only records what has been answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting for the server's format list.
    Start,
    /// Formats answered; waiting for the training PDU.
    Training,
    /// Negotiated: wave buffers may arrive.
    Ready,
}

impl_as_any!(AudioPlaybackDvc);

impl AudioPlaybackDvc {
    pub fn new(bridge: Arc<AudioBridge>) -> Self {
        Self {
            bridge,
            state: State::Start,
            version: None,
            seen_wave: false,
        }
    }

    /// Answer the server's format list with the one format we take, plus the
    /// quality mode if this version accepts one.
    ///
    /// The reply carries our format only when the server offered exactly it. An
    /// empty list is the honest answer otherwise — the server then redirects
    /// nothing, which is better than agreeing to a format whose buffers the
    /// encoder downstream would then read as the format it was told to expect.
    fn answer_formats(&mut self, offered: &[AudioFormat], version: Version) -> Vec<DvcMessage> {
        let ours = audio_format(PCM_CD_QUALITY);
        let formats = if offered.contains(&ours) {
            self.bridge.publish_format(PCM_CD_QUALITY);
            vec![ours]
        } else {
            warn!(
                "rdp: the remote offers no {} Hz {}-bit stereo PCM audio format, so it will \
                 redirect nothing ({} format(s) offered)",
                PCM_CD_QUALITY.sample_rate,
                PCM_CD_QUALITY.bits_per_sample,
                offered.len()
            );
            Vec::new()
        };
        let mut out = vec![message(ClientAudioOutputPdu::AudioFormat(ClientAudioFormatPdu {
            version,
            // ALIVE is what says "this client will consume audio"; neither volume
            // nor pitch is claimed, so the remote should never send either.
            flags: AudioFormatFlags::ALIVE,
            formats,
            volume_left: 0xFFFF,
            volume_right: 0xFFFF,
            pitch: 0x0001_0000,
            // No UDP: this session's audio rides the TCP connection it negotiated
            // on, and there is no second transport to point at.
            dgram_port: 0,
        }))];
        if version >= Version::V6 {
            out.push(message(ClientAudioOutputPdu::QualityMode(QualityModePdu {
                quality_mode: QualityMode::High,
            })));
        }
        out
    }
}

impl DvcProcessor for AudioPlaybackDvc {
    fn channel_name(&self) -> &str {
        DYNAMIC_TRANSPORT
    }

    /// Nothing on creation: in MS-RDPEA the server opens the conversation with its
    /// format list, and a client that spoke first would be answering a question
    /// nobody asked.
    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let pdu =
            ServerAudioOutputPdu::decode(&mut ReadCursor::new(payload)).map_err(|e| decode_err!(e))?;
        if !self.bridge.claim_transport(DYNAMIC_TRANSPORT) {
            return Ok(Vec::new());
        }
        Ok(match pdu {
            // Sent to open the conversation, and again whenever the remote wants
            // to renegotiate — which restarts the sequence rather than continuing
            // it, exactly as the static path treats it.
            ServerAudioOutputPdu::AudioFormat(server) => {
                self.version = Some(server.version);
                self.state = State::Training;
                self.answer_formats(&server.formats, server.version)
            }
            ServerAudioOutputPdu::Training(training) => {
                if self.state == State::Training {
                    self.state = State::Ready;
                }
                // The confirmation echoes the timestamp and the size of what
                // arrived; the payload itself is a bandwidth probe with no
                // meaning to a consumer.
                vec![message(ClientAudioOutputPdu::TrainingConfirm(
                    TrainingConfirmPdu {
                        timestamp: training.timestamp,
                        pack_size: u16::try_from(training.data.len()).unwrap_or(u16::MAX),
                    },
                ))]
            }
            ServerAudioOutputPdu::Wave2(wave) => {
                if wave.format_no != 0 {
                    debug!(
                        "rdp: ignoring an audio buffer in unadvertised format {}",
                        wave.format_no
                    );
                } else {
                    if !self.seen_wave {
                        self.seen_wave = true;
                        info!(
                            "rdp: the remote is redirecting audio over {DYNAMIC_TRANSPORT} \
                             ({} bytes in the first buffer)",
                            wave.data.len()
                        );
                    }
                    self.bridge.wave(wave.data.into_owned());
                }
                // Confirmed either way: a buffer this client will not play is
                // still a buffer the remote must stop holding.
                vec![message(ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                    timestamp: wave.timestamp,
                    block_no: wave.block_no,
                }))]
            }
            ServerAudioOutputPdu::Close => {
                debug!("rdp: the remote closed {DYNAMIC_TRANSPORT}");
                self.state = State::Start;
                self.seen_wave = false;
                self.bridge.clear_format();
                Vec::new()
            }
            // Both are capabilities this client does not claim, so a conformant
            // remote sends neither. The remaining variants are the pre-v8 wave
            // forms, which `ironrdp-rdpsnd` does not decode into anything usable.
            other => {
                debug!("rdp: ignoring an audio PDU this client does not act on: {other:?}");
                Vec::new()
            }
        })
    }

    fn close(&mut self, _channel_id: u32) {
        self.state = State::Start;
        self.seen_wave = false;
        self.bridge.clear_format();
        self.bridge.release_transport(DYNAMIC_TRANSPORT);
    }
}

impl DvcClientProcessor for AudioPlaybackDvc {}

/// One outgoing client PDU as a [`DvcMessage`].
///
/// The wrapper exists for the orphan rule and nothing else: `DvcEncode` belongs to
/// `ironrdp-dvc` and `ClientAudioOutputPdu` to `ironrdp-rdpsnd`, so neither crate
/// can join them and only a local type can.
struct Message(ClientAudioOutputPdu);

fn message(pdu: ClientAudioOutputPdu) -> DvcMessage {
    Box::new(Message(pdu))
}

impl Encode for Message {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        self.0.encode(dst)
    }

    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn size(&self) -> usize {
        self.0.size()
    }
}

impl DvcEncode for Message {}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use ironrdp::core::encode_vec;
    use ironrdp::rdpsnd::pdu::{ServerAudioFormatPdu, TrainingPdu, Wave2Pdu};

    use super::*;

    /// A server PDU on the wire, as a real server would put it there.
    fn encode_server(pdu: ServerAudioOutputPdu<'_>) -> Vec<u8> {
        encode_vec(&pdu).expect("a server PDU encodes")
    }

    /// The next item from a listener's packet stream, or a failure saying so.
    ///
    /// Bounded, like the equivalents in [`crate::audio`] and [`crate::session`], and for
    /// a reason these tests need more than most: what they exercise is a *plumbing*
    /// path — a wave PDU reaching a listener — and the way that breaks is by producing
    /// nothing at all. `cargo test` has no per-test timeout, so an unguarded `next()`
    /// would hang the suite instead of naming the hop that stopped forwarding.
    async fn next_packets(
        stream: &mut (impl futures_util::Stream<Item = Vec<Vec<u8>>> + Unpin),
    ) -> Vec<Vec<u8>> {
        use futures_util::StreamExt as _;

        tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("timed out waiting for audio to reach the listener")
            .expect("the listener's stream ended instead of carrying audio")
    }

    /// Enough PCM for exactly one 20 ms Opus frame at the negotiated format.
    /// Smaller buffers are held by the encoder rather than emitted, so a test
    /// that sent a handful of bytes would wait forever for a packet.
    ///
    /// The stream is Opus, so these tests cannot compare the bytes they sent
    /// against the bytes that came out. What they still prove is the plumbing — a
    /// wave PDU reaching a listener — and a packet count is the observable form of
    /// that. The encoding itself is covered in [`crate::opus_stream`], which
    /// decodes what it encoded.
    fn one_frame_of_pcm() -> Vec<u8> {
        let frames = crate::opus_stream::FRAME_FRAMES * PCM_CD_QUALITY.sample_rate as usize
            / crate::opus_stream::OPUS_SAMPLE_RATE as usize;
        vec![0u8; frames * usize::from(PCM_CD_QUALITY.block_align())]
    }

    /// The server's format list, announced as MS-RDPEA version 8 — the version
    /// every current Windows host negotiates, and the one that carries `Wave2`.
    fn server_formats(formats: &[AudioFormat]) -> Vec<u8> {
        encode_server(ServerAudioOutputPdu::AudioFormat(ServerAudioFormatPdu {
            version: Version::V8,
            formats: formats.to_vec(),
        }))
    }

    /// A PCM format that is *not* the one we ask for, used to occupy index 0 of
    /// the server's list so a format index resolved against the wrong list picks
    /// it.
    fn decoy_format() -> AudioFormat {
        AudioFormat {
            format: WaveFormat::PCM,
            n_channels: 1,
            n_samples_per_sec: 22_050,
            n_avg_bytes_per_sec: 44_100,
            n_block_align: 2,
            bits_per_sample: 16,
            data: None,
        }
    }

    #[tokio::test]
    async fn one_pcm_format_is_advertised_and_publishing_it_is_the_negotiation() {
        let bridge = Arc::new(AudioBridge::new());
        let listener = bridge.take_listener();
        let handler = Handler::new(Arc::clone(&bridge));

        // Nothing has asked for the formats, so nothing has been negotiated.
        assert_eq!(listener.negotiated_format(), None);

        let formats = handler.get_formats();
        assert_eq!(
            formats,
            [AudioFormat {
                format: WaveFormat::PCM,
                n_channels: 2,
                n_samples_per_sec: 44_100,
                n_avg_bytes_per_sec: 176_400,
                n_block_align: 4,
                bits_per_sample: 16,
                data: None,
            }],
            "exactly one format, or a buffer's format index becomes a guess"
        );
        assert_eq!(listener.negotiated_format(), Some(PCM_CD_QUALITY));
    }

    #[tokio::test]
    async fn a_wave_reaches_the_listener_and_a_closed_channel_forgets_the_format() {
        let bridge = Arc::new(AudioBridge::new());
        let listener = bridge.take_listener();
        let mut handler = Handler::new(Arc::clone(&bridge));
        handler.get_formats();
        let format = listener.negotiated_format().expect("answering the formats negotiates");
        let (_head, packets) = listener.into_packets(format).expect("an encoder");
        let mut stream = Box::pin(packets);

        let frame = one_frame_of_pcm();
        handler.wave(0, 0, Cow::Borrowed(&frame));
        assert_eq!(next_packets(&mut stream).await.len(), 1);

        // An index we never advertised is dropped rather than played as if it
        // were PCM. Three frames on the bad index against one on the good one, so
        // the packet count says which of them reached the queue — the encoded bytes
        // no longer carry anything a test could recognise.
        let bogus = frame.repeat(3);
        handler.wave(1, 0, Cow::Borrowed(&bogus));
        handler.wave(0, 0, Cow::Borrowed(&frame));
        assert_eq!(
            next_packets(&mut stream).await.len(),
            1,
            "the wave on an unadvertised format index should have been dropped"
        );

        handler.close();
        let after_close = bridge.take_listener();
        assert_eq!(
            after_close.negotiated_format(),
            None,
            "a closed channel is no longer a live negotiation"
        );
    }

    /// The engine is torn down from under the handler in the ordinary case (the
    /// browser went away), and IronRDP may still drive a callback or two on the
    /// way out.
    #[tokio::test]
    async fn a_handler_with_nobody_listening_does_not_panic() {
        let bridge = Arc::new(AudioBridge::new());
        let mut handler = Handler::new(bridge);
        handler.get_formats();
        handler.wave(0, 0, Cow::Borrowed(&[1, 2, 3, 4]));
        handler.close();
    }

    /// The whole gateway side of MS-RDPEA, driven by the server half of the
    /// conversation: formats, training, and a wave buffer, encoded as a real
    /// server would and pushed through IronRDP's own client state machine.
    ///
    /// This is the only test that can prove the path without a cooperating
    /// Windows box, and it exists because a live one turned out not to be proof
    /// of anything: a server that offers no matching format opens no audio at all,
    /// which is indistinguishable from a gateway that does not work.
    ///
    /// It also pins the reasoning behind advertising a single format. The server
    /// below offers a decoy *first*, so the format we asked for is not at index 0
    /// of the server's list — and the wave still arrives, because with one
    /// advertised format the intersection has one entry and `format_no` 0 can only
    /// mean it.
    #[tokio::test]
    async fn a_server_speaking_rdpsnd_gets_its_audio_onto_a_listener() {
        use ironrdp::core::encode_vec;
        use ironrdp::rdpsnd::client::Rdpsnd;
        use ironrdp::rdpsnd::pdu::{
            ServerAudioFormatPdu, ServerAudioOutputPdu, TrainingPdu, Version, Wave2Pdu,
        };
        use ironrdp::svc::SvcProcessor as _;

        fn encoded(pdu: ServerAudioOutputPdu<'_>) -> Vec<u8> {
            encode_vec(&pdu).expect("a server PDU encodes")
        }

        let bridge = Arc::new(AudioBridge::new());
        let listener = bridge.take_listener();
        let mut rdpsnd = Rdpsnd::new(Box::new(Handler::new(Arc::clone(&bridge))));

        // A decoy at index 0, so a format index resolved against the server's
        // list would pick the wrong one.
        let decoy = AudioFormat {
            format: WaveFormat::PCM,
            n_channels: 1,
            n_samples_per_sec: 22_050,
            n_avg_bytes_per_sec: 44_100,
            n_block_align: 2,
            bits_per_sample: 16,
            data: None,
        };
        let answer = rdpsnd
            .process(&encoded(ServerAudioOutputPdu::AudioFormat(
                ServerAudioFormatPdu {
                    version: Version::V8,
                    formats: vec![decoy, audio_format(PCM_CD_QUALITY)],
                },
            )))
            .expect("the client answers the server's formats");
        assert!(!answer.is_empty(), "the client must reply with its formats");
        assert_eq!(
            listener.negotiated_format(),
            Some(PCM_CD_QUALITY),
            "answering the format list is what records that audio is up"
        );

        let answer = rdpsnd
            .process(&encoded(ServerAudioOutputPdu::Training(TrainingPdu {
                timestamp: 1,
                data: vec![0; 16],
            })))
            .expect("the client confirms training");
        assert!(!answer.is_empty(), "training must be confirmed");

        let (_head, packets) = listener.into_packets(PCM_CD_QUALITY).expect("an encoder");
        let mut stream = Box::pin(packets);

        let samples = one_frame_of_pcm();
        let answer = rdpsnd
            .process(&encoded(ServerAudioOutputPdu::Wave2(Wave2Pdu {
                timestamp: 2,
                format_no: 0,
                block_no: 7,
                audio_timestamp: 12_345,
                data: Cow::Borrowed(&samples),
            })))
            .expect("the client accepts a wave");
        assert!(
            !answer.is_empty(),
            "every buffer is confirmed, accepted or dropped"
        );
        assert_eq!(
            next_packets(&mut stream).await.len(),
            1,
            "the server's PCM should reach the listener as an encoded packet"
        );

        // And the server closing the channel ends the format, so the next
        // listener waits rather than being handed a header for a dead stream.
        rdpsnd.process(&encoded(ServerAudioOutputPdu::Close)).unwrap();
        assert_eq!(bridge.negotiated_format(), None);
    }

    /// The same conversation over the dynamic channel, which is the transport a
    /// current Windows host actually uses — and the one with no library code
    /// behind it, so this covers a state machine written here rather than one
    /// borrowed from IronRDP.
    #[tokio::test]
    async fn a_server_speaking_the_audio_dvc_gets_its_audio_onto_a_listener() {
        let bridge = Arc::new(AudioBridge::new());
        let listener = bridge.take_listener();
        let mut dvc = AudioPlaybackDvc::new(Arc::clone(&bridge));
        assert_eq!(dvc.channel_name(), "AUDIO_PLAYBACK_DVC");
        assert!(
            dvc.start(1).unwrap().is_empty(),
            "the server opens this conversation, not us"
        );

        // A decoy first again, so a format index read against the server's list
        // would pick the wrong entry.
        let answer = dvc
            .process(1, &server_formats(&[decoy_format(), audio_format(PCM_CD_QUALITY)]))
            .expect("the client answers the server's formats");
        assert_eq!(
            answer.len(),
            2,
            "the formats reply, and a quality mode for a V8 server"
        );
        assert_eq!(listener.negotiated_format(), Some(PCM_CD_QUALITY));

        let answer = dvc
            .process(
                1,
                &encode_server(ServerAudioOutputPdu::Training(TrainingPdu {
                    timestamp: 9,
                    data: vec![0; 32],
                })),
            )
            .expect("the client confirms training");
        assert_eq!(answer.len(), 1, "training is confirmed");

        let (_head, packets) = listener.into_packets(PCM_CD_QUALITY).expect("an encoder");
        let mut stream = Box::pin(packets);

        let samples = one_frame_of_pcm();
        let answer = dvc
            .process(
                1,
                &encode_server(ServerAudioOutputPdu::Wave2(Wave2Pdu {
                    timestamp: 10,
                    format_no: 0,
                    block_no: 3,
                    audio_timestamp: 999,
                    data: Cow::Borrowed(&samples),
                })),
            )
            .expect("the client accepts a wave");
        assert_eq!(answer.len(), 1, "every buffer is confirmed");
        assert_eq!(next_packets(&mut stream).await.len(), 1);

        dvc.close(1);
        assert_eq!(
            bridge.negotiated_format(),
            None,
            "a closed channel is no longer a live negotiation"
        );
    }

    /// A server that offers nothing we can carry is told so with an empty format
    /// list, and no format is published. Nothing refuses the listener over it — the
    /// response is silence either way — so this negotiation and the log line it
    /// drives are the only place that shows.
    #[tokio::test]
    async fn a_server_offering_no_pcm_is_answered_with_no_formats() {
        let bridge = Arc::new(AudioBridge::new());
        let listener = bridge.take_listener();
        let mut dvc = AudioPlaybackDvc::new(Arc::clone(&bridge));

        let answer = dvc.process(1, &server_formats(&[decoy_format()])).unwrap();
        assert_eq!(answer.len(), 2, "still answered, just with nothing offered");
        assert_eq!(
            listener.negotiated_format(),
            None,
            "nothing was negotiated, so nothing may be published"
        );
    }

    /// The guard against two transports feeding one queue. A server driving both
    /// would interleave two streams into one listener, which is worse than silence
    /// because nothing would report it.
    #[tokio::test]
    async fn the_second_transport_to_arrive_is_ignored() {
        let bridge = Arc::new(AudioBridge::new());
        let mut dvc = AudioPlaybackDvc::new(Arc::clone(&bridge));
        let mut statik = Handler::new(Arc::clone(&bridge));

        // The dynamic channel gets there first.
        dvc.process(1, &server_formats(&[audio_format(PCM_CD_QUALITY)]))
            .unwrap();
        let (_head, packets) = bridge
            .take_listener()
            .into_packets(PCM_CD_QUALITY)
            .expect("an encoder");
        let mut stream = Box::pin(packets);

        // The two buffers are deliberately different lengths, because Opus makes
        // them otherwise indistinguishable: three frames from the transport that
        // must be ignored, one from the transport that owns the queue. The first
        // item to arrive therefore says which buffer landed — and it was sent
        // first, so if it were queued it would be the one read.
        let ignored = one_frame_of_pcm().repeat(3);
        let owned = one_frame_of_pcm();
        statik.wave(0, 0, Cow::Borrowed(&ignored));
        dvc.process(
            1,
            &encode_server(ServerAudioOutputPdu::Wave2(Wave2Pdu {
                timestamp: 1,
                format_no: 0,
                block_no: 1,
                audio_timestamp: 0,
                data: Cow::Borrowed(&owned),
            })),
        )
        .unwrap();
        assert_eq!(
            next_packets(&mut stream).await.len(),
            1,
            "the dynamic channel owns the queue, so the static buffer never landed"
        );
    }
}
