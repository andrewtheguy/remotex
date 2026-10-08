//! `remotex serve --vp9-capture <DIR>`: every VP9 stream this gateway encodes, kept on disk
//! for analysis. Only the command line turns it on, so a config file copied between
//! gateways can never start filling a disk.
//!
//! One file per stream ([`crate::vp9::Stream`]), which is one picture size from a
//! keyframe on: a resize, or a stream rebuilt for any other reason, starts the next
//! file. Each is IVF — a 32-byte header, then per frame a `u32` length and a `u64`
//! timestamp in milliseconds, little-endian, and the frame — beside a `.csv` of each
//! frame's time, bytes, whether it was a keyframe and the dial it was coded at. The
//! timestamps count from the stream's first frame.
//!
//! Only what is encoded here is kept. A passed stream is the remote's, and a frame
//! the encoder produced no bitstream for is not a frame.
//!
//! The frame count in the header is rewritten after every frame, so a file is whole
//! at any moment, including one whose gateway was killed mid-session. A capture that
//! fails to write is logged and dropped; the session goes on without it.

use std::fs::File;
use std::io::{Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;

use crate::config::Chroma;
use crate::video::AccessUnit;

/// The IVF header's length, which its own header says again.
const HEADER_LEN: u16 = 32;
/// Where the header keeps its frame count.
const FRAME_COUNT_AT: u64 = 24;

/// Make the directory the captures go to, before the gateway listens, so a
/// directory it cannot make is a refused start.
pub fn prepare(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot make the VP9 capture directory {}", dir.display()))?;
    log::info!("capturing every VP9 stream this gateway encodes to {}", dir.display());
    Ok(())
}

/// Where a session's streams are kept, and the target they are named after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    dir: Arc<Path>,
    target: Arc<str>,
}

impl Capture {
    pub fn new(dir: Arc<Path>, target: &str) -> Self {
        Self { dir, target: target.into() }
    }

    /// The capture of one stream of a `coded` picture at `chroma`. Nothing is
    /// opened until its first frame.
    pub fn stream(&self, coded: (u16, u16), chroma: Chroma) -> StreamCapture {
        StreamCapture { capture: self.clone(), coded, chroma, files: None }
    }
}

/// One stream's capture, opened at its first frame.
pub struct StreamCapture {
    capture: Capture,
    coded: (u16, u16),
    chroma: Chroma,
    files: Option<Files>,
}

struct Files {
    ivf: File,
    csv: File,
    ivf_path: PathBuf,
    started: Instant,
    frames: u32,
}

impl StreamCapture {
    /// Append `unit`, coded at `quality`. Blocking: call it from the encode's worker.
    pub fn write(&mut self, unit: &AccessUnit, quality: u8) -> anyhow::Result<()> {
        let files = match &mut self.files {
            Some(files) => files,
            None => self.files.insert(self.open()?),
        };
        let ms = u64::try_from(files.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let len = u32::try_from(unit.data.len()).context("a VP9 frame past 4 GiB")?;
        let mut frame = Vec::with_capacity(12 + unit.data.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&ms.to_le_bytes());
        frame.extend_from_slice(&unit.data);
        let path = &files.ivf_path;
        files.ivf.write_all(&frame).with_context(|| format!("writing {}", path.display()))?;
        files.frames += 1;
        files.ivf.seek(SeekFrom::Start(FRAME_COUNT_AT)).with_context(|| format!("seeking in {}", path.display()))?;
        files.ivf.write_all(&files.frames.to_le_bytes()).with_context(|| format!("writing {}", path.display()))?;
        files.ivf.seek(SeekFrom::End(0)).with_context(|| format!("seeking in {}", path.display()))?;
        let row = format!("{},{ms},{len},{},{quality}\n", files.frames - 1, u8::from(unit.keyframe));
        files.csv.write_all(row.as_bytes()).with_context(|| format!("writing {}.csv", path.display()))?;
        Ok(())
    }

    fn open(&self) -> anyhow::Result<Files> {
        let (w, h) = self.coded;
        let chroma = match self.chroma {
            Chroma::Subsampled => "420",
            Chroma::Full => "444",
        };
        let name = format!("{}-{}-{w}x{h}-{chroma}.ivf", file_safe(&self.capture.target), utc_stamp(SystemTime::now()));
        let ivf_path = self.capture.dir.join(name);
        let mut csv_path = ivf_path.clone().into_os_string();
        csv_path.push(".csv");
        let mut ivf = File::create_new(&ivf_path).with_context(|| format!("creating {}", ivf_path.display()))?;
        ivf.write_all(&header(w, h)).with_context(|| format!("writing {}", ivf_path.display()))?;
        let mut csv = File::create_new(&csv_path).with_context(|| format!("creating {}", Path::new(&csv_path).display()))?;
        csv.write_all(b"frame,ms,bytes,keyframe,quality\n").with_context(|| format!("writing {}.csv", ivf_path.display()))?;
        log::info!("video: capturing the stream to {}", ivf_path.display());
        Ok(Files { ivf, csv, ivf_path, started: Instant::now(), frames: 0 })
    }
}

/// An IVF header for a VP9 stream of a `w`×`h` picture in milliseconds, its frame
/// count still zero.
fn header(w: u16, h: u16) -> [u8; HEADER_LEN as usize] {
    let mut header = [0; HEADER_LEN as usize];
    header[0..4].copy_from_slice(b"DKIF");
    header[6..8].copy_from_slice(&HEADER_LEN.to_le_bytes());
    header[8..12].copy_from_slice(b"VP90");
    header[12..14].copy_from_slice(&w.to_le_bytes());
    header[14..16].copy_from_slice(&h.to_le_bytes());
    // The time base, as a rate over a scale: 1000 ticks a second.
    header[16..20].copy_from_slice(&1000u32.to_le_bytes());
    header[20..24].copy_from_slice(&1u32.to_le_bytes());
    header
}

/// A target's name as a piece of a file name: what is not a letter, a digit, `.`,
/// `-` or `_` becomes `_`.
fn file_safe(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect()
}

/// `at` in UTC as `YYYYMMDD-HHMMSS.mmm`, which sorts as it reads.
fn utc_stamp(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (days, of_day) = (secs / 86_400, secs % 86_400);
    // Days to a civil date: Howard Hinnant's `civil_from_days`, over eras of 400 years.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}.{:03}",
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60,
        since.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_stamp_is_the_utc_date_and_time() {
        assert_eq!(utc_stamp(UNIX_EPOCH), "19700101-000000.000");
        // 2024-02-29 is a leap day, and 23:59:59.999 the last instant of it.
        assert_eq!(utc_stamp(UNIX_EPOCH + Duration::from_millis(1_709_251_199_999)), "20240229-235959.999");
        assert_eq!(utc_stamp(UNIX_EPOCH + Duration::from_secs(1_791_331_200)), "20261007-000000.000");
    }

    #[test]
    fn a_target_name_is_kept_to_what_a_file_name_takes() {
        assert_eq!(file_safe("Mac mini (HP)/4K"), "Mac_mini__HP__4K");
        assert_eq!(file_safe("windows-rdp_2.local"), "windows-rdp_2.local");
    }

    /// The file is IVF as libvpx and ffmpeg read it, whole after every frame, and the
    /// CSV has a row per frame.
    #[test]
    fn a_stream_is_written_as_ivf_with_a_row_per_frame() {
        let dir = tempfile::tempdir().unwrap();
        let mut stream = Capture::new(dir.path().into(), "a target").stream((1440, 900), Chroma::Full);
        stream.write(&AccessUnit { data: vec![1, 2, 3], keyframe: true }, 90).unwrap();
        stream.write(&AccessUnit { data: vec![4, 5], keyframe: false }, 61).unwrap();
        let ivf_path = stream.files.as_ref().unwrap().ivf_path.clone();
        let name = ivf_path.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(name.starts_with("a_target-") && name.ends_with("-1440x900-444.ivf"), "{name}");

        let ivf = std::fs::read(&ivf_path).unwrap();
        assert_eq!(&ivf[0..4], b"DKIF");
        assert_eq!(u16::from_le_bytes([ivf[6], ivf[7]]), 32);
        assert_eq!(&ivf[8..12], b"VP90");
        assert_eq!(u16::from_le_bytes([ivf[12], ivf[13]]), 1440);
        assert_eq!(u16::from_le_bytes([ivf[14], ivf[15]]), 900);
        assert_eq!(u32::from_le_bytes(ivf[16..20].try_into().unwrap()), 1000);
        assert_eq!(u32::from_le_bytes(ivf[20..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(ivf[24..28].try_into().unwrap()), 2, "the frame count");
        assert_eq!(u32::from_le_bytes(ivf[32..36].try_into().unwrap()), 3);
        assert_eq!(&ivf[44..47], &[1, 2, 3]);
        assert_eq!(u32::from_le_bytes(ivf[47..51].try_into().unwrap()), 2);
        assert_eq!(&ivf[59..], &[4, 5]);

        let mut csv_path = ivf_path.clone().into_os_string();
        csv_path.push(".csv");
        let csv = std::fs::read_to_string(&csv_path).unwrap();
        let rows: Vec<Vec<&str>> = csv.lines().map(|line| line.split(',').collect()).collect();
        assert_eq!(rows[0], ["frame", "ms", "bytes", "keyframe", "quality"]);
        assert_eq!((rows[1][0], rows[1][2], rows[1][3], rows[1][4]), ("0", "3", "1", "90"));
        assert_eq!((rows[2][0], rows[2][2], rows[2][3], rows[2][4]), ("1", "2", "0", "61"));
        assert_eq!(rows.len(), 3);
    }
}
