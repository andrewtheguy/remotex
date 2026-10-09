//! Apple High Performance's picture and sound, the way Apple's viewer takes them:
//! HEVC and AAC-ELD over the media stream, not ZRLE over RFB.
//!
//! A High Performance viewer that advertises encoding **1010**
//! ([`ENCODING_MEDIA_STREAM`]) and sends message **`0x1c`**
//! (`RFBMediaStreamServerConfiguration`, [`configuration`]) gets its screen and its
//! sound from `ScreensharingAgent`'s AVConference sender: two RTP streams,
//! SRTP-protected, over UDP straight to this side. RFB carries only the
//! negotiation — the Mac answers in encoding-1010 rectangles ([`MediaReply`]) —
//! and, once the stream is up, no pixels at all while nothing asks it for them.
//! None of it is documented by Apple; every rule here was measured against macOS
//! 26.6 and is recorded in `docs/apple-vnc-889.md` ("The media stream: High
//! Performance's picture and sound").
//!
//! A target takes this path with `subtype = "ard-high-performance"`. Everything but
//! the picture's decoder — offers, replies, SRTP, depacketizing, the receiver, and
//! passing the stream on — is the gateway's own; the decoder is FFmpeg's
//! libavcodec, loaded from the system (`crate::libav`). A gateway whose host
//! lacks it ends a session that does not pass the picture before it dials the
//! Mac. The sound is never decoded here: its AAC-ELD units go to every browser as
//! they came ([`PASSED_SOUND`]).
//!
//! The offer is two AVConference negotiation blobs, rebuilt field by field from the
//! ones Apple's client produced ([`audio_offer_blob`], [`video_offer_blob`]). The
//! Mac refuses a configuration without either, so the two legs go together: the
//! picture comes from one and the sound from the other, and while the sound leg
//! runs the Mac mutes its own output, as it does for Apple's viewer. A session
//! with two virtual displays offers a video blob for each, and the Mac sends
//! each display's picture on a leg of its own ([`MediaStream`]).
//!
//! A stream that fails ends the session, as it ends Apple's viewer's, which has no
//! way back to RFB pixels: one the Mac refuses, one that brings no picture or no
//! sound within [`STREAM_START`] of its offer, one that sends neither for
//! [`STREAM_SILENCE`], and one whose receiver fails ([`MediaStream::overdue`],
//! [`MediaStream::failure`]).
//! ZRLE rectangles are stepped over by their length, never inflated or shown.
//! Until the first media picture, and across display changes which stop the
//! stream until the next offer, the browser says the screen is not available
//! ([`crate::protocol::ServerMsg::ScreenUnavailable`]).
//!
//! Every packet in is authenticated before it is decrypted — AES-256 counter mode
//! with an HMAC-SHA1-80 tag, RFC 3711 keys from the masters this side put in the
//! offer ([`SrtpReceiver`]) — and every report out is SRTCP under this side's own
//! keys ([`SrtcpSender`]). A packet whose tag does not match is dropped, and so is
//! an authentic one no newer than a packet already received.
//!
//! Two fields of the offer differ from Apple's, each measured:
//!
//! - the flags word carries [`FLAG_NO_CURSOR`], which makes the agent capture the
//!   screen without the pointer (`send cursor with video 0`) — the pointer keeps
//!   arriving as its own shape over RFB;
//! - `tilesPerFrame` is 1 for a stream passed to the browser, so each picture is
//!   one HEVC picture of the whole display. A stream decoded here is offered
//!   Apple's 4, which codes the display as four strips ([`Strips`]).
//!
//! Its bitrate entries are Apple's, and so is what bounds them: the Mac's rate
//! controller walks the picture between 20 and 60 Mbit/s by the one-way delay this
//! side reports ([`RateFeedback`]), every 50 ms as Apple's viewer does.
//!
//! The stream itself is HEVC Range Extensions, 4:4:4, full-range BT.709, RTP payload
//! type 100 packed as RFC 7798: single NAL units, aggregation packets and
//! fragmentation units, with decoding order numbers only where the display comes in
//! strips ([`Strips`]). [`Depacketizer`] reassembles access units from it, and a
//! lost packet costs a refresh ([`rtcp_refresh`]): a picture the Mac predicts from
//! one this side acknowledged ([`rtcp_reference_ack`]), in place of an IDR. Where
//! nothing is left to predict from, a PLI ([`rtcp_pli`]) brings an IDR within tens
//! of milliseconds. The Mac drops either request when its last keyframe is under a
//! second old, so one stays owed until a picture comes of it.

use std::io::Write as _;

use anyhow::Context as _;

use aes::Aes256;
use aes::cipher::{BlockCipherEncrypt as _, KeyInit as _};
use hmac::{Hmac, Mac as _};
use sha1::Sha1;

use crate::vnc_apple;

/// Encoding 1010 (`0x3f2`), `kSSVideoEncoding_AVCMediaStream`: the viewer takes its
/// picture from the media stream, and the Mac's media-stream replies arrive as
/// rectangles of it.
pub const ENCODING_MEDIA_STREAM: i32 = 1010;

/// The sound leg as every browser is sent it: the Mac's AAC-ELD units as they
/// came, one a packet, described by the AudioSpecificConfig the Mac never
/// sends ([`crate::aac_eld`]). The codec string names what the stream is; which
/// configuration a browser's `AudioDecoder` actually decodes it under is the
/// page's to find out (`frontend/src/appleMedia.ts`).
pub const PASSED_SOUND: crate::audio::PassedFormat = crate::audio::PassedFormat {
    codec: "mp4a.40.39",
    sample_rate: 48_000,
    channels: crate::aac_eld::CHANNELS as u16,
    packet_frames: crate::aac_eld::FRAME_SAMPLES as u32,
    head: &crate::aac_eld::AUDIO_SPECIFIC_CONFIG,
};

/// The second `SetEncodings`, sent once the first layout has arrived and held
/// for the rest of the session: the media stream first, as Apple's viewer
/// lists it in High Performance mode. Naming the stream makes the Mac name its
/// ports. The Mac's preferred codec is the first it
/// knows in the list, and to a viewer whose preferred codec is the media stream
/// its framebuffer sender sends no pixels: cursor shapes, layouts and the stream's
/// ports still come.
///
/// That is what keeps the Mac's two framing threads apart. The sender frames its
/// updates under a lock, and the thread that reads this side's messages frames
/// the answer to an offer without it; a record from each at once fails its
/// integrity check here and ends the session. `SetEncodings` is acted on under
/// that lock, so an update being written is out before the first offer is
/// read, and none follows it or any later offer.
pub fn encodings_preferring_media_stream() -> Vec<i32> {
    let mut encodings = vec![ENCODING_MEDIA_STREAM];
    encodings.extend_from_slice(vnc_apple::ENCODINGS);
    encodings
}

/// `0x1c` flag bit 0: the viewer takes 60 frames a second. `screensharingd` sets it
/// (with bit 1) for a viewer whose message predates version 2, so every viewer it
/// knows of has it.
pub const FLAG_60FPS: u32 = 0x1;
/// `0x1c` flag bit 2: capture without the pointer. The agent logs `send cursor with
/// video 0` for it; without it the pointer is drawn into every picture.
pub const FLAG_NO_CURSOR: u32 = 0x4;
/// `0x1c` flag bit 1: [`FLAG_60FPS`] for the second video stream, which a
/// two-display offer sets beside it.
pub const FLAG_60FPS_SECOND: u32 = 0x2;
/// The flags this viewer sends for one display.
pub const FLAGS: u32 = FLAG_60FPS | FLAG_NO_CURSOR;

/// The most displays a session's stream carries, one video leg each: the Mac
/// creates one virtual display or two.
pub const MAX_DISPLAYS: usize = 2;

/// An SRTP master key as the `0x1c` message carries it: 32 bytes of AES-256 key and
/// 14 bytes of salt.
pub type MasterKey = [u8; 46];

// ---------------------------------------------------------------------------
// The AVConference offers
// ---------------------------------------------------------------------------

/// The `avcMediaStreamNegotiatorMode` of an audio offer.
const MODE_AUDIO: u8 = 8;
/// And of a screen-video offer.
const MODE_VIDEO: u8 = 7;

/// `avcMediaStreamOptionRemoteEndpointInfo`: the viewer's model and builds, as
/// Apple's client on macOS 26.6 (25G83) reported them.
const REMOTE_ENDPOINT_INFO: &[u8] = &[
    0x08, 0x00, // f1 = 0
    0x10, 0x01, // f2 = 1
    0x1a, 0x0d, b'V', b'i', b'r', b't', b'u', b'a', b'l', b'M', b'a', b'c', b'2', b',', b'1',
    0x22, 0x08, b'2', b'2', b'1', b'5', b'.', b'5', b'.', b'1',
    0x2a, 0x05, b'2', b'5', b'G', b'8', b'3',
];

/// The blob's `f6`: the negotiation library's name and version.
const VICEROY: &str = "Viceroy 1.7.0";

/// The blob's `f13`, which differs between the two modes and was not decoded
/// further.
const AUDIO_F13: u64 = 17_169_649_764_059_066_368;
const VIDEO_F13: u64 = 17_169_649_764_516_085_760;

/// The blob's repeated `f9` entries, `(f1, f2, f3)`: the audio codecs (AAC-ELD
/// `{16, 4100}`, AMR-NB `{1, 299}`, EVS `{4, 6500}`) and, with `f1 = 0`, bitrates
/// in bits per second. The audio offer lists them in this order; the video offer
/// moves one to the front.
const CODEC_ENTRIES: &[(u64, u64, Option<u64>)] = &[
    (1, 299, None),
    (4074, 0, Some(16_384)),
    (0, 75_000_000, Some(524_288)),
    (0, 20_000_000, Some(98_304)),
    (0, 40_000_000, Some(12_288)),
    (0, 60_000_000, Some(262_144)),
    (16, 4100, None),
    (0, 100_000_000, Some(1_048_576)),
    (4, 6500, None),
    (0, 6_000_000, Some(131_072)),
];

/// The strips Apple's own `tilesPerFrame` of 4 codes a display in, which a stream
/// decoded here is offered. A stream passed to the browser is offered 1: one
/// picture of the whole display, which is what a `VideoDecoder` there shows.
const STRIPS: usize = 4;

/// The rows of each strip of a display `height` rows high: a quarter of them,
/// rounded up to a multiple of 16.
fn strip_rows(height: u16) -> usize {
    usize::from(height).div_ceil(STRIPS).next_multiple_of(16)
}

/// Whether a display `height` rows high is offered in strips. The rounding can
/// leave the last strip starting past the display's last row, and offered four
/// tiles for such a display the Mac answers, its encoder refuses every frame,
/// and no picture comes: at 80, 90 and 136 rows, where one tile brought the
/// picture. Those are heights under 48 rows, from 65 to 95 and from 129 to 143,
/// which are offered one tile. A last strip that starts just at the end, as at
/// 96 and 144 rows, is sent, with nothing of the display in it. Neither side's
/// negotiation looks at the size: the tile count is the lesser of the two
/// sides', so this is for the viewer to avoid.
fn in_strips(height: u16) -> bool {
    strip_rows(height) * (STRIPS - 1) <= usize::from(height)
}

/// Minimal protocol-buffers writer: varints and length-delimited fields are the
/// whole of what the offers use.
#[derive(Default)]
struct Proto(Vec<u8>);

impl Proto {
    fn varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.0.push((value as u8) | 0x80);
            value >>= 7;
        }
        self.0.push(value as u8);
    }

    fn uint(&mut self, field: u64, value: u64) {
        self.varint(field << 3);
        self.varint(value);
    }

    fn bytes(&mut self, field: u64, value: &[u8]) {
        self.varint((field << 3) | 2);
        self.varint(value.len() as u64);
        self.0.extend_from_slice(value);
    }

    fn message(&mut self, field: u64, inner: Proto) {
        self.bytes(field, &inner.0);
    }
}

fn codec_entry((f1, f2, f3): (u64, u64, Option<u64>)) -> Proto {
    let mut entry = Proto::default();
    entry.uint(1, f1);
    entry.uint(2, f2);
    if let Some(f3) = f3 {
        entry.uint(3, f3);
    }
    entry
}

/// The tail every blob ends with: the library name, `f8`, the codec list in the
/// given order, `f13`, `f14 = 2`, `f16 = 0`.
fn blob_tail(blob: &mut Proto, codecs: &[(u64, u64, Option<u64>)], f13: u64) {
    blob.bytes(6, VICEROY.as_bytes());
    blob.uint(8, 0);
    for &entry in codecs {
        blob.message(9, codec_entry(entry));
    }
    blob.uint(13, f13);
    blob.uint(14, 2);
    blob.uint(16, 0);
}

/// The audio offer's blob, before compression: one audio stream, in field 3, whose
/// SSRC is `ssrc`.
fn audio_offer_blob(ssrc: u32) -> Vec<u8> {
    let mut blob = Proto::default();
    blob.uint(1, 1);
    blob.uint(2, 1);
    let mut stream = Proto::default();
    stream.uint(1, u64::from(ssrc));
    stream.uint(2, 0);
    stream.uint(3, 0);
    // The RTP payload types this side takes, one bit each; `0x1000` is AAC-ELD's
    // 101. Apple's viewer sends the same `0x5E7F`. The Mac sets the rate itself.
    stream.uint(4, 24_191);
    stream.uint(5, 0);
    stream.uint(6, 0);
    blob.message(3, stream);
    blob_tail(&mut blob, CODEC_ENTRIES, AUDIO_F13);
    blob.0
}

/// One codec's capability line in the video stream, repeated per level.
fn video_codec_level(which: u64) -> Proto {
    let mut level = Proto::default();
    level.uint(1, 1);
    level.uint(2, which);
    level.uint(3, 50_115);
    level.uint(4, 0);
    level
}

fn video_codec(id: u64, levels: &[u64], features: &str, f4: u64) -> Proto {
    let mut codec = Proto::default();
    codec.uint(1, id);
    for &level in levels {
        codec.message(2, video_codec_level(level));
    }
    codec.bytes(3, features.as_bytes());
    codec.uint(4, f4);
    codec
}

/// The screen-video offer's blob: one stream at `size` backing pixels offering
/// H.264 (RTP payload `123`) and HEVC (`100`), `tiles` pictures to a frame. The stream's fields
/// follow `initWithScreenSSRC:…:customVideoWidth:customVideoHeight:tilesPerFrame:
/// ltrpEnabled:pixelFormats:…`; the Mac answers with HEVC.
fn video_offer_blob(ssrc: u32, (width, height): (u16, u16), tiles: u64) -> Vec<u8> {
    let mut blob = Proto::default();
    blob.uint(1, 1);
    blob.uint(2, 1);
    let mut stream = Proto::default();
    stream.uint(1, u64::from(ssrc));
    stream.uint(2, 0);
    stream.message(
        3,
        video_codec(
            123,
            &[1, 2, 1, 2],
            "FLS;MS:-1;LF:-1;LTR;CABAC;POS:0;EOD:1;HTS:2;RR:3;AR:4/3,5/8;XR:4/3,5/8;",
            1,
        ),
    );
    stream.message(
        3,
        video_codec(
            100,
            &[1, 2],
            "FLS;LF:-1;POS:5;EOD:1;HTS:2;RR:3;POSE:4;AR:4/3,5/8;XR:4/3,5/8;",
            14,
        ),
    );
    stream.uint(4, u64::from(width));
    stream.uint(5, u64::from(height));
    stream.uint(6, tiles);
    stream.uint(7, 1);
    stream.uint(8, 63);
    stream.uint(9, 1);
    stream.uint(12, 1);
    blob.message(5, stream);
    // The same entries, with `{0, 40000000, 12288}` moved to the front.
    let mut codecs = CODEC_ENTRIES.to_vec();
    let bitrate = codecs.remove(4);
    codecs.insert(0, bitrate);
    blob_tail(&mut blob, &codecs, VIDEO_F13);
    blob.0
}

/// A value in the offer dictionary. Only what the offers hold.
enum Plist<'a> {
    Data(&'a [u8]),
    Int(u8),
    Str(&'a str),
}

/// A binary property list (`bplist00`) of one dictionary with these entries, in
/// this order: an object table (the dictionary, its keys, its values), an offset
/// table, and a 32-byte trailer.
fn bplist_dict(entries: &[(&str, Plist<'_>)]) -> Vec<u8> {
    fn marker(out: &mut Vec<u8>, kind: u8, count: usize) {
        if count < 15 {
            out.push(kind | count as u8);
        } else if count < 256 {
            out.extend_from_slice(&[kind | 0x0f, 0x10, count as u8]);
        } else {
            out.extend_from_slice(&[kind | 0x0f, 0x11]);
            out.extend_from_slice(&(count as u16).to_be_bytes());
        }
    }

    let mut out = b"bplist00".to_vec();
    let mut offsets = Vec::with_capacity(1 + entries.len() * 2);
    // Object 0: the dictionary, whose key refs are 1..=n and value refs n+1..=2n.
    offsets.push(out.len());
    let n = entries.len();
    marker(&mut out, 0xd0, n);
    for i in 0..n {
        out.push((1 + i) as u8);
    }
    for i in 0..n {
        out.push((1 + n + i) as u8);
    }
    for (key, _) in entries {
        offsets.push(out.len());
        marker(&mut out, 0x50, key.len());
        out.extend_from_slice(key.as_bytes());
    }
    for (_, value) in entries {
        offsets.push(out.len());
        match value {
            Plist::Data(bytes) => {
                marker(&mut out, 0x40, bytes.len());
                out.extend_from_slice(bytes);
            }
            Plist::Int(v) => out.extend_from_slice(&[0x10, *v]),
            Plist::Str(s) => {
                marker(&mut out, 0x50, s.len());
                out.extend_from_slice(s.as_bytes());
            }
        }
    }
    let table = out.len();
    for offset in &offsets {
        out.extend_from_slice(&(*offset as u16).to_be_bytes());
    }
    // Trailer: five unused bytes, sort version, offset int size, object ref size,
    // object count, top object, offset table offset.
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 2, 1]);
    out.extend_from_slice(&(offsets.len() as u64).to_be_bytes());
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(&(table as u64).to_be_bytes());
    out
}

fn deflate(blob: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(blob).expect("writing to a Vec cannot fail");
    encoder.finish().expect("finishing a Vec-backed zlib stream cannot fail")
}

/// One AVConference offer plist: the negotiator mode, the compressed blob, the
/// viewer's endpoint info and the call id.
fn offer(mode: u8, blob: &[u8], call_id: &str) -> Vec<u8> {
    let compressed = deflate(blob);
    bplist_dict(&[
        ("avcMediaStreamOptionRemoteEndpointInfo", Plist::Data(REMOTE_ENDPOINT_INFO)),
        ("avcMediaStreamNegotiatorMode", Plist::Int(mode)),
        ("avcMediaStreamNegotiatorMediaBlob", Plist::Data(&compressed)),
        ("avcMediaStreamOptionCallID", Plist::Str(call_id)),
    ])
}

// ---------------------------------------------------------------------------
// The 0x1c message and the replies
// ---------------------------------------------------------------------------

/// Keys are `(viewer_to_server, server_to_viewer)`.
pub type KeyPair = (MasterKey, MasterKey);

/// `RFBMediaStreamServerConfiguration`, version 3.
///
/// ```text
/// +0x00 u8   0x1c
/// +0x01 u8   pad
/// +0x02 u16  body length (everything after this field)
/// +0x04 u16  version = 3
/// +0x06 u32  flags
/// +0x0a u16  audio offer length
/// +0x0c u16  video1 offer length
/// +0x0e u16  video2 offer length, 0 for one display
/// +0x14 16B  session UUID
/// +0x24 46B  audio SRTP master key, viewer -> server
/// +0x52 46B  audio SRTP master key, server -> viewer
/// +0x80      audio offer, then 46B video1 key v->s, 46B video1 key s->v, video1 offer,
///            and for a second display 46B video2 key v->s, 46B s->v, video2 offer
/// ```
///
/// `videos` is each display's offer and keys, one or two.
fn configuration_message(
    flags: u32,
    session_uuid: &[u8; 16],
    audio_offer: &[u8],
    audio_keys: &KeyPair,
    videos: &[(&[u8], &KeyPair)],
) -> Vec<u8> {
    let mut msg = vec![0u8; 0x80];
    msg[0] = 0x1c;
    msg[4..6].copy_from_slice(&3u16.to_be_bytes());
    msg[6..10].copy_from_slice(&flags.to_be_bytes());
    msg[0x0a..0x0c].copy_from_slice(&(audio_offer.len() as u16).to_be_bytes());
    for (index, (offer, _)) in videos.iter().enumerate().take(MAX_DISPLAYS) {
        let at = 0x0c + 2 * index;
        msg[at..at + 2].copy_from_slice(&(offer.len() as u16).to_be_bytes());
    }
    msg[0x14..0x24].copy_from_slice(session_uuid);
    msg[0x24..0x52].copy_from_slice(&audio_keys.0);
    msg[0x52..0x80].copy_from_slice(&audio_keys.1);
    msg.extend_from_slice(audio_offer);
    for (offer, keys) in videos.iter().take(MAX_DISPLAYS) {
        msg.extend_from_slice(&keys.0);
        msg.extend_from_slice(&keys.1);
        msg.extend_from_slice(offer);
    }
    let body_len = (msg.len() - 4) as u16;
    msg[2..4].copy_from_slice(&body_len.to_be_bytes());
    msg
}

/// One session's negotiation identity: the ids, SSRCs and keys every offer of the
/// session carries. The Mac starts a fresh stream (new SSRC, same ports) on each
/// offer, so the keys need not change between them.
struct Offers {
    session_uuid: [u8; 16],
    call_id: String,
    audio_keys: KeyPair,
    /// This side's SSRCs, which its RTCP reports carry.
    audio_ssrc: u32,
    /// Each display's video leg, in the Mac's order: its first virtual display,
    /// then its second.
    videos: Vec<VideoOffer>,
    /// Whether the picture is decoded here, and so offered in strips where the
    /// display's height allows ([`in_strips`]).
    decoded: bool,
}

/// One video leg's keys, and this side's SSRC on it.
struct VideoOffer {
    keys: KeyPair,
    ssrc: u32,
}

impl Offers {
    /// For `displays` virtual displays, a video leg each, whose picture is
    /// `decoded` here or passed.
    fn new(displays: usize, decoded: bool) -> Self {
        let key = || {
            let mut k = [0u8; 46];
            rand::fill(&mut k[..]);
            k
        };
        let audio_keys = (key(), key());
        let videos = (0..displays.clamp(1, MAX_DISPLAYS))
            .map(|_| VideoOffer { keys: (key(), key()), ssrc: rand::random() })
            .collect();
        let uuid = uuid::Uuid::new_v4();
        Self {
            session_uuid: *uuid.as_bytes(),
            call_id: uuid.hyphenated().to_string().to_ascii_uppercase(),
            audio_keys,
            audio_ssrc: rand::random(),
            videos,
            decoded,
        }
    }

    /// Whether the leg of a display of `size` is offered in strips.
    fn strips(&self, size: (u16, u16)) -> bool {
        self.decoded && in_strips(size.1)
    }

    /// The `0x1c` message for displays of `sizes` backing pixels, one per video
    /// leg. Every stream carries the session's one call id, as Apple's viewer's do.
    fn configuration(&self, sizes: &[(u16, u16)]) -> Vec<u8> {
        let audio = offer(MODE_AUDIO, &audio_offer_blob(self.audio_ssrc), &self.call_id);
        let offers: Vec<Vec<u8>> = self
            .videos
            .iter()
            .zip(sizes)
            .map(|(video, size)| {
                let tiles = if self.strips(*size) { STRIPS } else { 1 };
                offer(MODE_VIDEO, &video_offer_blob(video.ssrc, *size, tiles as u64), &self.call_id)
            })
            .collect();
        let videos: Vec<(&[u8], &KeyPair)> =
            offers.iter().zip(&self.videos).map(|(offer, video)| (offer.as_slice(), &video.keys)).collect();
        let flags = if videos.len() > 1 { FLAGS | FLAG_60FPS_SECOND } else { FLAGS };
        configuration_message(flags, &self.session_uuid, &audio, &self.audio_keys, &videos)
    }
}

/// What the Mac put in an encoding-1010 rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaReply {
    /// Message 1: the streams are being set up; audio comes from and RTCP goes to
    /// `audio_port`, the picture from `video_port`, on the Mac's address, and this
    /// side receives on the same port numbers. A display change re-sends it on its
    /// own, with no stream behind it until the next offer. `video2_port` is the
    /// second display's leg, when the Mac has two virtual displays to send.
    Ports { audio_port: u16, video_port: u16, video2_port: Option<u16> },
    /// Message 2: AVConference accepted the offer, with an answer for this many
    /// video legs.
    Answer { videos: usize },
    /// Message 3: the Mac could not start the streams.
    Error { kind: u32, sub_code: u32 },
    /// A message type this client does not know.
    Other(u16),
}

/// Parse the body of an encoding-1010 rectangle (after its `u16` size).
pub fn parse_media_reply(body: &[u8]) -> anyhow::Result<MediaReply> {
    anyhow::ensure!(body.len() >= 8, "a media-stream reply of {} bytes has no header", body.len());
    let kind = u16::from_be_bytes([body[0], body[1]]);
    match kind {
        1 => {
            anyhow::ensure!(
                body.len() >= 36,
                "media-stream message 1 is {} bytes, shorter than its three port records",
                body.len()
            );
            let audio_flags =
                u32::from_be_bytes(body[10..14].try_into().expect("four bytes checked"));
            let video_flags =
                u32::from_be_bytes(body[16..20].try_into().expect("four bytes checked"));
            let video2_flags =
                u32::from_be_bytes(body[22..26].try_into().expect("four bytes checked"));
            anyhow::ensure!(
                audio_flags & video_flags & 1 != 0,
                "media-stream message 1 did not enable both its audio and video legs"
            );
            Ok(MediaReply::Ports {
                audio_port: u16::from_be_bytes([body[8], body[9]]),
                video_port: u16::from_be_bytes([body[14], body[15]]),
                video2_port: (video2_flags & 1 != 0).then(|| u16::from_be_bytes([body[20], body[21]])),
            })
        }
        2 => {
            anyhow::ensure!(
                body.len() >= 18,
                "media-stream answer is {} bytes, too short for its offer lengths",
                body.len()
            );
            let audio = usize::from(u16::from_be_bytes([body[8], body[9]]));
            let video = usize::from(u16::from_be_bytes([body[10], body[11]]));
            let video2 = usize::from(u16::from_be_bytes([body[12], body[13]]));
            anyhow::ensure!(
                body.len() == 18 + audio + video + video2,
                "media-stream answer is {} bytes, not the {} its offer lengths describe",
                body.len(),
                18 + audio + video + video2
            );
            Ok(MediaReply::Answer { videos: if video2 == 0 { 1 } else { 2 } })
        }
        3 => {
            anyhow::ensure!(
                body.len() >= 16,
                "media-stream error message is {} bytes, too short for its codes",
                body.len()
            );
            Ok(MediaReply::Error {
                kind: u32::from_be_bytes([body[8], body[9], body[10], body[11]]),
                sub_code: u32::from_be_bytes([body[12], body[13], body[14], body[15]]),
            })
        }
        other => Ok(MediaReply::Other(other)),
    }
}

// ---------------------------------------------------------------------------
// SRTP and SRTCP (RFC 3711): AES-256 counter mode, HMAC-SHA1-80
// ---------------------------------------------------------------------------

/// The authentication tag both directions carry.
const AUTH_TAG_LEN: usize = 10;

type HmacSha1 = Hmac<Sha1>;

/// AES counter-mode keystream from `iv`, XORed into `data`. The counter is the
/// whole 128-bit block, which agrees with RFC 3711's 16-bit block counter for any
/// packet under a megabyte.
fn aes_ctr_xor(cipher: &Aes256, iv: u128, data: &mut [u8]) {
    for (i, chunk) in data.chunks_mut(16).enumerate() {
        let mut block = iv.wrapping_add(i as u128).to_be_bytes();
        cipher.encrypt_block((&mut block).into());
        for (byte, key) in chunk.iter_mut().zip(block) {
            *byte ^= key;
        }
    }
}

/// The session keys of one direction of one stream: RFC 3711 §4.3 with
/// `key_derivation_rate = 0`, from labels `base` (encryption), `base + 1`
/// (authentication) and `base + 2` (salt) — 0 for SRTP, 3 for SRTCP.
struct SessionKeys {
    cipher: Aes256,
    auth: [u8; 20],
    salt: u128,
}

impl SessionKeys {
    fn derive(master: &MasterKey, base: u8) -> Self {
        let master_key: [u8; 32] = master[..32].try_into().expect("46 > 32");
        let master_cipher = Aes256::new(&master_key.into());
        let mut master_salt = [0u8; 16];
        master_salt[2..].copy_from_slice(&master[32..46]);
        let x = u128::from_be_bytes(master_salt);
        let derive = |label: u8, n: usize| {
            let mut out = vec![0u8; n];
            aes_ctr_xor(&master_cipher, (x ^ (u128::from(label) << 48)) << 16, &mut out);
            out
        };
        let key: [u8; 32] = derive(base, 32).try_into().expect("32 bytes were asked for");
        let auth: [u8; 20] = derive(base + 1, 20).try_into().expect("20 bytes were asked for");
        let mut salt = [0u8; 16];
        salt[2..].copy_from_slice(&derive(base + 2, 14));
        Self { cipher: Aes256::new(&key.into()), auth, salt: u128::from_be_bytes(salt) }
    }

    fn iv(&self, ssrc: u32, index: u64) -> u128 {
        (self.salt << 16) ^ (u128::from(ssrc) << 64) ^ (u128::from(index) << 16)
    }

    fn tag(&self, parts: &[&[u8]]) -> [u8; AUTH_TAG_LEN] {
        let mut mac = HmacSha1::new_from_slice(&self.auth).expect("HMAC takes any key length");
        for part in parts {
            mac.update(part);
        }
        let full = mac.finalize().into_bytes();
        full[..AUTH_TAG_LEN].try_into().expect("SHA-1 is 20 bytes")
    }
}

/// Why a datagram was not an RTP packet this side could use.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SrtpError {
    #[error("not RTP version 2, or too short to be")]
    NotRtp,
    #[error("an RTCP packet")]
    Rtcp,
    #[error("the authentication tag does not match")]
    Forged,
    #[error("a duplicate, or older than a packet already received")]
    Stale,
}

/// An RTP packet's header fields, and where its payload sits in the datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub payload_type: u8,
    pub marker: bool,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    /// The Mac's mark on a picture that predicts only from one this side
    /// acknowledged ([`rtcp_reference_ack`]): its answer to [`rtcp_refresh`].
    pub refresh: bool,
    /// The payload, as a range of the datagram, without the authentication tag.
    pub payload: (usize, usize),
}

/// The profile of the header extension the Mac's video packets carry, less the
/// bits that vary, and the bit of it that marks a refresh picture.
const EXTENSION_PROFILE: u16 = 0x9301;
const EXTENSION_VARIES: u16 = 0x0030;
const EXTENSION_REFRESH: u16 = 0x0020;

/// Read an RTP header. RTCP (packet types 200–207 in the whole second byte, which
/// RTP would read as payload types 72–79) is told apart first.
fn rtp_header(data: &[u8]) -> Result<RtpHeader, SrtpError> {
    if data.len() < 12 || data[0] >> 6 != 2 {
        return Err(SrtpError::NotRtp);
    }
    if (200..=207).contains(&data[1]) {
        return Err(SrtpError::Rtcp);
    }
    let cc = usize::from(data[0] & 0x0f);
    let mut at = 12 + 4 * cc;
    let mut refresh = false;
    if data[0] & 0x10 != 0 {
        if data.len() < at + 4 {
            return Err(SrtpError::NotRtp);
        }
        let profile = u16::from_be_bytes([data[at], data[at + 1]]);
        refresh = profile & !EXTENSION_VARIES == EXTENSION_PROFILE && profile & EXTENSION_REFRESH != 0;
        let words = usize::from(u16::from_be_bytes([data[at + 2], data[at + 3]]));
        at += 4 + 4 * words;
    }
    if data.len() < at + AUTH_TAG_LEN {
        return Err(SrtpError::NotRtp);
    }
    Ok(RtpHeader {
        payload_type: data[1] & 0x7f,
        marker: data[1] & 0x80 != 0,
        sequence: u16::from_be_bytes([data[2], data[3]]),
        timestamp: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
        ssrc: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
        refresh,
        payload: (at, data.len() - AUTH_TAG_LEN),
    })
}

/// The receiving side of one leg's SRTP: its session keys, and the rollover
/// counter that extends a 16-bit sequence number to a packet index.
pub struct SrtpReceiver {
    keys: SessionKeys,
    /// For each SSRC, the highest sequence number authenticated so far and its
    /// rollover counter. A picture in strips comes under an SSRC for each, with
    /// sequence numbers of its own, and every offer starts a stream with new ones:
    /// the [`STRIPS`] heard from most recently are kept, the newest last.
    last: Vec<(u32, u16, u32)>,
}

impl SrtpReceiver {
    pub fn new(master: &MasterKey) -> Self {
        Self { keys: SessionKeys::derive(master, 0), last: Vec::with_capacity(STRIPS) }
    }

    /// RFC 3711 Appendix A: the rollover counter `seq` most likely belongs to.
    fn guess_roc(&self, ssrc: u32, seq: u16) -> u32 {
        let Some(&(_, last_seq, roc)) = self.last.iter().find(|(last, ..)| *last == ssrc) else {
            return 0;
        };
        if last_seq < 0x8000 {
            if seq.wrapping_sub(last_seq) > 0x8000 && seq > last_seq { roc.wrapping_sub(1) } else { roc }
        } else if seq < last_seq.wrapping_sub(0x8000) {
            roc.wrapping_add(1)
        } else {
            roc
        }
    }

    /// Authenticate and decrypt one datagram in place, returning its header; the
    /// payload is then `data[header.payload.0..header.payload.1]` in the clear.
    ///
    /// An authentic packet no newer than the newest already received is refused
    /// as [`SrtpError::Stale`]: a duplicate, or one overtaken on the way. The sound
    /// decoder keeps state from unit to unit, and the depacketizer has no use for
    /// either, so neither leg takes them.
    pub fn unprotect(&mut self, data: &mut [u8]) -> Result<RtpHeader, SrtpError> {
        let header = rtp_header(data)?;
        let roc = self.guess_roc(header.ssrc, header.sequence);
        let (end, len) = (header.payload.1, data.len());
        let tag = self.keys.tag(&[&data[..end], &roc.to_be_bytes()]);
        if tag[..] != data[end..len] {
            return Err(SrtpError::Forged);
        }
        let index = (u64::from(roc) << 16) | u64::from(header.sequence);
        let known = self.last.iter().position(|(ssrc, ..)| *ssrc == header.ssrc);
        if let Some(at) = known {
            let (_, seq, last_roc) = self.last[at];
            if index <= (u64::from(last_roc) << 16 | u64::from(seq)) {
                return Err(SrtpError::Stale);
            }
            self.last.remove(at);
        } else if self.last.len() == STRIPS {
            self.last.remove(0);
        }
        let iv = self.keys.iv(header.ssrc, index);
        aes_ctr_xor(&self.keys.cipher, iv, &mut data[header.payload.0..end]);
        self.last.push((header.ssrc, header.sequence, roc));
        Ok(header)
    }
}

/// The sending side of one SRTCP stream: RFC 3711 §3.4, every packet encrypted
/// (`E = 1`) under a 31-bit index of its own.
pub struct SrtcpSender {
    keys: SessionKeys,
    index: u32,
}

impl SrtcpSender {
    pub fn new(master: &MasterKey) -> Self {
        Self { keys: SessionKeys::derive(master, 3), index: 0 }
    }

    /// `report` protected: its first eight bytes in the clear, the rest encrypted,
    /// then `E || index` and the tag over all of it.
    pub fn protect(&mut self, report: &[u8]) -> Vec<u8> {
        debug_assert!(report.len() >= 8, "an RTCP packet has an eight-byte header");
        let ssrc = u32::from_be_bytes([report[4], report[5], report[6], report[7]]);
        let mut out = report.to_vec();
        let iv = self.keys.iv(ssrc, u64::from(self.index));
        aes_ctr_xor(&self.keys.cipher, iv, &mut out[8..]);
        out.extend_from_slice(&(0x8000_0000 | self.index).to_be_bytes());
        let tag = self.keys.tag(&[&out]);
        out.extend_from_slice(&tag);
        self.index = (self.index + 1) & 0x7fff_ffff;
        out
    }
}

/// The receiving side of one SRTCP stream: the Mac's reports, which it sends on
/// each leg about once a second whether or not the leg carries media. They are
/// authenticated for liveness and never decrypted, since nothing reads them.
pub struct SrtcpReceiver {
    keys: SessionKeys,
    /// The highest SRTCP index authenticated from each sender SSRC, kept for as
    /// long as the keys: every offer starts a stream with a new SSRC under the
    /// same keys, and an earlier stream's reports must not come back.
    last: std::collections::HashMap<u32, u32>,
}

impl SrtcpReceiver {
    pub fn new(master: &MasterKey) -> Self {
        Self { keys: SessionKeys::derive(master, 3), last: std::collections::HashMap::new() }
    }

    /// Whether `data` is an authentic report newer than any already received:
    /// RFC 3711 §3.4's tag over the packet through its `E || index` word.
    pub fn authenticate(&mut self, data: &[u8]) -> Result<(), SrtpError> {
        if data.len() < 8 + 4 + AUTH_TAG_LEN || data[0] >> 6 != 2 {
            return Err(SrtpError::NotRtp);
        }
        let end = data.len() - AUTH_TAG_LEN;
        if self.keys.tag(&[&data[..end]])[..] != data[end..] {
            return Err(SrtpError::Forged);
        }
        let ssrc = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let index = u32::from_be_bytes([data[end - 4], data[end - 3], data[end - 2], data[end - 1]]) & 0x7fff_ffff;
        if self.last.get(&ssrc).is_some_and(|&last| index <= last) {
            return Err(SrtpError::Stale);
        }
        self.last.insert(ssrc, index);
        Ok(())
    }
}

/// An RTCP receiver report with no report blocks, from `ssrc`. The Mac stops a
/// stream that hears nothing from its receiver for a few seconds.
pub fn rtcp_receiver_report(ssrc: u32) -> [u8; 8] {
    let mut report = [0x80, 201, 0, 1, 0, 0, 0, 0];
    report[4..].copy_from_slice(&ssrc.to_be_bytes());
    report
}

/// AVConference's acknowledgement of a picture this side has whole, by its RTP
/// `timestamp`: an APP packet whose name is the number 5. The Mac's encoder keeps
/// the newest acknowledged picture as a long-term reference, which is what lets it
/// answer [`rtcp_refresh`] without an IDR. Alone in its datagram, as the rate
/// report is.
pub fn rtcp_reference_ack(ssrc: u32, timestamp: u32) -> [u8; 16] {
    let mut ack = [0x80, 204, 0, 3, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0];
    ack[4..8].copy_from_slice(&ssrc.to_be_bytes());
    ack[12..].copy_from_slice(&timestamp.to_be_bytes());
    ack
}

/// AVConference's request for a picture to go on from after a loss, from `ssrc`
/// about `media_ssrc`: payload-specific feedback of format 2 carrying the
/// stream's `size`, which the Mac checks against its encoder's. It answers with a
/// picture predicted from the last one acknowledged, marked
/// [`RtpHeader::refresh`], or with an IDR when it holds none; the IDR is a
/// fraction of the size of the one a PLI brings.
pub fn rtcp_refresh(ssrc: u32, media_ssrc: u32, size: (u16, u16)) -> [u8; 16] {
    let mut refresh = [0x82, 206, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    refresh[4..8].copy_from_slice(&ssrc.to_be_bytes());
    refresh[8..12].copy_from_slice(&media_ssrc.to_be_bytes());
    refresh[12..14].copy_from_slice(&size.0.to_be_bytes());
    refresh[14..].copy_from_slice(&size.1.to_be_bytes());
    refresh
}

/// A Picture Loss Indication (RFC 4585 §6.3.1) from `ssrc` about `media_ssrc`: the
/// Mac answers it with an IDR.
pub fn rtcp_pli(ssrc: u32, media_ssrc: u32) -> [u8; 12] {
    let mut pli = [0x81, 206, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0];
    pli[4..8].copy_from_slice(&ssrc.to_be_bytes());
    pli[8..12].copy_from_slice(&media_ssrc.to_be_bytes());
    pli
}

/// The picture leg's RTP clock, in ticks a second.
const VIDEO_CLOCK: f64 = 24_000.0;

/// The bandwidth estimate a rate report carries, in kbit/s: the controller's
/// 60 Mbit/s ceiling. The Mac's controller moves on the one-way delay alone, and
/// this side makes no estimate of its own.
const REPORTED_BANDWIDTH: u16 = 60_000;

/// What the Mac's rate controller hears from this side: the `RCTL` report Apple's
/// viewer sends every 50 ms on the picture's leg, built from the picture packets
/// that arrived here and nothing downstream of them. See `docs/apple-vnc-889.md`,
/// "Rate control", for the layout and what the Mac does with it.
pub struct RateFeedback {
    /// Where the report's clock counts from.
    epoch: std::time::Instant,
    /// The Mac's SSRC for the stream the rest describes. Each offer starts a stream
    /// with a new one, and the report starts again with it.
    ssrc: u32,
    /// The last picture packet's RTP timestamp, and when it arrived.
    last: Option<(u32, std::time::Instant)>,
    /// Picture packets of this stream received, which the report carries mod 4096.
    received: u32,
    delay: OneWayDelay,
}

impl RateFeedback {
    pub fn new(epoch: std::time::Instant) -> Self {
        Self { epoch, ssrc: 0, last: None, received: 0, delay: OneWayDelay::default() }
    }

    /// An authentic picture packet from `ssrc`, stamped `timestamp`, that arrived at
    /// `at`.
    pub fn received(&mut self, ssrc: u32, timestamp: u32, at: std::time::Instant) {
        if ssrc != self.ssrc {
            *self = Self { ssrc, ..Self::new(self.epoch) };
        }
        self.last = Some((timestamp, at));
        self.received = self.received.wrapping_add(1);
        self.delay.sample(timestamp, at);
    }

    /// The one-way delay the next report carries, in seconds.
    pub fn delay(&self) -> f64 {
        self.delay.owrd
    }

    /// The report from `ssrc` at `now`, once a picture packet has arrived to echo.
    /// The Mac takes it only alone in its datagram.
    pub fn report(&self, ssrc: u32, now: std::time::Instant) -> Option<[u8; 32]> {
        let (timestamp, at) = self.last?;
        let millis = |d: std::time::Duration| d.as_millis().min(0xffff) as u16;
        let clock = (now.saturating_duration_since(self.epoch).as_secs_f64() * 1024.0) as u64 as u16;
        let delay = (self.delay.owrd * 8192.0).min(f64::from(u16::MAX)) as u16;
        let mut report = [0u8; 32];
        report[..4].copy_from_slice(&[0x80, 204, 0, 7]);
        report[4..8].copy_from_slice(&ssrc.to_be_bytes());
        report[8..12].copy_from_slice(b"RCTL");
        report[12..16].copy_from_slice(&[0x85, 0, 0, 4]);
        report[16..18].copy_from_slice(&((timestamp >> 8) as u16).to_be_bytes());
        report[22..24].copy_from_slice(&millis(now.saturating_duration_since(at)).to_be_bytes());
        report[24..26].copy_from_slice(&clock.to_be_bytes());
        report[26..28].copy_from_slice(&delay.to_be_bytes());
        report[28..30].copy_from_slice(&((self.received & 0xfff) as u16).to_be_bytes());
        report[30..32].copy_from_slice(&REPORTED_BANDWIDTH.to_be_bytes());
        Some(report)
    }
}

/// The one-way relative delay, as Apple's receiver estimates it. Each picture's
/// first packet gives a lag: its arrival less its RTP timestamp, both counted from
/// the stream's first picture. A short average follows the lag, a long one settles
/// on its floor, and the delay is how far the short one stands above it — a queue
/// building between the Mac and here, with no clock shared between the two.
#[derive(Default)]
struct OneWayDelay {
    /// The first picture's timestamp and arrival, which lags count from.
    first: Option<(u32, std::time::Instant)>,
    /// The latest picture's timestamp, and ticks since the first.
    previous: u32,
    ticks: u64,
    short: f64,
    long: f64,
    owrd: f64,
}

/// A lag this far, in seconds, from either average is a clock that jumped rather
/// than a queue, and the estimate starts again from it.
const SPURIOUS_LAG: f64 = 30.0;

impl OneWayDelay {
    fn sample(&mut self, timestamp: u32, at: std::time::Instant) {
        let Some((_, since)) = self.first else {
            return self.restart(timestamp, at);
        };
        // A later packet of the same picture, or one out of order.
        let step = timestamp.wrapping_sub(self.previous) as i32;
        if step <= 0 {
            return;
        }
        self.previous = timestamp;
        self.ticks += step as u64;
        let lag = at.saturating_duration_since(since).as_secs_f64() - self.ticks as f64 / VIDEO_CLOCK;
        if lag - self.short > SPURIOUS_LAG || self.long - lag > SPURIOUS_LAG {
            return self.restart(timestamp, at);
        }
        self.long = 0.9999 * self.long + 0.0001 * lag;
        self.short = 0.9 * self.short + 0.1 * lag;
        self.owrd = self.short - self.long;
        if self.owrd < 0.0 {
            self.long = self.short;
            self.owrd = 0.0;
        }
    }

    fn restart(&mut self, timestamp: u32, at: std::time::Instant) {
        *self = Self { first: Some((timestamp, at)), previous: timestamp, ..Self::default() };
    }
}

// ---------------------------------------------------------------------------
// HEVC over RTP (RFC 7798)
// ---------------------------------------------------------------------------

/// The NAL unit type of a two-byte HEVC NAL header's first byte.
fn nal_type(first: u8) -> u8 {
    (first >> 1) & 0x3f
}

/// RFC 7798's aggregation packet and fragmentation unit.
const NAL_AP: u8 = 48;
const NAL_FU: u8 = 49;

/// Whether a NAL unit type starts a picture the decoder can begin at: an IRAP
/// picture (16–23) or a parameter set ahead of one.
fn is_random_access(kind: u8) -> bool {
    (16..=23).contains(&kind) || (32..=34).contains(&kind)
}

/// One picture's NAL units, in order, without start codes.
pub type AccessUnit = Vec<Vec<u8>>;

/// What one packet did to the stream.
#[derive(Debug, PartialEq, Eq)]
pub enum Depacketized {
    /// Nothing complete yet.
    Pending,
    /// A whole access unit, decodable from what came before it.
    Unit(AccessUnit),
    /// Packets were lost: everything up to the next picture the stream can go on
    /// from is dropped, and the sender should be asked for one —
    /// [`Depacketizer::resumes_at_refresh`] says which kind.
    Lost,
}

/// One picture's packets, put back together: the reassembly under both
/// [`Depacketizer`] and [`Strips`], which each decide what a whole picture is
/// worth where it stands in its stream.
///
/// A picture ends at the packet with the marker bit, or where the timestamp moves
/// on. A gap in the sequence numbers damages the picture it fell in.
#[derive(Default)]
struct Reassembler {
    /// Whether the payloads carry decoding order numbers, as a stream in strips
    /// does, and not as RFC 7798 lays them out: an aggregation packet has the one
    /// number, with none between its units, and every fragment of a unit has it,
    /// not the first alone.
    donl: bool,
    next_sequence: Option<u16>,
    timestamp: Option<u32>,
    unit: AccessUnit,
    fragment: Option<Vec<u8>>,
    /// The current picture lost a packet.
    damaged: bool,
    /// The current picture carries the Mac's refresh mark.
    refresh: bool,
    /// The current picture's decoding order number, which each of its packets
    /// carries.
    don: Option<u16>,
}

/// What one packet did to the picture in flight.
struct Pushed {
    /// Packets before this one never came.
    lost: bool,
    /// The picture the packet ended, if every packet of it came.
    whole: Option<Whole>,
}

/// A picture every packet of which came.
struct Whole {
    unit: AccessUnit,
    refresh: bool,
    don: Option<u16>,
}

impl Reassembler {
    fn numbered() -> Self {
        Self { donl: true, ..Self::default() }
    }

    /// Take one packet. `None` for a duplicate, or a late one whose picture has
    /// gone.
    fn push(&mut self, header: &RtpHeader, payload: &[u8]) -> Option<Pushed> {
        let mut lost = false;
        if let Some(expected) = self.next_sequence {
            let ahead = header.sequence.wrapping_sub(expected);
            if ahead >= 0x8000 {
                return None;
            }
            lost = ahead != 0;
        }
        self.next_sequence = Some(header.sequence.wrapping_add(1));
        if self.timestamp.is_some_and(|ts| ts != header.timestamp) && (self.damaged || !self.unit.is_empty()) {
            // The previous picture's last packet (the marked one) never came.
            self.damaged = true;
            self.finish();
        }
        self.damaged |= lost;
        self.timestamp = Some(header.timestamp);
        self.refresh |= header.refresh;
        self.parse(payload);
        let whole = if header.marker { self.finish() } else { None };
        Some(Pushed { lost, whole })
    }

    fn parse(&mut self, payload: &[u8]) {
        // The number sits after the payload header, and in a fragment after the
        // fragment header that follows it.
        let numbered = if self.donl { 2 } else { 0 };
        if payload.len() < 2 {
            self.damaged = true;
            return;
        }
        let kind = nal_type(payload[0]);
        let body = if kind == NAL_FU { 3 } else { 2 };
        let Some(rest) = payload.get(body + numbered..) else {
            self.damaged = true;
            return;
        };
        if self.donl {
            self.don.get_or_insert(u16::from_be_bytes([payload[body], payload[body + 1]]));
        }
        match kind {
            NAL_AP => {
                let mut rest = rest;
                while rest.len() >= 2 {
                    let size = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
                    if size < 2 || rest.len() < 2 + size {
                        self.damaged = true;
                        return;
                    }
                    self.unit.push(rest[2..2 + size].to_vec());
                    rest = &rest[2 + size..];
                }
            }
            NAL_FU => {
                let fu = payload[2];
                let (start, end, kind) = (fu & 0x80 != 0, fu & 0x40 != 0, fu & 0x3f);
                if start {
                    let mut nal = vec![(payload[0] & 0x81) | (kind << 1), payload[1]];
                    nal.extend_from_slice(rest);
                    self.fragment = Some(nal);
                } else if let Some(nal) = self.fragment.as_mut() {
                    nal.extend_from_slice(rest);
                } else {
                    self.damaged = true;
                    return;
                }
                if end && let Some(nal) = self.fragment.take() {
                    self.unit.push(nal);
                }
            }
            _ => {
                let mut nal = payload[..2].to_vec();
                nal.extend_from_slice(rest);
                self.unit.push(nal);
            }
        }
    }

    /// Pass over a packet, keeping in step with the stream: the sequence number
    /// and the picture in flight are known when its packets are wanted again, so
    /// the first one pushed is neither read as a duplicate nor taken for the
    /// start of a picture it is the middle of. Whether the packet was one to
    /// pass over, and not a duplicate or a late one.
    fn skip(&mut self, header: &RtpHeader) -> bool {
        if self.next_sequence.is_some_and(|expected| header.sequence.wrapping_sub(expected) >= 0x8000) {
            return false;
        }
        *self = Self {
            donl: self.donl,
            next_sequence: Some(header.sequence.wrapping_add(1)),
            timestamp: Some(header.timestamp),
            damaged: !header.marker,
            ..Self::default()
        };
        true
    }

    /// Close the current picture: it, if it is whole.
    fn finish(&mut self) -> Option<Whole> {
        let unit = std::mem::take(&mut self.unit);
        let damaged = std::mem::take(&mut self.damaged) || self.fragment.take().is_some();
        let refresh = std::mem::take(&mut self.refresh);
        let don = self.don.take();
        (!damaged && !unit.is_empty()).then_some(Whole { unit, refresh, don })
    }
}

/// Access units out of a run of RTP payloads, of a stream that sends each
/// picture whole: RFC 7798 without decoding order numbers.
///
/// A gap in the sequence numbers drops the picture it fell in and everything
/// after it until an IRAP picture or one the Mac marks as a refresh arrives,
/// because every other picture predicts from one that was lost.
#[derive(Default)]
pub struct Depacketizer {
    ssrc: Option<u32>,
    packets: Reassembler,
    /// Whether the stream is decodable from here: a random-access picture has
    /// arrived since the last loss.
    synced: bool,
    /// Out of step by lost packets alone, with every picture before them handed
    /// on: a refresh picture predicts from one of those, so the stream goes on
    /// from it. Not after [`Self::resync`], which gave pictures up.
    resumable: bool,
}

impl Depacketizer {
    pub fn push(&mut self, header: &RtpHeader, payload: &[u8]) -> Depacketized {
        if self.ssrc != Some(header.ssrc) {
            // A new stream, after an offer: it starts with an IDR of its own.
            *self = Self { ssrc: Some(header.ssrc), ..Self::default() };
        }
        let Some(pushed) = self.packets.push(header, payload) else {
            return Depacketized::Pending;
        };
        let mut out = Depacketized::Pending;
        if pushed.lost {
            self.resumable |= self.synced;
            self.synced = false;
            out = Depacketized::Lost;
        }
        if header.marker {
            if let Some(unit) = pushed.whole.and_then(|whole| self.judge(whole)) {
                return Depacketized::Unit(unit);
            }
            if !self.synced {
                out = Depacketized::Lost;
            }
        }
        out
    }

    /// Drop everything up to the next random-access picture: a unit this side
    /// could not keep was lost to every picture that predicts from it.
    pub fn resync(&mut self) {
        self.synced = false;
        self.resumable = false;
    }

    /// Whether a refresh picture is enough to go on from, where the stream is
    /// out of step: the pictures handed on are all still there to predict from.
    pub fn resumes_at_refresh(&self) -> bool {
        !self.synced && self.resumable
    }

    /// Pass over a packet nobody is shown ([`Reassembler::skip`]). Its pictures
    /// are given up, as by [`Self::resync`].
    pub fn skip(&mut self, header: &RtpHeader) {
        if self.ssrc != Some(header.ssrc) {
            *self = Self { ssrc: Some(header.ssrc), ..Self::default() };
        }
        if self.packets.skip(header) {
            self.resync();
        }
    }

    /// A whole picture, if the stream is decodable at it.
    fn judge(&mut self, whole: Whole) -> Option<AccessUnit> {
        if !self.synced
            && (whole.refresh && self.resumable
                || whole.unit.iter().any(|nal| is_random_access(nal_type(nal[0]))))
        {
            self.synced = true;
            self.resumable = false;
        }
        self.synced.then_some(whole.unit)
    }
}

/// Access units out of a display's picture sent in [`STRIPS`] strips, which is
/// what the Mac makes of `tilesPerFrame`.
///
/// The display is cut into strips of its whole width, each a sixteenth-rounded
/// quarter of its height, top to bottom, the last running past the display's
/// last row. Each strip of a frame is coded as a picture of its own and sent
/// under an SSRC of its own, the display's plus the strip's number, with the
/// frame's timestamp. A frame carries only the strips that changed.
///
/// The strips are one HEVC stream all the same: they share their parameter sets
/// and one decoding order, which each packet's decoding order number gives, a
/// picture's order count being its place in it. A strip predicts from its own
/// earlier pictures, but the first after a keyframe may predict from another
/// strip's, so one decoder takes them all, in that order. A keyframe is an IDR
/// of the first strip followed by intra pictures of the others.
///
/// A lost packet, or a picture missing from the order, drops everything up to
/// the next such keyframe: the stream has no refresh pictures.
pub struct Strips {
    /// The display's SSRC, the first strip's.
    base: Option<u32>,
    strips: [Reassembler; STRIPS],
    /// The decoding order number the next picture carries, while the stream is
    /// decodable.
    next: Option<u16>,
}

impl Default for Strips {
    fn default() -> Self {
        Self { base: None, strips: std::array::from_fn(|_| Reassembler::numbered()), next: None }
    }
}

impl Strips {
    /// Take `ssrc` as the stream's or a new stream's, and return the stream's:
    /// the first strip's. A new stream, after an offer, starts with the first
    /// strip's IDR, so an SSRC outside the strips' is one's first.
    pub fn stream(&mut self, ssrc: u32) -> u32 {
        match self.base {
            Some(base) if ssrc.wrapping_sub(base) < STRIPS as u32 => base,
            // That includes the SSRC just below one taken for the first strip's,
            // of a stream first heard at a later strip.
            _ => {
                *self = Self { base: Some(ssrc), ..Self::default() };
                ssrc
            }
        }
    }

    /// Which strip `ssrc` carries.
    fn strip(&mut self, ssrc: u32) -> usize {
        ssrc.wrapping_sub(self.stream(ssrc)) as usize
    }

    /// Take one packet, returning with it the strip it belongs to.
    pub fn push(&mut self, header: &RtpHeader, payload: &[u8]) -> (usize, Depacketized) {
        let strip = self.strip(header.ssrc);
        let Some(pushed) = self.strips[strip].push(header, payload) else {
            return (strip, Depacketized::Pending);
        };
        if pushed.lost {
            self.next = None;
        }
        let out = match pushed.whole {
            Some(Whole { unit, don: Some(don), .. })
                if self.next == Some(don)
                    || strip == 0 && unit.iter().any(|nal| (16..=23).contains(&nal_type(nal[0]))) =>
            {
                self.next = Some(don.wrapping_add(1));
                Depacketized::Unit(unit)
            }
            Some(_) => {
                self.next = None;
                Depacketized::Lost
            }
            None if pushed.lost || header.marker && self.next.is_none() => Depacketized::Lost,
            None => Depacketized::Pending,
        };
        (strip, out)
    }

    /// Drop everything up to the next keyframe.
    pub fn resync(&mut self) {
        self.next = None;
    }

    /// Pass over a packet nobody is shown ([`Reassembler::skip`]), giving its
    /// pictures up.
    pub fn skip(&mut self, header: &RtpHeader) {
        let strip = self.strip(header.ssrc);
        if self.strips[strip].skip(header) {
            self.resync();
        }
    }
}

/// How a leg's packets become access units, by what its stream was offered:
/// each picture whole, or in strips.
enum Assembly {
    Whole(Depacketizer),
    Strips(Box<Strips>),
}

impl Assembly {
    fn new(strips: bool) -> Self {
        if strips { Self::Strips(Box::default()) } else { Self::Whole(Depacketizer::default()) }
    }

    /// The SSRC that names the stream `ssrc` is of, or begins: its own, or of a
    /// stream in strips the first strip's.
    fn stream(&mut self, ssrc: u32) -> u32 {
        match self {
            Self::Whole(_) => ssrc,
            Self::Strips(strips) => strips.stream(ssrc),
        }
    }

    /// Take one packet, returning with it the strip it belongs to, of a stream
    /// in strips.
    fn push(&mut self, header: &RtpHeader, payload: &[u8]) -> (Option<usize>, Depacketized) {
        match self {
            Self::Whole(whole) => (None, whole.push(header, payload)),
            Self::Strips(strips) => {
                let (strip, taken) = strips.push(header, payload);
                (Some(strip), taken)
            }
        }
    }

    fn resync(&mut self) {
        match self {
            Self::Whole(whole) => whole.resync(),
            Self::Strips(strips) => strips.resync(),
        }
    }

    fn resumes_at_refresh(&self) -> bool {
        match self {
            Self::Whole(whole) => whole.resumes_at_refresh(),
            Self::Strips(_) => false,
        }
    }

    fn skip(&mut self, header: &RtpHeader) {
        match self {
            Self::Whole(whole) => whole.skip(header),
            Self::Strips(strips) => strips.skip(header),
        }
    }
}

// ---------------------------------------------------------------------------
// Passing the stream through
// ---------------------------------------------------------------------------

/// NAL unit type of a sequence parameter set.
const NAL_SPS: u8 = 33;

/// An access unit passed to the browser as the Mac sent it: one picture, as the
/// Annex B stream a `VideoDecoder` configured with [`Self::decode`] takes.
#[derive(Debug, PartialEq, Eq)]
pub struct PassedUnit {
    /// The display's size, from the stream's parameter sets.
    pub size: (u16, u16),
    /// The configuration string, from the same.
    pub decode: String,
    /// An IRAP picture, which a decoder can start at.
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// What a sequence parameter set says about the pictures after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamParams {
    /// The cropped picture: the coded size less the conformance window.
    pub size: (u16, u16),
    /// The configuration string, `hev1` for parameter sets in band, per ISO/IEC
    /// 14496-15 Annex E: `hev1.4.10.L150.BE.8` for the Mac's 4:4:4 stream.
    pub decode: String,
}

/// The access units of a stream, turned into [`PassedUnit`]s: each unit's
/// parameter sets update what the next ones are described with, and a unit before
/// the first is dropped, since nothing could configure a decoder for it.
#[derive(Default)]
pub struct Passer {
    params: Option<StreamParams>,
}

impl Passer {
    pub fn pass(&mut self, unit: &AccessUnit) -> Option<PassedUnit> {
        for nal in unit.iter().filter(|nal| nal_type(nal[0]) == NAL_SPS) {
            match parse_sps(nal) {
                Some(params) => self.params = Some(params),
                None => log::warn!("vnc: the Mac's HEVC carried a sequence parameter set this side cannot read"),
            }
        }
        let params = self.params.as_ref()?;
        let mut data = Vec::with_capacity(unit.iter().map(|nal| nal.len() + 4).sum());
        for nal in unit {
            data.extend_from_slice(&[0, 0, 0, 1]);
            data.extend_from_slice(nal);
        }
        Some(PassedUnit {
            size: params.size,
            decode: params.decode.clone(),
            keyframe: unit.iter().any(|nal| (16..=23).contains(&nal_type(nal[0]))),
            data,
        })
    }
}

/// Bits out of an RBSP, most significant first.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = self.data.get(self.at / 8)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(u32::from(bit))
    }

    fn bits(&mut self, n: u32) -> Option<u64> {
        (0..n).try_fold(0u64, |value, _| Some((value << 1) | u64::from(self.bit()?)))
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.at += n;
        (self.at <= self.data.len() * 8).then_some(())
    }

    /// `ue(v)`: unsigned Exp-Golomb.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        u32::try_from((1u64 << zeros) - 1 + self.bits(zeros)?).ok()
    }
}

/// A NAL unit's payload with its emulation-prevention bytes taken out.
fn rbsp(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for &byte in payload {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// The picture size and configuration string a sequence parameter set NAL unit
/// (its two-byte header included) describes (H.265 7.3.2.2).
pub fn parse_sps(nal: &[u8]) -> Option<StreamParams> {
    let data = rbsp(nal.get(2..)?);
    let mut r = Bits { data: &data, at: 0 };
    r.skip(4)?; // sps_video_parameter_set_id
    let sub_layers = r.bits(3)? as usize; // sps_max_sub_layers_minus1
    r.skip(1)?; // sps_temporal_id_nesting_flag
    // profile_tier_level(1, sps_max_sub_layers_minus1)
    let space = r.bits(2)?;
    let tier = r.bit()?;
    let profile = r.bits(5)?;
    let compatibility = (0..32).try_fold(0u32, |flags, j| Some(flags | (r.bit()? << j)))?;
    let constraints: Vec<u8> = (0..6).map(|_| r.bits(8).map(|b| b as u8)).collect::<Option<_>>()?;
    let level = r.bits(8)?;
    let present: Vec<(u32, u32)> = (0..sub_layers).map(|_| Some((r.bit()?, r.bit()?))).collect::<Option<_>>()?;
    if sub_layers > 0 {
        r.skip(2 * (8 - sub_layers))?;
    }
    for (profile_present, level_present) in present {
        r.skip(88 * profile_present as usize + 8 * level_present as usize)?;
    }
    r.ue()?; // sps_seq_parameter_set_id
    let chroma_format = r.ue()?;
    let separate_planes = chroma_format == 3 && r.bit()? == 1;
    let width = r.ue()?;
    let height = r.ue()?;
    let (mut crop_w, mut crop_h) = (0, 0);
    if r.bit()? == 1 {
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        // SubWidthC and SubHeightC: 2 where chroma is halved on that axis.
        let (sub_w, sub_h) = match (chroma_format, separate_planes) {
            (1, _) => (2, 2),
            (2, _) => (2, 1),
            _ => (1, 1),
        };
        crop_w = sub_w * left.checked_add(right)?;
        crop_h = sub_h * top.checked_add(bottom)?;
    }
    let size = (
        u16::try_from(width.checked_sub(crop_w)?).ok()?,
        u16::try_from(height.checked_sub(crop_h)?).ok()?,
    );
    let space = ["", "A", "B", "C"][space as usize];
    let tier = if tier == 1 { 'H' } else { 'L' };
    let kept = constraints.iter().rposition(|&b| b != 0).map_or(0, |last| last + 1);
    let constraints: String = constraints[..kept].iter().map(|b| format!(".{b:X}")).collect();
    let decode = format!("hev1.{space}{profile}.{compatibility:X}.{tier}{level}{constraints}");
    Some(StreamParams { size, decode })
}

// ---------------------------------------------------------------------------
// The decoder
// ---------------------------------------------------------------------------

/// A decoded picture, as packed RGB888 — what [`crate::encode::VideoSink`] takes.
pub struct Picture {
    pub size: (u16, u16),
    pub rgb: Vec<u8>,
}

/// The decoder's slice threads. The Mac's stream sets
/// `entropy_coding_sync_enabled_flag`, so a picture's CTU rows decode in parallel.
/// Replaying captured 1600×1000 pictures on a six-core host, one thread took
/// 14–23 ms a picture, too slow for 60 a second, and four took 7–14 ms. Four,
/// since a larger display has more rows to share out. Frame threads would hold
/// each picture back by one per thread, so there are none. These threads take no
/// part in a picture VideoToolbox decodes.
const DECODE_THREADS: &std::ffi::CStr = c"4";

/// FFmpeg's HEVC decoder, one context for the session: HEVC access units in,
/// pictures out. libavcodec is the system's or, with `apple-hp-media-static`, the
/// one linked in (`crate::libav`).
///
/// On macOS the context is handed a VideoToolbox device, and FFmpeg's VideoToolbox
/// hwaccel gives each picture to VideoToolbox, which for HEVC is asked to enable
/// its hardware decoder, not required to use it. A context with a device is
/// offered VideoToolbox's pictures first by FFmpeg's own format choice, which
/// asks again without them when the hwaccel fails to start on a stream, so a
/// stream VideoToolbox will not take, or a Mac with no device to open, decodes on
/// the CPU. A picture VideoToolbox fails once started is an error, not decoded on
/// the CPU instead. Either way the pictures are the same: HEVC decoding is exact.
struct Hevc {
    api: &'static crate::libav::Api,
    ctx: *mut std::ffi::c_void,
    packet: *mut crate::libav::Packet,
    frame: *mut crate::libav::Frame,
    /// A VideoToolbox picture, copied out of its pixel buffer.
    copy: *mut crate::libav::Frame,
    /// The pixel formats read, by this libavutil's numbers.
    formats: PixelFormats,
    /// The access unit as an Annex B byte stream, kept between units.
    stream: Vec<u8>,
    /// What decoded the last picture, logged whenever it changes.
    decoded_by: Option<DecodedBy>,
}

/// The pixel formats a picture comes in, looked up by name.
#[derive(Clone, Copy)]
struct PixelFormats {
    yuv444p: std::ffi::c_int,
    yuv420p: std::ffi::c_int,
    nv24: std::ffi::c_int,
    nv12: std::ffi::c_int,
    videotoolbox: std::ffi::c_int,
}

/// What decoded a picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodedBy {
    VideoToolbox,
    Software,
}

// SAFETY: the context, packet and frame are created, used and freed on one thread
// at a time — the decoder thread that owns this value. libavcodec's slice threads
// work only inside a call to it.
unsafe impl Send for Hevc {}

impl Hevc {
    /// `hardware` hands the pictures to VideoToolbox where this is a Mac that has
    /// it; without it, or anywhere else, they decode on the CPU.
    fn new(hardware: bool) -> anyhow::Result<Self> {
        let api = crate::libav::api()?;
        // SAFETY: every allocation is checked before use, and `Drop` frees each of
        // them, taking null for any that failed. Names are NUL-terminated.
        unsafe {
            // FFmpeg would print its complaints about a damaged unit to stderr,
            // outside the gateway's log. The failed call is reported instead.
            (api.av_log_set_level)(crate::libav::LOG_QUIET);
            let codec = (api.avcodec_find_decoder_by_name)(c"hevc".as_ptr());
            anyhow::ensure!(!codec.is_null(), "libavcodec has no HEVC decoder");
            let format = |name: &std::ffi::CStr| (api.av_get_pix_fmt)(name.as_ptr());
            let decoder = Self {
                api,
                ctx: (api.avcodec_alloc_context3)(codec),
                packet: (api.av_packet_alloc)(),
                frame: (api.av_frame_alloc)(),
                copy: (api.av_frame_alloc)(),
                formats: PixelFormats {
                    yuv444p: format(c"yuv444p"),
                    yuv420p: format(c"yuv420p"),
                    nv24: format(c"nv24"),
                    nv12: format(c"nv12"),
                    videotoolbox: format(c"videotoolbox_vld"),
                },
                stream: Vec::new(),
                decoded_by: None,
            };
            anyhow::ensure!(
                !decoder.ctx.is_null()
                    && !decoder.packet.is_null()
                    && !decoder.frame.is_null()
                    && !decoder.copy.is_null(),
                "libavcodec could not allocate a decoder"
            );
            if hardware {
                attach_videotoolbox(api, decoder.ctx);
            }
            // The Mac codes with wavefront parallel processing, so its rows decode
            // on slice threads. See DECODE_THREADS. A unit that does not decode
            // fails its call rather than being skipped in silence, so the receive
            // task asks for a keyframe.
            for (name, value) in [(c"threads", DECODE_THREADS), (c"thread_type", c"slice"), (c"err_detect", c"+explode")] {
                let err = (api.av_opt_set)(decoder.ctx, name.as_ptr(), value.as_ptr(), 0);
                anyhow::ensure!(
                    err >= 0,
                    "libavcodec refused {}={}: {}",
                    name.to_string_lossy(),
                    value.to_string_lossy(),
                    text(api, err)
                );
            }
            let err = (api.avcodec_open2)(decoder.ctx, codec, std::ptr::null_mut());
            anyhow::ensure!(err >= 0, "libavcodec could not open the HEVC decoder: {}", text(api, err));
            Ok(decoder)
        }
    }

    /// Decode one access unit, returning the picture it completed, if any.
    fn decode(&mut self, unit: &AccessUnit) -> anyhow::Result<Option<Picture>> {
        use std::ffi::c_int;

        let api = self.api;
        self.stream.clear();
        for nal in unit {
            self.stream.extend_from_slice(&[0, 0, 0, 1]);
            self.stream.extend_from_slice(nal);
        }
        let size = c_int::try_from(self.stream.len()).context("an access unit too large to decode")?;
        // SAFETY: a live context, packet and frame. The packet owns no buffer, so
        // `avcodec_send_packet` copies the stream, padded, before it returns.
        unsafe {
            (*self.packet).data = self.stream.as_mut_ptr();
            (*self.packet).size = size;
            let err = (api.avcodec_send_packet)(self.ctx, self.packet);
            (*self.packet).data = std::ptr::null_mut();
            (*self.packet).size = 0;
            anyhow::ensure!(err >= 0, "libavcodec refused an access unit: {}", text(api, err));
            let mut picture = None;
            loop {
                let err = (api.avcodec_receive_frame)(self.ctx, self.frame);
                if err == crate::libav::EAGAIN {
                    break;
                }
                anyhow::ensure!(err >= 0, "libavcodec: {}", text(api, err));
                // A later picture of the same unit replaces an earlier one: only
                // the newest is shown. The decoder sets the context's range from
                // the stream's parameters, which a copy out of VideoToolbox does
                // not carry.
                let mut range = 0;
                (api.av_opt_get_int)(self.ctx, c"color_range".as_ptr(), 0, &mut range);
                let decoded_by = if (*self.frame).format == self.formats.videotoolbox {
                    DecodedBy::VideoToolbox
                } else {
                    DecodedBy::Software
                };
                let rgb = match decoded_by {
                    DecodedBy::VideoToolbox => {
                        let err = (api.av_hwframe_transfer_data)(self.copy, self.frame, 0);
                        let rgb = if err < 0 {
                            Err(anyhow::anyhow!("could not copy a VideoToolbox picture out: {}", text(api, err)))
                        } else {
                            to_rgb(api, &self.formats, &*self.copy, range)
                        };
                        (api.av_frame_unref)(self.copy);
                        rgb
                    }
                    DecodedBy::Software => to_rgb(api, &self.formats, &*self.frame, range),
                };
                (api.av_frame_unref)(self.frame);
                if self.decoded_by != Some(decoded_by) {
                    match decoded_by {
                        DecodedBy::VideoToolbox => log::info!("vnc: the Mac's HEVC is decoded by VideoToolbox"),
                        DecodedBy::Software => log::info!("vnc: the Mac's HEVC is decoded in software"),
                    }
                    self.decoded_by = Some(decoded_by);
                }
                picture = Some(rgb?);
            }
            Ok(picture)
        }
    }
}

/// Give `ctx` a VideoToolbox device, whose pictures FFmpeg then prefers. A Mac
/// with no device to open, or a libavcodec whose context this cannot place it in,
/// decodes in software, and says so.
///
/// # Safety
///
/// `ctx` must be an allocated context of `api`'s libavcodec that has not been
/// opened.
#[cfg(target_os = "macos")]
unsafe fn attach_videotoolbox(api: &crate::libav::Api, ctx: *mut std::ffi::c_void) {
    let Some(offset) = crate::libav::hw_device_ctx(api) else {
        // SAFETY: a plain version query.
        let version = unsafe { (api.avcodec_version)() } >> 16;
        log::warn!("vnc: libavcodec {version} is not one VideoToolbox is set up on here, so the Mac's HEVC decodes in software");
        return;
    };
    let mut device = std::ptr::null_mut();
    // SAFETY: `device` is written only on success, and then owned here.
    let err = unsafe {
        let kind = (api.av_hwdevice_find_type_by_name)(c"videotoolbox".as_ptr());
        (api.av_hwdevice_ctx_create)(&mut device, kind, std::ptr::null(), std::ptr::null_mut(), 0)
    };
    if err < 0 {
        log::warn!("vnc: no VideoToolbox device, so the Mac's HEVC decodes in software: {}", text(api, err));
        return;
    }
    // SAFETY: `offset` is `hw_device_ctx`'s place in this libavcodec's context,
    // which is null until set. The context takes a reference of its own and frees
    // it with itself; this one is dropped once it has. A failed reference leaves
    // the context without a device, which decodes in software.
    unsafe {
        ctx.byte_add(offset).cast::<*mut std::ffi::c_void>().write((api.av_buffer_ref)(device));
        (api.av_buffer_unref)(&mut device);
    }
}

/// No VideoToolbox off macOS: the pictures decode in software.
#[cfg(not(target_os = "macos"))]
unsafe fn attach_videotoolbox(_api: &crate::libav::Api, _ctx: *mut std::ffi::c_void) {}

impl Drop for Hevc {
    fn drop(&mut self) {
        // SAFETY: freed exactly once, here; each call takes null and nulls its
        // pointer.
        unsafe {
            (self.api.avcodec_free_context)(&mut self.ctx);
            (self.api.av_packet_free)(&mut self.packet);
            (self.api.av_frame_free)(&mut self.frame);
            (self.api.av_frame_free)(&mut self.copy);
        }
    }
}

fn text(api: &crate::libav::Api, err: std::ffi::c_int) -> String {
    let mut text = [0 as std::ffi::c_char; crate::libav::ERROR_TEXT];
    // SAFETY: `av_strerror` writes a NUL-terminated description of any code, known
    // or not, within the length it is given.
    unsafe {
        (api.av_strerror)(err, text.as_mut_ptr(), text.len());
        std::ffi::CStr::from_ptr(text.as_ptr()).to_string_lossy().into_owned()
    }
}

/// A decoded picture as packed RGB888. The Mac sends full-range BT.709, 8-bit, at
/// 4:4:4; 4:2:0 is taken too, in case it ever chooses it. FFmpeg's own pictures are
/// planar (`yuv444p`, `yuv420p`), and a VideoToolbox one, copied out, is the same
/// samples semi-planar (`nv24`, `nv12`). `range` is the decoded picture's, which a
/// copy out of VideoToolbox does not carry.
///
/// # Safety
///
/// `frame` must be a picture libavcodec just returned, or a copy of one, and has not
/// yet been unreferenced.
unsafe fn to_rgb(
    api: &crate::libav::Api,
    formats: &PixelFormats,
    frame: &crate::libav::Frame,
    range: i64,
) -> anyhow::Result<Picture> {
    use yuv::{YuvBiPlanarImage, YuvConversionMode, YuvPlanarImage, YuvRange, YuvStandardMatrix};

    // (format, planar, 4:4:4)
    let read = [
        (formats.yuv444p, true, true),
        (formats.yuv420p, true, false),
        (formats.nv24, false, true),
        (formats.nv12, false, false),
    ];
    let Some(&(_, planar, full)) = read.iter().find(|(format, ..)| *format == frame.format) else {
        // SAFETY: a static string for any known format, and null for any other.
        let name = unsafe { (api.av_get_pix_fmt_name)(frame.format) };
        let name = if name.is_null() {
            format!("pixel format {}", frame.format)
        } else {
            // SAFETY: non-null, so one of libavutil's static names.
            unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned()
        };
        anyhow::bail!("the Mac sent video as {name}, and only 8-bit 4:4:4 and 4:2:0 are read");
    };
    let (width, height) = (frame.width as usize, frame.height as usize);
    let chroma_rows = if full { height } else { height.div_ceil(2) };
    let plane = |channel: usize, rows: usize| {
        let stride = frame.linesize[channel] as usize;
        // SAFETY: per this function's contract, a decoded plane of `rows` rows of
        // `stride` bytes each.
        (unsafe { std::slice::from_raw_parts(frame.data[channel], stride * rows) }, stride as u32)
    };
    let range = if range == crate::libav::RANGE_FULL { YuvRange::Full } else { YuvRange::Limited };
    let mut rgb = vec![0u8; width * height * 3];
    let stride = width as u32 * 3;
    let (y_plane, y_stride) = plane(0, height);
    if planar {
        let (u_plane, u_stride) = plane(1, chroma_rows);
        let (v_plane, v_stride) = plane(2, chroma_rows);
        let image = YuvPlanarImage {
            y_plane,
            y_stride,
            u_plane,
            u_stride,
            v_plane,
            v_stride,
            width: width as u32,
            height: height as u32,
        };
        if full {
            yuv::yuv444_to_rgb(&image, &mut rgb, stride, range, YuvStandardMatrix::Bt709)?;
        } else {
            yuv::yuv420_to_rgb(&image, &mut rgb, stride, range, YuvStandardMatrix::Bt709)?;
        }
    } else {
        let (uv_plane, uv_stride) = plane(1, chroma_rows);
        let image = YuvBiPlanarImage { y_plane, y_stride, uv_plane, uv_stride, width: width as u32, height: height as u32 };
        // Balanced: the mode the planar conversions above use.
        let mode = YuvConversionMode::Balanced;
        if full {
            yuv::yuv_nv24_to_rgb(&image, &mut rgb, stride, range, YuvStandardMatrix::Bt709, mode)?;
        } else {
            yuv::yuv_nv12_to_rgb(&image, &mut rgb, stride, range, YuvStandardMatrix::Bt709, mode)?;
        }
    }
    Ok(Picture { size: (width as u16, height as u16), rgb })
}

// ---------------------------------------------------------------------------
// The session's media stream
// ---------------------------------------------------------------------------

/// What the receiver hands the read loop.
pub enum Pictures {
    /// The newest decoded picture, to show. A picture is the whole display, so an
    /// unshown one is simply replaced. `None` after a picture means the receiver has
    /// stopped.
    Decoded(tokio::sync::watch::Receiver<Option<std::sync::Arc<Picture>>>),
    /// Every access unit, in order, to pass to the browser: a unit depends on the
    /// ones before it, so none is replaced. `None` means the receiver has stopped.
    Passed(tokio::sync::mpsc::Receiver<Option<PassedUnit>>),
}

/// The sending half of [`Pictures`], which each receiver the stream binds is handed.
#[derive(Clone)]
enum Outlet {
    Decoded(tokio::sync::watch::Sender<Option<std::sync::Arc<Picture>>>),
    Passed(tokio::sync::mpsc::Sender<Option<PassedUnit>>),
}

/// Access units the read loop may be behind by in passing them to the browser.
/// Reaching it drops to the next keyframe, as the decoder's queue does: the loop
/// waits on the browser's link ([`crate::encode::VideoSink::pass_hevc`]), so this is
/// where a link that cannot carry the stream sheds it. Half a second of the virtual
/// display's 30 Hz ([`vnc_apple::DISPLAY_HZ`]).
const PASS_QUEUE: usize = 15;

/// What [`MediaStream::offer`] has the session send.
#[derive(Debug, PartialEq, Eq)]
pub enum Offer {
    /// The `SetEncodings` naming [`ENCODING_MEDIA_STREAM`], which the Mac names its
    /// ports for.
    Encodings,
    /// The `0x1c` offer for the display.
    Configuration(Vec<u8>),
}

/// One session's media stream: its offers, and once the Mac names its ports, the
/// receiver. Shared between the engine's two loops, which each decide on offers.
///
/// An offer goes out only once the Mac has named its ports, as Apple's viewer
/// makes it: the Mac names them for the encodings naming the stream and after each
/// display change, never in reply to an offer, and each naming is good for one
/// offer.
///
/// Only one offer is ever out: a second one, sent while the first's capture was
/// starting, failed to start (`error 32000`) and left a display stream behind that
/// crashed WindowServer when the virtual display went away.
///
/// A session with two virtual displays has a video leg for each, offered, answered
/// and taken down together: one offer names both, and the Mac sends the second
/// display's picture from the port after the first's.
pub struct MediaStream {
    offers: Offers,
    /// The Mac, as the TCP session reached it: where the streams come from and RTCP
    /// goes. Read by the receiver alone, which only a build with the decoders has.
    peer: std::net::IpAddr,
    /// This side's address on that connection, which the UDP sockets bind.
    local: std::net::IpAddr,
    /// Whether the encodings naming the media stream have gone out.
    asked: bool,
    /// An offer is out that the Mac has not answered.
    pending: bool,
    /// The Mac has named its ports, and no offer has gone out on that naming yet.
    invited: bool,
    /// The sizes the live (or starting) stream was offered for, one per display;
    /// `None` while there is none, as after a display change.
    offered: Option<Vec<(u16, u16)>>,
    /// What the stream owes the session next — see [`MediaStream::overdue`].
    owed: Owed,
    /// Signalled at every new deadline — see [`MediaStream::offered`].
    offer_made: std::sync::Arc<tokio::sync::Notify>,
    /// The ports the receiver is bound to, and the receiver.
    receiver: Option<(Ports, tokio::task::JoinHandle<()>)>,
    /// Each video leg, as the receiver shares it, in the Mac's order.
    legs: Vec<Leg>,
    /// Whether the Mac's legs carry the session's two displays the other way
    /// round — see [`MediaStream::arrange`].
    swapped: bool,
    /// Why the receiver stopped, which it leaves here before the `None` that says
    /// so — see [`MediaStream::failure`].
    failed: Failure,
    /// When the sound leg last brought an authentic packet, which the receiver
    /// notes — see [`MediaStream::overdue`].
    sound_heard: Heard,
    /// When the sound leg last brought sound, an SRTP packet: what the offer's
    /// first sound is, which a report cannot stand in for.
    sounded: Heard,
    /// Where the sound leg's decoded PCM goes: the session's audio bridge, when
    /// the browser can be sent sound. `None` drains the leg unread.
    sound: Option<std::sync::Arc<crate::audio::AudioBridge>>,
}

/// The ports the Mac named: the sound's, and each display's picture's.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ports {
    audio: u16,
    videos: Vec<u16>,
}

/// One display's video leg, as the stream and its receiver share it.
#[derive(Clone)]
struct Leg {
    pictures: Outlet,
    /// Signalled by [`MediaStream::want_keyframe`] and [`MediaStream::show`], for
    /// the receiver to ask the Mac.
    keyframe_wanted: std::sync::Arc<tokio::sync::Notify>,
    /// When the leg last brought an authentic packet, SRTP or SRTCP.
    heard: Heard,
    /// When the leg last brought a picture's packet, SRTP: what stands for the
    /// first picture of a display nobody is shown ([`MediaStream::show`]).
    pictured: Heard,
    /// Whether anybody is shown this display. The pictures of one nobody is shown
    /// are authenticated and dropped: neither decoded nor passed on.
    shown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The size the leg's stream was last offered for: the display its strips
    /// are put together as.
    offered: std::sync::Arc<std::sync::Mutex<Option<(u16, u16)>>>,
    /// Whether that offer asked for the display in strips.
    strips: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Where the receiver and its decoder threads leave the first reason they stopped.
type Failure = std::sync::Arc<std::sync::Mutex<Option<anyhow::Error>>>;

/// When a leg last brought an authentic packet, SRTP or SRTCP. The Mac sends a
/// report on each leg about once a second whether or not the leg carries media: a
/// still screen sends no picture for as long as it stays still, while the sound
/// leg sends a packet every 10 ms whether or not anything plays.
type Heard = std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>;

/// What the stream owes the session, each a deadline the session ends at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owed {
    /// Nothing: no stream is offered, as before the first display and across a
    /// display change.
    Nothing,
    /// The Mac's ports, without which no offer can go out, since this instant: the
    /// encodings naming the stream went then, or the display settled without them.
    Ports(std::time::Instant),
    /// An answer to the offer that went at this instant, for a display that has
    /// changed since. No other offer can go out until it comes.
    Answer(std::time::Instant),
    /// Every leg, for the offer that went at `offered`: the first picture of each
    /// display and the first sound within [`STREAM_START`] of it, and after them
    /// a packet on each leg within [`STREAM_SILENCE`] of the last. `pictured` is
    /// when the latest picture of each display came.
    Stream { offered: std::time::Instant, pictured: [Option<std::time::Instant>; MAX_DISPLAYS] },
}

/// When `heard` last brought a packet, if it has since `since`.
fn heard_since(heard: &Heard, since: std::time::Instant) -> Option<std::time::Instant> {
    heard.lock().unwrap().filter(|&at| at >= since)
}

/// When a leg is overdue on the offer made at `offered`, the leg having last
/// delivered at `last`.
fn leg_due(offered: std::time::Instant, last: Option<std::time::Instant>) -> std::time::Instant {
    match last {
        Some(at) => at + STREAM_SILENCE,
        None => offered + STREAM_START,
    }
}

impl MediaStream {
    /// A stream for `displays` virtual displays, with a [`Pictures`] for each in
    /// the Mac's order. `pass` hands the read loop the Mac's access units rather
    /// than pictures decoded from them — see [`Pictures`]. The first display is
    /// shown from the start, and any other once [`Self::show`] says so.
    pub fn new(
        peer: std::net::SocketAddr,
        local: std::net::SocketAddr,
        pass: bool,
        displays: usize,
    ) -> (Self, Vec<Pictures>) {
        let offers = Offers::new(displays, !pass);
        let (legs, pictures) = (0..offers.videos.len())
            .map(|index| {
                let (pictures, rx) = if pass {
                    let (tx, rx) = tokio::sync::mpsc::channel(PASS_QUEUE);
                    (Outlet::Passed(tx), Pictures::Passed(rx))
                } else {
                    let (tx, rx) = tokio::sync::watch::channel(None);
                    (Outlet::Decoded(tx), Pictures::Decoded(rx))
                };
                let leg = Leg {
                    pictures,
                    keyframe_wanted: std::sync::Arc::default(),
                    heard: Heard::default(),
                    pictured: Heard::default(),
                    shown: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(index == 0)),
                    offered: std::sync::Arc::default(),
                    strips: std::sync::Arc::default(),
                };
                (leg, rx)
            })
            .unzip();
        let media = Self {
            offers,
            peer: peer.ip(),
            local: local.ip(),
            asked: false,
            pending: false,
            invited: false,
            offered: None,
            owed: Owed::Nothing,
            offer_made: std::sync::Arc::default(),
            receiver: None,
            legs,
            swapped: false,
            failed: Failure::default(),
            sound_heard: Heard::default(),
            sounded: Heard::default(),
            sound: None,
        };
        (media, pictures)
    }

    /// Carry the sound leg to `bridge`, the session's, for every stream from here.
    pub fn with_sound(mut self, bridge: Option<std::sync::Arc<crate::audio::AudioBridge>>) -> Self {
        self.sound = bridge;
        self
    }

    /// How many displays the stream carries, a video leg each.
    pub fn displays(&self) -> usize {
        self.legs.len()
    }

    /// Say which display each leg carries. The Mac sends its displays in the
    /// order they sit in the framebuffer it spans over them, not the order the
    /// session asked for them in: with the second display arranged to the left
    /// of the first or above it, on the Mac, the first leg carries the second.
    /// `swapped` is whether that is so, as the layout places them. Every display
    /// this stream is asked about or hands a picture of is the session's.
    ///
    /// Seen with the second display to the left, the right and above; one placed
    /// diagonally has not been.
    pub fn arrange(&mut self, swapped: bool) {
        let swapped = swapped && self.legs.len() == 2;
        if std::mem::replace(&mut self.swapped, swapped) != swapped {
            // Who is shown a display goes with the display.
            let relaxed = std::sync::atomic::Ordering::Relaxed;
            let first = self.legs[0].shown.load(relaxed);
            let second = self.legs[1].shown.swap(first, relaxed);
            self.legs[0].shown.store(second, relaxed);
        }
    }

    /// The leg that carries `display`, and the display leg `display` carries:
    /// with two the one is the other's inverse.
    pub fn leg_of(&self, display: usize) -> usize {
        if self.swapped && display < 2 { 1 - display } else { display }
    }

    /// What to send toward a stream for displays of `sizes` backing pixels, which
    /// the session's displays now are and nothing is about to change, unless an
    /// offer is already out or the stream already runs at those sizes: first the
    /// `SetEncodings` naming [`ENCODING_MEDIA_STREAM`], then, once the Mac has
    /// named its ports for it, the `0x1c` offer. Until it has, the Mac owes them.
    /// Nothing for sizes that are not one per leg: a layout still to catch up with
    /// the displays asked for.
    pub fn offer(&mut self, sizes: &[(u16, u16)]) -> Option<Offer> {
        if self.pending || sizes.len() != self.legs.len() || self.offered.as_deref() == Some(sizes) {
            return None;
        }
        let now = std::time::Instant::now();
        if !std::mem::replace(&mut self.asked, true) {
            self.owe(Owed::Ports(now));
            return Some(Offer::Encodings);
        }
        if !std::mem::take(&mut self.invited) {
            if self.owed == Owed::Nothing {
                self.owe(Owed::Ports(now));
            }
            return None;
        }
        self.pending = true;
        self.offered = Some(sizes.to_vec());
        self.owe(Owed::Stream { offered: now, pictured: [None; MAX_DISPLAYS] });
        let by_leg: Vec<(u16, u16)> = (0..sizes.len()).map(|leg| sizes[self.leg_of(leg)]).collect();
        for (leg, size) in self.legs.iter().zip(&by_leg) {
            *leg.offered.lock().unwrap() = Some(*size);
            leg.strips.store(self.offers.strips(*size), std::sync::atomic::Ordering::Relaxed);
        }
        Some(Offer::Configuration(self.offers.configuration(&by_leg)))
    }

    /// Owe `owed` from here, which sets a new deadline.
    fn owe(&mut self, owed: Owed) {
        self.owed = owed;
        self.offer_made.notify_one();
    }

    /// The newest decoded picture of display `leg`, for a browser that needs the
    /// whole desktop again. A passed stream has none: [`Self::want_keyframe`] is
    /// its repaint.
    pub fn latest(&self, leg: usize) -> Option<std::sync::Arc<Picture>> {
        match &self.legs.get(self.leg_of(leg))?.pictures {
            Outlet::Decoded(pictures) => pictures.borrow().clone(),
            Outlet::Passed(_) => None,
        }
    }

    /// Ask the Mac for an IDR of display `leg`, with a PLI on its picture's leg: a
    /// passed stream's browser has to start over, after a reattach or its own
    /// decoder's failure. The Mac answers within tens of milliseconds. A decoded
    /// stream asks nothing: its repaint is [`Self::latest`].
    pub fn want_keyframe(&self, leg: usize) {
        if let Some(leg) = self.legs.get(self.leg_of(leg))
            && matches!(leg.pictures, Outlet::Passed(_))
        {
            leg.keyframe_wanted.notify_one();
        }
    }

    /// Whether anybody is shown display `leg` from here. One coming into view
    /// starts at an IDR the Mac is asked for, decoded or passed: the pictures since
    /// it went out of view were dropped, and every one predicts from the last.
    ///
    /// A display's first picture is owed by its offer, and one coming into view
    /// long after it has none on record: it counts as delivered here, and owes its
    /// next packet like any running leg.
    ///
    /// `true` when the display came into view with this call, and so has no
    /// picture to show until that IDR.
    pub fn show(&mut self, leg: usize, shown: bool) -> bool {
        let leg = self.leg_of(leg);
        let Some(shared) = self.legs.get(leg) else {
            return false;
        };
        let came = !shared.shown.swap(shown, std::sync::atomic::Ordering::Relaxed) && shown;
        if came {
            shared.keyframe_wanted.notify_one();
            if let Owed::Stream { offered, pictured } = &mut self.owed
                && pictured[leg].is_none()
                && heard_since(&shared.pictured, *offered).is_some()
            {
                pictured[leg] = Some(std::time::Instant::now());
            }
        }
        came
    }

    /// Whether an offer is out that the Mac has not answered: no display change may
    /// go out meanwhile. One left unanswered ends the session at [`STREAM_START`].
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// The display changed. The Mac stops every stream for it and starts them
    /// again only on an offer, which the settled layout gets once the Mac has named
    /// its ports for it; until then the stream owes nothing but an answer to an
    /// offer still out, which holds back that one.
    pub fn stopped(&mut self) {
        self.offered = None;
        self.owed = match self.owed {
            Owed::Stream { offered, .. } | Owed::Answer(offered) if self.pending => Owed::Answer(offered),
            _ => Owed::Nothing,
        };
    }

    /// A picture of `size` came from the receiver for display `leg`. Each one of
    /// the display the stream was offered for puts the picture's deadline off; one
    /// of another display, the old one's last, does not.
    pub fn pictured(&mut self, leg: usize, size: (u16, u16)) {
        let carried = self.leg_of(leg);
        if let Owed::Stream { pictured, .. } = &mut self.owed
            && self.offered.as_ref().and_then(|sizes| sizes.get(leg)) == Some(&size)
            && let Some(pictured) = pictured.get_mut(carried)
        {
            *pictured = Some(std::time::Instant::now());
        }
    }

    /// When display `leg`'s picture last delivered: its latest packet, once the
    /// offered display's first picture has come, which a packet before it cannot
    /// stand in for. A display nobody is shown hands no picture on, and its first
    /// is the first packet of one since the offer at `offered`.
    fn picture_last(
        &self,
        leg: usize,
        offered: std::time::Instant,
        pictured: Option<std::time::Instant>,
    ) -> Option<std::time::Instant> {
        let shared = self.legs.get(leg)?;
        let unshown = !shared.shown.load(std::sync::atomic::Ordering::Relaxed);
        let pictured =
            pictured.or_else(|| unshown.then(|| heard_since(&shared.pictured, offered)).flatten())?;
        Some(heard_since(&shared.heard, pictured).unwrap_or(pictured))
    }

    /// When the sound leg last delivered: its latest packet, once sound has come
    /// since the offer at `offered`, which a report before it cannot stand in for.
    fn sound_last(&self, offered: std::time::Instant) -> Option<std::time::Instant> {
        let sounded = heard_since(&self.sounded, offered)?;
        Some(heard_since(&self.sound_heard, sounded).unwrap_or(sounded))
    }

    /// Signalled at every new [`deadline`](Self::deadline), which an offer sets: an
    /// offer can go out from either of the engine's loops, and the one that waits
    /// for the deadline may be idle behind a still screen when the other sends it.
    pub fn offered(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.offer_made)
    }

    /// When the stream is next overdue, for the session to wake at.
    pub fn deadline(&self) -> Option<std::time::Instant> {
        match self.owed {
            Owed::Nothing => None,
            Owed::Ports(since) | Owed::Answer(since) => Some(since + STREAM_START),
            Owed::Stream { offered, pictured } => (0..self.legs.len())
                .map(|leg| leg_due(offered, self.picture_last(leg, offered, pictured[leg])))
                .chain([leg_due(offered, self.sound_last(offered))])
                .min(),
        }
    }

    /// The error the session ends with when the stream is overdue at `now`: its
    /// offer has gone unanswered, or brought no picture of a display or no sound,
    /// in [`STREAM_START`], or the running stream has sent nothing on a leg, neither
    /// media nor a report, for [`STREAM_SILENCE`]. Apple's viewer ends its session on the same failures,
    /// counted in RTCP timeouts on each leg, and never falls back to RFB pixels.
    pub fn overdue(&self, now: std::time::Instant) -> Option<anyhow::Error> {
        let first = STREAM_START.as_secs();
        let silence = STREAM_SILENCE.as_secs();
        let unanswered = || anyhow::anyhow!("the Mac did not answer the media-stream offer within {first}s");
        let unnamed = || anyhow::anyhow!("the Mac named no ports for its media stream within {first}s");
        let portless = || {
            anyhow::anyhow!("the Mac accepted the media stream but named no ports for it within {first}s")
        };
        let firewalled = |what: &str, port: u16| {
            anyhow::anyhow!(
                "no {what} came over the Mac's media stream within {first}s of its offer; if \
                 nothing reached UDP {port}, a firewall or NAT between the Mac and this gateway \
                 is what that looks like"
            )
        };
        let ports = self.receiver.as_ref().map(|(ports, _)| ports);
        let (offered, pictured) = match self.owed {
            Owed::Nothing => return None,
            Owed::Ports(since) => return (now >= since + STREAM_START).then(unnamed),
            Owed::Answer(offered) => return (now >= offered + STREAM_START).then(unanswered),
            Owed::Stream { offered, pictured } => (offered, pictured),
        };
        for (leg, pictured) in pictured.into_iter().enumerate().take(self.legs.len()) {
            let last = self.picture_last(leg, offered, pictured);
            if now < leg_due(offered, last) {
                continue;
            }
            // One display's picture is "the picture"; two are told apart.
            let (what, its_leg) = if self.legs.len() == 1 {
                ("picture".to_owned(), "the picture's leg".to_owned())
            } else {
                let display = leg + 1;
                (format!("picture of display {display}"), format!("the leg of display {display}'s picture"))
            };
            return Some(match (last, ports.and_then(|ports| ports.videos.get(leg))) {
                (None, _) if self.pending => unanswered(),
                (None, None) => portless(),
                (None, Some(port)) => firewalled(&what, *port),
                (Some(_), _) => {
                    anyhow::anyhow!("the Mac's media stream sent nothing on {its_leg} for {silence}s")
                }
            });
        }
        let heard = self.sound_last(offered);
        if now >= leg_due(offered, heard) {
            return Some(match (heard, ports) {
                (None, _) if self.pending => unanswered(),
                (None, None) => portless(),
                (None, Some(ports)) => firewalled("sound", ports.audio),
                (Some(_), _) => {
                    anyhow::anyhow!("the Mac's media stream sent nothing on the sound's leg for {silence}s")
                }
            });
        }
        None
    }

    /// Why the receiver stopped, once it has said so with a `None` picture.
    pub fn failure(&self) -> anyhow::Error {
        self.failed
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| anyhow::anyhow!("its receiver stopped"))
    }

    /// Act on an encoding-1010 rectangle. `true` when a stream offered for the
    /// displays went down with it: the Mac named its ports again, which it does
    /// after every display change, its own included, and the browser is told the
    /// screen is not available until the offer that naming allows delivers.
    ///
    /// An error ends the session: the Mac refused the stream, or described one this
    /// side cannot receive. Apple's viewer shows the refusal and closes.
    pub fn on_reply(&mut self, body: &[u8]) -> anyhow::Result<bool> {
        match parse_media_reply(body)? {
            MediaReply::Ports { audio_port, video_port, video2_port } => {
                let videos: Vec<u16> = [Some(video_port), video2_port].into_iter().flatten().collect();
                anyhow::ensure!(
                    videos.len() == self.legs.len(),
                    "media-stream message 1 enabled {} video leg(s) for the {} virtual display(s) \
                     this session asked for",
                    videos.len(),
                    self.legs.len()
                );
                self.invited = true;
                if matches!(self.owed, Owed::Ports(_)) {
                    self.owed = Owed::Nothing;
                }
                let down = self.offered.is_some();
                if down {
                    log::debug!("vnc: the Mac re-announced its media streams; they are down until offered");
                    self.stopped();
                }
                let ports = Ports { audio: audio_port, videos };
                // The Mac names the same ports every time, and the receiver carries
                // on across display changes.
                if self.receiver.as_ref().is_some_and(|(bound, _)| *bound == ports) {
                    return Ok(down);
                }
                if let Some((_, receiver)) = self.receiver.take() {
                    receiver.abort();
                }
                log::info!(
                    "vnc: the Mac opened its media streams: screen video at UDP {}, sound at {audio_port}",
                    ports.videos.iter().map(u16::to_string).collect::<Vec<_>>().join(" and ")
                );
                let receiver = self.receive(&ports)?;
                self.receiver = Some((ports, receiver));
                return Ok(down);
            }
            MediaReply::Answer { videos } => {
                anyhow::ensure!(
                    videos == self.legs.len(),
                    "the Mac answered {videos} video leg(s) of the {} this session offered",
                    self.legs.len()
                );
                log::debug!("vnc: the Mac accepted the media-stream offer");
                self.pending = false;
                // The display it was for has gone, and the new one's offer can
                // go out now.
                if matches!(self.owed, Owed::Answer(_)) {
                    self.owed = Owed::Nothing;
                }
            }
            // What the Mac answers a viewer that asks for the stream while
            // another holds it.
            MediaReply::Error { kind: 1, sub_code: 1 } => anyhow::bail!(
                "another viewer already has the Mac's High Performance stream, which it gives \
                 to one at a time (error type 1, sub-code 1)"
            ),
            MediaReply::Error { kind, sub_code } => anyhow::bail!(
                "the Mac refused the media stream (error type {kind}, sub-code {sub_code})"
            ),
            MediaReply::Other(kind) => log::debug!("vnc: ignoring media-stream message type {kind}"),
        }
        Ok(false)
    }

    /// Bind the ports the Mac named and start receiving on them.
    fn receive(&self, ports: &Ports) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        Ok(tokio::spawn(Receiver::bind(self, ports)?.run()))
    }
}

impl Drop for MediaStream {
    fn drop(&mut self) {
        if let Some((_, receiver)) = self.receiver.take() {
            receiver.abort();
        }
    }
}

/// How long an offer has to be answered, and to bring its display's first picture
/// and the first sound, before the session ends. The Mac answers in under a second
/// and both legs follow within one more. Apple's viewer gives up on a stream that
/// has not started after three of its 3-second RTCP timeouts on a leg.
pub const STREAM_START: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a running stream may send nothing on a leg, neither media nor a
/// report, before the session ends: 16 of Apple's 3-second RTCP timeouts on a
/// leg, the count at which its viewer disconnects. A still screen sends no
/// picture, but the Mac reports on both legs about once a second, and a display
/// change, which stops the stream, owes nothing until its offer.
pub const STREAM_SILENCE: std::time::Duration = std::time::Duration::from_secs(48);

/// Frames in strips the HEVC decoder thread may be behind by, each [`STRIPS`]
/// access units. Reaching it drops the unit, which costs a keyframe: every later
/// picture predicts from it.
const DECODE_QUEUE: usize = 8;

/// The least time between two keyframe requests. The Mac answers one in tens of
/// milliseconds; this keeps a burst of losses from asking for one per packet. It
/// is also how often a request the Mac dropped is made again: with one picture
/// to a frame the Mac drops any that comes within a second of its last keyframe.
const PLI_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// How often the Mac's rate controller is sent a [`RateFeedback`] report: Apple's
/// viewer's cadence.
const RATE_FEEDBACK: std::time::Duration = std::time::Duration::from_millis(50);

/// Seconds between the debug log's picture rates: what the Mac actually sends,
/// which its 60 fps flag does not settle.
const RATE_REPORT: u32 = 10;

/// The receive buffer the screen video's socket asks for. The Mac sends a picture
/// as one burst at the link's speed, and on a 3200×2000 display a burst overflowed
/// Linux's default 208 KB, however promptly it was read: keyframes lost fragments,
/// and a display never had its first picture. Linux grants at most
/// `net.core.rmem_max`.
const VIDEO_RECEIVE_BUFFER: usize = 4 << 20;

/// How long the Mac may name its ports without a packet arriving before the log
/// says so, naming the port. A firewall or NAT between it and this gateway's UDP
/// ports is what that looks like, and the session ends at [`STREAM_START`].
const SILENT_START: std::time::Duration = std::time::Duration::from_secs(5);

/// `REMOTEX_HP_DUMP=<dir>`: the stream as it arrives, for replay outside the
/// gateway (`tests/hp_capture.sh`). `video.h265` is every access unit the
/// depacketizer completes, as Annex B; `audio.eld` is every AAC-ELD unit, each
/// behind its length as a big-endian u32. Both are written before any decoder
/// sees them, whether the session decodes the picture or passes it on.
///
/// The files are written on a thread of their own, so a slow disk never holds up
/// the sockets the stream is read from: the receiver hands each unit over and
/// moves on, and a disk [`DUMP_QUEUE`] units behind stops the dump rather than the
/// stream.
struct MediaDump {
    units: std::sync::mpsc::SyncSender<(Track, Vec<u8>)>,
}

/// Which file a dumped unit goes to.
enum Track {
    Video,
    Audio,
}

/// How many units the dump's writer may fall behind the receiver.
const DUMP_QUEUE: usize = 4096;

impl MediaDump {
    fn from_env() -> Option<Self> {
        let dir = std::path::PathBuf::from(std::env::var_os("REMOTEX_HP_DUMP")?);
        let open = |name: &str| {
            std::fs::File::create(dir.join(name))
                .map(std::io::BufWriter::new)
                .with_context(|| format!("create {}", dir.join(name).display()))
        };
        let opened = std::fs::create_dir_all(&dir)
            .with_context(|| format!("create {}", dir.display()))
            .and_then(|()| Ok((open("video.h265")?, open("audio.eld")?)));
        let (mut video, mut audio) = match opened {
            Ok(files) => files,
            Err(e) => {
                log::warn!("vnc: not dumping the media stream: {e:#}");
                return None;
            }
        };
        let (units, queued) = std::sync::mpsc::sync_channel::<(Track, Vec<u8>)>(DUMP_QUEUE);
        let writer = std::thread::Builder::new().name("hp-dump".into()).spawn(move || {
            let written = queued
                .iter()
                .try_for_each(|(track, unit)| match track {
                    Track::Video => video.write_all(&unit),
                    Track::Audio => audio.write_all(&unit),
                })
                .and_then(|()| video.flush())
                .and_then(|()| audio.flush());
            if let Err(e) = written {
                log::warn!("vnc: stopped dumping the media stream: {e}");
            }
        });
        if let Err(e) = writer {
            log::warn!("vnc: not dumping the media stream: no writer thread: {e}");
            return None;
        }
        log::info!("vnc: dumping the media stream to {} (REMOTEX_HP_DUMP)", dir.display());
        Some(Self { units })
    }

    fn video(&self, unit: &AccessUnit) -> anyhow::Result<()> {
        let mut annex_b = Vec::with_capacity(unit.iter().map(|nal| 4 + nal.len()).sum());
        for nal in unit {
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(nal);
        }
        self.hand_over(Track::Video, annex_b)
    }

    fn audio(&self, unit: &[u8]) -> anyhow::Result<()> {
        let len = u32::try_from(unit.len()).context("an AAC-ELD unit past 4 GiB")?;
        let mut framed = Vec::with_capacity(4 + unit.len());
        framed.extend_from_slice(&len.to_be_bytes());
        framed.extend_from_slice(unit);
        self.hand_over(Track::Audio, framed)
    }

    fn hand_over(&self, track: Track, bytes: Vec<u8>) -> anyhow::Result<()> {
        match self.units.try_send((track, bytes)) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                anyhow::bail!("the disk fell {DUMP_QUEUE} units behind the stream")
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                anyhow::bail!("its writer stopped")
            }
        }
    }
}

/// The UDP side: RTCP out on every leg once a second and rate reports on each
/// picture's every [`RATE_FEEDBACK`], video in and depacketized, sound in and
/// decoded or passed. It runs until the session drops it, and stops early only
/// on a failure, which it leaves in `failed` and which ends the session.
struct Receiver {
    audio: tokio::net::UdpSocket,
    audio_srtp: SrtpReceiver,
    audio_rtcp: SrtcpSender,
    audio_reports: SrtcpReceiver,
    audio_ssrc: u32,
    /// Each display's video leg, in the Mac's order.
    videos: Vec<VideoLeg>,
    failed: Failure,
    sound_heard: Heard,
    sounded: Heard,
    sound: Option<std::sync::Arc<crate::audio::AudioBridge>>,
}

/// One display's video leg in the receiver: its socket and keys, and where its
/// pictures have got to.
struct VideoLeg {
    /// The display's number, from one, for the log.
    display: usize,
    socket: tokio::net::UdpSocket,
    port: u16,
    srtp: SrtpReceiver,
    rtcp: SrtcpSender,
    reports: SrtcpReceiver,
    /// This side's SSRC on the leg, and the Mac's, once a packet has named it.
    ssrc: u32,
    media_ssrc: u32,
    shared: Leg,
    onward: Onward,
    /// Set by the decoder thread for a unit that failed to decode.
    keyframe: std::sync::Arc<std::sync::atomic::AtomicBool>,
    assembly: Assembly,
    feedback: RateFeedback,
    keyframe_request: KeyframeRequest,
    /// The stream's picture size, from its parameter sets: what a refresh
    /// request names.
    size: Option<(u16, u16)>,
    /// The RTP timestamp of a picture handed on and not yet acknowledged.
    unacknowledged: Option<u32>,
    packets: u64,
    forged: u64,
    behind: u64,
    /// Frames handed on since the log last gave their rate, and the timestamp
    /// of the last: a frame in strips is as many pictures as changed.
    pictures: u64,
    counted: Option<u32>,
    /// Refreshes and keyframes asked of the Mac.
    asks: u64,
}

/// The picture a leg's stream needs before it can go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Ask {
    /// One predicted from a picture already handed on, after lost packets
    /// ([`rtcp_refresh`]).
    Refresh,
    /// An IDR ([`rtcp_pli`]): whoever is shown the stream has nothing to predict
    /// from.
    Keyframe,
}

/// A picture still to come on a leg: asked for as soon as the leg's stream has
/// an SSRC to name, and again every [`PLI_INTERVAL`] until one arrives, since
/// the Mac drops a request made too soon after its last keyframe and a still
/// screen sends nothing more to show that it did.
#[derive(Default)]
struct KeyframeRequest {
    owed: Option<Ask>,
    asked: Option<tokio::time::Instant>,
}

impl KeyframeRequest {
    /// An IDR wanted outlasts a refresh wanted, not the other way round.
    fn want(&mut self, ask: Ask) {
        self.owed = self.owed.max(Some(ask));
    }

    /// A picture the stream can go on from has arrived, or the stream it was
    /// asked of is over.
    fn settle(&mut self) {
        self.owed = None;
    }

    /// What to ask for at `now`, which counts as asking.
    fn due(&mut self, now: tokio::time::Instant) -> Option<Ask> {
        if self.asked.is_some_and(|at| now.duration_since(at) < PLI_INTERVAL) {
            return None;
        }
        let ask = self.owed?;
        self.asked = Some(now);
        Some(ask)
    }
}

/// What one datagram on a video leg asks of the receiver.
enum Took {
    Nothing,
    /// The stream cannot go on from here without this.
    Wants(Ask),
}

impl VideoLeg {
    /// Its sender, for the log: which display, where there is more than one.
    fn name(&self, alone: bool) -> String {
        if alone { "screen video".to_owned() } else { format!("screen video of display {}", self.display) }
    }

    /// One datagram off the leg's socket: authenticated, and its picture, once
    /// whole, handed on to whoever is shown the display.
    fn take(
        &mut self,
        data: &mut [u8],
        arrived: std::time::Instant,
        dump: &mut Option<MediaDump>,
        alone: bool,
    ) -> anyhow::Result<Took> {
        let header = match self.srtp.unprotect(data) {
            Ok(header) => header,
            // The Mac's report, which keeps the leg alive while a still screen
            // sends no picture.
            Err(SrtpError::Rtcp) => {
                if self.reports.authenticate(data).is_ok() {
                    *self.shared.heard.lock().unwrap() = Some(std::time::Instant::now());
                }
                return Ok(Took::Nothing);
            }
            Err(SrtpError::Forged) => {
                self.forged += 1;
                if self.forged <= 3 {
                    log::warn!(
                        "vnc: dropped a {} packet whose SRTP tag did not match",
                        self.name(alone)
                    );
                }
                return Ok(Took::Nothing);
            }
            Err(_) => return Ok(Took::Nothing),
        };
        *self.shared.heard.lock().unwrap() = Some(arrived);
        *self.shared.pictured.lock().unwrap() = Some(arrived);
        // Each offer says anew whether its stream comes in strips, by the
        // display's height, and the stream before it has stopped by then.
        let strips = self.shared.strips.load(std::sync::atomic::Ordering::Relaxed);
        if strips != matches!(self.assembly, Assembly::Strips(_)) {
            self.assembly = Assembly::new(strips);
        }
        let stream = self.assembly.stream(header.ssrc);
        self.feedback.received(stream, header.timestamp, arrived);
        self.packets += 1;
        if self.packets == 1 {
            log::info!("vnc: the Mac's {} is flowing (SSRC {:#x})", self.name(alone), header.ssrc);
        }
        if self.media_ssrc != stream {
            // A new stream, after an offer, starts with an IDR of its own.
            self.keyframe_request.settle();
            self.size = None;
        }
        self.media_ssrc = stream;
        // A display nobody is shown costs its packets' authentication and nothing
        // more. It starts over at the IDR [`MediaStream::show`] has asked for by
        // the time it is back in view, whose first packet may be the next one.
        if !self.shared.shown.load(std::sync::atomic::Ordering::Relaxed) {
            self.assembly.skip(&header);
            self.keyframe_request.settle();
            return Ok(Took::Nothing);
        }
        let payload = &data[header.payload.0..header.payload.1];
        let (strip, taken) = self.assembly.push(&header, payload);
        match taken {
            Depacketized::Pending => Ok(Took::Nothing),
            Depacketized::Lost if self.assembly.resumes_at_refresh() => Ok(Took::Wants(Ask::Refresh)),
            Depacketized::Lost => Ok(Took::Wants(Ask::Keyframe)),
            Depacketized::Unit(unit) => {
                // Only a stream that has had a picture it can go on from yields one.
                self.keyframe_request.settle();
                if let Some(params) =
                    unit.iter().find(|nal| nal_type(nal[0]) == NAL_SPS).and_then(|sps| parse_sps(sps))
                {
                    self.size = Some(params.size);
                }
                // The dump is the first display's stream.
                if self.display == 1
                    && let Some(d) = dump.as_ref()
                    && let Err(e) = d.video(&unit)
                {
                    log::warn!("vnc: stopped dumping the media stream: {e:#}");
                    *dump = None;
                }
                match self.onward.send(Coded { unit, strip, timestamp: header.timestamp }) {
                    Sent::Queued => {
                        if self.counted.replace(header.timestamp) != Some(header.timestamp) {
                            self.pictures += 1;
                        }
                        // A stream in strips has no refresh pictures to predict
                        // from an acknowledged one.
                        if strip.is_none() {
                            self.unacknowledged = Some(header.timestamp);
                        }
                        Ok(Took::Nothing)
                    }
                    Sent::Full(depth) => {
                        self.behind += 1;
                        if self.behind <= 3 {
                            log::warn!(
                                "vnc: {} fell {depth} pictures behind the Mac's {}; dropping to \
                                 its next keyframe",
                                self.onward.name(),
                                self.name(alone)
                            );
                        }
                        self.assembly.resync();
                        Ok(Took::Wants(Ask::Keyframe))
                    }
                    Sent::Unready => Ok(Took::Wants(Ask::Keyframe)),
                    Sent::Stopped => Err(anyhow::anyhow!("{} stopped", self.onward.name())),
                }
            }
        }
    }

    /// Ask the Mac for the picture `wanted` now or still owed, when the leg's
    /// stream has named its SSRC and the last request is [`PLI_INTERVAL`] old. It
    /// stays owed until [`Self::take`] has a picture.
    async fn ask_keyframe(&mut self, wanted: Option<Ask>) {
        if let Some(ask) = wanted {
            self.keyframe_request.want(ask);
        }
        if self.media_ssrc == 0 {
            return;
        }
        let Some(ask) = self.keyframe_request.due(tokio::time::Instant::now()) else {
            return;
        };
        self.asks += 1;
        let request = match (ask, self.size) {
            (Ask::Refresh, Some(size)) => self.rtcp.protect(&rtcp_refresh(self.ssrc, self.media_ssrc, size)),
            _ => self.rtcp.protect(&rtcp_pli(self.ssrc, self.media_ssrc)),
        };
        let _ = self.socket.send(&request).await;
    }

    /// Tell the Mac's encoder of the picture just handed on, which it may then
    /// predict a refresh from.
    async fn acknowledge(&mut self) {
        if let Some(timestamp) = self.unacknowledged.take() {
            let ack = self.rtcp.protect(&rtcp_reference_ack(self.ssrc, timestamp));
            let _ = self.socket.send(&ack).await;
        }
    }
}

/// A datagram off the second display's socket, on a stream that has one.
async fn second_received(videos: &[VideoLeg], datagram: &mut [u8]) -> std::io::Result<usize> {
    match videos.get(1) {
        Some(leg) => leg.socket.recv(datagram).await,
        None => std::future::pending().await,
    }
}

/// Which display the session asked a keyframe of.
async fn keyframe_wanted(videos: &[VideoLeg]) -> usize {
    match videos {
        [] => std::future::pending().await,
        [only] => {
            only.shared.keyframe_wanted.notified().await;
            0
        }
        [first, second, ..] => tokio::select! {
            () = first.shared.keyframe_wanted.notified() => 0,
            () = second.shared.keyframe_wanted.notified() => 1,
        },
    }
}

impl Receiver {
    /// The sockets, bound to the port numbers the Mac named and connected to
    /// the Mac's. Every Mac names the same ones (its RFB port and the next, and the
    /// one after for a second display), so a
    /// second gateway on this host with a High Performance session of its own
    /// binds them too: address and port reuse let both, and each socket being
    /// connected is what has the kernel hand each gateway its own Mac's packets.
    fn bind(media: &MediaStream, ports: &Ports) -> anyhow::Result<Self> {
        let bind = |port: u16, buffer: Option<usize>| -> anyhow::Result<tokio::net::UdpSocket> {
            let at = std::net::SocketAddr::new(media.local, port);
            let to = std::net::SocketAddr::new(media.peer, port);
            let socket = socket2::Socket::new(
                socket2::Domain::for_address(at),
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )?;
            socket.set_reuse_address(true)?;
            #[cfg(unix)]
            socket.set_reuse_port(true)?;
            if let Some(buffer) = buffer {
                socket
                    .set_recv_buffer_size(buffer)
                    .with_context(|| format!("size UDP {at}'s receive buffer"))?;
                let granted = socket.recv_buffer_size()?;
                // Linux reads back twice what it grants, the half beside the
                // payload being its own bookkeeping.
                #[cfg(target_os = "linux")]
                let granted = granted / 2;
                if granted < buffer {
                    log::warn!(
                        "vnc: UDP {port} was given a {granted}-byte receive buffer of the \
                         {buffer} asked for, so a keyframe of the Mac's screen video can \
                         overflow it; raise net.core.rmem_max to {buffer} on Linux"
                    );
                }
            }
            socket
                .bind(&at.into())
                .with_context(|| format!("bind UDP {at} for the Mac's media stream"))?;
            socket.connect(&to.into()).with_context(|| format!("connect UDP {at} to {to}"))?;
            socket.set_nonblocking(true)?;
            Ok(tokio::net::UdpSocket::from_std(socket.into())?)
        };
        let epoch = std::time::Instant::now();
        let videos = media
            .legs
            .iter()
            .zip(&media.offers.videos)
            .zip(&ports.videos)
            .enumerate()
            .map(|(index, ((shared, offer), port))| {
                let keyframe = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let onward = match &shared.pictures {
                    Outlet::Decoded(pictures) => {
                        let depth = DECODE_QUEUE * STRIPS;
                        let (units, decoder) = spawn_decoder(
                            pictures.clone(),
                            std::sync::Arc::clone(&keyframe),
                            std::sync::Arc::clone(&media.failed),
                            std::sync::Arc::clone(&shared.offered),
                            depth,
                        );
                        Onward::Decoder(units, decoder, depth)
                    }
                    Outlet::Passed(units) => Onward::Browser(Passer::default(), units.clone()),
                };
                Ok(VideoLeg {
                    display: index + 1,
                    socket: bind(*port, Some(VIDEO_RECEIVE_BUFFER))?,
                    port: *port,
                    srtp: SrtpReceiver::new(&offer.keys.1),
                    rtcp: SrtcpSender::new(&offer.keys.0),
                    reports: SrtcpReceiver::new(&offer.keys.1),
                    ssrc: offer.ssrc,
                    media_ssrc: 0,
                    shared: shared.clone(),
                    onward,
                    keyframe,
                    assembly: Assembly::new(shared.strips.load(std::sync::atomic::Ordering::Relaxed)),
                    feedback: RateFeedback::new(epoch),
                    keyframe_request: KeyframeRequest::default(),
                    size: None,
                    unacknowledged: None,
                    packets: 0,
                    forged: 0,
                    behind: 0,
                    pictures: 0,
                    counted: None,
                    asks: 0,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            audio: bind(ports.audio, None)?,
            audio_srtp: SrtpReceiver::new(&media.offers.audio_keys.1),
            audio_rtcp: SrtcpSender::new(&media.offers.audio_keys.0),
            audio_reports: SrtcpReceiver::new(&media.offers.audio_keys.1),
            audio_ssrc: media.offers.audio_ssrc,
            videos,
            failed: std::sync::Arc::clone(&media.failed),
            sound_heard: std::sync::Arc::clone(&media.sound_heard),
            sounded: std::sync::Arc::clone(&media.sounded),
            sound: media.sound.clone(),
        })
    }

    async fn run(mut self) {
        let alone = self.videos.len() == 1;
        let mut sound = self.sound.take().map(Sound::start);
        let mut rtcp = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut rate = tokio::time::interval(RATE_FEEDBACK);
        rate.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let started = tokio::time::Instant::now();
        let mut ticks = 0u32;
        let mut warned_silent = false;
        let mut datagram = vec![0u8; 65_536];
        let mut second_datagram = vec![0u8; if alone { 0 } else { 65_536 }];
        let mut sound_datagram = vec![0u8; 2048];
        let mut dump = MediaDump::from_env();
        let failure = loop {
            let mut want_keyframe = [None; MAX_DISPLAYS];
            tokio::select! {
                _ = rtcp.tick() => {
                    let audio = self.audio_rtcp.protect(&rtcp_receiver_report(self.audio_ssrc));
                    let _ = self.audio.send(&audio).await;
                    ticks += 1;
                    for leg in &mut self.videos {
                        let report = leg.rtcp.protect(&rtcp_receiver_report(leg.ssrc));
                        let _ = leg.socket.send(&report).await;
                        if ticks.is_multiple_of(RATE_REPORT) && leg.pictures > 0 {
                            log::debug!(
                                "vnc: {:.1} pictures a second of the Mac's {} over the last {RATE_REPORT}s, \
                                 {} dropped behind {} so far, {} refreshes or keyframes asked for, \
                                 {:.1} ms of one-way delay reported",
                                leg.pictures as f64 / f64::from(RATE_REPORT),
                                leg.name(alone),
                                leg.behind,
                                leg.onward.name(),
                                leg.asks,
                                leg.feedback.delay() * 1000.0
                            );
                            leg.pictures = 0;
                        }
                        if leg.packets == 0 && !warned_silent && started.elapsed() >= SILENT_START {
                            warned_silent = true;
                            log::warn!(
                                "vnc: the Mac named UDP {} for its {} but nothing has \
                                 arrived in {}s — it sends to this gateway's address on that port, \
                                 so a firewall or NAT between them is what this looks like",
                                leg.port,
                                leg.name(alone),
                                SILENT_START.as_secs()
                            );
                        }
                    }
                }
                _ = rate.tick() => {
                    for leg in &mut self.videos {
                        if let Some(report) = leg.feedback.report(leg.ssrc, std::time::Instant::now()) {
                            let report = leg.rtcp.protect(&report);
                            let _ = leg.socket.send(&report).await;
                        }
                    }
                }
                received = self.audio.recv(&mut sound_datagram) => {
                    let len = match received {
                        Ok(len) => len,
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => continue,
                        Err(e) => break anyhow::Error::new(e).context("the Mac's sound socket failed"),
                    };
                    let data = &mut sound_datagram[..len];
                    match (self.audio_srtp.unprotect(data), sound.as_mut()) {
                        (Ok(header), sound) => {
                            let now = Some(std::time::Instant::now());
                            *self.sounded.lock().unwrap() = now;
                            *self.sound_heard.lock().unwrap() = now;
                            if let Some(d) = dump.as_ref()
                                && let Err(e) = d.audio(&data[header.payload.0..header.payload.1])
                            {
                                log::warn!("vnc: stopped dumping the media stream: {e:#}");
                                dump = None;
                            }
                            if let Some(sound) = sound {
                                sound.push(&header, &data[header.payload.0..header.payload.1]);
                            }
                        }
                        (Err(SrtpError::Forged), Some(sound)) => sound.forged(),
                        (Err(SrtpError::Rtcp), _) => {
                            if self.audio_reports.authenticate(data).is_ok() {
                                *self.sound_heard.lock().unwrap() = Some(std::time::Instant::now());
                            }
                        }
                        (Err(_), _) => {}
                    }
                }
                received = self.videos[0].socket.recv(&mut datagram) => {
                    let arrived = std::time::Instant::now();
                    let len = match received {
                        Ok(len) => len,
                        // A connected socket's report of an ICMP unreachable, for a
                        // report sent before the Mac's side was up.
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => continue,
                        Err(e) => {
                            break anyhow::Error::new(e).context("the Mac's screen video socket failed");
                        }
                    };
                    match self.videos[0].take(&mut datagram[..len], arrived, &mut dump, alone) {
                        Ok(Took::Nothing) => {}
                        Ok(Took::Wants(ask)) => want_keyframe[0] = Some(ask),
                        Err(e) => break e,
                    }
                }
                received = second_received(&self.videos, &mut second_datagram) => {
                    let arrived = std::time::Instant::now();
                    let len = match received {
                        Ok(len) => len,
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => continue,
                        Err(e) => {
                            break anyhow::Error::new(e)
                                .context("the Mac's second screen video socket failed");
                        }
                    };
                    match self.videos[1].take(&mut second_datagram[..len], arrived, &mut dump, alone) {
                        Ok(Took::Nothing) => {}
                        Ok(Took::Wants(ask)) => want_keyframe[1] = Some(ask),
                        Err(e) => break e,
                    }
                }
                leg = keyframe_wanted(&self.videos) => {
                    self.videos[leg].assembly.resync();
                    self.videos[leg].keyframe_request.want(Ask::Keyframe);
                }
            }
            for (leg, wanted) in self.videos.iter_mut().zip(want_keyframe) {
                let failed = leg.keyframe.swap(false, std::sync::atomic::Ordering::Relaxed);
                if failed {
                    leg.assembly.resync();
                }
                leg.acknowledge().await;
                leg.ask_keyframe(if failed { Some(Ask::Keyframe) } else { wanted }).await;
            }
        };
        // The stream has failed while the RFB session goes on. The reason, then
        // `None`, tell the read loop, which ends the session; `None` goes after the
        // decoder's last picture, or that picture would be the last word.
        log::warn!("vnc: the Mac's media receiver stopped: {failure:#}");
        fail(&self.failed, failure);
        for leg in self.videos {
            match leg.onward {
                Onward::Decoder(units, decoder, _) => {
                    drop(units);
                    let _ = tokio::task::spawn_blocking(move || decoder.join()).await;
                    if let Outlet::Decoded(pictures) = &leg.shared.pictures {
                        pictures.send_replace(None);
                    }
                }
                Onward::Browser(_, units) => {
                    let _ = units.send(None).await;
                }
            }
        }
    }
}

/// An access unit on its way [`Onward`].
struct Coded {
    unit: AccessUnit,
    /// Which strip of the display it is, of a stream in strips.
    strip: Option<usize>,
    /// Its frame's RTP timestamp, which the strips of one frame share.
    timestamp: u32,
}

/// Where the receiver sends each access unit it reassembles.
enum Onward {
    /// The decoder thread, whose pictures the session encodes as VP9.
    /// Its queue is as many units deep.
    Decoder(std::sync::mpsc::SyncSender<Coded>, std::thread::JoinHandle<()>, usize),
    /// The read loop, which passes each unit to the browser as it came.
    Browser(Passer, tokio::sync::mpsc::Sender<Option<PassedUnit>>),
}

/// What became of one access unit sent [`Onward`].
enum Sent {
    Queued,
    /// Its queue, this deep, is full: dropped, and the stream is dropped to its next
    /// keyframe.
    Full(usize),
    /// No parameter set has come to describe it yet: dropped, and a keyframe, which
    /// carries them, asked for.
    Unready,
    Stopped,
}

impl Onward {
    fn send(&mut self, coded: Coded) -> Sent {
        match self {
            Self::Decoder(units, _, depth) => match units.try_send(coded) {
                Ok(()) => Sent::Queued,
                Err(std::sync::mpsc::TrySendError::Full(_)) => Sent::Full(*depth),
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Sent::Stopped,
            },
            Self::Browser(passer, units) => {
                let Some(passed) = passer.pass(&coded.unit) else {
                    return Sent::Unready;
                };
                match units.try_send(Some(passed)) {
                    Ok(()) => Sent::Queued,
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Sent::Full(PASS_QUEUE),
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Sent::Stopped,
                }
            }
        }
    }

    /// What the units go to, for the log.
    fn name(&self) -> &'static str {
        match self {
            Self::Decoder(..) => "the HEVC decoder",
            Self::Browser(..) => "the browser's link",
        }
    }
}

/// Leave `error` as why the stream stopped, unless an earlier reason is there: a
/// decoder thread's own, which the receive task's is only the consequence of.
fn fail(failed: &Failure, error: anyhow::Error) {
    let mut failed = failed.lock().unwrap();
    if failed.is_none() {
        *failed = Some(error);
    }
}

/// How long a frame some of whose strips have come waits for the rest before it
/// is shown as it stands. Nothing says how many strips a frame has: the next
/// frame's first strip does, or this. The Mac codes a frame's strips one after
/// another, and they arrived up to 7 ms apart.
const STRIP_WAIT: std::time::Duration = std::time::Duration::from_millis(8);

/// A display's picture, put together from its strips as they decode.
struct Canvas {
    picture: Picture,
    /// A strip's rows, the last strip's running past the display's.
    pitch: usize,
    /// The strips placed since the canvas was made, a bit each: the display is
    /// shown once it has them all.
    placed: u8,
    /// The strips placed since the display was last shown.
    fresh: u8,
}

/// Every strip's bit.
const ALL_STRIPS: u8 = (1 << STRIPS) - 1;

impl Canvas {
    /// Put the decoded `part` in as strip `strip` of a display of `size`,
    /// starting `canvas` over where it is laid out for another.
    fn place(canvas: &mut Option<Self>, size: Option<(u16, u16)>, strip: usize, part: &Picture) -> anyhow::Result<()> {
        let (width, height) = size.context("a strip came for a display no stream was offered for")?;
        let pitch = usize::from(part.size.1);
        let rows = usize::from(height);
        anyhow::ensure!(
            part.size.0 == width && pitch * STRIPS >= rows && pitch * (STRIPS - 1) <= rows,
            "a {}\u{d7}{} strip is not a quarter of the {width}\u{d7}{height} display offered",
            part.size.0,
            part.size.1
        );
        let canvas = match canvas {
            Some(canvas) if canvas.picture.size == (width, height) && canvas.pitch == pitch => canvas,
            _ => canvas.insert(Self {
                picture: Picture { size: (width, height), rgb: vec![0; usize::from(width) * rows * 3] },
                pitch,
                placed: 0,
                fresh: 0,
            }),
        };
        let row = usize::from(width) * 3;
        let from = (strip * pitch * row).min(canvas.picture.rgb.len());
        let into = &mut canvas.picture.rgb[from..];
        let len = into.len().min(part.rgb.len());
        into[..len].copy_from_slice(&part.rgb[..len]);
        canvas.placed |= 1 << strip;
        canvas.fresh |= 1 << strip;
        Ok(())
    }

    /// The display as it stands, once every strip of it has come.
    fn show(&mut self) -> Option<std::sync::Arc<Picture>> {
        self.fresh = 0;
        (self.placed == ALL_STRIPS)
            .then(|| std::sync::Arc::new(Picture { size: self.picture.size, rgb: self.picture.rgb.clone() }))
    }
}

/// The decoder thread: access units in, pictures out to the watch. Its queue's
/// sender is the handle, and the thread, whose own handle comes with it, ends when
/// the receive task drops it.
/// `keyframe` is how it says a unit failed to decode, which the receive task turns
/// into a PLI. A decoder that cannot be opened leaves why in `failed`.
///
/// A stream in strips decodes to its strips, in their one decoding order, and
/// the display is shown when a frame's strips are all in: every strip has come
/// for its timestamp, the next unit is another frame's, or [`STRIP_WAIT`] has
/// passed. `offered` is the display the strips are a quarter each of.
fn spawn_decoder(
    pictures: tokio::sync::watch::Sender<Option<std::sync::Arc<Picture>>>,
    keyframe: std::sync::Arc<std::sync::atomic::AtomicBool>,
    failed: Failure,
    offered: std::sync::Arc<std::sync::Mutex<Option<(u16, u16)>>>,
    queue: usize,
) -> (std::sync::mpsc::SyncSender<Coded>, std::thread::JoinHandle<()>) {
    use std::sync::mpsc::RecvTimeoutError;

    let (units, inbox) = std::sync::mpsc::sync_channel::<Coded>(queue);
    let thread = std::thread::spawn(move || {
        let mut decoder = match Hevc::new(true) {
            Ok(decoder) => decoder,
            Err(e) => return fail(&failed, e.context("no HEVC decoder")),
        };
        let mut failures: u64 = 0;
        let mut canvas: Option<Canvas> = None;
        // The timestamp of the frame whose strips are placed and not yet shown.
        let mut waiting: Option<u32> = None;
        let show = |canvas: &mut Option<Canvas>| {
            if let Some(picture) = canvas.as_mut().and_then(Canvas::show) {
                pictures.send_replace(Some(picture));
            }
        };
        loop {
            let coded = match waiting {
                None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(_) => inbox.recv_timeout(STRIP_WAIT),
            };
            let coded = match coded {
                Ok(coded) => coded,
                Err(RecvTimeoutError::Timeout) => {
                    waiting = None;
                    show(&mut canvas);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };
            if waiting.is_some_and(|timestamp| timestamp != coded.timestamp) {
                waiting = None;
                show(&mut canvas);
            }
            let decoded = decoder.decode(&coded.unit).and_then(|picture| match (picture, coded.strip) {
                (Some(picture), None) => {
                    pictures.send_replace(Some(std::sync::Arc::new(picture)));
                    Ok(())
                }
                (Some(part), Some(strip)) => {
                    Canvas::place(&mut canvas, *offered.lock().unwrap(), strip, &part)?;
                    if canvas.as_ref().is_some_and(|canvas| canvas.fresh == ALL_STRIPS) {
                        waiting = None;
                        show(&mut canvas);
                    } else {
                        waiting = Some(coded.timestamp);
                    }
                    Ok(())
                }
                // Each strip is a picture, and one the decoder kept back would
                // come out as the next strip's.
                (None, Some(_)) => anyhow::bail!("the decoder held a strip's picture back"),
                (None, None) => Ok(()),
            });
            if let Err(e) = decoded {
                failures += 1;
                if failures <= 3 {
                    log::warn!("vnc: a screen video picture did not decode: {e:#}");
                }
                keyframe.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
    (units, thread)
}

/// The sound leg on the receive task's side: authenticated, decrypted access units
/// out to the bridge as they came ([`crate::audio::AudioBridge::unit`]), for the
/// browser to decode. With no browser listening the bridge drops them.
struct Sound {
    bridge: std::sync::Arc<crate::audio::AudioBridge>,
    packets: u64,
    forged: u64,
}

impl Sound {
    fn start(bridge: std::sync::Arc<crate::audio::AudioBridge>) -> Self {
        Self { bridge, packets: 0, forged: 0 }
    }

    /// One authenticated, decrypted RTP packet of the sound leg: one AAC-ELD
    /// access unit, 10 ms of 48 kHz stereo.
    fn push(&mut self, header: &RtpHeader, unit: &[u8]) {
        self.packets += 1;
        if self.packets == 1 {
            log::info!(
                "vnc: the Mac's sound is flowing: RTP payload type {}, {} bytes per unit, SSRC {:#x}",
                header.payload_type,
                unit.len(),
                header.ssrc
            );
        }
        self.bridge.unit(unit.to_vec());
    }

    fn forged(&mut self) {
        self.forged += 1;
        if self.forged <= 3 {
            log::warn!("vnc: dropped a sound packet whose SRTP tag did not match");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_keyframe_request_is_repeated_until_a_picture_settles_it() {
        use super::Ask;
        let start = tokio::time::Instant::now();
        let mut request = super::KeyframeRequest::default();
        assert_eq!(request.due(start), None, "nothing is owed");
        request.want(Ask::Refresh);
        assert_eq!(request.due(start), Some(Ask::Refresh));
        assert_eq!(request.due(start + super::PLI_INTERVAL / 2), None, "too soon to ask again");
        // The Mac may have dropped the first: it is still owed.
        assert_eq!(request.due(start + super::PLI_INTERVAL), Some(Ask::Refresh));
        // An IDR wanted meanwhile is what goes out, and a refresh does not undo it.
        request.want(Ask::Keyframe);
        request.want(Ask::Refresh);
        assert_eq!(request.due(start + super::PLI_INTERVAL * 2), Some(Ask::Keyframe));
        request.settle();
        assert_eq!(request.due(start + super::PLI_INTERVAL * 4), None, "a picture came");
    }

    /// The packets a probe of macvm sent, which its encoder answered with a
    /// refresh picture.
    #[test]
    fn the_refresh_request_and_the_acknowledgement_are_avconferences() {
        assert_eq!(
            super::rtcp_refresh(0xb54e_0c1c, 0x39b8_5839, (1600, 1000)),
            [0x82, 0xce, 0, 3, 0xb5, 0x4e, 0x0c, 0x1c, 0x39, 0xb8, 0x58, 0x39, 0x06, 0x40, 0x03, 0xe8]
        );
        assert_eq!(
            super::rtcp_reference_ack(0x1203_c98d, 200_800),
            [0x80, 0xcc, 0, 3, 0x12, 0x03, 0xc9, 0x8d, 0, 0, 0, 5, 0x00, 0x03, 0x10, 0x60]
        );
    }

    /// The Mac's extension word is `0x9301` or `0x9311`, and `0x9331` on a
    /// refresh picture.
    #[test]
    fn a_refresh_picture_is_marked_in_the_header_extension() {
        let packet = |profile: u16| {
            let mut data = vec![0x90, 100, 0, 1, 0, 0, 0, 0, 0, 0, 0, 9];
            data.extend_from_slice(&profile.to_be_bytes());
            data.extend_from_slice(&[0, 1, 0, 1, 0xd5, 0x2f]);
            data.extend_from_slice(&[0; 2 + super::AUTH_TAG_LEN]);
            super::rtp_header(&data).map(|header| header.refresh)
        };
        assert_eq!(packet(0x9301), Ok(false));
        assert_eq!(packet(0x9311), Ok(false));
        assert_eq!(packet(0x9331), Ok(true));
        assert_eq!(packet(0x1020), Ok(false), "another extension's bits mean nothing");
    }

    /// After lost packets the pictures handed on are still there to predict
    /// from, so the stream goes on at the Mac's refresh picture; after a resync
    /// they are not, and only a random-access picture will do.
    #[test]
    fn a_stream_goes_on_from_a_refresh_picture_after_a_loss_but_not_after_a_resync() {
        let refresh = |sequence, timestamp| RtpHeader { refresh: true, ..header(sequence, timestamp, true) };
        let mut d = Depacketizer::default();
        assert_eq!(d.push(&header(10, 0, false), &aggregation(&[VPS, SPS])), Depacketized::Pending);
        assert!(matches!(d.push(&header(11, 0, true), IDR), Depacketized::Unit(_)));
        // 12 is lost: 13 predicts from it, and the refresh picture does not.
        assert_eq!(d.push(&header(13, 800, true), TRAIL), Depacketized::Lost);
        assert!(d.resumes_at_refresh());
        assert_eq!(d.push(&header(14, 1200, true), TRAIL), Depacketized::Lost);
        assert_eq!(d.push(&refresh(15, 1600), TRAIL), Depacketized::Unit(vec![TRAIL.to_vec()]));
        assert!(!d.resumes_at_refresh());
        assert_eq!(d.push(&header(16, 2000, true), TRAIL), Depacketized::Unit(vec![TRAIL.to_vec()]));
        // A refresh picture that lost a packet of its own is no start.
        assert_eq!(d.push(&RtpHeader { marker: false, ..refresh(18, 2400) }, TRAIL), Depacketized::Lost);
        assert_eq!(d.push(&header(20, 2400, true), TRAIL), Depacketized::Lost);
        assert!(d.resumes_at_refresh());
        // Whoever was shown the stream gave pictures up: a refresh predicts from
        // one of them.
        d.resync();
        assert!(!d.resumes_at_refresh());
        assert_eq!(d.push(&refresh(21, 2800), TRAIL), Depacketized::Lost);
        assert_eq!(d.push(&header(22, 3200, false), &aggregation(&[VPS, SPS])), Depacketized::Pending);
        assert!(matches!(d.push(&header(23, 3200, true), IDR), Depacketized::Unit(_)));
    }

    use super::*;

    fn unhex(hex: &str) -> Vec<u8> {
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
    }

    /// The media blob Apple's own `AVCMediaStreamNegotiator initWithMode:8` produced
    /// on macOS 26.6 for SSRC 3606155525, byte for byte.
    const CAPTURED_AUDIO_BLOB: &str = "080110011a120885a2c6b70d1000180020ffbc0128003000320d56696365726f7920312e372e3040004a05080110ab024a0908ea1f1000188080014a0b080010c0d1e123188080204a0b08001080dac409188080064a0a08001080b489131880604a0b080010808ece1c188080104a0508101084204a0b08001080c2d72f188080404a05080410e4324a0b080010809bee02188080086880e0f084deafb6a3ee017002800100";

    /// And the `initWithMode:7` blob for SSRC 3023179925 at 1600×1000, with
    /// Apple's four tiles to a frame.
    const CAPTURED_VIDEO_BLOB: &str = "080110012af5010895a1c8a10b10001a7d087b120a0801100118c387032000120a0801100218c387032000120a0801100118c387032000120a0801100218c3870320001a47464c533b4d533a2d313b4c463a2d313b4c54523b43414241433b504f533a303b454f443a313b4854533a323b52523a333b41523a342f332c352f383b58523a342f332c352f383b20011a5c0864120a0801100118c387032000120a0801100218c3870320001a3e464c533b4c463a2d313b504f533a353b454f443a313b4854533a323b52523a333b504f53453a343b41523a342f332c352f383b58523a342f332c352f383b200e20c00c28e80730043801403f48016001320d56696365726f7920312e372e3040004a0a08001080b489131880604a05080110ab024a0908ea1f1000188080014a0b080010c0d1e123188080204a0b08001080dac409188080064a0b080010808ece1c188080104a0508101084204a0b08001080c2d72f188080404a05080410e4324a0b080010809bee0218808008688080e7dedfafb6a3ee017002800100";

    #[test]
    fn the_blobs_are_what_apples_negotiator_produces() {
        assert_eq!(audio_offer_blob(3_606_155_525), unhex(CAPTURED_AUDIO_BLOB));
        assert_eq!(
            video_offer_blob(3_023_179_925, (1600, 1000), 4),
            unhex(CAPTURED_VIDEO_BLOB)
        );
    }

    /// A protobuf's top-level fields as `(field, varint)` and `(field, bytes)`,
    /// read independently of the writer above.
    fn fields(mut b: &[u8]) -> Vec<(u64, Result<u64, Vec<u8>>)> {
        fn varint(b: &mut &[u8]) -> u64 {
            let mut v = 0;
            let mut shift = 0;
            loop {
                let byte = b[0];
                *b = &b[1..];
                v |= u64::from(byte & 0x7f) << shift;
                shift += 7;
                if byte & 0x80 == 0 {
                    return v;
                }
            }
        }
        let mut out = Vec::new();
        while !b.is_empty() {
            let tag = varint(&mut b);
            match tag & 7 {
                0 => out.push((tag >> 3, Ok(varint(&mut b)))),
                2 => {
                    let len = varint(&mut b) as usize;
                    out.push((tag >> 3, Err(b[..len].to_vec())));
                    b = &b[len..];
                }
                other => panic!("wire type {other}"),
            }
        }
        out
    }

    /// One tile to a frame, as a passed stream is offered, and Apple's bitrate
    /// entries, the 40 Mbit/s one first, up to 100 Mbit/s.
    #[test]
    fn the_video_offer_asks_for_one_picture_a_frame_at_apples_bitrates() {
        let blob = video_offer_blob(7, (1280, 800), 1);
        let top = fields(&blob);
        let stream = top.iter().find_map(|(f, v)| (*f == 5).then(|| v.clone().unwrap_err())).unwrap();
        let stream = fields(&stream);
        let value = |field| stream.iter().find_map(|(f, v)| (*f == field).then(|| *v.as_ref().unwrap()));
        assert_eq!(value(4), Some(1280));
        assert_eq!(value(5), Some(800));
        assert_eq!(value(6), Some(1), "tilesPerFrame");
        let bitrates: Vec<u64> = top
            .iter()
            .filter(|(f, _)| *f == 9)
            .map(|(_, v)| fields(v.as_ref().unwrap_err()))
            .filter(|entry| entry[0] == (1, Ok(0)))
            .map(|entry| *entry[1].1.as_ref().unwrap())
            .collect();
        assert_eq!(
            bitrates,
            [40_000_000, 75_000_000, 20_000_000, 60_000_000, 100_000_000, 6_000_000]
        );
    }

    /// The plist around the blob: header, the dictionary's shape, and a trailer
    /// whose offsets land on the objects they name.
    #[test]
    fn the_offer_is_a_binary_plist_of_four_entries() {
        let plist = offer(MODE_AUDIO, &audio_offer_blob(1), "910BCF8F-D1D7-4EB6-B728-E1FDB02DD3B6");
        assert_eq!(&plist[..8], b"bplist00");
        assert_eq!(&plist[8..17], &[0xd4, 1, 2, 3, 4, 5, 6, 7, 8]);
        let trailer = &plist[plist.len() - 32..];
        assert_eq!(&trailer[..8], &[0, 0, 0, 0, 0, 0, 2, 1]);
        assert_eq!(u64::from_be_bytes(trailer[8..16].try_into().unwrap()), 9);
        let table = u64::from_be_bytes(trailer[24..32].try_into().unwrap()) as usize;
        let offsets: Vec<usize> = (0..9)
            .map(|i| usize::from(u16::from_be_bytes([plist[table + 2 * i], plist[table + 2 * i + 1]])))
            .collect();
        assert_eq!(offsets[0], 8, "the dictionary is the first object");
        for (i, key) in [
            "avcMediaStreamOptionRemoteEndpointInfo",
            "avcMediaStreamNegotiatorMode",
            "avcMediaStreamNegotiatorMediaBlob",
            "avcMediaStreamOptionCallID",
        ]
        .iter()
        .enumerate()
        {
            let at = offsets[1 + i];
            assert_eq!(&plist[at..at + 3], &[0x5f, 0x10, key.len() as u8]);
            assert_eq!(&plist[at + 3..at + 3 + key.len()], key.as_bytes());
        }
        assert_eq!(&plist[offsets[6]..offsets[6] + 2], &[0x10, MODE_AUDIO]);
        assert_eq!(&plist[offsets[8]..offsets[8] + 3], &[0x5f, 0x10, 36]);
    }

    #[test]
    fn the_configuration_message_lays_out_as_measured() {
        let a = ([0xa1; 46], [0xa2; 46]);
        let v = ([0xb1; 46], [0xb2; 46]);
        let msg = configuration_message(FLAGS, &[0x11; 16], &[0xaa; 300], &a, &[(&[0xbb; 400], &v)]);
        assert_eq!(msg[0], 0x1c);
        assert_eq!(usize::from(u16::from_be_bytes([msg[2], msg[3]])), msg.len() - 4);
        assert_eq!(&msg[4..6], &[0, 3]);
        assert_eq!(&msg[6..10], &[0, 0, 0, 5], "60 fps, and the pointer left out");
        assert_eq!(&msg[0x0a..0x0c], &300u16.to_be_bytes());
        assert_eq!(&msg[0x0c..0x0e], &400u16.to_be_bytes());
        assert_eq!(&msg[0x0e..0x10], &[0, 0]);
        assert_eq!(&msg[0x14..0x24], &[0x11; 16]);
        assert_eq!(&msg[0x24..0x52], &a.0);
        assert_eq!(&msg[0x52..0x80], &a.1);
        let video_keys = 0x80 + 300;
        assert_eq!(&msg[video_keys..video_keys + 46], &v.0);
        assert_eq!(&msg[video_keys + 46..video_keys + 92], &v.1);
        assert_eq!(msg.len(), video_keys + 92 + 400);

        // A second display's keys and offer follow the first's, its length beside
        // the first's in the header.
        let w = ([0xc1; 46], [0xc2; 46]);
        let two = configuration_message(
            FLAGS | FLAG_60FPS_SECOND,
            &[0x11; 16],
            &[0xaa; 300],
            &a,
            &[(&[0xbb; 400], &v), (&[0xcc; 500], &w)],
        );
        assert_eq!(usize::from(u16::from_be_bytes([two[2], two[3]])), two.len() - 4);
        assert_eq!(&two[6..10], &[0, 0, 0, 7]);
        assert_eq!(&two[0x0c..0x0e], &400u16.to_be_bytes());
        assert_eq!(&two[0x0e..0x10], &500u16.to_be_bytes());
        assert_eq!(two[..0x0e], {
            let mut head = msg[..0x0e].to_vec();
            head[2..4].copy_from_slice(&two[2..4]);
            head[6..10].copy_from_slice(&two[6..10]);
            head
        });
        let second_keys = msg.len();
        assert_eq!(two[0x10..second_keys], msg[0x10..]);
        assert_eq!(&two[second_keys..second_keys + 46], &w.0);
        assert_eq!(&two[second_keys + 46..second_keys + 92], &w.1);
        assert_eq!(two.len(), second_keys + 92 + 500);
        // What the Mac checks the message against: 0xd8 with the first display's
        // keys, the offers, and 0x5c of keys before a second display's offer.
        assert_eq!(two.len() - 4, 0xd8 + 300 + 400 + 0x5c + 500);
    }

    #[test]
    fn the_replies_parse_as_measured() {
        // Message 1 as the Mac sent it: audio at 5900, video at 5901.
        let ports = unhex("0001000100000000170c00000001170d0000000100000000000000000000000000000000");
        assert_eq!(
            parse_media_reply(&ports).unwrap(),
            MediaReply::Ports { audio_port: 5900, video_port: 5901, video2_port: None }
        );
        let answer = unhex("000200020000000000020003000000000000aabbccddee");
        assert_eq!(parse_media_reply(&answer).unwrap(), MediaReply::Answer { videos: 1 });
        let error = unhex("00030001000000000000000200000000");
        assert_eq!(parse_media_reply(&error).unwrap(), MediaReply::Error { kind: 2, sub_code: 0 });
        assert_eq!(parse_media_reply(&[0, 9, 0, 1, 0, 0, 0, 0]).unwrap(), MediaReply::Other(9));
        assert!(parse_media_reply(&[0, 1, 0, 1]).is_err());
        assert!(parse_media_reply(&[0, 1, 0, 1, 0, 0, 0, 0, 0]).is_err());
        let mut disabled_audio = ports.clone();
        disabled_audio[13] = 0;
        assert!(parse_media_reply(&disabled_audio).is_err());
        // Two virtual displays: the second's leg enabled, at the port after the
        // first's, and an answer with a blob for it.
        let mut second_video = ports.clone();
        second_video[20..22].copy_from_slice(&5902u16.to_be_bytes());
        second_video[25] = 1;
        assert_eq!(
            parse_media_reply(&second_video).unwrap(),
            MediaReply::Ports { audio_port: 5900, video_port: 5901, video2_port: Some(5902) }
        );
        let two_answers = unhex("000200020000000000020003000100000000aabbccddeeff");
        assert_eq!(parse_media_reply(&two_answers).unwrap(), MediaReply::Answer { videos: 2 });
        let mut wrong_answer_size = answer;
        wrong_answer_size.pop();
        assert!(parse_media_reply(&wrong_answer_size).is_err());
    }

    /// The master key `0, 1, …, 45` both vectors below were made with.
    fn master() -> MasterKey {
        std::array::from_fn(|i| i as u8)
    }

    /// A packet protected by the Python probe's SRTP — the one that decrypted and
    /// authenticated the Mac's own packets — is authenticated and decrypted here.
    #[test]
    fn srtp_unprotects_what_an_independent_implementation_protected() {
        let packet = unhex("80e412340a0b0c0dcafebabe610005049285d9955b8928769900c5323d679c04bbccbf");
        let mut data = packet.clone();
        let header = SrtpReceiver::new(&master()).unprotect(&mut data).unwrap();
        assert_eq!(
            (header.payload_type, header.marker, header.sequence, header.timestamp, header.ssrc),
            (100, true, 0x1234, 0x0a0b_0c0d, 0xcafe_babe)
        );
        assert_eq!(&data[header.payload.0..header.payload.1], b"\x02\x01hello, hevc");

        let mut forged = packet.clone();
        forged[14] ^= 1;
        assert_eq!(SrtpReceiver::new(&master()).unprotect(&mut forged), Err(SrtpError::Forged));
        let mut other_key = packet;
        assert_eq!(SrtpReceiver::new(&[7; 46]).unprotect(&mut other_key), Err(SrtpError::Forged));
    }

    #[test]
    fn srtcp_protects_as_an_independent_implementation_does() {
        let mut sender = SrtcpSender::new(&master());
        assert_eq!(
            sender.protect(&rtcp_pli(0x0102_0304, 0xcafe_babe)),
            unhex("81ce00020102030456a0870b8000000085553297faec4eb2d978")
        );
        assert_eq!(
            sender.protect(&rtcp_receiver_report(0x0102_0304)),
            unhex("80c900010102030480000001bcb6f8d4262f3495b496"),
            "the index counts up"
        );
    }

    /// The Mac's reports are authenticated with the same derivation this side's
    /// are protected with, whose vectors an independent implementation made.
    #[test]
    fn srtcp_authenticates_a_report_and_refuses_a_forged_or_replayed_one() {
        let mut sender = SrtcpSender::new(&master());
        let first = sender.protect(&rtcp_receiver_report(0x0102_0304));
        let second = sender.protect(&rtcp_receiver_report(0x0102_0304));
        let mut reports = SrtcpReceiver::new(&master());
        assert_eq!(reports.authenticate(&first), Ok(()));
        assert_eq!(reports.authenticate(&first), Err(SrtpError::Stale), "a duplicate");
        assert_eq!(reports.authenticate(&second), Ok(()), "the index counts up");
        assert_eq!(reports.authenticate(&first), Err(SrtpError::Stale), "overtaken");
        let mut forged = second.clone();
        forged[9] ^= 1;
        assert_eq!(SrtcpReceiver::new(&master()).authenticate(&forged), Err(SrtpError::Forged));
        assert_eq!(SrtcpReceiver::new(&[7; 46]).authenticate(&second), Err(SrtpError::Forged));
        assert_eq!(SrtcpReceiver::new(&master()).authenticate(&second[..20]), Err(SrtpError::NotRtp));

        let other = SrtcpSender::new(&master()).protect(&rtcp_receiver_report(0x0506_0708));
        assert_eq!(reports.authenticate(&other), Ok(()), "a new stream starts over");
        assert_eq!(reports.authenticate(&second), Err(SrtpError::Stale), "an earlier stream's stays spent");
    }

    /// Byte for byte a report the Mac took from the rate-control probe, 5.9 s into
    /// its stream with 2970 picture packets in: the echo of a timestamp >> 8, no
    /// hold since it, the clock, no delay, the count and the bandwidth.
    #[test]
    fn a_rate_report_is_one_the_mac_took() {
        let epoch = std::time::Instant::now();
        let at = epoch + std::time::Duration::from_nanos(5_908_203_200);
        let mut feedback = RateFeedback::new(epoch);
        assert_eq!(feedback.report(0x0d96_7839, at), None, "nothing to echo yet");
        for _ in 0..2970 {
            feedback.received(0x1234_5678, 0x8042, at);
        }
        assert_eq!(
            feedback.report(0x0d96_7839, at).unwrap()[..],
            unhex("80cc00070d9678395243544c85000004008000000000000017a200000b9aea60")[..]
        );
    }

    /// A lag that holds steady reads as no delay, one that grows as a queue builds
    /// reads as that queue, and the floor follows the lag back down at once.
    #[test]
    fn the_reported_delay_is_a_queue_building_between_the_mac_and_here() {
        let epoch = std::time::Instant::now();
        let ms = |n: u64| epoch + std::time::Duration::from_millis(n);
        let mut feedback = RateFeedback::new(epoch);
        // 30 pictures a second, 800 ticks of 24 kHz apart, each 5 ms in transit.
        let picture = |feedback: &mut RateFeedback, n: u64, queued: u64| {
            feedback.received(9, 0xffff_f000_u32.wrapping_add(n as u32 * 800), ms(n * 100 / 3 + 5 + queued));
        };
        for n in 0..300 {
            picture(&mut feedback, n, 0);
        }
        assert!(feedback.delay() < 0.001, "{}", feedback.delay());

        // Later packets of a picture arrive after its first and change nothing.
        let before = feedback.delay();
        feedback.received(9, 0xffff_f000_u32.wrapping_add(299 * 800), ms(20_000));
        assert_eq!(feedback.delay(), before);

        // A queue growing by 10 ms a picture, to 300 ms.
        for n in 300..330 {
            picture(&mut feedback, n, (n - 299) * 10);
        }
        let queued = feedback.delay();
        assert!((0.2..0.3).contains(&queued), "{queued}");
        let report = feedback.report(1, ms(12_000)).unwrap();
        assert_eq!(u16::from_be_bytes([report[26], report[27]]), (queued * 8192.0) as u16);

        // It drains, and the delay falls with it.
        for n in 330..400 {
            picture(&mut feedback, n, 0);
        }
        assert!(feedback.delay() < 0.001, "{}", feedback.delay());

        // A new stream is a new SSRC: its count and delay start again.
        feedback.received(10, 5, ms(20_000));
        let report = feedback.report(1, ms(20_000)).unwrap();
        assert_eq!(&report[26..30], &[0, 0, 0, 1]);
    }

    #[test]
    fn srtp_refuses_a_duplicate_and_a_straggler() {
        let packet = unhex("80e412340a0b0c0dcafebabe610005049285d9955b8928769900c5323d679c04bbccbf");
        let mut srtp = SrtpReceiver::new(&master());
        assert!(srtp.unprotect(&mut packet.clone()).is_ok());
        assert_eq!(srtp.unprotect(&mut packet.clone()), Err(SrtpError::Stale), "a duplicate");
        srtp.last = vec![(0xcafe_babe, 0x1235, 0)];
        assert_eq!(srtp.unprotect(&mut packet.clone()), Err(SrtpError::Stale), "overtaken");
        srtp.last = vec![(0x0102_0304, 0x1235, 0)];
        assert!(srtp.unprotect(&mut packet.clone()).is_ok(), "a new stream starts over");
    }

    #[test]
    fn the_rollover_counter_follows_a_wrap_and_a_straggler() {
        let mut srtp = SrtpReceiver::new(&master());
        assert_eq!(srtp.guess_roc(1, 5), 0);
        srtp.last = vec![(1, 0xfff0, 0)];
        assert_eq!(srtp.guess_roc(1, 0x0002), 1, "past the wrap");
        assert_eq!(srtp.guess_roc(1, 0xffff), 0);
        srtp.last = vec![(1, 0x0002, 1)];
        assert_eq!(srtp.guess_roc(1, 0xfff5), 0, "a late packet from before the wrap");
        assert_eq!(srtp.guess_roc(1, 0x0010), 1);
        assert_eq!(srtp.guess_roc(2, 0x0010), 0, "a new stream starts over");
    }

    #[test]
    fn rtcp_and_rtp_are_told_apart() {
        let mut rtcp = unhex("80c90001010203040000000000000000000000");
        assert_eq!(SrtpReceiver::new(&master()).unprotect(&mut rtcp), Err(SrtpError::Rtcp));
        assert_eq!(SrtpReceiver::new(&master()).unprotect(&mut [0x40; 30]), Err(SrtpError::NotRtp));
        assert_eq!(rtcp_receiver_report(0x0102_0304), [0x80, 201, 0, 1, 1, 2, 3, 4]);
    }

    fn header(sequence: u16, timestamp: u32, marker: bool) -> RtpHeader {
        RtpHeader { payload_type: 100, marker, sequence, timestamp, ssrc: 9, refresh: false, payload: (0, 0) }
    }

    const VPS: &[u8] = &[0x40, 0x01, 0xaa];
    const SPS: &[u8] = &[0x42, 0x01, 0xbb];
    const IDR: &[u8] = &[0x28, 0x01, 1, 2, 3, 4, 5, 6];
    const TRAIL: &[u8] = &[0x02, 0x01, 7, 8];

    fn aggregation(nals: &[&[u8]]) -> Vec<u8> {
        let mut ap = vec![NAL_AP << 1, 0x01];
        for nal in nals {
            ap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            ap.extend_from_slice(nal);
        }
        ap
    }

    fn fragments(nal: &[u8], pieces: usize) -> Vec<Vec<u8>> {
        let body = &nal[2..];
        let size = body.len().div_ceil(pieces);
        let chunks: Vec<&[u8]> = body.chunks(size).collect();
        let last = chunks.len() - 1;
        chunks
            .iter()
            .enumerate()
            .map(|(i, chunk)| {
                let mut fu = vec![(NAL_FU << 1) | (nal[0] & 0x81), nal[1]];
                fu.push(u8::from(i == 0) << 7 | u8::from(i == last) << 6 | nal_type(nal[0]));
                fu.extend_from_slice(chunk);
                fu
            })
            .collect()
    }

    /// A display out of view has its packets passed over, for as long as it is,
    /// and is asked for an IDR as it comes back: on a still screen the next
    /// packet is that IDR's first, and the whole of it has to be taken.
    #[test]
    fn a_stream_passed_over_takes_the_idr_that_starts_at_the_next_packet() {
        let mut d = Depacketizer::default();
        assert_eq!(d.push(&header(10, 0, false), &aggregation(&[VPS, SPS])), Depacketized::Pending);
        assert!(matches!(d.push(&header(11, 0, true), IDR), Depacketized::Unit(_)));
        // 40,000 packets out of view, which a sequence number left where it was
        // would read as behind.
        let on = 11u16.wrapping_add(40_000);
        for passed in 1..=40_000u16 {
            d.skip(&header(11u16.wrapping_add(passed), 400, passed == 40_000));
        }
        assert_eq!(d.push(&header(on + 1, 800, false), &aggregation(&[VPS, SPS])), Depacketized::Pending);
        assert_eq!(
            d.push(&header(on + 2, 800, true), IDR),
            Depacketized::Unit(vec![VPS.to_vec(), SPS.to_vec(), IDR.to_vec()])
        );
    }

    /// Back in view in the middle of a picture: its tail is no start, though an
    /// IDR's tail holds slices a decoder could be handed.
    #[test]
    fn a_stream_passed_over_mid_picture_starts_at_the_next_whole_one() {
        let mut d = Depacketizer::default();
        d.skip(&header(10, 0, false));
        assert_eq!(d.push(&header(11, 0, true), IDR), Depacketized::Lost);
        assert_eq!(d.push(&header(12, 400, true), TRAIL), Depacketized::Lost);
        assert!(!d.resumes_at_refresh(), "its pictures were given up");
        // The picture passed over never ended with a marked packet: the next
        // timestamp is a new picture all the same.
        d.skip(&header(13, 800, false));
        assert_eq!(d.push(&header(14, 1200, false), &aggregation(&[VPS, SPS])), Depacketized::Pending);
        assert!(matches!(d.push(&header(15, 1200, true), IDR), Depacketized::Unit(_)));
    }

    /// Apple's first picture: the parameter sets aggregated, the IDR fragmented;
    /// then a picture in one packet.
    #[test]
    fn access_units_come_out_of_aggregates_fragments_and_single_units() {
        let mut d = Depacketizer::default();
        assert_eq!(d.push(&header(10, 0, false), &aggregation(&[VPS, SPS])), Depacketized::Pending);
        let pieces = fragments(IDR, 3);
        assert_eq!(d.push(&header(11, 0, false), &pieces[0]), Depacketized::Pending);
        assert_eq!(d.push(&header(12, 0, false), &pieces[1]), Depacketized::Pending);
        assert_eq!(
            d.push(&header(13, 0, true), &pieces[2]),
            Depacketized::Unit(vec![VPS.to_vec(), SPS.to_vec(), IDR.to_vec()])
        );
        assert_eq!(d.push(&header(14, 400, true), TRAIL), Depacketized::Unit(vec![TRAIL.to_vec()]));
    }

    /// Nothing is decodable until a random-access picture, and a gap drops
    /// everything up to the next one — asking for it on the way.
    #[test]
    fn a_lost_packet_drops_pictures_until_the_next_idr() {
        let mut d = Depacketizer::default();
        assert_eq!(d.push(&header(1, 0, true), TRAIL), Depacketized::Lost, "joined mid-stream");
        assert_eq!(d.push(&header(2, 400, true), IDR), Depacketized::Unit(vec![IDR.to_vec()]));
        assert_eq!(d.push(&header(3, 800, true), TRAIL), Depacketized::Unit(vec![TRAIL.to_vec()]));
        assert_eq!(d.push(&header(5, 1200, true), TRAIL), Depacketized::Lost, "4 never came");
        assert_eq!(d.push(&header(6, 1600, true), TRAIL), Depacketized::Lost);
        assert_eq!(d.push(&header(6, 1600, true), TRAIL), Depacketized::Pending, "a duplicate");
        assert_eq!(d.push(&header(7, 2000, true), IDR), Depacketized::Unit(vec![IDR.to_vec()]));

        // A fragment lost in the middle of a picture takes the picture with it.
        let pieces = fragments(IDR, 3);
        assert_eq!(d.push(&header(8, 2400, false), &pieces[0]), Depacketized::Pending);
        assert_eq!(d.push(&header(10, 2400, true), &pieces[2]), Depacketized::Lost);
    }

    #[test]
    fn a_new_stream_starts_its_own_picture_count() {
        let mut d = Depacketizer::default();
        assert_eq!(d.push(&header(100, 0, true), IDR), Depacketized::Unit(vec![IDR.to_vec()]));
        let next = RtpHeader { ssrc: 10, ..header(5000, 0, true) };
        assert_eq!(d.push(&next, IDR), Depacketized::Unit(vec![IDR.to_vec()]), "not a gap");
    }

    /// A payload with the decoding order number a stream in strips carries: after
    /// the payload header, and in a fragment after its fragment header.
    fn numbered(don: u16, payload: &[u8]) -> Vec<u8> {
        let at = if nal_type(payload[0]) == NAL_FU { 3 } else { 2 };
        [&payload[..at], &don.to_be_bytes()[..], &payload[at..]].concat()
    }

    fn strip(strip: u32, sequence: u16, timestamp: u32, marker: bool) -> RtpHeader {
        RtpHeader { ssrc: 9 + strip, ..header(sequence, timestamp, marker) }
    }

    /// A keyframe as the Mac sends one in strips, the first strip's parameter
    /// sets aggregated under the one number and its IDR in fragments that each
    /// carry it; then the strips that changed, each under its own SSRC and
    /// sequence numbers, in the one decoding order.
    #[test]
    fn strips_come_out_in_their_one_decoding_order() {
        let mut s = Strips::default();
        assert_eq!(s.push(&strip(0, 10, 0, false), &numbered(7, &aggregation(&[VPS, SPS]))), (0, Depacketized::Pending));
        let pieces = fragments(IDR, 3);
        assert_eq!(s.push(&strip(0, 11, 0, false), &numbered(7, &pieces[0])), (0, Depacketized::Pending));
        assert_eq!(s.push(&strip(0, 12, 0, false), &numbered(7, &pieces[1])), (0, Depacketized::Pending));
        assert_eq!(
            s.push(&strip(0, 13, 0, true), &numbered(7, &pieces[2])),
            (0, Depacketized::Unit(vec![VPS.to_vec(), SPS.to_vec(), IDR.to_vec()]))
        );
        let trail = Depacketized::Unit(vec![TRAIL.to_vec()]);
        assert_eq!(s.push(&strip(1, 500, 0, true), &numbered(8, TRAIL)), (1, trail));
        let trail = || Depacketized::Unit(vec![TRAIL.to_vec()]);
        // A frame carries the strips that changed.
        assert_eq!(s.push(&strip(3, 900, 0, true), &numbered(9, TRAIL)), (3, trail()));
        assert_eq!(s.push(&strip(0, 14, 400, true), &numbered(10, TRAIL)), (0, trail()));
        assert_eq!(s.stream(12), 9, "the display's SSRC is its first strip's");
    }

    /// A picture missing from the decoding order is one every strip may predict
    /// from, as is a packet lost to one strip: nothing is decodable until the
    /// first strip's next IDR.
    #[test]
    fn a_picture_missing_from_the_order_drops_strips_until_the_first_strips_idr() {
        let mut s = Strips::default();
        let trail = || Depacketized::Unit(vec![TRAIL.to_vec()]);
        let idr = || Depacketized::Unit(vec![IDR.to_vec()]);
        // Joined mid-stream, at a strip taken for the first until an earlier one comes.
        assert_eq!(s.push(&strip(1, 1, 0, true), &numbered(3, TRAIL)), (0, Depacketized::Lost));
        assert_eq!(s.push(&strip(0, 1, 400, true), &numbered(4, IDR)), (0, idr()));
        assert_eq!(s.push(&strip(1, 2, 400, true), &numbered(5, TRAIL)), (1, trail()));
        assert_eq!(s.push(&strip(2, 1, 800, true), &numbered(7, TRAIL)), (2, Depacketized::Lost), "6 never came");
        assert_eq!(s.push(&strip(0, 2, 1200, true), &numbered(8, TRAIL)), (0, Depacketized::Lost));
        assert_eq!(s.push(&strip(2, 2, 1200, true), &numbered(9, IDR)), (2, Depacketized::Lost), "not the first strip's");
        assert_eq!(s.push(&strip(0, 3, 1600, true), &numbered(10, IDR)), (0, idr()));
        assert_eq!(s.push(&strip(3, 1, 1600, true), &numbered(11, TRAIL)), (3, trail()));

        // A strip's own sequence numbers show a packet lost before the order does.
        let pieces = fragments(TRAIL, 2);
        assert_eq!(s.push(&strip(3, 3, 2000, true), &numbered(12, &pieces[1])), (3, Depacketized::Lost));
        assert_eq!(s.push(&strip(0, 4, 2400, true), &numbered(13, TRAIL)), (0, Depacketized::Lost));
        assert_eq!(s.push(&strip(0, 5, 2800, true), &numbered(14, IDR)), (0, idr()));

        // A new stream, after an offer, is another display's worth of SSRCs.
        let next = RtpHeader { ssrc: 5000, ..header(77, 0, true) };
        assert_eq!(s.push(&next, &numbered(0, IDR)), (0, idr()));
        assert_eq!(s.stream(5003), 5000);
    }

    /// The strip heights the Mac sent, and the display heights it sent no picture
    /// for when offered four tiles.
    #[test]
    fn a_display_is_offered_in_strips_unless_its_last_would_start_past_its_end() {
        for (height, rows) in [(64, 16), (96, 32), (120, 32), (144, 48), (600, 160), (800, 208), (900, 240), (1000, 256), (1080, 272)] {
            assert_eq!(strip_rows(height), rows, "{height} rows");
            assert!(in_strips(height), "{height} rows");
        }
        for height in [80, 90, 136] {
            assert!(!in_strips(height), "{height} rows");
        }
        let tiles = |offers: &Offers, size| {
            let blob = video_offer_blob(7, size, if offers.strips(size) { STRIPS } else { 1 } as u64);
            let stream = fields(&blob).iter().find_map(|(f, v)| (*f == 5).then(|| v.clone().unwrap_err())).unwrap();
            fields(&stream).iter().find_map(|(f, v)| (*f == 6).then(|| *v.as_ref().unwrap())).unwrap()
        };
        assert_eq!(tiles(&Offers::new(1, true), (1600, 1000)), 4);
        assert_eq!(tiles(&Offers::new(1, true), (160, 90)), 1);
        assert_eq!(tiles(&Offers::new(1, false), (1600, 1000)), 1, "a passed stream");
    }

    /// Each strip's SSRC has sequence numbers of its own, so its own rollover.
    #[test]
    fn the_rollover_counter_is_each_strips_own() {
        let mut srtp = SrtpReceiver::new(&master());
        srtp.last = vec![(1, 0xfff0, 0), (2, 0x0005, 3)];
        assert_eq!(srtp.guess_roc(1, 0x0002), 1);
        assert_eq!(srtp.guess_roc(2, 0x0006), 3);
        assert_eq!(srtp.guess_roc(3, 0x0006), 0, "a strip not heard from yet");
    }

    /// Strips are placed a strip's height apart, the last one's rows past the
    /// display's left out, and the display is shown once it has all four.
    #[test]
    fn a_display_is_put_together_from_its_strips() {
        let part = |value: u8| Picture { size: (2, 3), rgb: vec![value; 2 * 3 * 3] };
        let mut canvas = None;
        for strip in [0, 1, 3] {
            Canvas::place(&mut canvas, Some((2, 10)), strip, &part(strip as u8 + 1)).unwrap();
        }
        assert!(canvas.as_mut().unwrap().show().is_none(), "a strip is still to come");
        Canvas::place(&mut canvas, Some((2, 10)), 2, &part(3)).unwrap();
        let shown = canvas.as_mut().unwrap().show().unwrap();
        assert_eq!(shown.size, (2, 10));
        let rows: Vec<u8> = shown.rgb.chunks(6).map(|row| row[0]).collect();
        assert_eq!(rows, [1, 1, 1, 2, 2, 2, 3, 3, 3, 4]);
        assert!(shown.rgb.chunks(6).all(|row| row.iter().all(|&v| v == row[0])));

        // One strip of the next frame, and the display is whole with it.
        Canvas::place(&mut canvas, Some((2, 10)), 1, &part(9)).unwrap();
        assert_eq!(canvas.as_ref().unwrap().fresh, 0b0010);
        assert_eq!(canvas.as_mut().unwrap().show().unwrap().rgb[3 * 6], 9);

        // A last strip that starts at the display's end has nothing to place.
        Canvas::place(&mut canvas, Some((2, 9)), 3, &part(7)).unwrap();
        assert!(canvas.as_ref().unwrap().picture.rgb.iter().all(|&v| v == 0));
        assert!(Canvas::place(&mut canvas, Some((2, 13)), 0, &part(1)).is_err(), "not a quarter of 13 rows");
        assert!(Canvas::place(&mut canvas, Some((2, 8)), 0, &part(1)).is_err(), "the last strip past 8 rows");
        assert!(Canvas::place(&mut canvas, Some((4, 10)), 0, &part(1)).is_err());
        assert!(Canvas::place(&mut canvas, None, 0, &part(1)).is_err());
    }

    /// A 64×48 4:4:4 stream from x265, full-range BT.709 as the Mac's is, and six
    /// pixels of its last picture as ffmpeg converts them. The two conversions
    /// round differently, by a step at most.
    #[test]
    fn a_444_stream_decodes_to_the_colours_ffmpeg_converts_it_to() {
        let (pictures, decoded_by) = decode_fixture(false);
        assert_eq!(decoded_by, Some(DecodedBy::Software));
        assert_the_fixture_colours(&pictures);
    }

    /// The same stream through VideoToolbox: the same pictures, which on a Mac that
    /// is not virtual must be VideoToolbox's — FFmpeg's fallback to decoding them
    /// itself would pass the colours too. That is VideoToolbox, not necessarily its
    /// hardware decoder, which is not asked. A virtual Mac may have no VideoToolbox
    /// decoder to reach, and elsewhere there is no VideoToolbox, so both decode in
    /// software.
    #[test]
    fn the_same_stream_decodes_to_the_same_colours_through_videotoolbox() {
        let (pictures, decoded_by) = decode_fixture(true);
        if cfg!(target_os = "macos") && !virtual_mac() {
            assert_eq!(decoded_by, Some(DecodedBy::VideoToolbox));
        }
        assert_the_fixture_colours(&pictures);
    }

    /// The fixture's pictures, and what decoded the last.
    fn decode_fixture(hardware: bool) -> (Vec<Picture>, Option<DecodedBy>) {
        let units = fixture_units();
        assert_eq!(units.len(), 3);
        let mut hevc = Hevc::new(hardware).unwrap();
        let pictures: Vec<Picture> = units.iter().filter_map(|unit| hevc.decode(unit).unwrap()).collect();
        assert_eq!(pictures.len(), 3, "each unit yields its picture at once");
        (pictures, hevc.decoded_by)
    }

    /// Whether this Mac is a virtual machine, from the kernel's `kern.hv_vmm_present`.
    fn virtual_mac() -> bool {
        let out = std::process::Command::new("sysctl").args(["-n", "kern.hv_vmm_present"]).output().unwrap();
        assert!(out.status.success(), "sysctl -n kern.hv_vmm_present failed");
        String::from_utf8_lossy(&out.stdout).trim() == "1"
    }

    fn assert_the_fixture_colours(pictures: &[Picture]) {
        let last = pictures.last().unwrap();
        assert_eq!(last.size, (64, 48));
        for ((x, y), want) in [
            ((2, 2), [76, 0, 38]),
            ((20, 10), [0, 60, 8]),
            ((40, 10), [176, 141, 109]),
            ((60, 30), [2, 62, 62]),
            ((10, 40), [255, 4, 0]),
            ((33, 24), [0, 0, 62]),
        ] {
            let at = (y * 64 + x) * 3;
            let got = &last.rgb[at..at + 3];
            assert!(
                got.iter().zip(want).all(|(&g, w): (&u8, u8)| g.abs_diff(w) <= 2),
                "pixel ({x}, {y}) is {got:?}, ffmpeg says {want:?}"
            );
        }
    }

    /// The 64×48 4:4:4 fixture's access units, one per picture.
    fn fixture_units() -> Vec<AccessUnit> {
        let stream = include_bytes!("../tests/fixtures/hevc-444-64x48.h265");
        let mut nals: Vec<Vec<u8>> = Vec::new();
        let mut rest = &stream[..];
        while let Some(at) = rest.windows(3).position(|w| w == [0, 0, 1]) {
            rest = &rest[at + 3..];
            let end = rest.windows(3).position(|w| w == [0, 0, 1]).unwrap_or(rest.len());
            let mut nal = rest[..end].to_vec();
            while nal.last() == Some(&0) {
                nal.pop();
            }
            nals.push(nal);
        }
        // One access unit per picture: a VCL unit closes one.
        let mut units: Vec<AccessUnit> = vec![Vec::new()];
        for nal in nals {
            let vcl = nal_type(nal[0]) < 32;
            units.last_mut().unwrap().push(nal);
            if vcl {
                units.push(Vec::new());
            }
        }
        units.retain(|unit| !unit.is_empty());
        units
    }

    /// The sequence parameter set macwork's stream opened with: 1600×1000, 4:4:4.
    const MACWORK_SPS: &str = "420101040800000300be080000030000969000641007e3e2710087722904173f89f85fd0d\
        fa87fd5047eaa09fd55057eaaa0bfd555067eaaaa0dfd5555077eaaaaa0ffd55555021faaaaaa94dc30340402";

    /// The configuration string is read from the stream's own parameter sets, as a
    /// passed stream's announcement needs: the string WebCodecs decoded macwork's
    /// capture with in Chrome and Safari, and the picture's size.
    #[test]
    fn a_sequence_parameter_set_names_its_configuration_and_size() {
        let sps: Vec<u8> = (0..MACWORK_SPS.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&MACWORK_SPS[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(
            parse_sps(&sps),
            Some(StreamParams { size: (1600, 1000), decode: "hev1.4.10.L150.BE.8".to_owned() })
        );
        assert_eq!(parse_sps(&sps[..12]), None, "a truncated one says nothing");
    }

    /// Each unit goes to the browser as Annex B behind the configuration its
    /// parameter sets give, the first one a keyframe that carries them.
    #[test]
    fn access_units_pass_as_annex_b_described_by_their_parameter_sets() {
        let units = fixture_units();
        let mut passer = Passer::default();
        let passed: Vec<PassedUnit> = units.iter().map(|unit| passer.pass(unit).unwrap()).collect();
        assert_eq!(passed.len(), 3);
        assert!(passed[0].keyframe, "the stream opens on an IDR");
        assert!(!passed[1].keyframe && !passed[2].keyframe);
        for (unit, passed) in units.iter().zip(&passed) {
            assert_eq!(passed.size, (64, 48));
            assert_eq!(passed.decode, "hev1.4.10.L30.9E.8");
            let annex_b: Vec<u8> = unit.iter().flat_map(|nal| [&[0, 0, 0, 1][..], nal].concat()).collect();
            assert_eq!(passed.data, annex_b);
        }
        // A unit before any parameter set has nothing to be configured by.
        assert_eq!(Passer::default().pass(&units[1]), None);
    }

    fn media() -> MediaStream {
        let peer = "[fd00::2]:5900".parse().unwrap();
        let local = "[fd00::1]:50000".parse().unwrap();
        MediaStream::new(peer, local, false, 1).0
    }

    /// Offer the stream for `size` as the session does once the Mac has named its
    /// ports. The naming is noted as [`MediaStream::on_reply`] notes it: binding
    /// the ports takes an address of this host's, which the tests' is not.
    fn offer_on_ports(m: &mut MediaStream, size: (u16, u16)) -> Vec<u8> {
        m.asked = true;
        m.invited = true;
        match m.offer(&[size]) {
            Some(Offer::Configuration(msg)) => msg,
            other => panic!("no offer for {size:?}: {other:?}"),
        }
    }

    const ANSWER: &str = "000200020000000000000000000000000000";
    const PORTS: &str = "0001000100000000170c00000001170d0000000100000000000000000000000000000000";

    /// The encodings first, then an offer only on the ports the Mac names for
    /// them or for a display change, one per naming, one out at a time, one per
    /// display.
    #[test]
    fn an_offer_waits_for_the_ports_and_goes_out_once_per_display_and_one_at_a_time() {
        let mut m = media();
        assert_eq!(m.offer(&[(1600, 1000)]), Some(Offer::Encodings), "the encodings ask for the ports");
        assert!(m.offer(&[(1600, 1000)]).is_none(), "no offer before the Mac names its ports");
        m.invited = true;
        let Some(Offer::Configuration(msg)) = m.offer(&[(1600, 1000)]) else {
            panic!("the named ports allow an offer")
        };
        assert_eq!(msg[0], 0x1c);
        assert!(m.pending());
        assert!(m.offer(&[(1280, 800)]).is_none(), "not while one is out");
        assert!(!m.on_reply(&unhex(ANSWER)).unwrap());
        assert!(!m.pending());
        assert!(m.offer(&[(1600, 1000)]).is_none(), "the stream runs at this size");
        m.stopped();
        assert!(m.offer(&[(1600, 1000)]).is_none(), "a display change waits for the ports named after it");
        m.invited = true;
        assert!(matches!(m.offer(&[(1600, 1000)]), Some(Offer::Configuration(_))));
    }

    /// The ports are owed from the encodings that ask for them. Named, they allow
    /// an offer; named again, which the Mac does after a display change of its
    /// own, they take the stream offered for the old display down until the next.
    #[test]
    fn named_ports_allow_an_offer_and_named_again_take_the_stream_down() {
        let mut m = media();
        m.offer(&[(1600, 1000)]).unwrap();
        let due = m.deadline().expect("the ports are owed once asked for");
        let unnamed = m.overdue(due).expect("overdue at the deadline");
        assert!(unnamed.to_string().contains("named no ports"), "{unnamed}");

        // Binding them is what fails here, once the naming is noted.
        let bound = m.on_reply(&unhex(PORTS)).unwrap_err();
        assert!(format!("{bound:#}").contains("for the Mac's media stream"), "{bound:#}");
        assert_eq!(m.deadline(), None, "named");
        assert!(matches!(m.offer(&[(1600, 1000)]), Some(Offer::Configuration(_))));
        assert!(!m.on_reply(&unhex(ANSWER)).unwrap());

        m.on_reply(&unhex(PORTS)).unwrap_err();
        assert_eq!(m.offered, None, "the stream is down");
        assert!(matches!(m.offer(&[(1600, 1000)]), Some(Offer::Configuration(_))), "and offered again");
    }

    /// A refusal is an error, which ends the session as it ends Apple's viewer's.
    #[test]
    fn a_refusal_ends_the_session() {
        let mut m = media();
        offer_on_ports(&mut m, (1600, 1000));
        let refused = m.on_reply(&unhex("00030001000000000000000200000000")).unwrap_err();
        assert!(
            format!("{refused:#}").contains("refused the media stream (error type 2, sub-code 0)"),
            "{refused:#}"
        );
    }

    /// An offer owes its display's first picture and the first sound within
    /// [`STREAM_START`], and the running stream a packet on each leg every
    /// [`STREAM_SILENCE`]; past any of them the session ends. A picture of another
    /// display, the old one's last, pays nothing, nor does sound from before the
    /// offer, nor a report before the first picture or the first sound, and a
    /// display change owes nothing until its own offer.
    #[test]
    fn a_stream_without_pictures_or_sound_is_overdue_but_not_across_a_display_change() {
        let mut m = media();
        assert_eq!(m.deadline(), None, "nothing is owed before an offer");
        *m.sounded.lock().unwrap() = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        let offered = std::time::Instant::now();
        offer_on_ports(&mut m, (1600, 1000));
        let first = m.deadline().expect("an offer owes a picture and sound");
        assert!(first >= offered + STREAM_START);
        assert!(m.overdue(first - std::time::Duration::from_millis(1)).is_none());
        let unanswered = m.overdue(first).expect("overdue at the deadline");
        assert!(unanswered.to_string().contains("did not answer"), "{unanswered}");

        assert!(!m.on_reply(&unhex(ANSWER)).unwrap());
        let portless = m.overdue(first).expect("still overdue once answered");
        assert!(portless.to_string().contains("named no ports"), "{portless}");

        m.pictured(0, (1280, 800));
        assert_eq!(m.deadline(), Some(first), "another display's picture settles nothing");
        *m.legs[0].heard.lock().unwrap() = Some(std::time::Instant::now());
        assert_eq!(m.deadline(), Some(first), "nor does a report before the first picture");

        m.pictured(0, (1600, 1000));
        let Owed::Stream { pictured: [Some(pictured), _], .. } = m.owed else {
            panic!("the picture was not noted: {:?}", m.owed)
        };
        assert_eq!(m.deadline(), Some(first), "the first sound is still owed, none since the offer");
        assert!(m.overdue(first).is_some(), "and overdue without it");

        *m.sound_heard.lock().unwrap() = Some(pictured);
        assert_eq!(m.deadline(), Some(first), "a report is not the first sound");
        assert!(m.overdue(first).is_some(), "and overdue without it");
        *m.sounded.lock().unwrap() = Some(pictured);
        *m.legs[0].heard.lock().unwrap() = Some(pictured);
        let next = m.deadline().expect("a running stream owes a packet on each leg");
        assert_eq!(next, pictured + STREAM_SILENCE);
        assert!(m.overdue(first).is_none(), "the first picture and sound settled the offer");
        let silent = m.overdue(next).expect("overdue after the silence");
        assert!(silent.to_string().contains("sent nothing on the picture's leg for 48s"), "{silent}");

        // A still screen sends no picture, and the Mac's report on the picture's
        // leg is what keeps it alive.
        let reported = pictured + std::time::Duration::from_secs(1);
        *m.legs[0].heard.lock().unwrap() = Some(reported);
        assert_eq!(m.deadline(), Some(next), "the sound is due first now");
        let quiet = m.overdue(next).expect("a leg without sound is overdue too");
        assert!(quiet.to_string().contains("sent nothing on the sound's leg for 48s"), "{quiet}");
        *m.sound_heard.lock().unwrap() = Some(reported);
        assert_eq!(m.deadline(), Some(reported + STREAM_SILENCE), "a report on each leg puts both off");
        assert!(m.overdue(next).is_none());

        m.stopped();
        assert_eq!(m.deadline(), None, "a display change owes nothing");
        assert!(m.overdue(next + STREAM_SILENCE).is_none());
        offer_on_ports(&mut m, (1280, 800));
        assert!(m.deadline().is_some(), "until its own offer");
    }

    /// A session with two virtual displays offers a video leg for each in the one
    /// message, and takes the Mac's ports and answer only when they name both: a
    /// stream of fewer legs than displays, or more, is one this side cannot show.
    #[test]
    fn two_displays_are_offered_named_and_answered_together() {
        let peer = "[fd00::2]:5900".parse().unwrap();
        let local = "[fd00::1]:50000".parse().unwrap();
        let (mut m, pictures) = MediaStream::new(peer, local, true, 2);
        assert_eq!((m.displays(), pictures.len()), (2, 2));

        let sizes = [(1600, 1000), (1280, 800)];
        assert_eq!(m.offer(&sizes[..1]), None, "a layout of one display is not the two asked for");
        assert_eq!(m.offer(&sizes), Some(Offer::Encodings));
        m.invited = true;
        let Some(Offer::Configuration(msg)) = m.offer(&sizes) else {
            panic!("the named ports allow an offer")
        };
        let length = |at: usize| usize::from(u16::from_be_bytes([msg[at], msg[at + 1]]));
        assert_eq!(&msg[6..10], &[0, 0, 0, 7], "60 fps on both streams, and the pointer left out");
        assert!(length(0x0c) > 0 && length(0x0e) > 0, "an offer for each display");
        assert_eq!(msg.len() - 4, 0xd8 + length(0x0a) + length(0x0c) + 0x5c + length(0x0e));
        assert!(m.offer(&sizes).is_none(), "one offer at a time");

        let one = m.on_reply(&unhex(ANSWER)).unwrap_err();
        assert!(format!("{one:#}").contains("answered 1 video leg(s) of the 2"), "{one:#}");
        assert!(!m.on_reply(&unhex("000200020000000000020003000100000000aabbccddeeff")).unwrap());
        assert!(!m.pending());

        // Message 1 without the second leg, to this session, and with it, to a
        // session of one display.
        let short = m.on_reply(&unhex(PORTS)).unwrap_err();
        assert!(format!("{short:#}").contains("enabled 1 video leg(s) for the 2"), "{short:#}");
        let mut both = unhex(PORTS);
        both[20..22].copy_from_slice(&5902u16.to_be_bytes());
        both[25] = 1;
        let extra = media().on_reply(&both).unwrap_err();
        assert!(format!("{extra:#}").contains("enabled 2 video leg(s) for the 1"), "{extra:#}");
    }

    /// With the second display arranged ahead of the first on the Mac, the legs
    /// carry them the other way round: a display is asked about, shown and
    /// credited on the leg that carries it.
    #[test]
    fn the_legs_follow_the_macs_arrangement() {
        let peer = "[fd00::2]:5900".parse().unwrap();
        let local = "[fd00::1]:50000".parse().unwrap();
        let relaxed = std::sync::atomic::Ordering::Relaxed;
        let (mut m, _pictures) = MediaStream::new(peer, local, false, 2);
        m.asked = true;
        m.invited = true;
        assert_eq!((m.leg_of(0), m.leg_of(1)), (0, 1));
        m.show(0, true);
        m.arrange(true);
        assert_eq!((m.leg_of(0), m.leg_of(1)), (1, 0));
        assert!(m.legs[1].shown.load(relaxed) && !m.legs[0].shown.load(relaxed), "shown goes with the display");
        m.show(1, true);
        assert!(m.legs[0].shown.load(relaxed));

        // The first display's picture comes on the second leg, and is what that
        // leg owed.
        let sizes = [(1920, 911), (1915, 910)];
        assert!(m.offer(&sizes).is_some());
        m.pictured(0, (1920, 911));
        let Owed::Stream { pictured, .. } = m.owed else { panic!("an offer owes its stream") };
        assert!(pictured[1].is_some() && pictured[0].is_none());
        m.pictured(1, (1920, 911));
        let Owed::Stream { pictured, .. } = m.owed else { panic!("an offer owes its stream") };
        assert!(pictured[0].is_none(), "the second display is the other size");

        m.arrange(false);
        assert_eq!((m.leg_of(0), m.leg_of(1)), (0, 1));
        // One display has nothing to swap.
        let mut one = media();
        one.arrange(true);
        assert_eq!(one.leg_of(0), 0);
    }

    /// Each display owes its first picture. One nobody is shown hands none on, and
    /// its first packet stands for it; one coming into view long after its offer
    /// is not overdue for the picture it is only now asked for.
    #[test]
    fn a_display_nobody_is_shown_owes_its_packets_and_one_coming_into_view_its_next() {
        let peer = "[fd00::2]:5900".parse().unwrap();
        let local = "[fd00::1]:50000".parse().unwrap();
        let (mut m, _pictures) = MediaStream::new(peer, local, false, 2);
        m.asked = true;
        m.invited = true;
        let offered = std::time::Instant::now();
        assert!(m.offer(&[(1600, 1000), (1280, 800)]).is_some());
        m.on_reply(&unhex("000200020000000000020003000100000000aabbccddeeff")).unwrap();
        let first = m.deadline().expect("the offer owes both pictures and sound");

        // The first display's picture and the sound arrive; the second, in no
        // tab, has sent nothing.
        m.pictured(0, (1600, 1000));
        let now = Some(std::time::Instant::now());
        *m.sounded.lock().unwrap() = now;
        *m.sound_heard.lock().unwrap() = now;
        assert_eq!(m.deadline(), Some(first), "the second display's picture is still owed");
        let missing = m.overdue(first).expect("overdue without it");
        assert!(missing.to_string().contains("named no ports"), "{missing}");

        // Its packets are what it owes while nobody is shown it.
        *m.legs[1].pictured.lock().unwrap() = now;
        *m.legs[1].heard.lock().unwrap() = now;
        assert!(m.overdue(first).is_none());
        assert!(m.deadline().expect("a running stream") >= offered + STREAM_SILENCE);

        // Shown in a tab an age after the offer, it has no picture on record, and
        // is given from here to send one.
        assert!(m.show(1, true), "it came into view");
        assert!(!m.show(1, true), "and is in view");
        assert!(m.overdue(first + STREAM_START).is_none());
        let Owed::Stream { pictured: [Some(_), Some(shown)], .. } = m.owed else {
            panic!("the display coming into view was not noted: {:?}", m.owed)
        };
        *m.legs[1].heard.lock().unwrap() = Some(shown);
        *m.legs[0].heard.lock().unwrap() = Some(shown + STREAM_SILENCE);
        *m.sound_heard.lock().unwrap() = Some(shown + STREAM_SILENCE);
        let silent = m.overdue(shown + STREAM_SILENCE).expect("overdue after the silence");
        assert!(silent.to_string().contains("the leg of display 2's picture for 48s"), "{silent}");
    }

    /// A display change while an offer is out still owes that offer's answer by
    /// [`STREAM_START`], and the offer goes on holding back the next display's
    /// until it comes.
    #[test]
    fn an_offer_out_across_a_display_change_still_owes_its_answer() {
        let mut m = media();
        offer_on_ports(&mut m, (1600, 1000));
        let first = m.deadline().unwrap();
        m.stopped();
        m.stopped();
        assert!(m.pending());
        m.invited = true;
        assert!(m.offer(&[(1280, 800)]).is_none(), "not while the first is out, ports named or not");
        assert_eq!(m.deadline(), Some(first), "the answer is still owed");
        assert!(m.overdue(first - std::time::Duration::from_millis(1)).is_none());
        let unanswered = m.overdue(first).expect("unanswered at the deadline");
        assert!(unanswered.to_string().contains("did not answer"), "{unanswered}");

        assert!(!m.on_reply(&unhex(ANSWER)).unwrap());
        assert_eq!(m.deadline(), None, "answered, for a display that has gone");
        assert!(m.offer(&[(1280, 800)]).is_some(), "the new display's offer goes out");
    }
}
