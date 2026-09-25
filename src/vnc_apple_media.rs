//! Apple High Performance's picture and sound, the way Apple's viewer takes them:
//! HEVC and AAC-ELD over the media stream, not zlib over RFB and not AirPlay.
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
//! A target takes this path with `media_stream = true`, and only in a build with
//! the `apple-hp-media` feature: the wire half of this module — offers, replies,
//! SRTP, depacketizing — is always compiled and tested, and the feature adds the
//! two decoders and the receiver that feeds them. A build without it refuses the
//! key at config parse, so nothing here runs in it.
//!
//! The offer is two AVConference negotiation blobs, rebuilt field by field from the
//! ones Apple's client produced ([`audio_offer_blob`], [`video_offer_blob`]). The
//! Mac refuses a configuration without either, so the two legs go together: the
//! picture comes from one and the sound from the other, and while the sound leg
//! runs the Mac mutes its own output, as it does for Apple's viewer.
//!
//! Every packet in is authenticated before it is decrypted — AES-256 counter mode
//! with an HMAC-SHA1-80 tag, RFC 3711 keys from the masters this side put in the
//! offer ([`SrtpReceiver`]) — and every report out is SRTCP under this side's own
//! keys ([`SrtcpSender`]). A packet whose tag does not match is dropped.
//!
//! Three fields of the offer differ from Apple's, each measured:
//!
//! - the flags word carries [`FLAG_NO_CURSOR`], which makes the agent capture the
//!   screen without the pointer (`send cursor with video 0`) — the pointer keeps
//!   arriving as its own shape over RFB;
//! - `tilesPerFrame` is 1, so each picture is one HEVC picture of the whole
//!   display; Apple's 4 splits it into strips coded as separate pictures;
//! - the bitrate entries are capped at [`BITRATE_CAP`]; uncapped, the sender pads
//!   an animating lock screen out to about 19 Mbit/s.
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

/// The refresh rate a media-stream session asks for its virtual display, which
/// is what bounds the Mac's picture rate: the 60 fps flag above does not. Under a
/// full-screen animation the Mac sent about 57 pictures a second at 60 Hz, with or
/// without the flag, and 30.0 at 30 Hz. The browser is sent 30 frames a second
/// ([`crate::encode`]), so a faster display only doubles the HEVC decoding.
pub const DISPLAY_HZ: u8 = 30;

/// The ceiling put on every bitrate entry of the negotiation blobs, in bits per
/// second. Apple's entries run to 100 Mbit/s, and a sender with no feedback from its
/// receiver fills what it was offered: an animating lock screen came at
/// 19 Mbit/s uncapped and about 7 under an 8 Mbit/s cap, an idle desktop at almost
/// nothing either way.
pub const BITRATE_CAP: u64 = 12_000_000;

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
/// given order with its bitrates capped at `cap`, `f13`, `f14 = 2`, `f16 = 0`.
fn blob_tail(blob: &mut Proto, codecs: &[(u64, u64, Option<u64>)], cap: u64, f13: u64) {
    blob.bytes(6, VICEROY.as_bytes());
    blob.uint(8, 0);
    for &(f1, f2, f3) in codecs {
        let f2 = if f1 == 0 { f2.min(cap) } else { f2 };
        blob.message(9, codec_entry((f1, f2, f3)));
    }
    blob.uint(13, f13);
    blob.uint(14, 2);
    blob.uint(16, 0);
}

/// The audio offer's blob, before compression: `VCMediaNegotiationBlobV2` with one
/// audio stream whose SSRC is `ssrc`.
fn audio_offer_blob(ssrc: u32, cap: u64) -> Vec<u8> {
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
    blob_tail(&mut blob, CODEC_ENTRIES, cap, AUDIO_F13);
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
fn video_offer_blob(ssrc: u32, (width, height): (u16, u16), tiles: u64, cap: u64) -> Vec<u8> {
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
    blob_tail(&mut blob, &codecs, cap, VIDEO_F13);
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
        let audio = offer(MODE_AUDIO, &audio_offer_blob(self.audio_ssrc, BITRATE_CAP), &self.call_id);
        let video = offer(
            MODE_VIDEO,
            &video_offer_blob(self.video_ssrc, size, TILES_PER_FRAME, BITRATE_CAP),
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
                body.len() >= 16,
                "media-stream message 1 is {} bytes, too short for its ports",
                body.len()
            );
            Ok(MediaReply::Ports {
                audio_port: u16::from_be_bytes([body[8], body[9]]),
                video_port: u16::from_be_bytes([body[14], body[15]]),
            })
        }
        2 => Ok(MediaReply::Answer),
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
    pub fn unprotect(&mut self, data: &mut [u8]) -> Result<RtpHeader, SrtpError> {
        let header = rtp_header(data)?;
        let roc = self.guess_roc(header.ssrc, header.sequence);
        let (end, len) = (header.payload.1, data.len());
        let tag = self.keys.tag(&[&data[..end], &roc.to_be_bytes()]);
        if tag[..] != data[end..len] {
            return Err(SrtpError::Forged);
        }
        let index = (u64::from(roc) << 16) | u64::from(header.sequence);
        let iv = self.keys.iv(header.ssrc, index);
        aes_ctr_xor(&self.keys.cipher, iv, &mut data[header.payload.0..end]);
        let newer = match self.last {
            Some((ssrc, seq, last_roc)) if ssrc == header.ssrc => {
                (u64::from(roc) << 16 | u64::from(header.sequence))
                    > (u64::from(last_roc) << 16 | u64::from(seq))
            }
            _ => true,
        };
        if newer {
            self.last = Some((header.ssrc, header.sequence, roc));
        }
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
// The decoder
// ---------------------------------------------------------------------------

/// A decoded picture, as packed RGB888 — what [`crate::encode::VideoSink`] takes.
pub struct Picture {
    pub size: (u16, u16),
    pub rgb: Vec<u8>,
}

/// libde265's worker threads. The Mac's stream sets
/// `entropy_coding_sync_enabled_flag`, so a picture's CTU rows decode in parallel.
/// Replaying captured 1600×1000 pictures at 60 a second on a six-core host with a
/// VP9 encoder running beside them, one thread fell behind and dropped units, two
/// kept up at 91% busy, and three to five took about 13 ms a picture. Four, since
/// a larger display has more rows to share out.
#[cfg(feature = "apple-hp-media")]
const DECODE_THREADS: std::os::raw::c_int = 4;

/// libde265, one context for the session: HEVC access units in, pictures out.
#[cfg(feature = "apple-hp-media")]
struct Hevc(*mut de265_sys::de265_decoder_context);

// SAFETY: the context is created, used and freed on one thread at a time — the
// decoder thread that owns this value — and libde265 keeps no thread-local state.
#[cfg(feature = "apple-hp-media")]
unsafe impl Send for Hevc {}

#[cfg(feature = "apple-hp-media")]
impl Hevc {
    fn new() -> anyhow::Result<Self> {
        // SAFETY: no arguments; a null return is checked.
        let ctx = unsafe { de265_sys::de265_new_decoder() };
        anyhow::ensure!(!ctx.is_null(), "libde265 could not allocate a decoder");
        let decoder = Self(ctx);
        // The Mac codes with wavefront parallel processing, so its rows decode on
        // worker threads. See DECODE_THREADS.
        // SAFETY: a live context, before anything was pushed into it.
        let err = unsafe { de265_sys::de265_start_worker_threads(decoder.0, DECODE_THREADS) };
        anyhow::ensure!(
            err == de265_sys::de265_error_DE265_OK,
            "libde265 could not start its worker threads: {}",
            text(err)
        );
        Ok(decoder)
    }

    /// Decode one access unit, returning the picture it completed, if any.
    fn decode(&mut self, unit: &AccessUnit) -> anyhow::Result<Option<Picture>> {
        use de265_sys::*;
        use std::os::raw::c_int;

        // SAFETY: `self.0` is a live context; `de265_push_NAL` copies each unit, so
        // the slices need not outlive the call.
        unsafe {
            for nal in unit {
                let err = de265_push_NAL(self.0, nal.as_ptr().cast(), nal.len() as c_int, 0, std::ptr::null_mut());
                anyhow::ensure!(err == de265_error_DE265_OK, "libde265 refused a NAL unit: {}", text(err));
            }
            de265_push_end_of_frame(self.0);
            let mut picture = None;
            loop {
                let mut more: c_int = 0;
                let err = de265_decode(self.0, &mut more);
                let flow = err == de265_error_DE265_ERROR_WAITING_FOR_INPUT_DATA
                    || err == de265_error_DE265_ERROR_IMAGE_BUFFER_FULL;
                anyhow::ensure!(err == de265_error_DE265_OK || flow, "libde265: {}", text(err));
                let img = de265_get_next_picture(self.0);
                if !img.is_null() {
                    // A later picture of the same unit replaces an earlier one: only
                    // the newest is shown.
                    picture = Some(to_rgb(&*img)?);
                }
                if err == de265_error_DE265_ERROR_WAITING_FOR_INPUT_DATA || (more == 0 && img.is_null()) {
                    break;
                }
            }
            Ok(picture)
        }
    }
}

#[cfg(feature = "apple-hp-media")]
impl Drop for Hevc {
    fn drop(&mut self) {
        // SAFETY: freed exactly once, here.
        unsafe {
            de265_sys::de265_free_decoder(self.0);
        }
    }
}

#[cfg(feature = "apple-hp-media")]
fn text(err: de265_sys::de265_error) -> String {
    // SAFETY: libde265 returns a pointer to a static string for every code.
    unsafe { std::ffi::CStr::from_ptr(de265_sys::de265_get_error_text(err)) }
        .to_string_lossy()
        .into_owned()
}

/// A decoded picture as packed RGB888. The Mac sends full-range BT.709, 8-bit, at
/// 4:4:4; 4:2:0 is taken too, in case it ever chooses it.
///
/// # Safety
///
/// `img` must be a picture libde265 just returned and has not yet invalidated.
#[cfg(feature = "apple-hp-media")]
unsafe fn to_rgb(img: &de265_sys::de265_image) -> anyhow::Result<Picture> {
    use de265_sys::*;
    use yuv::{YuvPlanarImage, YuvRange, YuvStandardMatrix};

    // SAFETY: per this function's contract, every call below reads a live image.
    unsafe {
        let bits = de265_get_bits_per_pixel(img, 0);
        anyhow::ensure!(bits == 8, "the Mac sent {bits}-bit video, and only 8-bit is read");
        let width = de265_get_image_width(img, 0);
        let height = de265_get_image_height(img, 0);
        let plane = |channel: i32| {
            let mut stride = 0;
            let data = de265_get_image_plane(img, channel, &mut stride);
            let rows = de265_get_image_height(img, channel) as usize;
            (std::slice::from_raw_parts(data, stride as usize * rows), stride as u32)
        };
        let (y_plane, y_stride) = plane(0);
        let (u_plane, u_stride) = plane(1);
        let (v_plane, v_stride) = plane(2);
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
        let range =
            if de265_get_image_full_range_flag(img) != 0 { YuvRange::Full } else { YuvRange::Limited };
        let mut rgb = vec![0u8; width as usize * height as usize * 3];
        let stride = width as u32 * 3;
        let chroma = de265_get_chroma_format(img);
        if chroma == de265_chroma_de265_chroma_444 {
            yuv::yuv444_to_rgb(&image, &mut rgb, stride, range, YuvStandardMatrix::Bt709)?;
        } else if chroma == de265_chroma_de265_chroma_420 {
            yuv::yuv420_to_rgb(&image, &mut rgb, stride, range, YuvStandardMatrix::Bt709)?;
        } else {
            anyhow::bail!("the Mac sent video in chroma format {chroma}, which is not read");
        }
        Ok(Picture { size: (width as u16, height as u16), rgb })
    }
}

// ---------------------------------------------------------------------------
// The session's media stream
// ---------------------------------------------------------------------------

/// The newest decoded picture, for the read loop to show. A picture is the whole
/// display, so an unshown one is simply replaced.
pub type Pictures = tokio::sync::watch::Receiver<Option<std::sync::Arc<Picture>>>;

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
    /// When the offer that is out went, until the Mac answers it.
    pending: Option<std::time::Instant>,
    /// The size the live (or starting) stream was offered for; `None` while there
    /// is none, as after a display change.
    offered: Option<(u16, u16)>,
    /// The ports the receiver is bound to, and the receiver.
    receiver: Option<((u16, u16), tokio::task::JoinHandle<()>)>,
    pictures: tokio::sync::watch::Sender<Option<std::sync::Arc<Picture>>>,
    /// Where the sound leg's decoded PCM goes: the session's audio bridge, when
    /// the browser can be sent sound. `None` drains the leg unread.
    #[cfg_attr(not(feature = "apple-hp-media"), allow(dead_code))]
    sound: Option<std::sync::Arc<crate::audio::AudioBridge>>,
}

impl MediaStream {
    pub fn new(peer: std::net::SocketAddr, local: std::net::SocketAddr) -> (Self, Pictures) {
        let (pictures, rx) = tokio::sync::watch::channel(None);
        let media = Self {
            offers: Offers::new(),
            peer: peer.ip(),
            local: local.ip(),
            asked: false,
            pending: None,
            offered: None,
            receiver: None,
            pictures,
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
        if self.pending() || self.offered == Some(size) {
            return None;
        }
        self.pending = Some(std::time::Instant::now());
        self.offered = Some(size);
        let first = !std::mem::replace(&mut self.asked, true);
        Some((first, self.offers.configuration(size)))
    }

    /// The newest decoded picture, for a browser that needs the whole desktop again.
    pub fn latest(&self) -> Option<std::sync::Arc<Picture>> {
        self.pictures.borrow().clone()
    }

    /// Whether an offer is out: no display change may go out meanwhile. An offer
    /// unanswered for [`OFFER_ANSWER`] no longer counts, so a lost answer cannot
    /// hold every later resize.
    pub fn pending(&self) -> bool {
        self.pending.is_some_and(|at| at.elapsed() < OFFER_ANSWER)
    }

    /// A display change went out. The Mac stops both streams for it and starts
    /// them again only on an offer, which the settled layout gets.
    pub fn stopped(&mut self) {
        self.offered = None;
    }

    /// Act on an encoding-1010 rectangle. `true` when the stream is down: the
    /// Mac refused it, or re-announced its ports with no offer of this side's
    /// out, which is what it does after a display change of its own, with no
    /// stream behind the announcement.
    pub fn on_reply(&mut self, body: &[u8]) -> anyhow::Result<bool> {
        match parse_media_reply(body)? {
            MediaReply::Ports { audio_port, video_port } => {
                if !self.pending() {
                    log::debug!("vnc: the Mac re-announced its media streams unasked; they are down until offered");
                    self.offered = None;
                    return Ok(true);
                }
                let ports = (audio_port, video_port);
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
                self.pending = None;
            }
            MediaReply::Error { kind, sub_code } => {
                // `offered` keeps its size, so this layout is not offered again;
                // the picture stays on zlib until the next display change.
                log::warn!(
                    "vnc: the Mac refused the media stream (error type {kind}, sub-code \
                     {sub_code}); the picture stays on zlib"
                );
                self.pending = None;
                return Ok(true);
            }
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

/// How long an offer may go unanswered before it stops holding display changes
/// back. The Mac answers in under a second.
const OFFER_ANSWER: std::time::Duration = std::time::Duration::from_secs(10);

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

/// Seconds between the debug log's picture rates: what the Mac actually sends,
/// which its 60 fps flag does not settle.
#[cfg(feature = "apple-hp-media")]
const RATE_REPORT: u32 = 10;

/// How long the Mac may name its ports without a packet arriving before that is
/// said. A firewall or NAT between it and this gateway's UDP ports is what that
/// looks like; the picture stays on zlib meanwhile.
#[cfg(feature = "apple-hp-media")]
const SILENT_START: std::time::Duration = std::time::Duration::from_secs(5);

/// The UDP side: RTCP out on both legs once a second, video in and depacketized,
/// sound in and decoded.
#[cfg(feature = "apple-hp-media")]
struct Receiver {
    audio: tokio::net::UdpSocket,
    video: tokio::net::UdpSocket,
    video_port: u16,
    video_srtp: SrtpReceiver,
    audio_srtp: SrtpReceiver,
    audio_rtcp: SrtcpSender,
    video_rtcp: SrtcpSender,
    audio_ssrc: u32,
    video_ssrc: u32,
    pictures: tokio::sync::watch::Sender<Option<std::sync::Arc<Picture>>>,
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
        let bind = |port: u16| -> anyhow::Result<tokio::net::UdpSocket> {
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
            socket
                .bind(&at.into())
                .with_context(|| format!("bind UDP {at} for the Mac's media stream"))?;
            socket.connect(&to.into()).with_context(|| format!("connect UDP {at} to {to}"))?;
            socket.set_nonblocking(true)?;
            Ok(tokio::net::UdpSocket::from_std(socket.into())?)
        };
        Ok(Self {
            audio: bind(audio_port)?,
            video: bind(video_port)?,
            video_port,
            video_srtp: SrtpReceiver::new(&media.offers.video_keys.1),
            audio_srtp: SrtpReceiver::new(&media.offers.audio_keys.1),
            audio_rtcp: SrtcpSender::new(&media.offers.audio_keys.0),
            video_rtcp: SrtcpSender::new(&media.offers.video_keys.0),
            audio_ssrc: media.offers.audio_ssrc,
            video_ssrc: media.offers.video_ssrc,
            pictures: media.pictures.clone(),
            sound: media.sound.clone(),
        })
    }

    async fn run(mut self) {
        let keyframe = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let units = spawn_decoder(self.pictures.clone(), std::sync::Arc::clone(&keyframe));
        let mut sound = self.sound.take().map(Sound::start);
        let mut depacketizer = Depacketizer::default();
        let mut rtcp = tokio::time::interval(std::time::Duration::from_secs(1));
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
        loop {
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
                             {behind} dropped behind the decoder so far, {plis} keyframes asked for",
                            pictures as f64 / f64::from(RATE_REPORT)
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
                received = self.audio.recv(&mut sound_datagram) => {
                    let len = match received {
                        Ok(len) => len,
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => continue,
                        Err(e) => {
                            log::warn!("vnc: the Mac's sound socket failed: {e}");
                            break;
                        }
                    };
                    let Some(sound) = sound.as_mut() else {
                        continue;
                    };
                    let data = &mut sound_datagram[..len];
                    match self.audio_srtp.unprotect(data) {
                        Ok(header) => sound.push(&header, &data[header.payload.0..header.payload.1]),
                        Err(SrtpError::Forged) => sound.forged(),
                        Err(_) => {}
                    }
                }
                received = self.video.recv(&mut datagram) => {
                    let len = match received {
                        Ok(len) => len,
                        // A connected socket's report of an ICMP unreachable, for a
                        // report sent before the Mac's side was up.
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => continue,
                        Err(e) => {
                            log::warn!("vnc: the Mac's screen video socket failed: {e}");
                            break;
                        }
                    };
                    let data = &mut datagram[..len];
                    let header = match self.video_srtp.unprotect(data) {
                        Ok(header) => header,
                        Err(SrtpError::Forged) => {
                            forged += 1;
                            if forged <= 3 {
                                log::warn!("vnc: dropped a screen video packet whose SRTP tag did not match");
                            }
                            continue;
                        }
                        Err(_) => continue,
                    };
                    packets += 1;
                    if packets == 1 {
                        log::info!("vnc: the Mac's screen video is flowing (SSRC {:#x})", header.ssrc);
                    }
                    media_ssrc = header.ssrc;
                    let payload = &data[header.payload.0..header.payload.1];
                    match depacketizer.push(&header, payload) {
                        Depacketized::Pending => {}
                        Depacketized::Lost => want_keyframe = true,
                        Depacketized::Unit(unit) => match units.try_send(unit) {
                            Ok(()) => pictures += 1,
                            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                                behind += 1;
                                if behind <= 3 {
                                    log::warn!(
                                        "vnc: the HEVC decoder fell {DECODE_QUEUE} pictures behind the Mac; \
                                         dropping to its next keyframe"
                                    );
                                }
                                depacketizer.resync();
                                want_keyframe = true;
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                                log::warn!("vnc: the HEVC decoder is gone; the picture stays on zlib");
                                break;
                            }
                        },
                    }
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
        }
    }
}

/// The decoder thread: access units in, pictures out to the watch. Its queue's
/// sender is the handle, and the thread ends when the receive task drops it.
/// `keyframe` is how it says a unit failed to decode, which the receive task turns
/// into a PLI.
#[cfg(feature = "apple-hp-media")]
fn spawn_decoder(
    pictures: tokio::sync::watch::Sender<Option<std::sync::Arc<Picture>>>,
    keyframe: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::sync::mpsc::SyncSender<AccessUnit> {
    let (units, inbox) = std::sync::mpsc::sync_channel::<AccessUnit>(DECODE_QUEUE);
    std::thread::spawn(move || {
        let mut decoder = match Hevc::new() {
            Ok(decoder) => decoder,
            Err(e) => {
                log::warn!("vnc: no HEVC decoder, so the picture stays on zlib: {e:#}");
                return;
            }
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
    units
}

/// The sound leg on the receive task's side: authenticated, decrypted access units
/// out to a decoder thread of their own, which hands the bridge its PCM.
///
/// Fraunhofer's Rust decoder holds `Rc`s, so it is not `Send` and cannot sit in
/// the receive task; a thread of its own is the whole accommodation. The thread
/// ends when this is dropped — which aborting the receive task does — and `stale`
/// makes it stop at once rather than after draining its queue into a bridge the
/// next stream may already be filling. Dropping this also withdraws the format the
/// thread announced, since no more sound will follow it.
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
    fn start(bridge: std::sync::Arc<crate::audio::AudioBridge>) -> Self {
        use crate::aac_eld::{CHANNELS, EldDecoder, FRAME_SAMPLES};

        let (units, inbox) = std::sync::mpsc::sync_channel::<Vec<u8>>(SOUND_QUEUE);
        let stale = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (thread_bridge, thread_stale) = (std::sync::Arc::clone(&bridge), std::sync::Arc::clone(&stale));
        std::thread::spawn(move || {
            let mut decoder = match EldDecoder::new() {
                Ok(decoder) => decoder,
                Err(e) => {
                    log::warn!("vnc: no AAC-ELD decoder, so no sound from the Mac: {e:#}");
                    return;
                }
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
        match self.units.try_send(unit.to_vec()) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.overrun += 1;
                if self.overrun <= 3 {
                    log::warn!("vnc: the AAC-ELD decoder is {SOUND_QUEUE} units behind; dropping one");
                }
            }
            // The decoder never opened, which it said: drain the leg quietly.
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
        }
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
        assert_eq!(audio_offer_blob(3_606_155_525, u64::MAX), unhex(CAPTURED_AUDIO_BLOB));
        assert_eq!(
            video_offer_blob(3_023_179_925, (1600, 1000), 4, u64::MAX),
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

    /// The two fields this offer changes from Apple's: one tile to a frame, and no
    /// bitrate entry above the cap. Everything else is the captured offer's.
    #[test]
    fn the_video_offer_asks_for_one_picture_a_frame_at_a_capped_bitrate() {
        let blob = video_offer_blob(7, (1280, 800), TILES_PER_FRAME, BITRATE_CAP);
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
        assert_eq!(bitrates.len(), 6);
        assert!(bitrates.iter().all(|&b| b <= BITRATE_CAP), "{bitrates:?}");
        assert!(bitrates.contains(&6_000_000), "entries under the cap keep their value");
    }

    /// The plist around the blob: header, the dictionary's shape, and a trailer
    /// whose offsets land on the objects they name.
    #[test]
    fn the_offer_is_a_binary_plist_of_four_entries() {
        let plist = offer(MODE_AUDIO, &audio_offer_blob(1, BITRATE_CAP), "910BCF8F-D1D7-4EB6-B728-E1FDB02DD3B6");
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
        assert_eq!(parse_media_reply(&[0, 2, 0, 2, 0, 0, 0, 1]).unwrap(), MediaReply::Answer);
        let error = unhex("00030001000000000000000200000000");
        assert_eq!(parse_media_reply(&error).unwrap(), MediaReply::Error { kind: 2, sub_code: 0 });
        assert_eq!(parse_media_reply(&[0, 9, 0, 1, 0, 0, 0, 0]).unwrap(), MediaReply::Other(9));
        assert!(parse_media_reply(&[0, 1, 0, 1]).is_err());
        assert!(parse_media_reply(&[0, 1, 0, 1, 0, 0, 0, 0, 0]).is_err());
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
    fn libde265_decodes_a_444_stream_to_the_colours_ffmpeg_does() {
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

    fn media() -> MediaStream {
        let peer = "[fd00::2]:5900".parse().unwrap();
        let local = "[fd00::1]:50000".parse().unwrap();
        MediaStream::new(peer, local).0
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
        assert!(!m.on_reply(&[0, 2, 0, 2, 0, 0, 0, 1]).unwrap());
        assert!(!m.pending());
        assert!(m.offer((1600, 1000)).is_none(), "the stream runs at this size");
        m.stopped();
        let (first, _) = m.offer((1600, 1000)).expect("a display change needs a new one");
        assert!(!first, "the encodings went out with the first");
    }

    /// The stream is down on a refusal, and on ports re-announced with no offer
    /// out — which is what a display change the Mac made on its own sends.
    #[test]
    fn a_refusal_or_an_unasked_announcement_is_a_stream_gone() {
        let mut m = media();
        m.offer((1600, 1000)).unwrap();
        assert!(m.on_reply(&unhex("00030001000000000000000200000000")).unwrap());
        assert!(!m.pending());
        assert!(m.offer((1600, 1000)).is_none(), "a refused display is not offered again");

        let mut m = media();
        let ports = unhex("0001000100000000170c00000001170d0000000100000000000000000000000000000000");
        assert!(m.on_reply(&ports).unwrap());
        assert!(m.offer((1600, 1000)).is_some());
    }
}
