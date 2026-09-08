//! A damage tape: everything a `render_motion` session's encoder was handed, with
//! the moments it was handed it, written to a file so the region policy can be
//! replayed offline with different numbers.
//!
//! `RETUNE` and `STREAM_IDLE` in [`crate::regions`] were chosen to be legible and
//! are owed a measurement (see `docs/roadmap.md`). A live A/B per value is the
//! expensive way to get one: every run needs the same content driven the same way,
//! and two runs of a real desktop never are. A tape records the input once — the
//! damaged rectangles, the cells inside them that changed, and the engine's frame
//! boundaries — and the replay in `encode::tests` runs the motion detector, the
//! regions and the VP9 encoder over it as many times as there are values to try,
//! on a clock read from the tape rather than the wall. Same pixels in, so the
//! only thing that differs between two rows of its table is the number under test.
//!
//! Set `REMOTEX_MOTION_TAPE=<path>` on `remotex serve` to record one; the file is
//! written from the moment the session's encoder starts until it finishes. Only a
//! `render_motion = true` target records anything.
//!
//! ## Format
//!
//! Little-endian, no alignment. The pixels of a rectangle are one PNG, the same
//! encode the tiles use, so a scroll that repaints 1080p twenty times a second
//! writes tens rather than hundreds of megabytes a minute; decoding it back is the
//! replay's problem, off the engine's task.
//!
//! ```text
//! header  b"RXTAPE01" u8 quality u8 chroma(0 = 4:2:0, 1 = 4:4:4)
//! record  u8 kind u64 t_us …
//!   0 resize  u16 w u16 h f32 scale
//!   1 damage  u16 left u16 top u16 right u16 bottom
//!             u32 n  (u16 col u16 row) × n   u32 len  byte[len] png
//!   2 frame   (nothing)
//! ```
//!
//! `t_us` is microseconds since the tape opened. Writing happens on a thread of
//! its own behind an unbounded channel, so the engine's task pays one extra copy
//! of the rectangle and nothing else; the encode and the disk are the thread's.

use std::io::Write;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::Context;
use log::{info, warn};

use crate::config::Chroma;
use crate::protocol::Tile;
use crate::tiles::{Changed, Rect};

const MAGIC: &[u8; 8] = b"RXTAPE01";

/// The environment variable naming the file a session records to.
pub const ENV: &str = "REMOTEX_MOTION_TAPE";

/// What a session is asked for, in the order it was asked.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Resize { t_us: u64, w: u16, h: u16, scale: f32 },
    Damage { t_us: u64, rect: Rect, cells: Vec<(u16, u16)>, rgb: Vec<u8> },
    Frame { t_us: u64 },
}

impl Record {
    pub fn t_us(&self) -> u64 {
        match self {
            Record::Resize { t_us, .. } | Record::Damage { t_us, .. } | Record::Frame { t_us } => {
                *t_us
            }
        }
    }
}

/// The stream's configuration, so a replay builds the encoders the session did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub quality: u8,
    pub chroma: Chroma,
}

/// A recorder. Dropping it closes the file once everything queued is written.
pub struct Tape {
    tx: Option<mpsc::Sender<Record>>,
    writer: Option<std::thread::JoinHandle<()>>,
    opened: Instant,
}

impl Drop for Tape {
    fn drop(&mut self) {
        // Hang up first, so the writer's loop ends once the backlog is written, then
        // wait for it: the file is only complete once it has flushed.
        self.tx.take();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

impl Tape {
    /// Start recording to the file [`ENV`] names, or `None` when it is unset. A file
    /// that cannot be opened is a warning and no tape — a measurement aid must not
    /// end a session.
    pub fn from_env(header: Header) -> Option<Self> {
        let path = std::env::var_os(ENV)?;
        match Self::create(&path, header) {
            Ok(tape) => {
                info!("recording a motion tape to {}", path.to_string_lossy());
                Some(tape)
            }
            Err(e) => {
                warn!("not recording a motion tape to {}: {e:#}", path.to_string_lossy());
                None
            }
        }
    }

    fn create(path: &std::ffi::OsStr, header: Header) -> anyhow::Result<Self> {
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(path).with_context(|| "create the tape file")?,
        );
        file.write_all(MAGIC)?;
        file.write_all(&[header.quality, chroma_byte(header.chroma)])?;
        let (tx, rx) = mpsc::channel::<Record>();
        let writer = std::thread::Builder::new()
            .name("motion-tape".into())
            .spawn(move || {
                for record in rx {
                    if let Err(e) = write_record(&mut file, &record) {
                        warn!("motion tape: {e:#}; stopping");
                        return;
                    }
                }
                if let Err(e) = file.flush() {
                    warn!("motion tape: flush: {e:#}");
                }
            })
            .context("spawn the tape writer")?;
        Ok(Self { tx: Some(tx), writer: Some(writer), opened: Instant::now() })
    }

    fn send(&self, record: Record) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(record);
        }
    }

    fn now_us(&self) -> u64 {
        self.opened.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
    }

    pub fn resize(&self, w: u16, h: u16, scale: f32) {
        self.send(Record::Resize { t_us: self.now_us(), w, h, scale });
    }

    /// `rgb` is `changed.rect` packed, exactly as `TileSink::damage`'s `pack` gives it.
    pub fn damage(&self, changed: &Changed, rgb: Vec<u8>) {
        self.send(Record::Damage {
            t_us: self.now_us(),
            rect: changed.rect,
            cells: changed.cells.clone(),
            rgb,
        });
    }

    pub fn frame(&self) {
        self.send(Record::Frame { t_us: self.now_us() });
    }
}

fn chroma_byte(chroma: Chroma) -> u8 {
    match chroma {
        Chroma::Subsampled => 0,
        Chroma::Full => 1,
    }
}

fn write_record(out: &mut impl Write, record: &Record) -> anyhow::Result<()> {
    match record {
        Record::Resize { t_us, w, h, scale } => {
            out.write_all(&[0])?;
            out.write_all(&t_us.to_le_bytes())?;
            out.write_all(&w.to_le_bytes())?;
            out.write_all(&h.to_le_bytes())?;
            out.write_all(&scale.to_le_bytes())?;
        }
        Record::Damage { t_us, rect, cells, rgb } => {
            let png = Tile::from_rgb(rect.left, rect.top, rect.w(), rect.h(), rgb)
                .context("encode a damage rectangle")?
                .data;
            out.write_all(&[1])?;
            out.write_all(&t_us.to_le_bytes())?;
            for edge in [rect.left, rect.top, rect.right, rect.bottom] {
                out.write_all(&edge.to_le_bytes())?;
            }
            out.write_all(&u32::try_from(cells.len())?.to_le_bytes())?;
            for (col, row) in cells {
                out.write_all(&col.to_le_bytes())?;
                out.write_all(&row.to_le_bytes())?;
            }
            out.write_all(&u32::try_from(png.len())?.to_le_bytes())?;
            out.write_all(&png)?;
        }
        Record::Frame { t_us } => {
            out.write_all(&[2])?;
            out.write_all(&t_us.to_le_bytes())?;
        }
    }
    Ok(())
}

/// Read a tape back, PNGs decoded to the packed RGB the encoder was handed.
#[cfg(test)]
pub fn read(path: impl AsRef<std::path::Path>) -> anyhow::Result<(Header, Vec<Record>)> {
    let bytes = std::fs::read(path).context("read the tape")?;
    let mut rest: &[u8] = &bytes;
    let mut take = |n: usize| -> anyhow::Result<&[u8]> {
        anyhow::ensure!(rest.len() >= n, "tape ends inside a record");
        let (head, tail) = rest.split_at(n);
        rest = tail;
        Ok(head)
    };
    anyhow::ensure!(take(MAGIC.len())? == MAGIC, "not a motion tape");
    let quality = take(1)?[0];
    let chroma = match take(1)?[0] {
        0 => Chroma::Subsampled,
        1 => Chroma::Full,
        other => anyhow::bail!("unknown chroma byte {other}"),
    };
    let header = Header { quality, chroma };
    let mut records = Vec::new();
    loop {
        let Ok(kind) = take(1) else {
            break;
        };
        let kind = kind[0];
        let t_us = u64::from_le_bytes(take(8)?.try_into()?);
        let record = match kind {
            0 => {
                let w = u16::from_le_bytes(take(2)?.try_into()?);
                let h = u16::from_le_bytes(take(2)?.try_into()?);
                let scale = f32::from_le_bytes(take(4)?.try_into()?);
                Record::Resize { t_us, w, h, scale }
            }
            1 => {
                let mut edges = [0u16; 4];
                for edge in &mut edges {
                    *edge = u16::from_le_bytes(take(2)?.try_into()?);
                }
                let rect = Rect { left: edges[0], top: edges[1], right: edges[2], bottom: edges[3] };
                let n = u32::from_le_bytes(take(4)?.try_into()?) as usize;
                let mut cells = Vec::with_capacity(n);
                for _ in 0..n {
                    let col = u16::from_le_bytes(take(2)?.try_into()?);
                    let row = u16::from_le_bytes(take(2)?.try_into()?);
                    cells.push((col, row));
                }
                let len = u32::from_le_bytes(take(4)?.try_into()?) as usize;
                let rgb = decode_png(take(len)?, rect)?;
                Record::Damage { t_us, rect, cells, rgb }
            }
            2 => Record::Frame { t_us },
            other => anyhow::bail!("unknown record kind {other}"),
        };
        records.push(record);
    }
    Ok((header, records))
}

#[cfg(test)]
fn decode_png(png: &[u8], rect: Rect) -> anyhow::Result<Vec<u8>> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().context("decode a damage rectangle")?;
    let mut buf = vec![0u8; reader.output_buffer_size().context("png output size")?];
    let info = reader.next_frame(&mut buf).context("decode a damage rectangle")?;
    anyhow::ensure!(
        info.width == u32::from(rect.w())
            && info.height == u32::from(rect.h())
            && info.color_type == png::ColorType::Rgb
            && info.bit_depth == png::BitDepth::Eight,
        "a damage rectangle decoded to something other than {}x{} RGB8",
        rect.w(),
        rect.h()
    );
    buf.truncate(info.buffer_size());
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tape reads back as what was recorded, pixels included.
    #[test]
    fn a_tape_round_trips() {
        let dir = std::env::temp_dir().join(format!("remotex-tape-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("round-trip.tape");
        let rect = Rect::from_size(3, 5, 70, 9).unwrap();
        let rgb: Vec<u8> = (0..usize::from(rect.w()) * usize::from(rect.h()) * 3)
            .map(|i| (i * 7 % 251) as u8)
            .collect();
        let changed = Changed { rect, cells: vec![(0, 0), (1, 0)] };
        {
            let tape = Tape::create(path.as_os_str(), Header { quality: 40, chroma: Chroma::Full })
                .unwrap();
            tape.resize(1280, 800, 1.0);
            tape.damage(&changed, rgb.clone());
            tape.frame();
        }
        let (header, records) = read(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(header, Header { quality: 40, chroma: Chroma::Full });
        assert_eq!(records.len(), 3);
        assert!(matches!(records[0], Record::Resize { w: 1280, h: 800, scale, .. } if scale == 1.0));
        match &records[1] {
            Record::Damage { rect: got, cells, rgb: got_rgb, .. } => {
                assert_eq!(*got, rect);
                assert_eq!(*cells, changed.cells);
                assert_eq!(*got_rgb, rgb);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(records[2], Record::Frame { .. }));
        assert!(records.windows(2).all(|w| w[0].t_us() <= w[1].t_us()));
    }
}
