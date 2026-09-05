//! Apple High Performance system audio: the media-stream negotiation over RFB, and
//! the SRTP receiver it opens.
//!
//! High Performance mode carries the Mac's **system audio**, and it does not ride RFB.
//! RFB only negotiates it — one client message and two server rectangles — after
//! which `ScreensharingAgent` opens an AVConference `AVCAudioStream` to the viewer
//! over UDP with SRTP, and what arrives is AAC-ELD at 48 kHz stereo, one 480-sample
//! access unit per RTP packet. None of it is documented by Apple; every byte here is
//! measurement against macOS 26.6, recorded in `docs/apple-vnc-889.md` ("The media
//! stream") and first spoken by `tests/hp_audio_probe.py`, which this module is the
//! Rust of. The one thing the probe did not do is *synthesize* the AVConference
//! offers — it replayed plists generated on a Mac — and [`audio_offer`] and
//! [`video_offer`] are those bytes rebuilt field by field, checked against the
//! captured ones in the tests below.
//!
//! The wire half of this module is always compiled and always tested. What the
//! `apple-hp-audio` feature adds is the decoder ([`crate::aac_eld`]) and with it the
//! receiver that needs one; a build without the feature refuses `audio = true` on an
//! `ard-high-performance` target at config parse, so nothing here runs in it.
//!
//! ## The negotiation
//!
//! After the first display layout the client advertises encoding **1010**
//! ([`ENCODING_MEDIA_STREAM`]) in the second `SetEncodings` — the one that also asks
//! for zlib — and sends message **`0x1c`** (`RFBMediaStreamServerConfiguration`,
//! [`media_stream_configuration`]): a session UUID, an SRTP master key per direction
//! per stream, and an AVConference *offer* per stream. The Mac answers with two
//! rectangles: encoding 1010 carries message 1, the UDP port, or message 3, an error
//! ([`MediaReply`]); encoding 1011 carries message 2, the AVConference answer, which
//! nothing here needs. Audio arrives at the named port from the Mac, encrypted with
//! the server-to-viewer key the client itself chose ([`SrtpSession`]).
//!
//! **The Mac refuses audio alone.** A configuration whose video offer is empty
//! negotiates the audio and then tears the whole stream down (`unable to create
//! video config`, error type 2), so a valid screen-video offer rides alongside even
//! though nothing here ever opens the video port.
//!
//! **The codec is not negotiable.** The offer carries a codec list and the answer
//! echoes what it agreed to, and the transmitter encodes AAC-ELD regardless — proven
//! by an offer with AAC-ELD removed, agreed by the server, that streamed AAC-ELD
//! anyway. The offer below is what Apple's own client sends, verbatim; there is
//! nothing to gain by editing it.
//!
//! ## The stream
//!
//! RTP payload type 101, timestamps advancing 480 per packet at 48 kHz. SRTP is
//! AES-256 counter mode with RFC 3711 key derivation from the 46-byte master (32-byte
//! key, 14-byte salt) and an HMAC-SHA1-80 tag on every packet — 10 bytes this
//! receiver strips and does not verify: the packets arrive from the address the
//! session authenticated to over TLS-grade DH, and the key that decrypts them never
//! left this process. The Mac expects RTCP once a second and gives up after three
//! without, so the receiver sends a minimal receiver report on that cadence.
//!
//! Decoded PCM goes to the same [`AudioBridge`] the RDP engine feeds, two access
//! units at a time — 20 ms, one Opus packet's worth — and from there the path is the
//! one every target shares: the queue, the Opus or passthrough encoder, `/ws/audio`.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use aes::Aes256;
use aes::cipher::{BlockCipherEncrypt as _, KeyInit as _};
use log::{debug, info, warn};
use rand::Rng as _;

use crate::audio::{AudioBridge, PcmFormat};
use crate::vnc_apple;

/// Encoding for the Mac's media-stream replies: message 1 (the UDP ports) and
/// message 3 (an error). `kSSVideoEncoding_AVCMediaStream` in the client binary.
pub const ENCODING_MEDIA_STREAM: i32 = 1010;

/// Encoding for message 2, the AVConference answer. Never advertised — the Mac sends
/// it beside 1010 — and stepped over when it arrives.
pub const ENCODING_MEDIA_STREAM_ANSWER: i32 = 1011;

/// What the decoded stream is: AAC-ELD's 48 kHz stereo as 16-bit PCM. The
/// counterpart of [`crate::audio::PCM_CD_QUALITY`] for this source, and the format
/// the session builds its encoder for before the stream has come up.
pub const SOURCE_FORMAT: PcmFormat = PcmFormat {
    channels: 2,
    sample_rate: 48_000,
    bits_per_sample: 16,
};

/// The second `SetEncodings` with the media-stream encoding appended: the same list
/// that asks for zlib, then 1010. Measured order — the probe sent it last and the Mac
/// answered.
pub fn encodings_with_media_stream() -> Vec<i32> {
    let mut encodings = vnc_apple::ENCODINGS_WITH_ZLIB.to_vec();
    encodings.push(ENCODING_MEDIA_STREAM);
    encodings
}

/// An SRTP master key as the `0x1c` message carries it: 32 bytes of AES-256 key
/// followed by 14 bytes of salt.
pub type MasterKey = [u8; 46];

/// The RTP payload's trailing HMAC-SHA1-80 authentication tag, not verified here —
/// see the module doc.
pub const AUTH_TAG_LEN: usize = 10;

// ---------------------------------------------------------------------------
// The AVConference offers
// ---------------------------------------------------------------------------

/// The `avcMediaStreamNegotiatorMode` of an audio offer.
const MODE_AUDIO: u8 = 8;
/// And of the screen-video offer that has to ride beside it.
const MODE_VIDEO: u8 = 7;

/// `avcMediaStreamOptionRemoteEndpointInfo`: the viewer's model and builds, as
/// Apple's client on macOS 26.6 (25G83) reported them. The Mac never checked them
/// against anything measurable; they are what worked.
const REMOTE_ENDPOINT_INFO: &[u8] = &[
    0x08, 0x00, // f1 = 0
    0x10, 0x01, // f2 = 1
    0x1a, 0x0d, b'V', b'i', b'r', b't', b'u', b'a', b'l', b'M', b'a', b'c', b'2', b',', b'1',
    0x22, 0x08, b'2', b'2', b'1', b'5', b'.', b'5', b'.', b'1',
    0x2a, 0x05, b'2', b'5', b'G', b'8', b'3',
];

/// The media blob's `f6`: the AVConference negotiation library's name and version.
const VICEROY: &str = "Viceroy 1.7.0";

/// The blob's `f13`, which differs between the two modes and was not decoded
/// further. Carried as the numbers the captured offers hold.
const AUDIO_F13: u64 = 17_169_649_764_059_066_368;
const VIDEO_F13: u64 = 17_169_649_764_516_085_760;

/// The blob's repeated `f9` entries — the codec list, with AAC-ELD as `{16, 4100}`,
/// AMR-NB as `{1, 299}` and EVS as `{4, 6500}` — as `(f1, f2, Option<f3>)`. The
/// audio offer lists them in this order; the video offer moves one to the front.
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

/// The tail every media blob ends with: the library name, `f8`, the codec list in
/// the given order, `f13`, `f14 = 2`, `f16 = 0`.
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

/// The audio offer's `avcMediaStreamNegotiatorMediaBlob`, before compression:
/// `VCMediaNegotiationBlobV2` with one audio stream whose SSRC is `ssrc`.
pub fn audio_offer_blob(ssrc: u32) -> Vec<u8> {
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

/// One codec's capability line in the video stream: `{f1 = 1, f2 = which, f3 =
/// 50115, f4 = 0}`, repeated per level.
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

/// The screen-video offer's blob: one video stream at `size` offering HEVC (`123`)
/// and H.264 (`100`). Its picture is never received; it exists because the Mac will
/// not start the audio without it.
pub fn video_offer_blob(ssrc: u32, (width, height): (u16, u16)) -> Vec<u8> {
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
    stream.uint(6, 4);
    stream.uint(7, 1);
    stream.uint(8, 63);
    stream.uint(9, 1);
    stream.uint(12, 1);
    blob.message(5, stream);
    // The same ten entries, with `{0, 40000000, 12288}` moved to the front.
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
/// this order. Hand-written because the format is small and the dictionary is
/// four keys deep: an object table (the dictionary, its keys, its values), an
/// offset table, and a 32-byte trailer.
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
    use std::io::Write as _;
    let mut encoder =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(blob).expect("writing to a Vec cannot fail");
    encoder.finish().expect("finishing a Vec-backed zlib stream cannot fail")
}

/// One AVConference offer plist: the negotiator mode, the compressed media blob, the
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

/// The audio offer (`AVCMediaStreamNegotiator initWithMode:8`), ready to be placed
/// in the `0x1c` message.
pub fn audio_offer(ssrc: u32, call_id: &str) -> Vec<u8> {
    offer(MODE_AUDIO, &audio_offer_blob(ssrc), call_id)
}

/// The screen-video offer (`initWithMode:7`) for a display of `size` backing pixels.
pub fn video_offer(ssrc: u32, size: (u16, u16), call_id: &str) -> Vec<u8> {
    offer(MODE_VIDEO, &video_offer_blob(ssrc, size), call_id)
}

// ---------------------------------------------------------------------------
// The 0x1c message and the replies
// ---------------------------------------------------------------------------

/// `RFBMediaStreamServerConfiguration`, version 3.
///
/// ```text
/// +0x00 u8   0x1c
/// +0x01 u8   pad
/// +0x02 u16  body length (everything after this field)
/// +0x04 u16  version = 3
/// +0x06 u32  flags = 0
/// +0x0a u16  audio offer length
/// +0x0c u16  video1 offer length
/// +0x0e u16  video2 offer length = 0
/// +0x14 16B  session UUID
/// +0x24 46B  audio SRTP master key, viewer -> server
/// +0x52 46B  audio SRTP master key, server -> viewer
/// +0x80      audio offer, then 46B video1 key v->s, 46B video1 key s->v, video1 offer
/// ```
///
/// Keys are `(viewer_to_server, server_to_viewer)`.
pub fn media_stream_configuration(
    session_uuid: &[u8; 16],
    audio_offer: &[u8],
    audio_keys: (&MasterKey, &MasterKey),
    video_offer: &[u8],
    video_keys: (&MasterKey, &MasterKey),
) -> Vec<u8> {
    let mut msg = vec![0u8; 0x80];
    msg[0] = 0x1c;
    msg[4..6].copy_from_slice(&3u16.to_be_bytes());
    msg[0x0a..0x0c].copy_from_slice(&(audio_offer.len() as u16).to_be_bytes());
    msg[0x0c..0x0e].copy_from_slice(&(video_offer.len() as u16).to_be_bytes());
    msg[0x14..0x24].copy_from_slice(session_uuid);
    msg[0x24..0x52].copy_from_slice(audio_keys.0);
    msg[0x52..0x80].copy_from_slice(audio_keys.1);
    msg.extend_from_slice(audio_offer);
    msg.extend_from_slice(video_keys.0);
    msg.extend_from_slice(video_keys.1);
    msg.extend_from_slice(video_offer);
    let body_len = (msg.len() - 4) as u16;
    msg[2..4].copy_from_slice(&body_len.to_be_bytes());
    msg
}

/// What the Mac put in an encoding-1010 rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaReply {
    /// Message 1: the streams are up, and audio arrives at (and RTCP goes to)
    /// `audio_port`; `video_port` is the screen video's, which nothing opens.
    Ports { audio_port: u16, video_port: u16 },
    /// Message 3: the Mac could not start the streams. `kind` 2 with the video offer
    /// missing is the measured "unable to create video config".
    Error { kind: u32, sub_code: u32 },
    /// A message type this client does not know.
    Other(u16),
}

/// Parse the body of an encoding-1010 rectangle (after its `u16` size).
pub fn parse_media_reply(body: &[u8]) -> anyhow::Result<MediaReply> {
    anyhow::ensure!(body.len() >= 8, "a media-stream reply of {} bytes has no header", body.len());
    let kind = u16::from_be_bytes([body[0], body[1]]);
    let version = u16::from_be_bytes([body[2], body[3]]);
    match kind {
        1 => {
            anyhow::ensure!(
                body.len() >= 16,
                "media-stream message 1 (version {version}) is {} bytes, too short for its ports",
                body.len()
            );
            Ok(MediaReply::Ports {
                audio_port: u16::from_be_bytes([body[8], body[9]]),
                video_port: u16::from_be_bytes([body[14], body[15]]),
            })
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
// SRTP and RTP
// ---------------------------------------------------------------------------

/// AES-256 counter mode keystream from `iv`, XORed into `data`. The counter is the
/// whole 128-bit block, big-endian, which agrees with RFC 3711's 16-bit counter for
/// any packet under a megabyte.
fn aes_ctr_xor(cipher: &Aes256, iv: u128, data: &mut [u8]) {
    for (i, chunk) in data.chunks_mut(16).enumerate() {
        let mut block = iv.wrapping_add(i as u128).to_be_bytes();
        cipher.encrypt_block((&mut block).into());
        for (byte, key) in chunk.iter_mut().zip(block) {
            *byte ^= key;
        }
    }
}

/// RFC 3711 §4.3.1 key derivation with `key_derivation_rate = 0`: `n` bytes for
/// `label` from the master key and salt.
fn derive(cipher: &Aes256, master_salt: &[u8; 14], label: u8, n: usize) -> Vec<u8> {
    let mut salt = [0u8; 16];
    salt[2..].copy_from_slice(master_salt);
    let x = u128::from_be_bytes(salt) ^ (u128::from(label) << 48);
    let mut out = vec![0u8; n];
    aes_ctr_xor(cipher, x << 16, &mut out);
    out
}

/// One direction of an SRTP stream: the derived session key and salt, and the
/// rollover counter that turns a 16-bit sequence number into a packet index.
pub struct SrtpSession {
    cipher: Aes256,
    salt: u128,
    roc: u32,
    last_seq: Option<u16>,
}

impl SrtpSession {
    pub fn new(master: &MasterKey) -> Self {
        let master_key: [u8; 32] = master[..32].try_into().expect("46 > 32");
        let master_cipher = Aes256::new(&master_key.into());
        let master_salt: [u8; 14] = master[32..46].try_into().expect("46 - 32 = 14");
        let key: [u8; 32] = derive(&master_cipher, &master_salt, 0, 32).try_into().expect("32 bytes were asked for");
        let salt = derive(&master_cipher, &master_salt, 2, 14);
        let mut salt16 = [0u8; 16];
        salt16[2..].copy_from_slice(&salt);
        Self {
            cipher: Aes256::new(&key.into()),
            salt: u128::from_be_bytes(salt16),
            roc: 0,
            last_seq: None,
        }
    }

    /// Decrypt one packet's payload in place. `seq` is the RTP sequence number; a
    /// wrap from the top of the range to the bottom advances the rollover counter.
    pub fn decrypt(&mut self, ssrc: u32, seq: u16, payload: &mut [u8]) {
        if let Some(last) = self.last_seq
            && seq < 0x1000
            && last > 0xf000
        {
            self.roc += 1;
        }
        self.last_seq = Some(seq);
        let index = (u64::from(self.roc) << 16) | u64::from(seq);
        let iv = (self.salt << 16) ^ (u128::from(ssrc) << 64) ^ (u128::from(index) << 16);
        aes_ctr_xor(&self.cipher, iv, payload);
    }
}

/// An RTP packet's header fields and where its payload starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub payload_type: u8,
    pub marker: bool,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    /// Offset of the payload within the datagram.
    pub payload_at: usize,
}

/// What a datagram on the audio port turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Datagram {
    Rtp(RtpHeader),
    /// An RTCP packet (types 200–207), which the Mac sends once a second.
    Rtcp,
    /// Not RTP version 2, or too short to be.
    Other,
}

/// Classify a datagram and, for RTP, read its header. Padding is left in the
/// payload: under SRTP it is inside the encrypted part, and the decoder is given
/// the decrypted unit as a whole — the Mac sends none.
pub fn parse_datagram(data: &[u8]) -> Datagram {
    if data.len() < 12 || data[0] >> 6 != 2 {
        return Datagram::Other;
    }
    // RTCP's packet type is the whole second byte — 200 for a sender report, 201
    // for a receiver report — where RTP's payload type is its low seven bits. Read
    // as an RTP payload type, an RTCP packet is type 72–79 and would be decrypted
    // and fed to the decoder as a damaged access unit, which is what the probe did
    // once a second (its check masked the byte first) and what its two concealed
    // frames were.
    if (200..=207).contains(&data[1]) {
        return Datagram::Rtcp;
    }
    let payload_type = data[1] & 0x7f;
    let cc = usize::from(data[0] & 0x0f);
    let extension = data[0] & 0x10 != 0;
    let mut payload_at = 12 + 4 * cc;
    if extension {
        if data.len() < payload_at + 4 {
            return Datagram::Other;
        }
        let words = usize::from(u16::from_be_bytes([data[payload_at + 2], data[payload_at + 3]]));
        payload_at += 4 + 4 * words;
    }
    if data.len() < payload_at {
        return Datagram::Other;
    }
    Datagram::Rtp(RtpHeader {
        payload_type,
        marker: data[1] & 0x80 != 0,
        sequence: u16::from_be_bytes([data[2], data[3]]),
        timestamp: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
        ssrc: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
        payload_at,
    })
}

/// A minimal RTCP receiver report — version 2, no report blocks — from `ssrc`. Sent
/// in the clear, which is what the probe did and what the Mac kept streaming to.
pub fn rtcp_receiver_report(ssrc: u32) -> [u8; 8] {
    let mut report = [0x80, 201, 0, 1, 0, 0, 0, 0];
    report[4..].copy_from_slice(&ssrc.to_be_bytes());
    report
}

// ---------------------------------------------------------------------------
// The negotiation, from the read loop's side
// ---------------------------------------------------------------------------

/// One session's media-stream negotiation and, once the Mac names a port, its
/// receiver. Built when the target has audio and the session is High Performance;
/// dropped with the read loop, which ends the receiver and clears the bridge's
/// format.
pub struct MediaStream {
    bridge: Arc<AudioBridge>,
    /// The Mac, as the TCP session reached it: where audio comes from and RTCP goes.
    peer: IpAddr,
    /// This side's address on that connection, which is what the UDP socket binds —
    /// the interface the Mac already reaches.
    local: IpAddr,
    session_uuid: [u8; 16],
    call_id: String,
    audio_keys: (MasterKey, MasterKey),
    video_keys: (MasterKey, MasterKey),
    /// This side's SSRC in the audio offer, and the one its RTCP reports carry.
    viewer_ssrc: u32,
    video_ssrc: u32,
    offered: bool,
    receiver: Option<tokio::task::JoinHandle<()>>,
}

impl MediaStream {
    pub fn new(bridge: Arc<AudioBridge>, peer: SocketAddr, local: SocketAddr) -> Self {
        let mut rng = rand::rng();
        let mut key = || {
            let mut k = [0u8; 46];
            rng.fill_bytes(&mut k);
            k
        };
        let audio_keys = (key(), key());
        let video_keys = (key(), key());
        let uuid = uuid::Uuid::new_v4();
        Self {
            bridge,
            peer: peer.ip(),
            local: local.ip(),
            session_uuid: *uuid.as_bytes(),
            call_id: uuid.hyphenated().to_string().to_ascii_uppercase(),
            audio_keys,
            video_keys,
            viewer_ssrc: rand::random(),
            video_ssrc: rand::random(),
            offered: false,
            receiver: None,
        }
    }

    /// The `0x1c` message to send once the first display layout has arrived, for a
    /// virtual display of `size` backing pixels. `None` after the first call: the
    /// Mac is asked once per session.
    pub fn offer(&mut self, size: (u16, u16)) -> Option<Vec<u8>> {
        if self.offered {
            return None;
        }
        self.offered = true;
        let audio = audio_offer(self.viewer_ssrc, &self.call_id);
        let video = video_offer(self.video_ssrc, size, &self.call_id);
        info!(
            "vnc: asking the Mac for its system audio (offer {} + {} bytes)",
            audio.len(),
            video.len()
        );
        Some(media_stream_configuration(
            &self.session_uuid,
            &audio,
            (&self.audio_keys.0, &self.audio_keys.1),
            &video,
            (&self.video_keys.0, &self.video_keys.1),
        ))
    }

    /// Act on an encoding-1010 rectangle: start receiving on the port it names, or
    /// record the error it reports. Neither ends the desktop session — sound is an
    /// extra on it.
    pub fn on_reply(&mut self, body: &[u8]) -> anyhow::Result<()> {
        match parse_media_reply(body)? {
            MediaReply::Ports { audio_port, video_port } => {
                info!(
                    "vnc: the Mac opened its media streams: audio at UDP {audio_port}, \
                     screen video at {video_port} (unused)"
                );
                if self.receiver.is_some() {
                    debug!("vnc: media streams already running; ignoring a second port message");
                    return Ok(());
                }
                self.start(audio_port)
            }
            MediaReply::Error { kind, sub_code } => {
                warn!(
                    "vnc: the Mac refused the media stream (error type {kind}, sub-code \
                     {sub_code}); the session continues without sound"
                );
                self.bridge.clear_format();
                Ok(())
            }
            MediaReply::Other(kind) => {
                debug!("vnc: ignoring media-stream message type {kind}");
                Ok(())
            }
        }
    }

    #[cfg(feature = "apple-hp-audio")]
    fn start(&mut self, port: u16) -> anyhow::Result<()> {
        let bind = SocketAddr::new(self.local, port);
        let remote = SocketAddr::new(self.peer, port);
        let socket = std::net::UdpSocket::bind(bind)
            .map_err(|e| anyhow::anyhow!("bind UDP {bind} for the Mac's audio: {e}"))?;
        socket.set_nonblocking(true)?;
        let socket = tokio::net::UdpSocket::from_std(socket)?;
        let srtp = SrtpSession::new(&self.audio_keys.1);
        let bridge = Arc::clone(&self.bridge);
        let viewer_ssrc = self.viewer_ssrc;
        self.receiver = Some(tokio::spawn(receive(socket, remote, srtp, bridge, viewer_ssrc)));
        Ok(())
    }

    /// Unreachable in practice — config parse refuses `audio = true` on an
    /// `ard-high-performance` target in a build without the decoder — and stated
    /// rather than assumed.
    #[cfg(not(feature = "apple-hp-audio"))]
    fn start(&mut self, port: u16) -> anyhow::Result<()> {
        anyhow::bail!(
            "the Mac at {} offers its audio at UDP {port} (to {}), but this gateway was built \
             without the apple-hp-audio feature and has no AAC-ELD decoder",
            self.peer,
            self.local
        )
    }
}

impl Drop for MediaStream {
    fn drop(&mut self) {
        if let Some(receiver) = self.receiver.take() {
            receiver.abort();
            self.bridge.clear_format();
        }
    }
}

/// Access units per wave buffer handed to the bridge: two 10 ms units, one Opus
/// packet's worth, so the encoder downstream completes a packet per buffer.
#[cfg(feature = "apple-hp-audio")]
const UNITS_PER_WAVE: usize = 2;

/// How long without a single packet before saying so. The Mac starts streaming
/// within a second of naming the port; a firewall between it and this gateway's
/// UDP port is what silence past this looks like.
#[cfg(feature = "apple-hp-audio")]
const SILENT_START: std::time::Duration = std::time::Duration::from_secs(5);

/// The receiver: RTCP out once a second, SRTP in, AAC-ELD to PCM, PCM to the bridge.
#[cfg(feature = "apple-hp-audio")]
async fn receive(
    socket: tokio::net::UdpSocket,
    remote: SocketAddr,
    mut srtp: SrtpSession,
    bridge: Arc<AudioBridge>,
    viewer_ssrc: u32,
) {
    use crate::aac_eld::{CHANNELS, EldDecoder, FRAME_SAMPLES};

    let mut decoder = match EldDecoder::new() {
        Ok(decoder) => decoder,
        Err(e) => {
            warn!("vnc: no AAC-ELD decoder, so no Mac audio: {e:#}");
            return;
        }
    };
    bridge.publish_format(SOURCE_FORMAT);

    let wave_bytes = UNITS_PER_WAVE * FRAME_SAMPLES * CHANNELS * 2;
    let mut pending: Vec<u8> = Vec::with_capacity(wave_bytes);
    let mut datagram = vec![0u8; 2048];
    let mut rtcp = tokio::time::interval(std::time::Duration::from_secs(1));
    let started = tokio::time::Instant::now();
    let report = rtcp_receiver_report(viewer_ssrc);
    let mut packets: u64 = 0;
    let mut concealed: u64 = 0;
    let mut undecodable: u64 = 0;
    let mut warned_silent = false;
    let mut last_sequence: Option<u16> = None;
    loop {
        tokio::select! {
            _ = rtcp.tick() => {
                if let Err(e) = socket.send_to(&report, remote).await {
                    debug!("vnc: RTCP report to {remote} failed: {e}");
                }
                if packets == 0 && !warned_silent && started.elapsed() >= SILENT_START {
                    warned_silent = true;
                    warn!(
                        "vnc: the Mac named UDP {} for its audio but nothing has arrived in {}s — \
                         it sends to this gateway's address on that port, so a firewall or NAT \
                         between them is what this looks like",
                        remote.port(),
                        SILENT_START.as_secs()
                    );
                }
            }
            received = socket.recv_from(&mut datagram) => {
                let (len, from) = match received {
                    Ok(received) => received,
                    Err(e) => {
                        warn!("vnc: the audio socket failed, ending the Mac's audio: {e}");
                        break;
                    }
                };
                if from.ip() != remote.ip() {
                    debug!("vnc: ignoring a datagram from {from} on the audio port");
                    continue;
                }
                let header = match parse_datagram(&datagram[..len]) {
                    Datagram::Rtp(header) => header,
                    Datagram::Rtcp | Datagram::Other => continue,
                };
                let payload = &mut datagram[header.payload_at..len];
                if payload.len() <= AUTH_TAG_LEN {
                    continue;
                }
                let unit_len = payload.len() - AUTH_TAG_LEN;
                let unit = &mut payload[..unit_len];
                srtp.decrypt(header.ssrc, header.sequence, unit);
                packets += 1;
                if packets == 1 {
                    info!(
                        "vnc: Mac audio is flowing: RTP payload type {}, {} bytes per unit, \
                         SSRC {:#x}",
                        header.payload_type, unit_len, header.ssrc
                    );
                }
                if let Some(last) = last_sequence
                    && header.sequence != last.wrapping_add(1)
                {
                    debug!(
                        "vnc: audio sequence jumped from {last} to {} — a lost or reordered packet",
                        header.sequence
                    );
                }
                last_sequence = Some(header.sequence);
                match decoder.decode(unit, &mut pending) {
                    Ok(false) => {}
                    Ok(true) => concealed += 1,
                    Err(e) => {
                        undecodable += 1;
                        if undecodable <= 3 {
                            warn!("vnc: dropped an audio unit: {e:#}");
                        }
                    }
                }
                if packets.is_multiple_of(1000) {
                    debug!(
                        "vnc: {packets} audio packets, {concealed} concealed, {undecodable} \
                         undecodable, in {} ms",
                        started.elapsed().as_millis()
                    );
                }
                if pending.len() >= wave_bytes {
                    bridge.wave(std::mem::take(&mut pending));
                    pending.reserve(wave_bytes);
                }
            }
        }
    }
    bridge.clear_format();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The media blob Apple's own `AVCMediaStreamNegotiator initWithMode:8` produced
    /// on macOS 26.6 for SSRC 3606155525, byte for byte (`tmp/avc-audio-offer.plist`,
    /// inflated). Every field, its order and its encoding — zero-valued fields
    /// written out, the 64-bit `f13` — is pinned by this.
    const CAPTURED_AUDIO_BLOB: &str = "080110011a120885a2c6b70d1000180020ffbc0128003000320d56696365726f7920312e372e3040004a05080110ab024a0908ea1f1000188080014a0b080010c0d1e123188080204a0b08001080dac409188080064a0a08001080b489131880604a0b080010808ece1c188080104a0508101084204a0b08001080c2d72f188080404a05080410e4324a0b080010809bee02188080086880e0f084deafb6a3ee017002800100";

    /// And the `initWithMode:7` blob for SSRC 3023179925 at 1600×1000.
    const CAPTURED_VIDEO_BLOB: &str = "080110012af5010895a1c8a10b10001a7d087b120a0801100118c387032000120a0801100218c387032000120a0801100118c387032000120a0801100218c3870320001a47464c533b4d533a2d313b4c463a2d313b4c54523b43414241433b504f533a303b454f443a313b4854533a323b52523a333b41523a342f332c352f383b58523a342f332c352f383b20011a5c0864120a0801100118c387032000120a0801100218c3870320001a3e464c533b4c463a2d313b504f533a353b454f443a313b4854533a323b52523a333b504f53453a343b41523a342f332c352f383b58523a342f332c352f383b200e20c00c28e80730043801403f48016001320d56696365726f7920312e372e3040004a0a08001080b489131880604a05080110ab024a0908ea1f1000188080014a0b080010c0d1e123188080204a0b08001080dac409188080064a0b080010808ece1c188080104a0508101084204a0b08001080c2d72f188080404a05080410e4324a0b080010809bee0218808008688080e7dedfafb6a3ee017002800100";

    #[test]
    fn the_audio_blob_is_what_apples_negotiator_produces() {
        assert_eq!(audio_offer_blob(3_606_155_525), unhex(CAPTURED_AUDIO_BLOB));
    }

    #[test]
    fn the_video_blob_is_what_apples_negotiator_produces() {
        assert_eq!(video_offer_blob(3_023_179_925, (1600, 1000)), unhex(CAPTURED_VIDEO_BLOB));
    }

    /// The plist around the blob: header, the dictionary's shape, and a trailer whose
    /// offsets land on the objects they name. The captured file's dictionary marker,
    /// key order and trailer layout are reproduced exactly.
    #[test]
    fn the_offer_is_a_binary_plist_of_four_entries() {
        let plist = audio_offer(1, "910BCF8F-D1D7-4EB6-B728-E1FDB02DD3B6");
        assert_eq!(&plist[..8], b"bplist00");
        assert_eq!(&plist[8..17], &[0xd4, 1, 2, 3, 4, 5, 6, 7, 8]);
        let trailer = &plist[plist.len() - 32..];
        assert_eq!(&trailer[..8], &[0, 0, 0, 0, 0, 0, 2, 1]);
        assert_eq!(u64::from_be_bytes(trailer[8..16].try_into().unwrap()), 9);
        assert_eq!(u64::from_be_bytes(trailer[16..24].try_into().unwrap()), 0);
        let table = u64::from_be_bytes(trailer[24..32].try_into().unwrap()) as usize;
        let offsets: Vec<usize> = (0..9)
            .map(|i| usize::from(u16::from_be_bytes([plist[table + 2 * i], plist[table + 2 * i + 1]])))
            .collect();
        assert_eq!(offsets[0], 8, "the dictionary is the first object");
        // Keys are ASCII strings with long-form lengths; values are data, int, data, string.
        for (i, key) in ["avcMediaStreamOptionRemoteEndpointInfo", "avcMediaStreamNegotiatorMode", "avcMediaStreamNegotiatorMediaBlob", "avcMediaStreamOptionCallID"].iter().enumerate() {
            let at = offsets[1 + i];
            assert_eq!(&plist[at..at + 3], &[0x5f, 0x10, key.len() as u8]);
            assert_eq!(&plist[at + 3..at + 3 + key.len()], key.as_bytes());
        }
        assert_eq!(&plist[offsets[5]..offsets[5] + 3], &[0x4f, 0x10, REMOTE_ENDPOINT_INFO.len() as u8]);
        assert_eq!(&plist[offsets[6]..offsets[6] + 2], &[0x10, MODE_AUDIO]);
        assert_eq!(plist[offsets[7]], 0x4f, "the blob is data");
        assert_eq!(&plist[offsets[8]..offsets[8] + 3], &[0x5f, 0x10, 36]);
        assert_eq!(table, plist.len() - 32 - 18, "nine two-byte offsets before the trailer");
    }

    /// The compressed blob inflates back to the bytes it was made from.
    #[test]
    fn the_blob_survives_its_compression() {
        use std::io::Read as _;
        let blob = audio_offer_blob(7);
        let mut inflated = Vec::new();
        flate2::read::ZlibDecoder::new(deflate(&blob).as_slice()).read_to_end(&mut inflated).unwrap();
        assert_eq!(inflated, blob);
    }

    /// The `0x1c` layout the probe sent and the Mac answered.
    #[test]
    fn the_configuration_message_lays_out_as_measured() {
        let uuid = [0x11; 16];
        let a = ([0xa1; 46], [0xa2; 46]);
        let v = ([0xb1; 46], [0xb2; 46]);
        let audio = vec![0xaa; 300];
        let video = vec![0xbb; 400];
        let msg = media_stream_configuration(&uuid, &audio, (&a.0, &a.1), &video, (&v.0, &v.1));
        assert_eq!(msg[0], 0x1c);
        assert_eq!(msg[1], 0);
        assert_eq!(u16::from_be_bytes([msg[2], msg[3]]) as usize, msg.len() - 4);
        assert_eq!(&msg[4..6], &[0, 3]);
        assert_eq!(&msg[6..10], &[0; 4]);
        assert_eq!(&msg[0x0a..0x0c], &300u16.to_be_bytes());
        assert_eq!(&msg[0x0c..0x0e], &400u16.to_be_bytes());
        assert_eq!(&msg[0x0e..0x10], &[0, 0]);
        assert_eq!(&msg[0x14..0x24], &uuid);
        assert_eq!(&msg[0x24..0x52], &a.0);
        assert_eq!(&msg[0x52..0x80], &a.1);
        assert_eq!(&msg[0x80..0x80 + 300], audio.as_slice());
        let after_audio = 0x80 + 300;
        assert_eq!(&msg[after_audio..after_audio + 46], &v.0);
        assert_eq!(&msg[after_audio + 46..after_audio + 92], &v.1);
        assert_eq!(&msg[after_audio + 92..], video.as_slice());
    }

    #[test]
    fn the_replies_parse_as_measured() {
        // Message 1: type 1, version, flags, audio port, audio flags, video port, flags.
        let mut ports = vec![0, 1, 0, 1, 0, 0, 0, 0];
        ports.extend_from_slice(&50_004u16.to_be_bytes());
        ports.extend_from_slice(&[0; 4]);
        ports.extend_from_slice(&50_005u16.to_be_bytes());
        ports.extend_from_slice(&[0; 4]);
        assert_eq!(
            parse_media_reply(&ports).unwrap(),
            MediaReply::Ports { audio_port: 50_004, video_port: 50_005 }
        );
        let mut error = vec![0, 3, 0, 1, 0, 0, 0, 0];
        error.extend_from_slice(&2u32.to_be_bytes());
        error.extend_from_slice(&7u32.to_be_bytes());
        assert_eq!(parse_media_reply(&error).unwrap(), MediaReply::Error { kind: 2, sub_code: 7 });
        assert_eq!(parse_media_reply(&[0, 9, 0, 1, 0, 0, 0, 0]).unwrap(), MediaReply::Other(9));
        assert!(parse_media_reply(&[0, 1, 0, 1]).is_err(), "a truncated reply is refused");
        assert!(parse_media_reply(&[0, 1, 0, 1, 0, 0, 0, 0, 0]).is_err());
    }

    /// The probe's own SRTP arithmetic, as a fixture: with a known master key, the
    /// keystream for one packet is what RFC 3711's derivation says. The values were
    /// produced by the Python probe's `Srtp` class (`cryptography`'s AES-CTR) for
    /// this master, SSRC and sequence — an independent implementation of the same
    /// spec, which is the check that this one reads the spec the same way.
    #[test]
    fn srtp_decrypts_the_way_the_probe_did() {
        let master: MasterKey = std::array::from_fn(|i| i as u8);
        let mut srtp = SrtpSession::new(&master);
        let mut payload = [0u8; 32];
        srtp.decrypt(0x1234_5678, 1, &mut payload);
        assert_eq!(
            payload.to_vec(),
            unhex("1258651e2fd449f1c6ac45d232b31a51df148a57d75fd598890257e7343bf9bb")
        );
    }

    /// The rollover counter advances on a wrap and not on an ordinary step, which is
    /// what keeps the packet index — and so the keystream — right past 65 536
    /// packets (eleven minutes of 10 ms units).
    #[test]
    fn the_rollover_counter_follows_the_sequence_wrap() {
        let master: MasterKey = [9; 46];
        let mut srtp = SrtpSession::new(&master);
        srtp.decrypt(1, 0xfff0, &mut []);
        assert_eq!(srtp.roc, 0);
        srtp.decrypt(1, 0xffff, &mut []);
        assert_eq!(srtp.roc, 0);
        srtp.decrypt(1, 0x0000, &mut []);
        assert_eq!(srtp.roc, 1);
        srtp.decrypt(1, 0x0001, &mut []);
        assert_eq!(srtp.roc, 1);
    }

    #[test]
    fn datagrams_are_classified_and_rtp_headers_read() {
        let mut rtp = vec![0x80, 0x80 | 101];
        rtp.extend_from_slice(&7u16.to_be_bytes());
        rtp.extend_from_slice(&960u32.to_be_bytes());
        rtp.extend_from_slice(&0xdead_beefu32.to_be_bytes());
        rtp.extend_from_slice(&[1, 2, 3]);
        assert_eq!(
            parse_datagram(&rtp),
            Datagram::Rtp(RtpHeader {
                payload_type: 101,
                marker: true,
                sequence: 7,
                timestamp: 960,
                ssrc: 0xdead_beef,
                payload_at: 12,
            })
        );
        // One CSRC and a one-word header extension push the payload out.
        let mut extended = rtp.clone();
        extended[0] = 0x80 | 0x10 | 0x01;
        extended.truncate(12);
        extended.extend_from_slice(&[0; 4]); // CSRC
        extended.extend_from_slice(&[0xbe, 0xde, 0, 1, 0, 0, 0, 0]); // extension, 1 word
        extended.push(0xff);
        match parse_datagram(&extended) {
            Datagram::Rtp(header) => assert_eq!(header.payload_at, 24),
            other => panic!("{other:?}"),
        }
        let mut rtcp = rtp.clone();
        rtcp[1] = 200;
        assert_eq!(parse_datagram(&rtcp), Datagram::Rtcp);
        assert_eq!(parse_datagram(&[0x80; 5]), Datagram::Other);
        assert_eq!(parse_datagram(&[0x40; 20]), Datagram::Other, "RTP version 1 is not RTP");
    }

    #[test]
    fn the_receiver_report_is_eight_bytes_of_version_two() {
        assert_eq!(rtcp_receiver_report(0x0102_0304), [0x80, 201, 0, 1, 1, 2, 3, 4]);
    }

    #[test]
    fn the_media_encoding_rides_last_on_the_zlib_list() {
        let encodings = encodings_with_media_stream();
        assert_eq!(encodings.last(), Some(&ENCODING_MEDIA_STREAM));
        assert_eq!(&encodings[..encodings.len() - 1], vnc_apple::ENCODINGS_WITH_ZLIB);
        assert!(!vnc_apple::ENCODINGS.contains(&ENCODING_MEDIA_STREAM));
    }

    /// The offer is sent once per session, and the message it produces is the
    /// configuration for the keys the stream will then decrypt with.
    #[test]
    fn a_media_stream_offers_once() {
        let bridge = Arc::new(AudioBridge::new());
        let peer: SocketAddr = "10.0.0.2:5900".parse().unwrap();
        let local: SocketAddr = "10.0.0.1:50000".parse().unwrap();
        let mut media = MediaStream::new(bridge, peer, local);
        let msg = media.offer((1600, 1000)).expect("the first layout gets an offer");
        assert_eq!(msg[0], 0x1c);
        assert_eq!(&msg[0x24..0x52], &media.audio_keys.0);
        assert_eq!(&msg[0x52..0x80], &media.audio_keys.1);
        assert!(media.offer((1600, 1000)).is_none(), "a later layout does not re-offer");
        assert_eq!(media.call_id.len(), 36);
        assert!(media.call_id.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-'));
    }

    /// An error reply is logged and leaves the session running.
    #[test]
    fn an_error_reply_is_not_fatal() {
        let bridge = Arc::new(AudioBridge::new());
        let peer: SocketAddr = "10.0.0.2:5900".parse().unwrap();
        let local: SocketAddr = "10.0.0.1:50000".parse().unwrap();
        let mut media = MediaStream::new(Arc::clone(&bridge), peer, local);
        let mut error = vec![0, 3, 0, 1, 0, 0, 0, 0];
        error.extend_from_slice(&2u32.to_be_bytes());
        error.extend_from_slice(&0u32.to_be_bytes());
        media.on_reply(&error).unwrap();
        assert!(media.receiver.is_none());
        assert_eq!(bridge.negotiated_format(), None);
    }
}
