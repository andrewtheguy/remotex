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
//! A target takes this path with `subtype = "ard-high-performance"`, and only in a
//! build with the `apple-hp-media` feature: the wire half of this module — offers,
//! replies, SRTP, depacketizing — is always compiled and tested, and the feature
//! adds the two decoders and the receiver that feeds them. A build without it
//! refuses that subtype at config parse, so nothing here runs in it.
//!
//! The offer is two AVConference negotiation blobs, rebuilt field by field from the
//! ones Apple's client produced ([`audio_offer_blob`], [`video_offer_blob`]). The
//! Mac refuses a configuration without either, so the two legs go together: the
//! picture comes from one and the sound from the other, and while the sound leg
//! runs the Mac mutes its own output, as it does for Apple's viewer.
//!
//! A stream that fails ends the session, as it ends Apple's viewer's, which has no
//! way back to RFB pixels: one the Mac refuses, one that brings no picture or no
//! sound within [`STREAM_START`] of its offer, one that sends neither for
//! [`STREAM_SILENCE`], and one whose receiver fails ([`MediaStream::overdue`],
//! [`MediaStream::failure`]).
//! ZRLE rectangles carry the picture only until the first one and across display
//! changes, which stop the stream until the next offer.
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
//! - `tilesPerFrame` is 1, so each picture is one HEVC picture of the whole
//!   display; Apple's 4 splits it into strips coded as separate pictures.
//!
//! Its bitrate entries are Apple's, and so is what bounds them: the Mac's rate
//! controller walks the picture between 20 and 60 Mbit/s by the one-way delay this
//! side reports ([`RateFeedback`]), every 50 ms as Apple's viewer does.
//!
//! The stream itself is HEVC Range Extensions, 4:4:4, full-range BT.709, RTP payload
//! type 100 packed as RFC 7798 without DONL: single NAL units, aggregation packets
//! and fragmentation units. [`Depacketizer`] reassembles access units from it, and a
//! lost packet costs a PLI ([`rtcp_pli`]), which the Mac answers with an IDR within
//! tens of milliseconds.

use std::io::Write as _;

#[cfg(feature = "apple-hp-media")]
use anyhow::Context as _;

use aes::Aes256;
use aes::cipher::{BlockCipherEncrypt as _, KeyInit as _};
use hmac::{Hmac, Mac as _};
use sha1::Sha1;

use crate::audio::PcmFormat;
use crate::vnc_apple;

/// Encoding 1010 (`0x3f2`), `kSSVideoEncoding_AVCMediaStream`: the viewer takes its
/// picture from the media stream, and the Mac's media-stream replies arrive as
/// rectangles of it.
pub const ENCODING_MEDIA_STREAM: i32 = 1010;

/// What the sound leg decodes to: AAC-ELD's 48 kHz stereo as 16-bit PCM. The
/// counterpart of [`crate::audio::PCM_CD_QUALITY`] for this source, and the format
/// the session builds its encoder for before the stream has come up.
pub const AUDIO_FORMAT: PcmFormat = PcmFormat {
    channels: 2,
    sample_rate: 48_000,
    bits_per_sample: 16,
};

/// The second `SetEncodings`, sent once the first layout has arrived: the opening
/// list with the media stream appended.
pub fn encodings_with_media_stream() -> Vec<i32> {
    let mut encodings = vnc_apple::ENCODINGS.to_vec();
    encodings.push(ENCODING_MEDIA_STREAM);
    encodings
}

/// `0x1c` flag bit 0: the viewer takes 60 frames a second. `screensharingd` sets it
/// (with bit 1) for a viewer whose message predates version 2, so every viewer it
/// knows of has it.
pub const FLAG_60FPS: u32 = 0x1;
/// `0x1c` flag bit 2: capture without the pointer. The agent logs `send cursor with
/// video 0` for it; without it the pointer is drawn into every picture.
pub const FLAG_NO_CURSOR: u32 = 0x4;
/// The flags this viewer sends.
pub const FLAGS: u32 = FLAG_60FPS | FLAG_NO_CURSOR;

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

/// The screen-video stream's `tilesPerFrame`: one picture per frame.
const TILES_PER_FRAME: u64 = 1;

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

/// The audio offer's blob, before compression: `VCMediaNegotiationBlobV2` with one
/// audio stream whose SSRC is `ssrc`.
fn audio_offer_blob(ssrc: u32) -> Vec<u8> {
    let mut blob = Proto::default();
    blob.uint(1, 1);
    blob.uint(2, 1);
    let mut stream = Proto::default();
    stream.uint(1, u64::from(ssrc));
    stream.uint(2, 0);
    stream.uint(3, 0);
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
/// HEVC (`123`) and H.264 (`100`), `tiles` pictures to a frame. The stream's fields
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
/// +0x0e u16  video2 offer length = 0
/// +0x14 16B  session UUID
/// +0x24 46B  audio SRTP master key, viewer -> server
/// +0x52 46B  audio SRTP master key, server -> viewer
/// +0x80      audio offer, then 46B video1 key v->s, 46B video1 key s->v, video1 offer
/// ```
fn configuration_message(
    flags: u32,
    session_uuid: &[u8; 16],
    audio_offer: &[u8],
    audio_keys: &KeyPair,
    video_offer: &[u8],
    video_keys: &KeyPair,
) -> Vec<u8> {
    let mut msg = vec![0u8; 0x80];
    msg[0] = 0x1c;
    msg[4..6].copy_from_slice(&3u16.to_be_bytes());
    msg[6..10].copy_from_slice(&flags.to_be_bytes());
    msg[0x0a..0x0c].copy_from_slice(&(audio_offer.len() as u16).to_be_bytes());
    msg[0x0c..0x0e].copy_from_slice(&(video_offer.len() as u16).to_be_bytes());
    msg[0x14..0x24].copy_from_slice(session_uuid);
    msg[0x24..0x52].copy_from_slice(&audio_keys.0);
    msg[0x52..0x80].copy_from_slice(&audio_keys.1);
    msg.extend_from_slice(audio_offer);
    msg.extend_from_slice(&video_keys.0);
    msg.extend_from_slice(&video_keys.1);
    msg.extend_from_slice(video_offer);
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
    video_keys: KeyPair,
    /// This side's SSRCs, which its RTCP reports carry.
    audio_ssrc: u32,
    video_ssrc: u32,
}

impl Offers {
    fn new() -> Self {
        let key = || {
            let mut k = [0u8; 46];
            rand::fill(&mut k[..]);
            k
        };
        let audio_keys = (key(), key());
        let video_keys = (key(), key());
        let uuid = uuid::Uuid::new_v4();
        Self {
            session_uuid: *uuid.as_bytes(),
            call_id: uuid.hyphenated().to_string().to_ascii_uppercase(),
            audio_keys,
            video_keys,
            audio_ssrc: rand::random(),
            video_ssrc: rand::random(),
        }
    }

    /// The `0x1c` message for a display of `size` backing pixels.
    fn configuration(&self, size: (u16, u16)) -> Vec<u8> {
        let audio = offer(MODE_AUDIO, &audio_offer_blob(self.audio_ssrc), &self.call_id);
        let video = offer(
            MODE_VIDEO,
            &video_offer_blob(self.video_ssrc, size, TILES_PER_FRAME),
            &self.call_id,
        );
        configuration_message(
            FLAGS,
            &self.session_uuid,
            &audio,
            &self.audio_keys,
            &video,
            &self.video_keys,
        )
    }
}

/// What the Mac put in an encoding-1010 rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaReply {
    /// Message 1: the streams are being set up; audio comes from and RTCP goes to
    /// `audio_port`, the picture from `video_port`, on the Mac's address, and this
    /// side receives on the same port numbers. A display change re-sends it on its
    /// own, with no stream behind it until the next offer.
    Ports { audio_port: u16, video_port: u16 },
    /// Message 2: AVConference accepted the offer.
    Answer,
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
            anyhow::ensure!(
                video2_flags & 1 == 0,
                "media-stream message 1 enabled a second video leg this one-display client did not offer"
            );
            Ok(MediaReply::Ports {
                audio_port: u16::from_be_bytes([body[8], body[9]]),
                video_port: u16::from_be_bytes([body[14], body[15]]),
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
                video2 == 0,
                "the Mac answered with a second video leg this one-display client did not offer"
            );
            anyhow::ensure!(
                body.len() == 18 + audio + video,
                "media-stream answer is {} bytes, not the {} its offer lengths describe",
                body.len(),
                18 + audio + video
            );
            Ok(MediaReply::Answer)
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
    /// The payload, as a range of the datagram, without the authentication tag.
    pub payload: (usize, usize),
}

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
    if data[0] & 0x10 != 0 {
        if data.len() < at + 4 {
            return Err(SrtpError::NotRtp);
        }
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
        payload: (at, data.len() - AUTH_TAG_LEN),
    })
}

/// The receiving side of one SRTP stream: its session keys, and the rollover
/// counter that extends a 16-bit sequence number to a packet index.
pub struct SrtpReceiver {
    keys: SessionKeys,
    /// The highest sequence number authenticated so far, and its rollover counter.
    /// Reset when the SSRC changes: every offer starts a stream with a new one.
    last: Option<(u32, u16, u32)>,
}

impl SrtpReceiver {
    pub fn new(master: &MasterKey) -> Self {
        Self { keys: SessionKeys::derive(master, 0), last: None }
    }

    /// RFC 3711 Appendix A: the rollover counter `seq` most likely belongs to.
    fn guess_roc(&self, ssrc: u32, seq: u16) -> u32 {
        let Some((last_ssrc, last_seq, roc)) = self.last else {
            return 0;
        };
        if last_ssrc != ssrc {
            return 0;
        }
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
        let newer = match self.last {
            Some((ssrc, seq, last_roc)) if ssrc == header.ssrc => index > (u64::from(last_roc) << 16 | u64::from(seq)),
            _ => true,
        };
        if !newer {
            return Err(SrtpError::Stale);
        }
        let iv = self.keys.iv(header.ssrc, index);
        aes_ctr_xor(&self.keys.cipher, iv, &mut data[header.payload.0..end]);
        self.last = Some((header.ssrc, header.sequence, roc));
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
// HEVC over RTP (RFC 7798, without DONL)
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
    /// Packets were lost: everything up to the next random-access picture is
    /// dropped, and the sender should be asked for one.
    Lost,
}

/// Access units out of a run of RTP payloads.
///
/// A picture ends at the packet with the marker bit, or where the timestamp moves
/// on. A gap in the sequence numbers drops the picture it fell in and everything
/// after it until an IRAP picture arrives, because every other picture predicts
/// from one that was lost.
#[derive(Default)]
pub struct Depacketizer {
    ssrc: Option<u32>,
    next_sequence: Option<u16>,
    timestamp: Option<u32>,
    unit: AccessUnit,
    fragment: Option<Vec<u8>>,
    /// The current picture lost a packet.
    damaged: bool,
    /// Whether the stream is decodable from here: a random-access picture has
    /// arrived since the last loss.
    synced: bool,
}

impl Depacketizer {
    pub fn push(&mut self, header: &RtpHeader, payload: &[u8]) -> Depacketized {
        if self.ssrc != Some(header.ssrc) {
            // A new stream, after an offer: it starts with an IDR of its own.
            *self = Self { ssrc: Some(header.ssrc), ..Self::default() };
        }
        let mut lost = false;
        if let Some(expected) = self.next_sequence {
            let ahead = header.sequence.wrapping_sub(expected);
            if ahead >= 0x8000 {
                // A duplicate or a late packet whose picture has gone.
                return Depacketized::Pending;
            }
            if ahead != 0 {
                lost = true;
            }
        }
        self.next_sequence = Some(header.sequence.wrapping_add(1));
        let mut out = Depacketized::Pending;
        if self.timestamp.is_some_and(|ts| ts != header.timestamp) && !self.unit.is_empty() {
            // The previous picture's last packet (the marked one) never came.
            self.damaged = true;
            self.finish();
        }
        if lost {
            self.damaged = true;
            self.synced = false;
            out = Depacketized::Lost;
        }
        self.timestamp = Some(header.timestamp);
        self.parse(payload);
        if header.marker {
            if let Some(unit) = self.finish() {
                return Depacketized::Unit(unit);
            }
            if !self.synced {
                out = Depacketized::Lost;
            }
        }
        out
    }

    fn parse(&mut self, payload: &[u8]) {
        if payload.len() < 2 {
            self.damaged = true;
            return;
        }
        match nal_type(payload[0]) {
            NAL_AP => {
                let mut rest = &payload[2..];
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
                if payload.len() < 3 {
                    self.damaged = true;
                    return;
                }
                let fu = payload[2];
                let (start, end, kind) = (fu & 0x80 != 0, fu & 0x40 != 0, fu & 0x3f);
                if start {
                    let mut nal = vec![(payload[0] & 0x81) | (kind << 1), payload[1]];
                    nal.extend_from_slice(&payload[3..]);
                    self.fragment = Some(nal);
                } else if let Some(nal) = self.fragment.as_mut() {
                    nal.extend_from_slice(&payload[3..]);
                } else {
                    self.damaged = true;
                    return;
                }
                if end && let Some(nal) = self.fragment.take() {
                    self.unit.push(nal);
                }
            }
            _ => self.unit.push(payload.to_vec()),
        }
    }

    /// Drop everything up to the next random-access picture: a unit this side
    /// could not keep was lost to every picture that predicts from it.
    pub fn resync(&mut self) {
        self.synced = false;
    }

    /// Close the current picture: it, if it is whole and decodable.
    fn finish(&mut self) -> Option<AccessUnit> {
        let unit = std::mem::take(&mut self.unit);
        let damaged = std::mem::take(&mut self.damaged) || self.fragment.take().is_some();
        if damaged || unit.is_empty() {
            return None;
        }
        if !self.synced && unit.iter().any(|nal| is_random_access(nal_type(nal[0]))) {
            self.synced = true;
        }
        self.synced.then_some(unit)
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
/// each picture back by one per thread, so there are none.
#[cfg(feature = "apple-hp-media")]
const DECODE_THREADS: std::os::raw::c_int = 4;

/// FFmpeg's HEVC decoder, one context for the session: HEVC access units in,
/// pictures out.
#[cfg(feature = "apple-hp-media")]
struct Hevc {
    ctx: *mut avcodec_hevc_sys::AVCodecContext,
    packet: *mut avcodec_hevc_sys::AVPacket,
    frame: *mut avcodec_hevc_sys::AVFrame,
    /// The access unit as an Annex B byte stream, kept between units.
    stream: Vec<u8>,
}

// SAFETY: the context, packet and frame are created, used and freed on one thread
// at a time — the decoder thread that owns this value. libavcodec's slice threads
// work only inside a call to it.
#[cfg(feature = "apple-hp-media")]
unsafe impl Send for Hevc {}

#[cfg(feature = "apple-hp-media")]
impl Hevc {
    fn new() -> anyhow::Result<Self> {
        use avcodec_hevc_sys::*;
        use std::os::raw::c_int;

        // SAFETY: every allocation is checked before use, and `Drop` frees each of
        // them, taking null for any that failed.
        unsafe {
            // FFmpeg would print its complaints about a damaged unit to stderr,
            // outside the gateway's log. The failed call is reported instead.
            av_log_set_level(AV_LOG_QUIET);
            let codec = avcodec_find_decoder(AVCodecID_AV_CODEC_ID_HEVC);
            anyhow::ensure!(!codec.is_null(), "libavcodec has no HEVC decoder");
            let decoder = Self {
                ctx: avcodec_alloc_context3(codec),
                packet: av_packet_alloc(),
                frame: av_frame_alloc(),
                stream: Vec::new(),
            };
            anyhow::ensure!(
                !decoder.ctx.is_null() && !decoder.packet.is_null() && !decoder.frame.is_null(),
                "libavcodec could not allocate a decoder"
            );
            // The Mac codes with wavefront parallel processing, so its rows decode
            // on slice threads. See DECODE_THREADS.
            (*decoder.ctx).thread_count = DECODE_THREADS;
            (*decoder.ctx).thread_type = FF_THREAD_SLICE as c_int;
            // A unit that does not decode fails its call rather than being skipped
            // in silence, so the receive task asks for a keyframe.
            (*decoder.ctx).err_recognition |= AV_EF_EXPLODE as c_int;
            let err = avcodec_open2(decoder.ctx, codec, std::ptr::null_mut());
            anyhow::ensure!(err >= 0, "libavcodec could not open the HEVC decoder: {}", text(err));
            Ok(decoder)
        }
    }

    /// Decode one access unit, returning the picture it completed, if any.
    fn decode(&mut self, unit: &AccessUnit) -> anyhow::Result<Option<Picture>> {
        use avcodec_hevc_sys::*;
        use std::os::raw::c_int;

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
            let err = avcodec_send_packet(self.ctx, self.packet);
            (*self.packet).data = std::ptr::null_mut();
            (*self.packet).size = 0;
            anyhow::ensure!(err >= 0, "libavcodec refused an access unit: {}", text(err));
            let mut picture = None;
            loop {
                let err = avcodec_receive_frame(self.ctx, self.frame);
                if err == AVERROR_EAGAIN {
                    break;
                }
                anyhow::ensure!(err >= 0, "libavcodec: {}", text(err));
                // A later picture of the same unit replaces an earlier one: only
                // the newest is shown.
                let rgb = to_rgb(&*self.frame);
                av_frame_unref(self.frame);
                picture = Some(rgb?);
            }
            Ok(picture)
        }
    }
}

#[cfg(feature = "apple-hp-media")]
impl Drop for Hevc {
    fn drop(&mut self) {
        use avcodec_hevc_sys::*;

        // SAFETY: freed exactly once, here; each call takes null and nulls its
        // pointer.
        unsafe {
            avcodec_free_context(&mut self.ctx);
            av_packet_free(&mut self.packet);
            av_frame_free(&mut self.frame);
        }
    }
}

#[cfg(feature = "apple-hp-media")]
fn text(err: std::os::raw::c_int) -> String {
    let mut text = [0 as std::os::raw::c_char; avcodec_hevc_sys::AV_ERROR_MAX_STRING_SIZE as usize];
    // SAFETY: `av_strerror` writes a NUL-terminated description of any code, known
    // or not, within the length it is given.
    unsafe {
        avcodec_hevc_sys::av_strerror(err, text.as_mut_ptr(), text.len());
        std::ffi::CStr::from_ptr(text.as_ptr()).to_string_lossy().into_owned()
    }
}

/// A decoded picture as packed RGB888. The Mac sends full-range BT.709, 8-bit, at
/// 4:4:4; 4:2:0 is taken too, in case it ever chooses it.
///
/// # Safety
///
/// `frame` must be a picture libavcodec just returned and has not yet been
/// unreferenced.
#[cfg(feature = "apple-hp-media")]
unsafe fn to_rgb(frame: &avcodec_hevc_sys::AVFrame) -> anyhow::Result<Picture> {
    use avcodec_hevc_sys::*;
    use yuv::{YuvPlanarImage, YuvRange, YuvStandardMatrix};

    let full = frame.format == AVPixelFormat_AV_PIX_FMT_YUV444P;
    let half = frame.format == AVPixelFormat_AV_PIX_FMT_YUV420P;
    if !full && !half {
        // SAFETY: a static string for any known format, and null for any other.
        let name = unsafe { av_get_pix_fmt_name(frame.format) };
        let name = if name.is_null() {
            format!("pixel format {}", frame.format)
        } else {
            // SAFETY: non-null, so one of libavutil's static names.
            unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned()
        };
        anyhow::bail!("the Mac sent video as {name}, and only 8-bit 4:4:4 and 4:2:0 are read");
    }
    let (width, height) = (frame.width as usize, frame.height as usize);
    let chroma_rows = if full { height } else { height.div_ceil(2) };
    let plane = |channel: usize, rows: usize| {
        let stride = frame.linesize[channel] as usize;
        // SAFETY: per this function's contract, a decoded plane of `rows` rows of
        // `stride` bytes each.
        (unsafe { std::slice::from_raw_parts(frame.data[channel], stride * rows) }, stride as u32)
    };
    let (y_plane, y_stride) = plane(0, height);
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
    let range = if frame.color_range == AVColorRange_AVCOL_RANGE_JPEG { YuvRange::Full } else { YuvRange::Limited };
    let mut rgb = vec![0u8; width * height * 3];
    let stride = width as u32 * 3;
    if full {
        yuv::yuv444_to_rgb(&image, &mut rgb, stride, range, YuvStandardMatrix::Bt709)?;
    } else {
        yuv::yuv420_to_rgb(&image, &mut rgb, stride, range, YuvStandardMatrix::Bt709)?;
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
    #[cfg_attr(not(feature = "apple-hp-media"), allow(dead_code))]
    Passed(tokio::sync::mpsc::Sender<Option<PassedUnit>>),
}

/// Access units the read loop may be behind by in passing them to the browser.
/// Reaching it drops to the next keyframe, as the decoder's queue does: the loop
/// waits on the browser's link ([`crate::encode::VideoSink::pass_hevc`]), so this is
/// where a link that cannot carry the stream sheds it. Half a second of the virtual
/// display's 30 Hz ([`vnc_apple::DISPLAY_HZ`]).
const PASS_QUEUE: usize = 15;

/// One session's media stream: its offers, and once the Mac names its ports, the
/// receiver. Shared between the engine's two loops, which each decide on offers.
///
/// Only one offer is ever out: a second one, sent while the first's capture was
/// starting, failed to start (`error 32000`) and left a display stream behind that
/// crashed WindowServer when the virtual display went away.
pub struct MediaStream {
    offers: Offers,
    /// The Mac, as the TCP session reached it: where the streams come from and RTCP
    /// goes. Read by the receiver alone, which only a build with the decoders has.
    #[cfg_attr(not(feature = "apple-hp-media"), allow(dead_code))]
    peer: std::net::IpAddr,
    /// This side's address on that connection, which the UDP sockets bind.
    #[cfg_attr(not(feature = "apple-hp-media"), allow(dead_code))]
    local: std::net::IpAddr,
    /// Whether the encodings naming the media stream have gone out.
    asked: bool,
    /// An offer is out that the Mac has not answered.
    pending: bool,
    /// The size the live (or starting) stream was offered for; `None` while there
    /// is none, as after a display change.
    offered: Option<(u16, u16)>,
    /// What the stream owes the session next — see [`MediaStream::overdue`].
    owed: Owed,
    /// Signalled at every offer — see [`MediaStream::offered`].
    offer_made: std::sync::Arc<tokio::sync::Notify>,
    /// The ports the receiver is bound to, and the receiver.
    receiver: Option<((u16, u16), tokio::task::JoinHandle<()>)>,
    pictures: Outlet,
    /// Signalled by [`MediaStream::want_keyframe`], for the receiver to ask the Mac.
    #[cfg_attr(not(feature = "apple-hp-media"), allow(dead_code))]
    keyframe_wanted: std::sync::Arc<tokio::sync::Notify>,
    /// Why the receiver stopped, which it leaves here before the `None` that says
    /// so — see [`MediaStream::failure`].
    failed: Failure,
    /// When each leg last brought an authentic packet, which the receiver notes
    /// — see [`MediaStream::overdue`].
    picture_heard: Heard,
    sound_heard: Heard,
    /// When the sound leg last brought sound, an SRTP packet: what the offer's
    /// first sound is, which a report cannot stand in for.
    sounded: Heard,
    /// Where the sound leg's decoded PCM goes: the session's audio bridge, when
    /// the browser can be sent sound. `None` drains the leg unread.
    #[cfg_attr(not(feature = "apple-hp-media"), allow(dead_code))]
    sound: Option<std::sync::Arc<crate::audio::AudioBridge>>,
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
    /// An answer to the offer that went at this instant, for a display that has
    /// changed since. No other offer can go out until it comes.
    Answer(std::time::Instant),
    /// Both legs, for the offer that went at `offered`: the first picture of its
    /// display and the first sound within [`STREAM_START`] of it, and after them
    /// a packet on each leg within [`STREAM_SILENCE`] of the last. `pictured` is
    /// when the latest picture of that display came.
    Stream { offered: std::time::Instant, pictured: Option<std::time::Instant> },
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
    /// `pass` hands the read loop the Mac's access units rather than pictures
    /// decoded from them — see [`Pictures`].
    pub fn new(peer: std::net::SocketAddr, local: std::net::SocketAddr, pass: bool) -> (Self, Pictures) {
        let (pictures, rx) = if pass {
            let (tx, rx) = tokio::sync::mpsc::channel(PASS_QUEUE);
            (Outlet::Passed(tx), Pictures::Passed(rx))
        } else {
            let (tx, rx) = tokio::sync::watch::channel(None);
            (Outlet::Decoded(tx), Pictures::Decoded(rx))
        };
        let media = Self {
            offers: Offers::new(),
            peer: peer.ip(),
            local: local.ip(),
            asked: false,
            pending: false,
            offered: None,
            owed: Owed::Nothing,
            offer_made: std::sync::Arc::default(),
            receiver: None,
            pictures,
            keyframe_wanted: std::sync::Arc::default(),
            failed: Failure::default(),
            picture_heard: Heard::default(),
            sound_heard: Heard::default(),
            sounded: Heard::default(),
            sound: None,
        };
        (media, rx)
    }

    /// Carry the sound leg to `bridge`, the session's, for every stream from here.
    pub fn with_sound(mut self, bridge: Option<std::sync::Arc<crate::audio::AudioBridge>>) -> Self {
        self.sound = bridge;
        self
    }

    /// What to send to offer the stream for a display of `size` backing pixels,
    /// unless an offer is already out or the stream already runs at that size: the
    /// `0x1c` message, and ahead of the session's first one, whether the
    /// `SetEncodings` naming [`ENCODING_MEDIA_STREAM`] has to precede it.
    pub fn offer(&mut self, size: (u16, u16)) -> Option<(bool, Vec<u8>)> {
        if self.pending || self.offered == Some(size) {
            return None;
        }
        self.pending = true;
        self.offered = Some(size);
        self.owed = Owed::Stream { offered: std::time::Instant::now(), pictured: None };
        self.offer_made.notify_one();
        let first = !std::mem::replace(&mut self.asked, true);
        Some((first, self.offers.configuration(size)))
    }

    /// The newest decoded picture, for a browser that needs the whole desktop again.
    /// A passed stream has none: [`Self::want_keyframe`] is its repaint.
    pub fn latest(&self) -> Option<std::sync::Arc<Picture>> {
        match &self.pictures {
            Outlet::Decoded(pictures) => pictures.borrow().clone(),
            Outlet::Passed(_) => None,
        }
    }

    /// Ask the Mac for an IDR, with a PLI on the picture's leg: a passed stream's
    /// browser has to start over, after a reattach, a takeover or its own decoder's
    /// failure. The Mac answers within tens of milliseconds. A decoded stream asks
    /// nothing: its repaint is [`Self::latest`].
    pub fn want_keyframe(&self) {
        if matches!(self.pictures, Outlet::Passed(_)) {
            self.keyframe_wanted.notify_one();
        }
    }

    /// Whether an offer is out that the Mac has not answered: no display change may
    /// go out meanwhile. One left unanswered ends the session at [`STREAM_START`].
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// The display changed. The Mac stops both streams for it and starts them
    /// again only on an offer, which the settled layout gets; until then the stream
    /// owes nothing but an answer to an offer still out, which holds back that one.
    pub fn stopped(&mut self) {
        self.offered = None;
        self.owed = match self.owed {
            Owed::Stream { offered, .. } | Owed::Answer(offered) if self.pending => Owed::Answer(offered),
            _ => Owed::Nothing,
        };
    }

    /// A picture of `size` came from the receiver. Each one of the display the
    /// stream was offered for puts the picture's deadline off; one of another
    /// display, the old one's last, does not.
    pub fn pictured(&mut self, size: (u16, u16)) {
        if let Owed::Stream { pictured, .. } = &mut self.owed
            && self.offered == Some(size)
        {
            *pictured = Some(std::time::Instant::now());
        }
    }

    /// When the picture's leg last delivered: its latest packet, once the offered
    /// display's first picture has come, which a packet before it cannot stand in
    /// for.
    fn picture_last(&self, pictured: Option<std::time::Instant>) -> Option<std::time::Instant> {
        let pictured = pictured?;
        Some(heard_since(&self.picture_heard, pictured).unwrap_or(pictured))
    }

    /// When the sound leg last delivered: its latest packet, once sound has come
    /// since the offer at `offered`, which a report before it cannot stand in for.
    fn sound_last(&self, offered: std::time::Instant) -> Option<std::time::Instant> {
        let sounded = heard_since(&self.sounded, offered)?;
        Some(heard_since(&self.sound_heard, sounded).unwrap_or(sounded))
    }

    /// Signalled at every offer, which sets a new [`deadline`](Self::deadline): an
    /// offer can go out from either of the engine's loops, and the one that waits
    /// for the deadline may be idle behind a still screen when the other sends it.
    pub fn offered(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.offer_made)
    }

    /// When the stream is next overdue, for the session to wake at.
    pub fn deadline(&self) -> Option<std::time::Instant> {
        match self.owed {
            Owed::Nothing => None,
            Owed::Answer(offered) => Some(offered + STREAM_START),
            Owed::Stream { offered, pictured } => {
                let picture = leg_due(offered, self.picture_last(pictured));
                Some(picture.min(leg_due(offered, self.sound_last(offered))))
            }
        }
    }

    /// The error the session ends with when the stream is overdue at `now`: its
    /// offer has gone unanswered, or brought no picture or no sound, in
    /// [`STREAM_START`], or the running stream has sent nothing on a leg, neither
    /// media nor a report, for [`STREAM_SILENCE`]. Apple's viewer ends its session on the same failures,
    /// counted in RTCP timeouts on each leg, and never falls back to RFB pixels.
    pub fn overdue(&self, now: std::time::Instant) -> Option<anyhow::Error> {
        let first = STREAM_START.as_secs();
        let silence = STREAM_SILENCE.as_secs();
        let unanswered = || anyhow::anyhow!("the Mac did not answer the media-stream offer within {first}s");
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
        let ports = self.receiver.as_ref().map(|(ports, _)| *ports);
        let (offered, pictured) = match self.owed {
            Owed::Nothing => return None,
            Owed::Answer(offered) => return (now >= offered + STREAM_START).then(unanswered),
            Owed::Stream { offered, pictured } => (offered, pictured),
        };
        if now >= leg_due(offered, self.picture_last(pictured)) {
            return Some(match (pictured, ports) {
                (None, _) if self.pending => unanswered(),
                (None, None) => portless(),
                (None, Some((_, video_port))) => firewalled("picture", video_port),
                (Some(_), _) => {
                    anyhow::anyhow!("the Mac's media stream sent nothing on the picture's leg for {silence}s")
                }
            });
        }
        let heard = self.sound_last(offered);
        if now >= leg_due(offered, heard) {
            return Some(match (heard, ports) {
                (None, _) if self.pending => unanswered(),
                (None, None) => portless(),
                (None, Some((audio_port, _))) => firewalled("sound", audio_port),
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

    /// Act on an encoding-1010 rectangle. `true` when the stream is down until the
    /// next offer: the Mac re-announced its ports with no offer of this side's out,
    /// which is what it does after a display change of its own, with no stream
    /// behind the announcement.
    ///
    /// An error ends the session: the Mac refused the stream, or described one this
    /// side cannot receive. Apple's viewer shows the refusal and closes.
    pub fn on_reply(&mut self, body: &[u8]) -> anyhow::Result<bool> {
        match parse_media_reply(body)? {
            MediaReply::Ports { audio_port, video_port } => {
                if !self.pending {
                    log::debug!("vnc: the Mac re-announced its media streams unasked; they are down until offered");
                    self.stopped();
                    return Ok(true);
                }
                let ports = (audio_port, video_port);
                // The Mac names the same ports every time, and the receiver carries
                // on across display changes.
                if self.receiver.as_ref().is_some_and(|(bound, _)| *bound == ports) {
                    return Ok(false);
                }
                if let Some((_, receiver)) = self.receiver.take() {
                    receiver.abort();
                }
                log::info!(
                    "vnc: the Mac opened its media streams: screen video at UDP {video_port}, \
                     sound at {audio_port}"
                );
                self.receiver = Some((ports, self.receive(ports)?));
            }
            MediaReply::Answer => {
                log::debug!("vnc: the Mac accepted the media-stream offer");
                self.pending = false;
                // The display it was for has gone, and the new one's offer can
                // go out now.
                if matches!(self.owed, Owed::Answer(_)) {
                    self.owed = Owed::Nothing;
                }
            }
            MediaReply::Error { kind, sub_code } => anyhow::bail!(
                "the Mac refused the media stream (error type {kind}, sub-code {sub_code})"
            ),
            MediaReply::Other(kind) => log::debug!("vnc: ignoring media-stream message type {kind}"),
        }
        Ok(false)
    }

    /// Bind the ports the Mac named and start receiving on them.
    #[cfg(feature = "apple-hp-media")]
    fn receive(&self, ports: (u16, u16)) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        Ok(tokio::spawn(Receiver::bind(self, ports)?.run()))
    }

    /// Without the decoders there is nothing to receive into, and the config
    /// refuses `media_stream` in such a build, so no offer ever went out.
    #[cfg(not(feature = "apple-hp-media"))]
    fn receive(&self, _: (u16, u16)) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        anyhow::bail!("this remotex was built without the apple-hp-media feature, so it cannot receive the media stream")
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

/// Access units the HEVC decoder thread may be behind by. Reaching it drops the
/// unit, which costs a keyframe: every later picture predicts from it.
#[cfg(feature = "apple-hp-media")]
const DECODE_QUEUE: usize = 8;

/// AAC-ELD units the sound's decoder thread may be behind by. A unit costs tens of
/// microseconds to decode, so this is a ceiling rather than a working depth, and
/// reaching it drops the newest unit: 10 ms of sound, and nothing after it
/// depends on it.
#[cfg(feature = "apple-hp-media")]
const SOUND_QUEUE: usize = 64;

/// Access units per wave buffer handed to the bridge: two 10 ms units, one Opus
/// packet's worth, so the encoder downstream completes a packet per buffer.
#[cfg(feature = "apple-hp-media")]
const UNITS_PER_WAVE: usize = 2;

/// The least time between two keyframe requests. The Mac answers one in tens of
/// milliseconds; this keeps a burst of losses from asking for one per packet.
#[cfg(feature = "apple-hp-media")]
const PLI_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// How often the Mac's rate controller is sent a [`RateFeedback`] report: Apple's
/// viewer's cadence.
#[cfg(feature = "apple-hp-media")]
const RATE_FEEDBACK: std::time::Duration = std::time::Duration::from_millis(50);

/// Seconds between the debug log's picture rates: what the Mac actually sends,
/// which its 60 fps flag does not settle.
#[cfg(feature = "apple-hp-media")]
const RATE_REPORT: u32 = 10;

/// The receive buffer the screen video's socket asks for. The Mac sends a picture
/// as one burst at the link's speed, and on a 3200×2000 display a burst overflowed
/// Linux's default 208 KB, however promptly it was read: keyframes lost fragments,
/// and a display never had its first picture. Linux grants at most
/// `net.core.rmem_max`.
#[cfg(feature = "apple-hp-media")]
const VIDEO_RECEIVE_BUFFER: usize = 4 << 20;

/// How long the Mac may name its ports without a packet arriving before the log
/// says so, naming the port. A firewall or NAT between it and this gateway's UDP
/// ports is what that looks like, and the session ends at [`STREAM_START`].
#[cfg(feature = "apple-hp-media")]
const SILENT_START: std::time::Duration = std::time::Duration::from_secs(5);

/// The UDP side: RTCP out on both legs once a second and rate reports on the
/// picture's every [`RATE_FEEDBACK`], video in and depacketized, sound in and
/// decoded. It runs until the session drops it, and stops early only
/// on a failure, which it leaves in `failed` and which ends the session.
#[cfg(feature = "apple-hp-media")]
struct Receiver {
    audio: tokio::net::UdpSocket,
    video: tokio::net::UdpSocket,
    video_port: u16,
    video_srtp: SrtpReceiver,
    audio_srtp: SrtpReceiver,
    audio_rtcp: SrtcpSender,
    video_rtcp: SrtcpSender,
    video_reports: SrtcpReceiver,
    audio_reports: SrtcpReceiver,
    audio_ssrc: u32,
    video_ssrc: u32,
    pictures: Outlet,
    keyframe_wanted: std::sync::Arc<tokio::sync::Notify>,
    failed: Failure,
    picture_heard: Heard,
    sound_heard: Heard,
    sounded: Heard,
    sound: Option<std::sync::Arc<crate::audio::AudioBridge>>,
}

#[cfg(feature = "apple-hp-media")]
impl Receiver {
    /// The two sockets, bound to the port numbers the Mac named and connected to
    /// the Mac's. Every Mac names the same ones (its RFB port and the next), so a
    /// second gateway on this host with a High Performance session of its own
    /// binds them too: address and port reuse let both, and each socket being
    /// connected is what has the kernel hand each gateway its own Mac's packets.
    fn bind(media: &MediaStream, (audio_port, video_port): (u16, u16)) -> anyhow::Result<Self> {
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
        Ok(Self {
            audio: bind(audio_port, None)?,
            video: bind(video_port, Some(VIDEO_RECEIVE_BUFFER))?,
            video_port,
            video_srtp: SrtpReceiver::new(&media.offers.video_keys.1),
            audio_srtp: SrtpReceiver::new(&media.offers.audio_keys.1),
            audio_rtcp: SrtcpSender::new(&media.offers.audio_keys.0),
            video_rtcp: SrtcpSender::new(&media.offers.video_keys.0),
            video_reports: SrtcpReceiver::new(&media.offers.video_keys.1),
            audio_reports: SrtcpReceiver::new(&media.offers.audio_keys.1),
            audio_ssrc: media.offers.audio_ssrc,
            video_ssrc: media.offers.video_ssrc,
            pictures: media.pictures.clone(),
            keyframe_wanted: std::sync::Arc::clone(&media.keyframe_wanted),
            failed: std::sync::Arc::clone(&media.failed),
            picture_heard: std::sync::Arc::clone(&media.picture_heard),
            sound_heard: std::sync::Arc::clone(&media.sound_heard),
            sounded: std::sync::Arc::clone(&media.sounded),
            sound: media.sound.clone(),
        })
    }

    async fn run(mut self) {
        let keyframe = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut onward = match &self.pictures {
            Outlet::Decoded(pictures) => {
                let (units, decoder) = spawn_decoder(
                    pictures.clone(),
                    std::sync::Arc::clone(&keyframe),
                    std::sync::Arc::clone(&self.failed),
                );
                Onward::Decoder(units, decoder)
            }
            Outlet::Passed(units) => Onward::Browser(Passer::default(), units.clone()),
        };
        let keyframe_wanted = std::sync::Arc::clone(&self.keyframe_wanted);
        let mut sound = self.sound.take().map(|bridge| Sound::start(bridge, std::sync::Arc::clone(&self.failed)));
        let mut depacketizer = Depacketizer::default();
        let mut rtcp = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut rate = tokio::time::interval(RATE_FEEDBACK);
        rate.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut feedback = RateFeedback::new(std::time::Instant::now());
        let started = tokio::time::Instant::now();
        let mut last_pli: Option<tokio::time::Instant> = None;
        let mut media_ssrc = 0u32;
        let mut packets: u64 = 0;
        let mut forged: u64 = 0;
        let mut behind: u64 = 0;
        let (mut ticks, mut pictures, mut plis) = (0u32, 0u64, 0u64);
        let mut warned_silent = false;
        let mut datagram = vec![0u8; 65_536];
        let mut sound_datagram = vec![0u8; 2048];
        let failure = loop {
            let mut want_keyframe = false;
            tokio::select! {
                _ = rtcp.tick() => {
                    let audio = self.audio_rtcp.protect(&rtcp_receiver_report(self.audio_ssrc));
                    let video = self.video_rtcp.protect(&rtcp_receiver_report(self.video_ssrc));
                    let _ = self.audio.send(&audio).await;
                    let _ = self.video.send(&video).await;
                    ticks += 1;
                    if ticks % RATE_REPORT == 0 && pictures > 0 {
                        log::debug!(
                            "vnc: {:.1} pictures a second from the Mac over the last {RATE_REPORT}s, \
                             {behind} dropped behind {} so far, {plis} keyframes asked for, \
                             {:.1} ms of one-way delay reported",
                            pictures as f64 / f64::from(RATE_REPORT),
                            onward.name(),
                            feedback.delay() * 1000.0
                        );
                        pictures = 0;
                    }
                    if packets == 0 && !warned_silent && started.elapsed() >= SILENT_START {
                        warned_silent = true;
                        log::warn!(
                            "vnc: the Mac named UDP {} for its screen video but nothing has \
                             arrived in {}s — it sends to this gateway's address on that port, \
                             so a firewall or NAT between them is what this looks like",
                            self.video_port,
                            SILENT_START.as_secs()
                        );
                    }
                }
                _ = rate.tick() => {
                    if let Some(report) = feedback.report(self.video_ssrc, std::time::Instant::now()) {
                        let report = self.video_rtcp.protect(&report);
                        let _ = self.video.send(&report).await;
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
                            if let Some(sound) = sound
                                && let Err(e) = sound.push(&header, &data[header.payload.0..header.payload.1])
                            {
                                break e;
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
                received = self.video.recv(&mut datagram) => {
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
                    let data = &mut datagram[..len];
                    let header = match self.video_srtp.unprotect(data) {
                        Ok(header) => header,
                        // The Mac's report, which keeps the leg alive while a still
                        // screen sends no picture.
                        Err(SrtpError::Rtcp) => {
                            if self.video_reports.authenticate(data).is_ok() {
                                *self.picture_heard.lock().unwrap() = Some(std::time::Instant::now());
                            }
                            continue;
                        }
                        Err(SrtpError::Forged) => {
                            forged += 1;
                            if forged <= 3 {
                                log::warn!("vnc: dropped a screen video packet whose SRTP tag did not match");
                            }
                            continue;
                        }
                        Err(_) => continue,
                    };
                    *self.picture_heard.lock().unwrap() = Some(arrived);
                    feedback.received(header.ssrc, header.timestamp, arrived);
                    packets += 1;
                    if packets == 1 {
                        log::info!("vnc: the Mac's screen video is flowing (SSRC {:#x})", header.ssrc);
                    }
                    media_ssrc = header.ssrc;
                    let payload = &data[header.payload.0..header.payload.1];
                    match depacketizer.push(&header, payload) {
                        Depacketized::Pending => {}
                        Depacketized::Lost => want_keyframe = true,
                        Depacketized::Unit(unit) => match onward.send(unit) {
                            Sent::Queued => pictures += 1,
                            Sent::Full(depth) => {
                                behind += 1;
                                if behind <= 3 {
                                    log::warn!(
                                        "vnc: {} fell {depth} pictures behind the Mac; \
                                         dropping to its next keyframe",
                                        onward.name()
                                    );
                                }
                                depacketizer.resync();
                                want_keyframe = true;
                            }
                            Sent::Unready => want_keyframe = true,
                            Sent::Stopped => break anyhow::anyhow!("{} stopped", onward.name()),
                        },
                    }
                }
                () = keyframe_wanted.notified() => {
                    depacketizer.resync();
                    want_keyframe = true;
                }
            }
            if keyframe.swap(false, std::sync::atomic::Ordering::Relaxed) {
                depacketizer.resync();
                want_keyframe = true;
            }
            if want_keyframe
                && media_ssrc != 0
                && last_pli.is_none_or(|at| at.elapsed() >= PLI_INTERVAL)
            {
                last_pli = Some(tokio::time::Instant::now());
                plis += 1;
                let pli = self.video_rtcp.protect(&rtcp_pli(self.video_ssrc, media_ssrc));
                let _ = self.video.send(&pli).await;
            }
        };
        // The stream has failed while the RFB session goes on. The reason, then
        // `None`, tell the read loop, which ends the session; `None` goes after the
        // decoder's last picture, or that picture would be the last word.
        log::warn!("vnc: the Mac's media receiver stopped: {failure:#}");
        fail(&self.failed, failure);
        match onward {
            Onward::Decoder(units, decoder) => {
                drop(units);
                let _ = tokio::task::spawn_blocking(move || decoder.join()).await;
                if let Outlet::Decoded(pictures) = &self.pictures {
                    pictures.send_replace(None);
                }
            }
            Onward::Browser(_, units) => {
                let _ = units.send(None).await;
            }
        }
    }
}

/// Where the receiver sends each access unit it reassembles.
#[cfg(feature = "apple-hp-media")]
enum Onward {
    /// The decoder thread, whose pictures the session encodes as VP9.
    Decoder(std::sync::mpsc::SyncSender<AccessUnit>, std::thread::JoinHandle<()>),
    /// The read loop, which passes each unit to the browser as it came.
    Browser(Passer, tokio::sync::mpsc::Sender<Option<PassedUnit>>),
}

/// What became of one access unit sent [`Onward`].
#[cfg(feature = "apple-hp-media")]
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

#[cfg(feature = "apple-hp-media")]
impl Onward {
    fn send(&mut self, unit: AccessUnit) -> Sent {
        match self {
            Self::Decoder(units, _) => match units.try_send(unit) {
                Ok(()) => Sent::Queued,
                Err(std::sync::mpsc::TrySendError::Full(_)) => Sent::Full(DECODE_QUEUE),
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Sent::Stopped,
            },
            Self::Browser(passer, units) => {
                let Some(passed) = passer.pass(&unit) else {
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
#[cfg(feature = "apple-hp-media")]
fn fail(failed: &Failure, error: anyhow::Error) {
    let mut failed = failed.lock().unwrap();
    if failed.is_none() {
        *failed = Some(error);
    }
}

/// The decoder thread: access units in, pictures out to the watch. Its queue's
/// sender is the handle, and the thread, whose own handle comes with it, ends when
/// the receive task drops it.
/// `keyframe` is how it says a unit failed to decode, which the receive task turns
/// into a PLI. A decoder that cannot be opened leaves why in `failed`.
#[cfg(feature = "apple-hp-media")]
fn spawn_decoder(
    pictures: tokio::sync::watch::Sender<Option<std::sync::Arc<Picture>>>,
    keyframe: std::sync::Arc<std::sync::atomic::AtomicBool>,
    failed: Failure,
) -> (std::sync::mpsc::SyncSender<AccessUnit>, std::thread::JoinHandle<()>) {
    let (units, inbox) = std::sync::mpsc::sync_channel::<AccessUnit>(DECODE_QUEUE);
    let thread = std::thread::spawn(move || {
        let mut decoder = match Hevc::new() {
            Ok(decoder) => decoder,
            Err(e) => return fail(&failed, e.context("no HEVC decoder")),
        };
        let mut failures: u64 = 0;
        while let Ok(unit) = inbox.recv() {
            match decoder.decode(&unit) {
                Ok(Some(picture)) => {
                    pictures.send_replace(Some(std::sync::Arc::new(picture)));
                }
                Ok(None) => {}
                Err(e) => {
                    failures += 1;
                    if failures <= 3 {
                        log::warn!("vnc: a screen video picture did not decode: {e:#}");
                    }
                    keyframe.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    });
    (units, thread)
}

/// The sound leg on the receive task's side: authenticated, decrypted access units
/// out to a decoder thread of their own, which hands the bridge its PCM.
///
/// Fraunhofer's Rust decoder holds `Rc`s, so it is not `Send` and cannot sit in
/// the receive task; a thread of its own is the whole accommodation. The thread
/// ends when this is dropped — which aborting the receive task does — and `stale`
/// makes it stop at once rather than after draining its queue into a bridge the
/// next stream may already be filling. Dropping this also withdraws the format the
/// thread announced, since no more sound will follow it. A decoder that cannot be
/// opened leaves why in `failed`, and the next unit ends the stream.
#[cfg(feature = "apple-hp-media")]
struct Sound {
    units: std::sync::mpsc::SyncSender<Vec<u8>>,
    stale: std::sync::Arc<std::sync::atomic::AtomicBool>,
    bridge: std::sync::Arc<crate::audio::AudioBridge>,
    packets: u64,
    overrun: u64,
    forged: u64,
}

#[cfg(feature = "apple-hp-media")]
impl Sound {
    fn start(bridge: std::sync::Arc<crate::audio::AudioBridge>, failed: Failure) -> Self {
        use crate::aac_eld::{CHANNELS, EldDecoder, FRAME_SAMPLES};

        let (units, inbox) = std::sync::mpsc::sync_channel::<Vec<u8>>(SOUND_QUEUE);
        let stale = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (thread_bridge, thread_stale) = (std::sync::Arc::clone(&bridge), std::sync::Arc::clone(&stale));
        std::thread::spawn(move || {
            let mut decoder = match EldDecoder::new() {
                Ok(decoder) => decoder,
                Err(e) => return fail(&failed, e.context("no AAC-ELD decoder")),
            };
            // Announced only once a decoder exists: an encoder built for a stream
            // that never produces anything would wait on it for the session.
            thread_bridge.publish_format(AUDIO_FORMAT);
            let wave_bytes = UNITS_PER_WAVE * FRAME_SAMPLES * CHANNELS * 2;
            let mut pending: Vec<u8> = Vec::with_capacity(wave_bytes);
            let (mut decoded, mut concealed, mut undecodable) = (0u64, 0u64, 0u64);
            // The level decoded over the last second, for the debug log: the Mac
            // sends a unit every 10 ms whether or not anything plays, so a count
            // of units says nothing about whether there was sound.
            let mut level = Level::default();
            while let Ok(unit) = inbox.recv() {
                if thread_stale.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                decoded += 1;
                let from = pending.len();
                let result = decoder.decode(&unit, &mut pending);
                level.add(&pending[from..]);
                match result {
                    Ok(false) => {}
                    Ok(true) => concealed += 1,
                    Err(e) => {
                        undecodable += 1;
                        if undecodable <= 3 {
                            log::warn!("vnc: dropped a sound unit: {e:#}");
                        }
                    }
                }
                if pending.len() >= wave_bytes {
                    thread_bridge.wave(std::mem::take(&mut pending));
                    pending.reserve(wave_bytes);
                }
                if decoded.is_multiple_of(100) {
                    log::debug!(
                        "vnc: {decoded} sound units decoded, {concealed} concealed, {undecodable} \
                         undecodable; last second {}",
                        level.take()
                    );
                }
            }
        });
        Self { units, stale, bridge, packets: 0, overrun: 0, forged: 0 }
    }

    /// One authenticated, decrypted RTP packet of the sound leg: one AAC-ELD
    /// access unit, 10 ms of 48 kHz stereo. An error is a decoder that has stopped.
    fn push(&mut self, header: &RtpHeader, unit: &[u8]) -> anyhow::Result<()> {
        self.packets += 1;
        if self.packets == 1 {
            log::info!(
                "vnc: the Mac's sound is flowing: RTP payload type {}, {} bytes per unit, SSRC {:#x}",
                header.payload_type,
                unit.len(),
                header.ssrc
            );
        }
        match self.units.try_send(unit.to_vec()) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.overrun += 1;
                if self.overrun <= 3 {
                    log::warn!("vnc: the AAC-ELD decoder is {SOUND_QUEUE} units behind; dropping one");
                }
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                anyhow::bail!("the AAC-ELD decoder stopped")
            }
        }
        Ok(())
    }

    fn forged(&mut self) {
        self.forged += 1;
        if self.forged <= 3 {
            log::warn!("vnc: dropped a sound packet whose SRTP tag did not match");
        }
    }
}

/// Peak and RMS of 16-bit PCM, accumulated until taken.
#[cfg(feature = "apple-hp-media")]
#[derive(Default)]
struct Level {
    peak: u16,
    squares: f64,
    samples: u64,
}

#[cfg(feature = "apple-hp-media")]
impl Level {
    fn add(&mut self, pcm: &[u8]) {
        for sample in pcm.as_chunks::<2>().0.iter().map(|s| i16::from_le_bytes(*s)) {
            self.peak = self.peak.max(sample.unsigned_abs());
            self.squares += f64::from(sample) * f64::from(sample);
            self.samples += 1;
        }
    }

    /// The level so far, as dBFS, and a fresh start.
    fn take(&mut self) -> String {
        let dbfs = |value: f64| {
            if value > 0.0 { format!("{:.1} dBFS", 20.0 * (value / 32768.0).log10()) } else { "silence".to_owned() }
        };
        let rms = if self.samples == 0 { 0.0 } else { (self.squares / self.samples as f64).sqrt() };
        let text = format!("peak {}, rms {}", dbfs(f64::from(self.peak)), dbfs(rms));
        *self = Self::default();
        text
    }
}

#[cfg(feature = "apple-hp-media")]
impl Drop for Sound {
    fn drop(&mut self) {
        self.stale.store(true, std::sync::atomic::Ordering::Relaxed);
        self.bridge.clear_format();
    }
}

#[cfg(test)]
mod tests {
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

    /// One tile to a frame, the field of this blob that differs from Apple's, and
    /// Apple's bitrate entries, the 40 Mbit/s one first, up to 100 Mbit/s.
    #[test]
    fn the_video_offer_asks_for_one_picture_a_frame_at_apples_bitrates() {
        let blob = video_offer_blob(7, (1280, 800), TILES_PER_FRAME);
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
        let msg = configuration_message(FLAGS, &[0x11; 16], &[0xaa; 300], &a, &[0xbb; 400], &v);
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
    }

    #[test]
    fn the_replies_parse_as_measured() {
        // Message 1 as the Mac sent it: audio at 5900, video at 5901.
        let ports = unhex("0001000100000000170c00000001170d0000000100000000000000000000000000000000");
        assert_eq!(parse_media_reply(&ports).unwrap(), MediaReply::Ports { audio_port: 5900, video_port: 5901 });
        let answer = unhex("000200020000000000020003000000000000aabbccddee");
        assert_eq!(parse_media_reply(&answer).unwrap(), MediaReply::Answer);
        let error = unhex("00030001000000000000000200000000");
        assert_eq!(parse_media_reply(&error).unwrap(), MediaReply::Error { kind: 2, sub_code: 0 });
        assert_eq!(parse_media_reply(&[0, 9, 0, 1, 0, 0, 0, 0]).unwrap(), MediaReply::Other(9));
        assert!(parse_media_reply(&[0, 1, 0, 1]).is_err());
        assert!(parse_media_reply(&[0, 1, 0, 1, 0, 0, 0, 0, 0]).is_err());
        let mut disabled_audio = ports.clone();
        disabled_audio[13] = 0;
        assert!(parse_media_reply(&disabled_audio).is_err());
        let mut second_video = ports.clone();
        second_video[25] = 1;
        assert!(parse_media_reply(&second_video).is_err());
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
        srtp.last = Some((0xcafe_babe, 0x1235, 0));
        assert_eq!(srtp.unprotect(&mut packet.clone()), Err(SrtpError::Stale), "overtaken");
        srtp.last = Some((0x0102_0304, 0x1235, 0));
        assert!(srtp.unprotect(&mut packet.clone()).is_ok(), "a new stream starts over");
    }

    #[test]
    fn the_rollover_counter_follows_a_wrap_and_a_straggler() {
        let mut srtp = SrtpReceiver::new(&master());
        assert_eq!(srtp.guess_roc(1, 5), 0);
        srtp.last = Some((1, 0xfff0, 0));
        assert_eq!(srtp.guess_roc(1, 0x0002), 1, "past the wrap");
        assert_eq!(srtp.guess_roc(1, 0xffff), 0);
        srtp.last = Some((1, 0x0002, 1));
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
        RtpHeader { payload_type: 100, marker, sequence, timestamp, ssrc: 9, payload: (0, 0) }
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

    /// A 64×48 4:4:4 stream from x265, full-range BT.709 as the Mac's is, and six
    /// pixels of its last picture as ffmpeg converts them. The two conversions
    /// round differently, by a step at most.
    #[test]
    #[cfg(feature = "apple-hp-media")]
    fn a_444_stream_decodes_to_the_colours_ffmpeg_converts_it_to() {
        let units = fixture_units();
        assert_eq!(units.len(), 3);

        let mut hevc = Hevc::new().unwrap();
        let pictures: Vec<Picture> = units.iter().filter_map(|unit| hevc.decode(unit).unwrap()).collect();
        assert_eq!(pictures.len(), 3, "each unit yields its picture at once");
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
        MediaStream::new(peer, local, false).0
    }

    /// One offer out at a time, one per display, and the encodings ahead of the
    /// first.
    #[test]
    fn an_offer_goes_out_once_per_display_and_one_at_a_time() {
        let mut m = media();
        let (first, msg) = m.offer((1600, 1000)).expect("the first display gets an offer");
        assert!(first);
        assert_eq!(msg[0], 0x1c);
        assert!(m.pending());
        assert!(m.offer((1280, 800)).is_none(), "not while one is out");
        assert!(!m.on_reply(&unhex("000200020000000000000000000000000000")).unwrap());
        assert!(!m.pending());
        assert!(m.offer((1600, 1000)).is_none(), "the stream runs at this size");
        m.stopped();
        let (first, _) = m.offer((1600, 1000)).expect("a display change needs a new one");
        assert!(!first, "the encodings went out with the first");
    }

    /// A refusal is an error, which ends the session as it ends Apple's viewer's.
    /// Ports re-announced with no offer out — which is what a display change the
    /// Mac made on its own sends — are a stream down until the next offer.
    #[test]
    fn a_refusal_ends_the_session_and_an_unasked_announcement_downs_the_stream() {
        let mut m = media();
        m.offer((1600, 1000)).unwrap();
        let refused = m.on_reply(&unhex("00030001000000000000000200000000")).unwrap_err();
        assert!(
            format!("{refused:#}").contains("refused the media stream (error type 2, sub-code 0)"),
            "{refused:#}"
        );

        let mut m = media();
        let ports = unhex("0001000100000000170c00000001170d0000000100000000000000000000000000000000");
        assert!(m.on_reply(&ports).unwrap());
        assert!(m.offer((1600, 1000)).is_some());
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
        m.offer((1600, 1000)).unwrap();
        let first = m.deadline().expect("an offer owes a picture and sound");
        assert!(first >= offered + STREAM_START);
        assert!(m.overdue(first - std::time::Duration::from_millis(1)).is_none());
        let unanswered = m.overdue(first).expect("overdue at the deadline");
        assert!(unanswered.to_string().contains("did not answer"), "{unanswered}");

        assert!(!m.on_reply(&unhex("000200020000000000000000000000000000")).unwrap());
        let portless = m.overdue(first).expect("still overdue once answered");
        assert!(portless.to_string().contains("named no ports"), "{portless}");

        m.pictured((1280, 800));
        assert_eq!(m.deadline(), Some(first), "another display's picture settles nothing");
        *m.picture_heard.lock().unwrap() = Some(std::time::Instant::now());
        assert_eq!(m.deadline(), Some(first), "nor does a report before the first picture");

        m.pictured((1600, 1000));
        let Owed::Stream { pictured: Some(pictured), .. } = m.owed else {
            panic!("the picture was not noted: {:?}", m.owed)
        };
        assert_eq!(m.deadline(), Some(first), "the first sound is still owed, none since the offer");
        assert!(m.overdue(first).is_some(), "and overdue without it");

        *m.sound_heard.lock().unwrap() = Some(pictured);
        assert_eq!(m.deadline(), Some(first), "a report is not the first sound");
        assert!(m.overdue(first).is_some(), "and overdue without it");
        *m.sounded.lock().unwrap() = Some(pictured);
        *m.picture_heard.lock().unwrap() = Some(pictured);
        let next = m.deadline().expect("a running stream owes a packet on each leg");
        assert_eq!(next, pictured + STREAM_SILENCE);
        assert!(m.overdue(first).is_none(), "the first picture and sound settled the offer");
        let silent = m.overdue(next).expect("overdue after the silence");
        assert!(silent.to_string().contains("sent nothing on the picture's leg for 48s"), "{silent}");

        // A still screen sends no picture, and the Mac's report on the picture's
        // leg is what keeps it alive.
        let reported = pictured + std::time::Duration::from_secs(1);
        *m.picture_heard.lock().unwrap() = Some(reported);
        assert_eq!(m.deadline(), Some(next), "the sound is due first now");
        let quiet = m.overdue(next).expect("a leg without sound is overdue too");
        assert!(quiet.to_string().contains("sent nothing on the sound's leg for 48s"), "{quiet}");
        *m.sound_heard.lock().unwrap() = Some(reported);
        assert_eq!(m.deadline(), Some(reported + STREAM_SILENCE), "a report on each leg puts both off");
        assert!(m.overdue(next).is_none());

        m.stopped();
        assert_eq!(m.deadline(), None, "a display change owes nothing");
        assert!(m.overdue(next + STREAM_SILENCE).is_none());
        m.offer((1280, 800)).unwrap();
        assert!(m.deadline().is_some(), "until its own offer");
    }

    /// A display change while an offer is out still owes that offer's answer by
    /// [`STREAM_START`], and the offer goes on holding back the next display's
    /// until it comes.
    #[test]
    fn an_offer_out_across_a_display_change_still_owes_its_answer() {
        let mut m = media();
        m.offer((1600, 1000)).unwrap();
        let first = m.deadline().unwrap();
        m.stopped();
        m.stopped();
        assert!(m.pending());
        assert!(m.offer((1280, 800)).is_none(), "not while the first is out");
        assert_eq!(m.deadline(), Some(first), "the answer is still owed");
        assert!(m.overdue(first - std::time::Duration::from_millis(1)).is_none());
        let unanswered = m.overdue(first).expect("unanswered at the deadline");
        assert!(unanswered.to_string().contains("did not answer"), "{unanswered}");

        assert!(!m.on_reply(&unhex("000200020000000000000000000000000000")).unwrap());
        assert_eq!(m.deadline(), None, "answered, for a display that has gone");
        assert!(m.offer((1280, 800)).is_some(), "the new display's offer goes out");
    }
}
