//! TESTING ONLY: a picture sent as tiles, a PNG of each tile of the desktop that
//! changed ([`crate::config::PngTiles`]), in place of the VP9 encoded here.
//!
//! A Windows host on the bitmap path draws its desktop as 64×64 tiles on a grid
//! from the top left, each lossless (measured: see "Bitmap updates" in
//! docs/rdp-client.md). A tile here is a cell of that same grid, so a tile the
//! host drew goes to the browser whole: the shadow in front of this trims a
//! damaged rectangle to what changed, and the grid takes it back out to the cells
//! it touches. A cell whose pixels are the ones it was last sent is not sent again.
//!
//! The cells are taken off the engine's task and coded on a blocking worker
//! ([`Taken::encode`]), as a round of VP9 is. Unlike a round, nothing chains one
//! take to the next: each tile stands alone, and their order is the queue's.

use crate::protocol::{Held, TileUnit};
use crate::shadow::Rect;
use crate::video::Mirror;

/// A tile's side, in desktop pixels: the host's own.
pub const TILE: u16 = 64;

/// Cells along a side of `len` pixels: the last one clipped.
fn cells(len: u16) -> usize {
    usize::from(len).div_ceil(usize::from(TILE))
}

/// The desktop as blitted, what the client was last sent of it, and the cells
/// between the two.
pub struct Tiles {
    /// The desktop, learned from the `Resize` the client is told; `None` until one.
    size: Option<(u16, u16)>,
    /// The desktop as blitted. Built by the first blit, which is on the engine's
    /// `?` path, where a failure ends the session with its reason.
    mirror: Option<Mirror>,
    /// The desktop as the client was last sent it, cell by cell.
    sent: Option<Mirror>,
    /// Cells blitted into since they were last taken, row by row.
    dirty: Vec<bool>,
    /// How many of [`Self::dirty`] are set.
    owed: usize,
    /// Every cell is owed, changed or not: the client holds nothing of the desktop.
    whole: bool,
}

impl Default for Tiles {
    fn default() -> Self {
        Self::new()
    }
}

impl Tiles {
    pub fn new() -> Self {
        Self { size: None, mirror: None, sent: None, dirty: Vec::new(), owed: 0, whole: true }
    }

    /// Adopt the desktop the client is about to be told about. A different size
    /// starts over, with every cell owed.
    pub fn want(&mut self, w: u16, h: u16) {
        if self.size != Some((w, h)) {
            self.size = Some((w, h));
            self.mirror = None;
            self.sent = None;
            self.dirty = vec![false; cells(w) * cells(h)];
            self.owed = 0;
            self.whole = true;
        }
    }

    /// Owe the client every cell: it holds nothing of the desktop — a reattach, a
    /// repaint — and is to be sent the whole of it however little has changed.
    pub fn repaint(&mut self) {
        self.whole = true;
    }

    /// Whether the desktop has had a pixel, so a repaint has something to send.
    pub fn pictured(&self) -> bool {
        self.mirror.is_some()
    }

    /// Whether there is anything to take.
    pub fn dirty(&self) -> bool {
        self.mirror.is_some() && (self.whole || self.owed > 0)
    }

    /// Copy a changed rectangle's pixels, packed RGB888, into the desktop, and
    /// mark the cells it touches.
    pub fn blit(&mut self, rect: Rect, rgb: &[u8]) -> anyhow::Result<()> {
        if self.mirror.is_none() {
            let (w, h) = self
                .size
                .ok_or_else(|| anyhow::anyhow!("the tiles were asked for pixels before a desktop size"))?;
            let mirror = Mirror::new(w, h)?;
            crate::video::check_picture(mirror.size())?;
            self.sent = Some(mirror.clone());
            self.mirror = Some(mirror);
        }
        self.mirror.as_mut().expect("just built").blit(rect, rgb)?;
        let (w, _) = self.size.expect("a mirror means a size");
        let columns = cells(w);
        let (first_col, last_col) = (usize::from(rect.left / TILE), usize::from(rect.right / TILE));
        for row in usize::from(rect.top / TILE)..=usize::from(rect.bottom / TILE) {
            for col in first_col..=last_col {
                let cell = &mut self.dirty[row * columns + col];
                if !*cell {
                    *cell = true;
                    self.owed += 1;
                }
            }
        }
        Ok(())
    }

    /// The cells to send, cropped out of the desktop: every cell when the client
    /// holds nothing, and otherwise those blitted into whose pixels are not the
    /// ones the client was last sent. `None` when there is nothing to send.
    pub fn take(&mut self) -> anyhow::Result<Option<Taken>> {
        if !self.dirty() {
            return Ok(None);
        }
        let (Some(mirror), Some(sent), Some((w, h))) = (&self.mirror, &mut self.sent, self.size) else {
            return Ok(None);
        };
        let whole = std::mem::take(&mut self.whole);
        let columns = cells(w);
        let mut crops = Vec::new();
        let (mut now, mut before) = (Vec::new(), Vec::new());
        for (at, cell) in self.dirty.iter_mut().enumerate() {
            if !whole && !*cell {
                continue;
            }
            *cell = false;
            let (col, row) = ((at % columns) as u16, (at / columns) as u16);
            let (x, y) = (col * TILE, row * TILE);
            let rect = Rect::from_size(x, y, TILE.min(w - x), TILE.min(h - y))
                .expect("a cell inside the desktop has a size");
            mirror.crop_into(rect, &mut now)?;
            if !whole {
                sent.crop_into(rect, &mut before)?;
                if now == before {
                    continue;
                }
            }
            sent.adopt(mirror, rect);
            crops.push(Crop { rect, rgb: std::mem::take(&mut now) });
        }
        self.owed = 0;
        Ok((!crops.is_empty()).then_some(Taken { whole, crops }))
    }
}

/// One cell of the desktop, cropped for its PNG.
struct Crop {
    rect: Rect,
    rgb: Vec<u8>,
}

/// The cells one [`Tiles::take`] found to send, for a blocking worker to code.
pub struct Taken {
    /// Every cell of the desktop is here, and the client is to start from them.
    pub whole: bool,
    crops: Vec<Crop>,
}

impl Taken {
    /// How many tiles this is.
    pub fn len(&self) -> usize {
        self.crops.len()
    }

    /// Never: a take with nothing in it is `None`.
    pub fn is_empty(&self) -> bool {
        self.crops.is_empty()
    }

    /// Code each cell as a PNG. Blocking: call it on a worker.
    pub fn encode(self) -> anyhow::Result<Vec<TileUnit>> {
        self.crops
            .into_iter()
            .map(|Crop { rect, rgb }| {
                let data = crate::protocol::encode_png(rect.w(), rect.h(), png::ColorType::Rgb, &rgb)?;
                Ok(TileUnit { x: rect.left, y: rect.top, w: rect.w(), h: rect.h(), data, held: Held::default() })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a rectangle with a size")
    }

    fn flat(rect: Rect, value: u8) -> Vec<u8> {
        vec![value; usize::from(rect.w()) * usize::from(rect.h()) * 3]
    }

    /// Where each taken tile is, and its size.
    fn places(taken: &Taken) -> Vec<(u16, u16, u16, u16)> {
        taken.crops.iter().map(|c| (c.rect.left, c.rect.top, c.rect.w(), c.rect.h())).collect()
    }

    /// A desktop whose sides are no multiple of the tile: the first take is every
    /// cell, the last column and row clipped to what is left, as the host clips
    /// its own.
    #[test]
    fn the_first_take_is_every_cell_clipped_at_the_edges() {
        let mut tiles = Tiles::new();
        tiles.want(150, 70);
        assert!(!tiles.dirty(), "nothing to send before a pixel");
        let whole = placed(0, 0, 150, 70);
        tiles.blit(whole, &flat(whole, 9)).unwrap();
        let taken = tiles.take().unwrap().expect("the whole desktop");
        assert!(taken.whole);
        assert_eq!(
            places(&taken),
            vec![(0, 0, 64, 64), (64, 0, 64, 64), (128, 0, 22, 64), (0, 64, 64, 6), (64, 64, 64, 6), (128, 64, 22, 6)]
        );
        assert!(!tiles.dirty() && tiles.take().unwrap().is_none());
    }

    /// A change is sent as the cells it touches, whole; a cell blitted with the
    /// pixels it was last sent is not sent again; a repaint sends every cell.
    #[test]
    fn a_change_is_sent_as_the_whole_cells_it_touches() {
        let mut tiles = Tiles::new();
        tiles.want(256, 128);
        let whole = placed(0, 0, 256, 128);
        tiles.blit(whole, &flat(whole, 0)).unwrap();
        tiles.take().unwrap();

        // Across the corner of four cells.
        let corner = placed(60, 60, 8, 8);
        tiles.blit(corner, &flat(corner, 200)).unwrap();
        let taken = tiles.take().unwrap().expect("the change");
        assert!(!taken.whole);
        assert_eq!(places(&taken), vec![(0, 0, 64, 64), (64, 0, 64, 64), (0, 64, 64, 64), (64, 64, 64, 64)]);
        let cell = &taken.crops[0].rgb;
        assert_eq!(cell.len(), 64 * 64 * 3, "a cell is cropped whole");
        assert_eq!(cell[(63 * 64 + 63) * 3], 200, "with the change in it");
        assert_eq!(cell[0], 0, "and what did not change around it");

        // The same pixels again: nothing.
        tiles.blit(corner, &flat(corner, 200)).unwrap();
        assert!(tiles.dirty(), "a blit is owed a look");
        assert!(tiles.take().unwrap().is_none(), "cells as they were last sent went again");

        // One cell of the four goes back: only it.
        let back = placed(60, 60, 4, 4);
        tiles.blit(back, &flat(back, 0)).unwrap();
        assert_eq!(places(&tiles.take().unwrap().expect("the one cell")), vec![(0, 0, 64, 64)]);

        tiles.repaint();
        let taken = tiles.take().unwrap().expect("a repaint");
        assert!(taken.whole);
        assert_eq!(taken.len(), 8, "a repaint is every cell, changed or not");
    }

    /// A new size starts over: nothing was sent of it, so every cell is owed.
    #[test]
    fn a_resize_owes_every_cell_of_the_new_desktop() {
        let mut tiles = Tiles::new();
        tiles.want(128, 64);
        let whole = placed(0, 0, 128, 64);
        tiles.blit(whole, &flat(whole, 1)).unwrap();
        tiles.take().unwrap();
        tiles.want(64, 64);
        let small = placed(0, 0, 8, 8);
        tiles.blit(small, &flat(small, 1)).unwrap();
        let taken = tiles.take().unwrap().expect("the new desktop");
        assert!(taken.whole);
        assert_eq!(places(&taken), vec![(0, 0, 64, 64)]);
    }

    /// Each tile codes to a PNG of itself, read back by an independent decoder.
    #[test]
    fn each_tile_is_a_png_of_its_own_pixels() {
        let mut tiles = Tiles::new();
        tiles.want(70, 64);
        let whole = placed(0, 0, 70, 64);
        let mut rgb = flat(whole, 0);
        for (i, px) in rgb.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            px.copy_from_slice(&[(i % 70) as u8, (i / 70) as u8, 77]);
        }
        tiles.blit(whole, &rgb).unwrap();
        let units = tiles.take().unwrap().expect("the desktop").encode().unwrap();
        assert_eq!(units.len(), 2);
        let edge = &units[1];
        assert_eq!((edge.x, edge.y, edge.w, edge.h), (64, 0, 6, 64));
        let decoder = png::Decoder::new(std::io::Cursor::new(&edge.data));
        let mut reader = decoder.read_info().unwrap();
        let mut out = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut out).unwrap();
        assert_eq!((info.width, info.height, info.color_type), (6, 64, png::ColorType::Rgb));
        // Row 5, column 2 of the tile is the desktop's (66, 5).
        assert_eq!(&out[(5 * 6 + 2) * 3..][..3], &[66, 5, 77]);
    }
}
