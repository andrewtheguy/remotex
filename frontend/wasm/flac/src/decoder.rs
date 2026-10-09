//! The page's decoder for a session's lossless sound: one FLAC frame in, its
//! samples out.
//!
//! A session started with lossless sound is sent FLAC instead of Opus:
//! wlshare's own frames passed as they came, or an RDP host's PCM coded by the
//! gateway with libFLAC. Either way a packet on the audio socket is one FLAC
//! frame, a stream of its own one block long (`sound-flac` makes them so), and
//! no browser's WebCodecs is asked to read that: this decodes it.
//!
//! It is here and nowhere else. The gateway's FLAC is libFLAC, native code
//! linked into it, which a page cannot have, and the gateway never decodes a frame it
//! sends on. So the decoder is this module's alone and written here, every
//! part of the format a 16-bit frame of one or two channels may use: tested
//! below against frames put together bit by bit, through the binding against
//! frames libFLAC made (`frontend/src/flacDecoder.test.ts`), and measured
//! against libFLAC by `bench/run.sh`.
//!
//! No stream header is sent. The `audioFormat` message names the rate, the
//! channels and the frames in a packet, and the samples are 16 bits, the one
//! width either source has. A frame states all four itself, so [`Decoder`] holds
//! each frame to what was announced rather than building a header to read it
//! behind: one of any other shape is refused, and costs its own samples and
//! nothing after it.
//!
//! What a frame costs is its residual, a Rice code a sample, and its
//! prediction, a sum over the samples before each one. So the bits are read
//! through a 64-bit window that is taken once for two samples ([`Bits`]), the
//! prediction is a loop of its own for each order an encoder commonly picks,
//! in 32-bit arithmetic wherever the frame's own widths say that is exact, and
//! the frame's checksum is taken eight bytes at a step.

use std::fmt;

/// What `audioFormat` announced: the shape every frame of the stream has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stream {
    /// Samples per second per channel: 48 000 for wlshare's, 44 100 for an RDP
    /// host's.
    pub rate: u32,
    /// 1 or 2.
    pub channels: u8,
    /// The frames of samples in every FLAC frame.
    pub block: u16,
}

/// What is wrong inside a frame that is the stream's by its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    HeaderChecksum,
    FrameChecksum,
    /// A value the format keeps for later, or forbids.
    Reserved,
    /// The frame ends before its samples do.
    Short,
    /// A residual cut into partitions the block or the predictor does not fit.
    Partition,
    /// A predictor of more samples than the block has, or a subframe with no
    /// bits left to a sample.
    Predictor,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::HeaderChecksum => "its header's checksum is not its header's",
            Self::FrameChecksum => "its checksum is not its bytes'",
            Self::Reserved => "a reserved value",
            Self::Short => "it ends before its samples do",
            Self::Partition => "a residual in partitions that do not fit its block",
            Self::Predictor => "a predictor that does not fit its block",
        })
    }
}

impl std::error::Error for Fault {}

/// Why a frame was not decoded.
#[derive(Debug)]
pub enum Error {
    Unsupported(Stream),
    NotAFrame(usize),
    Header,
    Decode(Fault),
    Shape { frames: u32, channels: u32, want: Stream },
    Trailing(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(stream) => {
                write!(f, "{stream:?} is not a stream carried here: 44.1 or 48 kHz, 1 or 2 channels")
            }
            Self::NotAFrame(len) => write!(f, "{len} bytes are not a FLAC frame"),
            Self::Header => {
                write!(f, "a FLAC frame whose header states another rate or sample width than the stream's")
            }
            Self::Decode(_) => write!(f, "decoding a FLAC frame"),
            Self::Shape { frames, channels, want } => {
                write!(f, "a FLAC frame of {frames} frames of {channels} channels, where {want:?} was announced")
            }
            Self::Trailing(len) => write!(f, "{len} bytes after the FLAC frame"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(e) => Some(e),
            _ => None,
        }
    }
}

/// The code a frame header states `rate` with, in the low four bits of its third
/// byte.
fn rate_code(rate: u32) -> Option<u8> {
    match rate {
        44_100 => Some(0b1001),
        48_000 => Some(0b1010),
        _ => None,
    }
}

/// The bits of a sample: the one width either source has.
const BITS: u32 = 16;

/// The header's checksum: CRC-8, polynomial x^8 + x^2 + x + 1.
const CRC8: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u8;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc << 1) ^ if crc & 0x80 != 0 { 0x07 } else { 0 };
            bit += 1;
        }
        table[byte] = crc;
        byte += 1;
    }
    table
};

/// The frame's checksum, CRC-16 with polynomial x^16 + x^15 + x^2 + 1, as eight
/// tables: `CRC16[n][b]` is what byte `b` comes to once `n` more bytes have
/// followed it, so eight bytes are one step and not eight.
const CRC16: [[u16; 256]; 8] = {
    let mut tables = [[0u16; 256]; 8];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = (byte as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc << 1) ^ if crc & 0x8000 != 0 { 0x8005 } else { 0 };
            bit += 1;
        }
        tables[0][byte] = crc;
        byte += 1;
    }
    let mut n = 1;
    while n < 8 {
        let mut byte = 0;
        while byte < 256 {
            let before = tables[n - 1][byte];
            tables[n][byte] = (before << 8) ^ tables[0][(before >> 8) as usize];
            byte += 1;
        }
        n += 1;
    }
    tables
};

fn crc8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |crc, &byte| CRC8[usize::from(crc ^ byte)])
}

fn crc16(bytes: &[u8]) -> u16 {
    let (words, rest) = bytes.as_chunks::<8>();
    let crc = words.iter().fold(0u16, |crc, word| {
        CRC16[7][usize::from(word[0] ^ (crc >> 8) as u8)]
            ^ CRC16[6][usize::from(word[1] ^ crc as u8)]
            ^ CRC16[5][usize::from(word[2])]
            ^ CRC16[4][usize::from(word[3])]
            ^ CRC16[3][usize::from(word[4])]
            ^ CRC16[2][usize::from(word[5])]
            ^ CRC16[1][usize::from(word[6])]
            ^ CRC16[0][usize::from(word[7])]
    });
    rest.iter().fold(crc, |crc, &byte| (crc << 8) ^ CRC16[0][usize::from((crc >> 8) as u8 ^ byte)])
}

/// A frame's bits, first bit first.
///
/// The frame is read as 64-bit words, each eight of its bytes with the first
/// uppermost, which [`Decoder::decode`] lays out once for the frame: a
/// WebAssembly module has no instruction that turns a word's bytes round, so
/// loading them in that order wherever the reader stands would cost each load
/// some twenty. The words end in zeros, and past them every bit reads as zero,
/// so nothing here fails on a frame cut short: the caller asks whether
/// [`Bits::at`] has passed the frame's end.
struct Bits<'a> {
    words: &'a [u64],
    /// The bits read so far.
    at: usize,
    /// The bits the frame has.
    end: usize,
}

impl Bits<'_> {
    /// The next 64 bits, the first uppermost.
    #[inline(always)]
    fn window(&self) -> u64 {
        let (first, bit) = (self.at >> 6, self.at & 63);
        let word = |at: usize| self.words.get(at).copied().unwrap_or(0);
        // In two steps, so that from a word's first bit nothing of the next
        // is taken.
        word(first) << bit | word(first + 1) >> 1 >> (63 - bit)
    }

    fn short(&self) -> bool {
        self.at > self.end
    }

    /// The next `n` bits, 32 at most.
    #[inline(always)]
    fn read(&mut self, n: u32) -> u32 {
        // In two steps, so that no bits at all is a shift of the whole window.
        let value = (self.window() >> 1 >> (63 - n)) as u32;
        self.at += n as usize;
        value
    }

    /// The next `n` bits as a two's complement number.
    #[inline(always)]
    fn signed(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        (self.read(n) << (32 - n)) as i32 >> (32 - n)
    }

    /// The zeros before the next one, which is read too.
    fn unary(&mut self) -> Result<u32, Fault> {
        let mut zeros = 0u32;
        loop {
            let run = self.window().leading_zeros();
            if run < 64 {
                self.at += run as usize + 1;
                return Ok(zeros + run);
            }
            // A frame cut short reads as zeros for ever, so the run ends where
            // the frame does.
            zeros = zeros.wrapping_add(64);
            self.at += 64;
            if self.short() {
                return Err(Fault::Short);
            }
        }
    }

    /// One Rice code where the window does not hold it whole.
    #[cold]
    #[inline(never)]
    fn rice_slowly(&mut self, parameter: u32) -> Result<u32, Fault> {
        let quotient = self.unary()?;
        Ok(quotient.wrapping_shl(parameter) | self.read(parameter))
    }

    /// A partition's residuals, each a Rice code of `parameter` low bits: a run
    /// of zeros that counts the rest, a one, the low bits, and the sign folded
    /// into the lowest. A window is taken once for a few of them and they are
    /// read out of it in registers, each waiting on the one before for no more
    /// than a count of zeros and a shift; the code the window does not hold
    /// whole is read apart.
    fn rice(&mut self, parameter: u32, residuals: &mut [i32]) -> Result<(), Fault> {
        // As many codes to a window as it mostly holds whole: a code is its
        // parameter's bits, the one, and a run that is seldom long.
        let together = (52 / (parameter as usize + 3)).clamp(2, 8);
        for group in residuals.chunks_mut(together) {
            let mut window = self.window();
            let mut have = 64;
            for residual in group {
                let zeros = window.leading_zeros();
                let length = zeros + 1 + parameter;
                // Fewer than the window has, so that the shift past the code
                // is one shift, of less than the window's width.
                let folded = if length < have {
                    let low = window << zeros << 1 >> 1 >> (63 - parameter);
                    window <<= length;
                    have -= length;
                    zeros << parameter | low as u32
                } else {
                    self.at += (64 - have) as usize;
                    let folded = self.rice_slowly(parameter)?;
                    (window, have) = (self.window(), 64);
                    folded
                };
                *residual = (folded >> 1) as i32 ^ -((folded & 1) as i32);
            }
            self.at += (64 - have) as usize;
        }
        Ok(())
    }
}

/// Each sample from `ORDER` on becomes itself, a residual, plus the prediction
/// from the `ORDER` before it, `coefficients[n]` being what the sample `n + 1`
/// back is weighed by. A function for each order, so that the sum is laid out
/// whole. In 32 bits, wrapping: the
/// caller knows the sum fits, or the stream is not one an encoder made.
fn predict<const ORDER: usize>(samples: &mut [i32], coefficients: &[i32], shift: u32) {
    let Some(coefficients) = coefficients.first_chunk::<ORDER>() else { return };
    if samples.len() < ORDER {
        return;
    }
    // The sample before, which the next waits on: kept out of memory.
    let mut last = samples[ORDER - 1];
    for at in ORDER..samples.len() {
        let (before, rest) = samples.split_at_mut(at);
        let Some(before) = before.last_chunk::<ORDER>() else { return };
        let mut sum = coefficients[0].wrapping_mul(last);
        for n in 1..ORDER {
            sum = sum.wrapping_add(coefficients[n].wrapping_mul(before[ORDER - 1 - n]));
        }
        last = rest[0].wrapping_add(sum >> shift);
        rest[0] = last;
    }
}

/// [`predict`] for an order with no function of its own.
fn predict_any(samples: &mut [i32], coefficients: &[i32], shift: u32) {
    let order = coefficients.len();
    for at in order..samples.len() {
        let (before, rest) = samples.split_at_mut(at);
        let sum = coefficients
            .iter()
            .zip(before.iter().rev())
            .fold(0i32, |sum, (&coefficient, &sample)| sum.wrapping_add(coefficient.wrapping_mul(sample)));
        rest[0] = rest[0].wrapping_add(sum >> shift);
    }
}

/// [`predict`] where the sum may pass 32 bits.
#[cold]
#[inline(never)]
fn predict_wide(samples: &mut [i32], coefficients: &[i32], shift: u32) {
    let order = coefficients.len();
    for at in order..samples.len() {
        let (before, rest) = samples.split_at_mut(at);
        let sum = coefficients
            .iter()
            .zip(before.iter().rev())
            .fold(0i64, |sum, (&coefficient, &sample)| sum.wrapping_add(i64::from(coefficient) * i64::from(sample)));
        rest[0] = rest[0].wrapping_add((sum >> shift) as i32);
    }
}

fn predict_narrow(samples: &mut [i32], coefficients: &[i32], shift: u32) {
    match coefficients.len() {
        0 => {}
        1 => predict::<1>(samples, coefficients, shift),
        2 => predict::<2>(samples, coefficients, shift),
        3 => predict::<3>(samples, coefficients, shift),
        4 => predict::<4>(samples, coefficients, shift),
        5 => predict::<5>(samples, coefficients, shift),
        6 => predict::<6>(samples, coefficients, shift),
        7 => predict::<7>(samples, coefficients, shift),
        8 => predict::<8>(samples, coefficients, shift),
        9 => predict::<9>(samples, coefficients, shift),
        10 => predict::<10>(samples, coefficients, shift),
        11 => predict::<11>(samples, coefficients, shift),
        12 => predict::<12>(samples, coefficients, shift),
        _ => predict_any(samples, coefficients, shift),
    }
}

/// The fixed predictors, each a polynomial through the samples before.
const FIXED: [&[i32]; 5] = [&[], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]];

/// The residuals of a subframe whose first `order` samples are stored: into
/// `samples[order..]`.
fn residual(bits: &mut Bits, order: usize, samples: &mut [i32]) -> Result<(), Fault> {
    // The width of a partition's Rice parameter, four bits or five, and what
    // says its residuals are stored plain instead: every bit set.
    let width = match bits.read(2) {
        0 => 4,
        1 => 5,
        _ => return Err(Fault::Reserved),
    };
    let escape = (1 << width) - 1;
    let partitions = 1usize << bits.read(4);
    let each = samples.len() / partitions;
    if each * partitions != samples.len() || each < order {
        return Err(Fault::Partition);
    }
    let mut rest = &mut samples[order..];
    for partition in 0..partitions {
        let (residuals, after) = rest.split_at_mut(if partition == 0 { each - order } else { each });
        rest = after;
        let parameter = bits.read(width);
        if parameter == escape {
            let stored = bits.read(5);
            for residual in residuals {
                *residual = bits.signed(stored);
            }
        } else {
            bits.rice(parameter, residuals)?;
        }
        if bits.short() {
            return Err(Fault::Short);
        }
    }
    Ok(())
}

/// One channel of a frame, its samples `width` bits each.
fn subframe(bits: &mut Bits, width: u32, samples: &mut [i32]) -> Result<(), Fault> {
    let header = bits.read(8);
    if header & 0x80 != 0 {
        return Err(Fault::Reserved);
    }
    // Low bits every sample has as zero, left out of what is coded.
    let wasted = if header & 1 != 0 { bits.unary()? + 1 } else { 0 };
    if wasted >= width {
        return Err(Fault::Predictor);
    }
    let width = width - wasted;
    match header >> 1 {
        0 => samples.fill(bits.signed(width)),
        1 => {
            for sample in samples.iter_mut() {
                *sample = bits.signed(width);
            }
        }
        kind @ 8..=12 => {
            let order = (kind - 8) as usize;
            if order > samples.len() {
                return Err(Fault::Predictor);
            }
            for sample in &mut samples[..order] {
                *sample = bits.signed(width);
            }
            residual(bits, order, samples)?;
            predict_narrow(samples, FIXED[order], 0);
        }
        kind @ 32..=63 => {
            let order = (kind - 31) as usize;
            if order > samples.len() {
                return Err(Fault::Predictor);
            }
            for sample in &mut samples[..order] {
                *sample = bits.signed(width);
            }
            let precision = bits.read(4) + 1;
            // A shift is written signed, and a negative one is no predictor.
            let shift = bits.read(5);
            if precision == 16 || shift >= 16 {
                return Err(Fault::Reserved);
            }
            let mut coefficients = [0i32; 32];
            let coefficients = &mut coefficients[..order];
            for coefficient in coefficients.iter_mut() {
                *coefficient = bits.signed(precision);
            }
            residual(bits, order, samples)?;
            // What libFLAC's encoder holds itself to before it predicts in 32
            // bits: the widest sample by the widest coefficient, `order` times.
            if width + precision + order.ilog2() <= 32 {
                predict_narrow(samples, coefficients, shift);
            } else {
                predict_wide(samples, coefficients, shift);
            }
        }
        _ => return Err(Fault::Reserved),
    }
    if wasted > 0 {
        for sample in samples.iter_mut() {
            *sample = sample.wrapping_shl(wasted);
        }
    }
    if bits.short() {
        return Err(Fault::Short);
    }
    Ok(())
}

/// What the frame's two channels are of the stream's.
#[derive(Clone, Copy)]
enum Channels {
    /// Each its own, one or two.
    Apart,
    /// The left, and the left less the right.
    LeftSide,
    /// The left less the right, and the right.
    SideRight,
    /// Their sum halved, and the left less the right.
    MidSide,
}

/// A sample as Web Audio plays it. The division is exact.
#[inline(always)]
fn float(sample: i32) -> f32 {
    sample as f32 * (1.0 / 32_768.0)
}

/// One stream's decoder. Each frame decodes on its own, so a frame that fails
/// leaves the decoder good for the next.
pub struct Decoder {
    stream: Stream,
    rate_code: u8,
    /// A frame's samples as they are coded, a block for each channel.
    samples: Vec<i32>,
    /// A frame's bytes as the words [`Bits`] reads.
    words: Vec<u64>,
}

impl Decoder {
    pub fn new(stream: Stream) -> Result<Self, Error> {
        let rate_code = rate_code(stream.rate).ok_or(Error::Unsupported(stream))?;
        if !(1..=2).contains(&stream.channels) || stream.block < 16 {
            return Err(Error::Unsupported(stream));
        }
        let samples = vec![0; usize::from(stream.block) * usize::from(stream.channels)];
        Ok(Self { stream, rate_code, samples, words: Vec::new() })
    }

    /// Decode one frame, the whole of `frame` and nothing more, and write its
    /// samples to `out` as planar floats in -1 to 1: every sample of the first
    /// channel, then every sample of the second. That is what Web Audio plays,
    /// and the division by 2^15 is exact, so nothing is lost on the way.
    ///
    /// The frame's CRCs are checked, and its header against the stream. A frame
    /// that is refused leaves `out` as it was.
    pub fn decode(&mut self, frame: &[u8], out: &mut Vec<f32>) -> Result<(), Error> {
        let not_a_frame = Error::NotAFrame(frame.len());
        // The sync code, then the rate and the sample width where the header
        // states them.
        if frame.len() < 6 || frame[0] != 0xFF || frame[1] & 0xFE != 0xF8 {
            return Err(not_a_frame);
        }
        // 0b100 is 16 bits a sample, and the bit after it is kept at zero.
        if frame[2] & 0x0F != self.rate_code || frame[3] & 0x0F != 0b1000 {
            return Err(Error::Header);
        }
        let (channels, layout) = match frame[3] >> 4 {
            apart @ 0..=7 => (u32::from(apart) + 1, Channels::Apart),
            8 => (2, Channels::LeftSide),
            9 => (2, Channels::SideRight),
            10 => (2, Channels::MidSide),
            _ => return Err(Error::Decode(Fault::Reserved)),
        };
        // The frame's number, in the bytes UTF-8 would give it: read past.
        let mut at = 4 + match frame[4].leading_ones() {
            0 => 1,
            1 | 8 => return Err(Error::Decode(Fault::Reserved)),
            bytes => bytes as usize,
        };
        let byte = |at: usize| frame.get(at).copied().ok_or(Error::NotAFrame(frame.len()));
        let frames = match frame[2] >> 4 {
            0 => return Err(Error::Decode(Fault::Reserved)),
            1 => 192,
            code @ 2..=5 => 576 << (code - 2),
            6 => {
                at += 1;
                u32::from(byte(at - 1)?) + 1
            }
            7 => {
                at += 2;
                (u32::from(byte(at - 2)?) << 8 | u32::from(byte(at - 1)?)) + 1
            }
            code => 256 << (code - 8),
        };
        if crc8(frame.get(..at).ok_or(not_a_frame)?) != byte(at)? {
            return Err(Error::Decode(Fault::HeaderChecksum));
        }
        if frames != u32::from(self.stream.block) || channels != u32::from(self.stream.channels) {
            return Err(Error::Shape { frames, channels, want: self.stream });
        }

        let block = usize::from(self.stream.block);
        let (whole, last) = frame.as_chunks::<8>();
        let mut rest = [0; 8];
        rest[..last.len()].copy_from_slice(last);
        self.words.clear();
        self.words.extend(whole.iter().chain([&rest]).map(|&word| u64::from_be_bytes(word)));
        let mut bits = Bits { words: &self.words, at: (at + 1) * 8, end: frame.len() * 8 };
        for (channel, samples) in self.samples.chunks_exact_mut(block).enumerate() {
            // A difference of two samples is a bit wider than either.
            let side = matches!((layout, channel), (Channels::LeftSide | Channels::MidSide, 1) | (Channels::SideRight, 0));
            subframe(&mut bits, BITS + u32::from(side), samples).map_err(Error::Decode)?;
        }
        // The frame is whole bytes, and its last two are the checksum of the
        // rest.
        let end = bits.at.div_ceil(8);
        let Some(&[high, low]) = frame.get(end..end + 2) else { return Err(Error::Decode(Fault::Short)) };
        if crc16(&frame[..end]) != u16::from_be_bytes([high, low]) {
            return Err(Error::Decode(Fault::FrameChecksum));
        }
        if end + 2 != frame.len() {
            return Err(Error::Trailing(frame.len() - end - 2));
        }

        out.clear();
        let (first, second) = self.samples.split_at(block);
        let pairs = || first.iter().zip(second);
        match layout {
            Channels::Apart => out.extend(self.samples.iter().map(|&sample| float(sample))),
            Channels::LeftSide => {
                out.extend(first.iter().map(|&left| float(left)));
                out.extend(pairs().map(|(&left, &side)| float(left.wrapping_sub(side))));
            }
            Channels::SideRight => {
                out.extend(pairs().map(|(&side, &right)| float(side.wrapping_add(right))));
                out.extend(second.iter().map(|&right| float(right)));
            }
            // The sum was halved, and what that dropped is the difference's
            // lowest bit.
            Channels::MidSide => {
                let sum = |mid: i32, side: i32| mid.wrapping_shl(1) | (side & 1);
                out.extend(pairs().map(|(&mid, &side)| float(sum(mid, side).wrapping_add(side) >> 1)));
                out.extend(pairs().map(|(&mid, &side)| float(sum(mid, side).wrapping_sub(side) >> 1)));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Frames put together here bit by bit, by an encoder that shares nothing
    //! with the decoder but the format: its sums are 64 bits wide and its
    //! checksums are taken a bit at a time. libFLAC's own frames, which hold
    //! what an encoder commonly picks, are the binding's test and the
    //! benchmark's check; these hold the rest of what a frame may.

    use super::*;

    const WLSHARE: Stream = Stream { rate: 48_000, channels: 2, block: 960 };
    const RDP: Stream = Stream { rate: 44_100, channels: 2, block: 882 };

    /// xorshift64*.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// A number of `bits` bits, signed.
        fn signed(&mut self, bits: u32) -> i32 {
            (self.next() as i64 >> (64 - bits)) as i32
        }

        fn below(&mut self, limit: usize) -> usize {
            (self.next() >> 33) as usize % limit
        }
    }

    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        /// The bits of the last byte still free.
        free: u32,
    }

    impl Writer {
        fn put(&mut self, value: u32, bits: u32) {
            for bit in (0..bits).rev() {
                if self.free == 0 {
                    self.bytes.push(0);
                    self.free = 8;
                }
                self.free -= 1;
                *self.bytes.last_mut().unwrap() |= ((value >> bit & 1) as u8) << self.free;
            }
        }

        fn unary(&mut self, zeros: u32) {
            for _ in 0..zeros {
                self.put(0, 1);
            }
            self.put(1, 1);
        }
    }

    /// A checksum the long way: `width` bits, a bit of the bytes at a time.
    fn checksum(bytes: &[u8], width: u32, polynomial: u32) -> u32 {
        let mut crc = 0u32;
        for &byte in bytes {
            crc ^= u32::from(byte) << (width - 8);
            for _ in 0..8 {
                crc = crc << 1 ^ if crc >> (width - 1) & 1 != 0 { polynomial } else { 0 };
            }
        }
        crc & ((1 << width) - 1)
    }

    /// How a subframe's residuals are written.
    #[derive(Clone, Copy)]
    enum Coding {
        /// Rice codes: the bits a parameter is written in, four or five, the
        /// partitions as a power of two, and the parameter.
        Rice { width: u32, partitions: u32, parameter: u32 },
        /// Stored plain, each in this many bits.
        Plain { bits: u32 },
    }

    /// How a channel is written.
    #[derive(Clone)]
    enum Subframe {
        Constant,
        Verbatim,
        /// A predictor: its coefficients, the bits each is written in, and the
        /// shift. One of the fixed predictors if it has no precision.
        Predicted { coefficients: Vec<i32>, precision: Option<u32>, shift: u32, coding: Coding },
    }

    fn subframe(writer: &mut Writer, how: &Subframe, width: u32, samples: &[i32]) {
        // The low bits every sample has as zero are left out, when the test
        // gave it such samples.
        let wasted = samples.iter().fold(0, |all, &sample| all | sample).trailing_zeros().min(width - 1);
        let wasted = if matches!(how, Subframe::Constant) { 0 } else { wasted };
        let samples: Vec<i32> = samples.iter().map(|&sample| sample >> wasted).collect();
        let width = width - wasted;
        let kind = match how {
            Subframe::Constant => 0,
            Subframe::Verbatim => 1,
            Subframe::Predicted { coefficients, precision: None, .. } => 8 + coefficients.len() as u32,
            Subframe::Predicted { coefficients, .. } => 31 + coefficients.len() as u32,
        };
        writer.put(kind << 1 | u32::from(wasted > 0), 8);
        if wasted > 0 {
            writer.unary(wasted - 1);
        }
        match how {
            Subframe::Constant => writer.put(samples[0] as u32, width),
            Subframe::Verbatim => samples.iter().for_each(|&sample| writer.put(sample as u32, width)),
            Subframe::Predicted { coefficients, precision, shift, coding } => {
                let order = coefficients.len();
                samples[..order].iter().for_each(|&sample| writer.put(sample as u32, width));
                if let Some(precision) = precision {
                    writer.put(precision - 1, 4);
                    writer.put(*shift, 5);
                    coefficients.iter().for_each(|&coefficient| writer.put(coefficient as u32, *precision));
                }
                let residuals: Vec<i32> = (order..samples.len())
                    .map(|at| {
                        let sum: i64 =
                            (0..order).map(|n| i64::from(coefficients[n]) * i64::from(samples[at - 1 - n])).sum();
                        i32::try_from(i64::from(samples[at]) - (sum >> shift)).expect("a residual of 32 bits")
                    })
                    .collect();
                match *coding {
                    Coding::Rice { width, partitions, parameter } => {
                        writer.put(u32::from(width == 5), 2);
                        writer.put(partitions, 4);
                        let each = samples.len() >> partitions;
                        let mut rest = &residuals[..];
                        for partition in 0..1 << partitions {
                            let (now, after) = rest.split_at(if partition == 0 { each - order } else { each });
                            rest = after;
                            writer.put(parameter, width);
                            for &residual in now {
                                let folded = (residual << 1 ^ residual >> 31) as u32;
                                writer.unary(folded >> parameter);
                                writer.put(folded, parameter);
                            }
                        }
                    }
                    Coding::Plain { bits } => {
                        writer.put(0, 2);
                        writer.put(0, 4);
                        writer.put(0b1111, 4);
                        writer.put(bits, 5);
                        residuals.iter().for_each(|&residual| writer.put(residual as u32, bits));
                    }
                }
            }
        }
    }

    /// A frame of `stream`: its channels as `assignment` says, 0 or 1 for each
    /// on its own and 8 to 10 for the three ways two are told apart, each
    /// written as its `Subframe` says.
    fn frame(stream: Stream, assignment: u32, how: &[Subframe], left: &[i32], right: &[i32]) -> Vec<u8> {
        let mut writer = Writer::default();
        writer.put(0xFFF8, 16);
        writer.put(7, 4);
        writer.put(u32::from(rate_code(stream.rate).unwrap()), 4);
        writer.put(assignment, 4);
        writer.put(0b1000, 4);
        writer.put(0, 8);
        writer.put(u32::from(stream.block) - 1, 16);
        writer.put(checksum(&writer.bytes, 8, 0x07), 8);
        let side: Vec<i32> = left.iter().zip(right).map(|(l, r)| l - r).collect();
        let mid: Vec<i32> = left.iter().zip(right).map(|(l, r)| (l + r) >> 1).collect();
        let channels: Vec<(&[i32], u32)> = match assignment {
            0 => vec![(left, 16)],
            1 => vec![(left, 16), (right, 16)],
            8 => vec![(left, 16), (&side, 17)],
            9 => vec![(&side, 17), (right, 16)],
            10 => vec![(&mid, 16), (&side, 17)],
            _ => unreachable!(),
        };
        for ((samples, width), how) in channels.into_iter().zip(how) {
            subframe(&mut writer, how, width, samples);
        }
        let crc = checksum(&writer.bytes, 16, 0x8005);
        writer.free = 0;
        writer.put(crc, 16);
        writer.bytes
    }

    fn sound(random: &mut Random, block: u16, bits: u32) -> Vec<i32> {
        (0..block).map(|_| random.signed(bits)).collect()
    }

    /// The frame decodes to exactly `left` and `right`.
    fn decodes(stream: Stream, frame: &[u8], left: &[i32], right: &[i32]) {
        let mut out = Vec::new();
        Decoder::new(stream).unwrap().decode(frame, &mut out).unwrap();
        let want: Vec<f32> =
            left.iter().chain(&right[..right.len() * (usize::from(stream.channels) - 1)]).map(|&s| float(s)).collect();
        assert!(out == want, "the samples decoded are not the samples written");
    }

    fn rice(parameter: u32) -> Coding {
        Coding::Rice { width: 4, partitions: 0, parameter }
    }

    fn fixed(order: usize, coding: Coding) -> Subframe {
        Subframe::Predicted { coefficients: FIXED[order].to_vec(), precision: None, shift: 0, coding }
    }

    #[test]
    fn checksums_by_table_are_the_checksums_bit_by_bit() {
        let mut random = Random(1);
        for len in [0, 1, 7, 8, 9, 15, 16, 17, 1000] {
            let bytes: Vec<u8> = (0..len).map(|_| random.next() as u8).collect();
            assert_eq!(u32::from(crc8(&bytes)), checksum(&bytes, 8, 0x07));
            assert_eq!(u32::from(crc16(&bytes)), checksum(&bytes, 16, 0x8005));
        }
    }

    #[test]
    fn two_channels_are_told_apart_each_way_a_frame_may() {
        let mut random = Random(2);
        for assignment in [1, 8, 9, 10] {
            // Both extremes of a sample in each channel, against each other,
            // which is the widest a difference gets.
            let mut left = sound(&mut random, 960, 16);
            let mut right = sound(&mut random, 960, 16);
            (left[0], right[0], left[1], right[1]) = (-32_768, 32_767, 32_767, -32_768);
            for how in [Subframe::Verbatim, fixed(2, Coding::Plain { bits: 20 })] {
                let frame = frame(WLSHARE, assignment, &[how.clone(), how], &left, &right);
                decodes(WLSHARE, &frame, &left, &right);
            }
        }
    }

    #[test]
    fn one_channel_and_the_other_block_decode() {
        let mut random = Random(3);
        let mono = Stream { channels: 1, ..RDP };
        let left = sound(&mut random, 882, 16);
        decodes(mono, &frame(mono, 0, &[fixed(1, rice(14))], &left, &left), &left, &left);
        // 882 is twice an odd number: two partitions and no more.
        let halves = fixed(3, Coding::Rice { width: 4, partitions: 1, parameter: 14 });
        let right = sound(&mut random, 882, 16);
        decodes(RDP, &frame(RDP, 1, &[halves.clone(), halves], &left, &right), &left, &right);
        let quarters = fixed(3, Coding::Rice { width: 4, partitions: 2, parameter: 14 });
        // The encoder here writes what it is told, so the partitions it wrote
        // are of 220 samples, which is not what four of 882 are.
        let mut decoder = Decoder::new(RDP).unwrap();
        let refused = decoder.decode(&frame(RDP, 1, &[quarters.clone(), quarters], &left, &right), &mut Vec::new());
        assert!(matches!(refused, Err(Error::Decode(Fault::Partition))));
    }

    #[test]
    fn every_kind_of_subframe_decodes() {
        let mut random = Random(4);
        let constant = vec![-1234; 960];
        let quiet = sound(&mut random, 960, 6);
        let soft = sound(&mut random, 960, 9);
        let loud = sound(&mut random, 960, 16);
        decodes(WLSHARE, &frame(WLSHARE, 1, &[Subframe::Constant, Subframe::Verbatim], &constant, &loud), &constant, &loud);
        for order in 0..=4 {
            // Rice parameters of four bits from the least to the most, in one
            // partition and in the most 960 samples divide into. A code with
            // no low bits at all has runs of zeros longer than any window.
            for (parameter, partitions) in [(0, 0), (1, 6), (3, 3), (7, 6), (12, 1), (14, 0)] {
                let how = fixed(order, Coding::Rice { width: 4, partitions, parameter });
                let right = if parameter < 7 { &soft } else { &loud };
                decodes(WLSHARE, &frame(WLSHARE, 1, &[how.clone(), how], &quiet, right), &quiet, right);
            }
            // Parameters of five bits, and residuals stored plain, in no bits
            // at all where there is nothing to store.
            let wide = fixed(order, Coding::Rice { width: 5, partitions: 2, parameter: 19 });
            decodes(WLSHARE, &frame(WLSHARE, 1, &[wide.clone(), wide], &quiet, &loud), &quiet, &loud);
            let plain = fixed(order, Coding::Plain { bits: 21 });
            decodes(WLSHARE, &frame(WLSHARE, 1, &[plain.clone(), plain], &quiet, &loud), &quiet, &loud);
        }
        let nothing = fixed(1, Coding::Plain { bits: 0 });
        decodes(WLSHARE, &frame(WLSHARE, 1, &[nothing.clone(), nothing], &constant, &constant), &constant, &constant);
    }

    #[test]
    fn a_predictor_of_every_order_decodes_in_either_width() {
        let mut random = Random(5);
        let left = sound(&mut random, 960, 16);
        let right = sound(&mut random, 960, 16);
        for order in 1..=32 {
            // Ten bits is what libFLAC writes for a block this long, and with
            // it every order's sum fits 32 bits; fifteen is the most a frame
            // may state, and from the second order on the sum may not.
            for precision in [10, 15] {
                let coefficients: Vec<i32> = (0..order).map(|_| random.signed(precision)).collect();
                let how = Subframe::Predicted {
                    coefficients,
                    precision: Some(precision),
                    shift: precision - 1,
                    coding: Coding::Rice { width: 5, partitions: 0, parameter: 18 },
                };
                // Told apart as a sum and a difference, so that one channel
                // is of 17 bits.
                decodes(WLSHARE, &frame(WLSHARE, 10, &[how.clone(), how], &left, &right), &left, &right);
            }
        }
    }

    #[test]
    fn low_bits_every_sample_lacks_are_put_back() {
        let mut random = Random(6);
        let left: Vec<i32> = sound(&mut random, 960, 13).iter().map(|s| s << 3).collect();
        let right: Vec<i32> = sound(&mut random, 960, 9).iter().map(|s| s << 7).collect();
        for how in [Subframe::Verbatim, fixed(2, rice(12))] {
            decodes(WLSHARE, &frame(WLSHARE, 1, &[how.clone(), how], &left, &right), &left, &right);
        }
    }

    #[test]
    fn a_frame_that_is_refused_says_why_and_leaves_the_samples_alone() {
        let mut random = Random(7);
        let left = sound(&mut random, 960, 16);
        let right = sound(&mut random, 960, 16);
        let good = frame(WLSHARE, 8, &[fixed(2, rice(14)), fixed(2, rice(14))], &left, &right);
        let mut decoder = Decoder::new(WLSHARE).unwrap();
        let mut out = vec![0.25];
        let mut refused = |frame: &[u8]| {
            let error = decoder.decode(frame, &mut out).unwrap_err();
            assert_eq!(out, [0.25]);
            error
        };
        assert!(matches!(refused(&good[..5]), Error::NotAFrame(5)));
        assert!(matches!(refused(&[&[0xFE], &good[1..]].concat()), Error::NotAFrame(_)));
        let mut other = good.clone();
        other[2] ^= 0b0011;
        assert!(matches!(refused(&other), Error::Header));
        other = good.clone();
        other[5] ^= 1;
        assert!(matches!(refused(&other), Error::Decode(Fault::HeaderChecksum)));
        other = good.clone();
        other[100] ^= 0x10;
        assert!(matches!(refused(&other), Error::Decode(_)));
        other = good.clone();
        *other.last_mut().unwrap() ^= 1;
        assert!(matches!(refused(&other), Error::Decode(Fault::FrameChecksum)));
        assert!(matches!(refused(&good[..good.len() - 1]), Error::Decode(Fault::Short)));
        assert!(matches!(refused(&good[..good.len() / 2]), Error::Decode(Fault::Short)));
        assert!(matches!(refused(&[&good[..], &[0, 0, 0]].concat()), Error::Trailing(3)));
        // Another block, and another count of channels, each with a header
        // that is whole.
        let short = Stream { block: 480, ..WLSHARE };
        let half = frame(short, 8, &[Subframe::Verbatim, Subframe::Verbatim], &left[..480], &right[..480]);
        assert!(matches!(refused(&half), Error::Shape { frames: 480, channels: 2, .. }));
        let mono = Stream { channels: 1, ..WLSHARE };
        let one = frame(mono, 0, &[Subframe::Verbatim], &left, &left);
        assert!(matches!(refused(&one), Error::Shape { frames: 960, channels: 1, .. }));
        // And the decoder is good for the next frame.
        decoder.decode(&good, &mut out).unwrap();
        assert_eq!(out.len(), 1920);
    }

    /// No frame, whatever is in it, does anything but decode or be refused:
    /// the module is built to abort on a panic, which would cost the page its
    /// sound for good.
    #[test]
    fn a_damaged_frame_is_refused_or_decoded_and_nothing_else() {
        let mut random = Random(8);
        let left = sound(&mut random, 960, 16);
        let right = sound(&mut random, 960, 10);
        let predicted = Subframe::Predicted {
            coefficients: vec![700, -300, 90, -20, 5, 3, -2, 1],
            precision: Some(12),
            shift: 9,
            coding: Coding::Rice { width: 4, partitions: 3, parameter: 13 },
        };
        let frames = [
            frame(WLSHARE, 10, &[predicted.clone(), predicted.clone()], &left, &right),
            frame(WLSHARE, 8, &[fixed(4, rice(14)), fixed(2, Coding::Plain { bits: 14 })], &left, &right),
            frame(WLSHARE, 9, &[fixed(0, Coding::Rice { width: 5, partitions: 6, parameter: 3 }), predicted], &right, &right),
            frame(WLSHARE, 1, &[Subframe::Constant, Subframe::Verbatim], &left, &right),
        ];
        let mut decoder = Decoder::new(WLSHARE).unwrap();
        let mut out = Vec::new();
        for frame in &frames {
            decoder.decode(frame, &mut out).unwrap();
            // Every bit of the header and of what follows it turned over, one
            // at a time, with the frame cut short there as well.
            for bit in 0..256.min(frame.len() * 8) {
                let mut damaged = frame.clone();
                damaged[bit / 8] ^= 0x80 >> (bit % 8);
                let _ = decoder.decode(&damaged, &mut out);
                let _ = decoder.decode(&damaged[..bit / 8], &mut out);
            }
            // And bytes anywhere, a few at a time.
            for _ in 0..4000 {
                let mut damaged = frame.clone();
                for _ in 0..1 + random.below(4) {
                    let at = random.below(damaged.len());
                    damaged[at] = random.next() as u8;
                }
                damaged.truncate(damaged.len() - random.below(3) * random.below(damaged.len()) / 2);
                let _ = decoder.decode(&damaged, &mut out);
            }
        }
    }
}
