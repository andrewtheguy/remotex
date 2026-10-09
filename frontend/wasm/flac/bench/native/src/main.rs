//! `flac-bench make DIR NAME...` writes each named sample as `NAME.frames`,
//! the FLAC frames `sound-flac` makes of a generated signal, each behind its
//! length as four little-endian bytes, and `NAME.pcm`, the signal itself as
//! interleaved little-endian 16-bit samples. `flac-bench decode FILE.frames
//! PASSES` decodes a sample with libFLAC, through `sound-flac` as wlshare's
//! and the gateway's own readers would, and prints the frames it decoded.
//!
//! A sample is a minute of sound, and most are sound nobody recorded: sound,
//! unlike a picture, is as good generated as captured, and a generated one is
//! the same on every machine. A name is a signal and a stream, `music-48000`
//! or `music-44100`, wlshare's 48 kHz in blocks of 960 and an RDP host's
//! 44.1 kHz in blocks of 882, both in stereo. A name whose signal is none of
//! the generated ones is a recording: its `NAME.pcm` is already in DIR, put
//! there by `bench/recorded.sh`, and only its frames are made.

use std::f64::consts::TAU;
use std::fs;
use std::path::Path;

use sound_flac::{Decoder, Encoder, Stream};

const SECONDS: usize = 60;

/// xorshift64*, so a sample is the same bytes wherever it is made.
struct Noise(u64);

impl Noise {
    /// Uniform in -1 to 1.
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let bits = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        bits as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// One channel's low-passed noise: a one-pole filter over white noise.
struct Coloured {
    noise: Noise,
    held: f64,
}

impl Coloured {
    fn next(&mut self, pole: f64) -> f64 {
        self.held = pole * self.held + (1.0 - pole) * self.noise.next();
        self.held
    }
}

/// A signal: its two channels at frame `n`, each in -1 to 1.
type Signal = Box<dyn FnMut(usize) -> (f64, f64)>;

/// The generated signal of a name, if it is one.
fn signal(name: &str, rate: f64) -> Option<Signal> {
    Some(match name {
        // What a desktop plays most of the time: nothing.
        "silence" => Box::new(|_| (0.0, 0.0)),
        // A notification's tone: one sine, the same in both channels.
        "tone" => Box::new(move |n| {
            let v = 0.5 * (TAU * 440.0 * n as f64 / rate).sin();
            (v, v)
        }),
        // Something like music: a chord of notes with overtones that changes
        // every half second, each note struck and decaying, panned apart, over
        // a floor of quiet coloured noise as a recording's room has.
        "music" => {
            let mut left = Coloured { noise: Noise(0x9E37_79B9_7F4A_7C15), held: 0.0 };
            let mut right = Coloured { noise: Noise(0xD1B5_4A32_D192_ED03), held: 0.0 };
            let chords: [[f64; 4]; 4] = [
                [130.81, 164.81, 196.00, 261.63],
                [110.00, 130.81, 164.81, 220.00],
                [87.31, 130.81, 174.61, 220.00],
                [98.00, 146.83, 196.00, 246.94],
            ];
            Box::new(move |n| {
                let t = n as f64 / rate;
                let chord = chords[(t * 2.0) as usize % 4];
                let struck = (t * 2.0).fract() / 2.0;
                let envelope = (-3.0 * struck).exp() * (1.0 - (-400.0 * struck).exp());
                let (mut l, mut r) = (0.0, 0.0);
                for (i, note) in chord.iter().enumerate() {
                    let mut v = 0.0;
                    for overtone in 1..=8 {
                        let h = f64::from(overtone);
                        v += (TAU * note * h * t).sin() / (h * h.sqrt());
                    }
                    let pan = (i as f64 + 0.5) / 4.0;
                    l += v * (1.0 - pan);
                    r += v * pan;
                }
                (0.22 * envelope * l + 0.004 * left.next(0.7), 0.22 * envelope * r + 0.004 * right.next(0.7))
            })
        }
        // A voice, a game, a crowd: loud coloured noise, different in each
        // channel, which no predictor follows far.
        "noise" => {
            let mut left = Coloured { noise: Noise(0x1234_5678_9ABC_DEF1), held: 0.0 };
            let mut right = Coloured { noise: Noise(0x0FED_CBA9_8765_4321), held: 0.0 };
            Box::new(move |_| (0.9 * left.next(0.9), 0.9 * right.next(0.9)))
        }
        // The most a frame can hold: full-scale white noise, which is stored
        // as it is.
        "white" => {
            let mut left = Noise(0xA5A5_5A5A_1234_4321);
            let mut right = Noise(0x5A5A_A5A5_4321_1234);
            Box::new(move |_| (left.next(), right.next()))
        }
        _ => return None,
    })
}

fn stream(name: &str) -> (Stream, &str) {
    let (signal, rate) = name.rsplit_once('-').unwrap_or_else(|| panic!("{name} is not SIGNAL-RATE"));
    let (rate, block) = match rate {
        "48000" => (48_000, 960),
        "44100" => (44_100, 882),
        other => panic!("no stream at {other} Hz: 48000 or 44100"),
    };
    (Stream { rate, channels: 2, bits: 16, block }, signal)
}

/// A file's frames, each behind its length.
fn frames(bytes: &[u8]) -> Vec<&[u8]> {
    let mut frames = Vec::new();
    let mut rest = bytes;
    while let Some((len, after)) = rest.split_first_chunk::<4>() {
        let (frame, after) = after.split_at(u32::from_le_bytes(*len) as usize);
        frames.push(frame);
        rest = after;
    }
    frames
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("make") => {
            let dir = Path::new(&args[1]);
            for name in &args[2..] {
                let (stream, kind) = stream(name);
                let pcm_path = dir.join(format!("{name}.pcm"));
                let pcm = match signal(kind, f64::from(stream.rate)) {
                    Some(mut signal) => {
                        let mut pcm = Vec::new();
                        for n in 0..SECONDS * stream.rate as usize {
                            let (l, r) = signal(n);
                            for v in [l, r] {
                                let sample = (v * 32_767.0).round().clamp(-32_768.0, 32_767.0) as i16;
                                pcm.extend_from_slice(&sample.to_le_bytes());
                            }
                        }
                        fs::write(&pcm_path, &pcm).expect("the signal written");
                        pcm
                    }
                    None => fs::read(&pcm_path)
                        .unwrap_or_else(|e| panic!("{kind} is no generated signal, and its recording is not read: {e}")),
                };
                let mut encoder = Encoder::new(stream).expect("an encoder");
                let (mut out, mut frame, mut block) = (Vec::new(), Vec::new(), Vec::new());
                // Whole blocks: a recording's last part of one is left out.
                for samples in pcm.chunks_exact(2 * stream.samples()) {
                    block.clear();
                    block.extend(samples.as_chunks::<2>().0.iter().map(|&s| i32::from(i16::from_le_bytes(s))));
                    frame.clear();
                    encoder.encode(&block, &mut frame).expect("a frame");
                    out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
                    out.extend_from_slice(&frame);
                }
                fs::write(dir.join(format!("{name}.frames")), &out).expect("the frames written");
            }
        }
        Some("decode") => {
            let path = Path::new(&args[1]);
            let passes: usize = args[2].parse().expect("a count of passes");
            let name = path.file_stem().and_then(|s| s.to_str()).expect("a sample's name");
            let bytes = fs::read(path).expect("the frames read");
            let frames = frames(&bytes);
            let mut decoder = Decoder::new(stream(name).0).expect("a decoder");
            let (mut out, mut sum) = (Vec::new(), 0i64);
            for _ in 0..passes {
                for frame in &frames {
                    decoder.decode(frame, &mut out).expect("a frame decoded");
                    sum += i64::from(out[out.len() / 2]);
                }
            }
            println!("{} frames (sum {sum})", passes * frames.len());
        }
        _ => panic!("usage: flac-bench make DIR NAME... | flac-bench decode FILE.frames PASSES"),
    }
}
