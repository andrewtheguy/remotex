//! Copy detection over the shadow: a scroll becomes `COPY` records instead of image bytes.
//!
//! The mechanism is guacamole-server's (`display-plan-search.c`), reshaped for this
//! gateway's operands. Every dirty 64×64-aligned cell of the *new* frame is hashed into a
//! 65,536-slot table; a 64×64 window then slides over the *shadow* — the pixels the client
//! already holds, which is the only thing a copy may read — across the damaged region, and
//! a hash hit that byte-verifies becomes a copy: the client moves pixels on its own canvas
//! and the image bytes never travel. The search is confined to the damage because a scroll
//! dirties everything it moves, source and destination alike.
//!
//! **A wrong copy cannot corrupt, only waste.** [`crate::tiles::Shadow::copy_within`]
//! applies each copy to the shadow exactly as the client applies it to its canvas, so the
//! two stay in lockstep whatever the copies do — and the tile pass that follows every
//! flush compares that shadow against the real frame and repaints whatever a copy got
//! wrong. The byte-verification here is therefore an economy, not a safety: it keeps the
//! waste near zero, while correctness rests where it always did.
//!
//! What this does not do, on purpose: search across frames older than the last one (the
//! shadow holds exactly one past), consider cells the shadow has not fully learned (the
//! whole search is gated on a fully-known shadow, which is the steady state of any live
//! session), or find a region tiled with copies of one pattern more than once (the table
//! holds one cell per distinct content — guacamole-server's documented limitation, shared
//! knowingly).

use crate::tiles::{Rect, Shadow};

/// The search granularity, and guacamole-server's: big enough that a match is
/// overwhelmingly a real move, small enough that a scrolled pane's edge waste is thin.
const CELL: u16 = 64;

/// Fewer dirty cells than this and the search does not run. A scroll dirties dozens of
/// cells; a caret, a clock and a hover highlight dirty a few — and the search costs a
/// walk over the damage's whole bounding box, which two far-apart small updates can
/// stretch across the desktop.
const MIN_CELLS: usize = 8;

/// One slot per 16-bit fold of the content hash, guacamole-server's shape: a flat array
/// beats a real map at the ~2M probes a full-screen scroll makes, and a collision only
/// costs an opportunity — the stored hash is checked, and every candidate is
/// byte-verified.
const TABLE_SLOTS: usize = 1 << 16;

/// Polynomial bases for the rolling hashes, one per axis so a transposed block does not
/// alias. Any odd 64-bit constants work; wrapping arithmetic is the modulus.
const ROW_BASE: u64 = 0x0000_0100_0000_01B3;
const COL_BASE: u64 = 0x9E37_79B9_7F4A_7C15;

/// One copy the client should perform: `src` and `dst` are the same size, both on the
/// client's canvas. Emission order matters between overlapping copies and the planner has
/// already chosen it — apply and send these in the order given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedCopy {
    pub src: Rect,
    pub dst: Rect,
}

/// Plan the copies that would carry this flush's damage without image bytes.
///
/// `new` is the live framebuffer in RGBX32 at `stride` bytes per row; `damage` is the
/// flush's staged rectangles, uncrossed and undrained. Returns the copies to apply — via
/// [`Shadow::copy_within`] and a `COPY` record, before the tile pass — or nothing when the
/// search is not worth making: a shadow with unknown pixels, a shadow that disagrees with
/// the frame about the desktop size (one turn of a resize), or damage too small to be a
/// scroll.
pub fn plan(
    new: &[u8],
    stride: usize,
    fw: u16,
    fh: u16,
    damage: &[Rect],
    shadow: &Shadow,
) -> Vec<PlannedCopy> {
    if !shadow.all_known() || shadow.size() != (fw, fh) || fw < CELL || fh < CELL {
        return Vec::new();
    }

    // The dirty cells, 64-aligned and fully inside the frame. A partial cell at the right
    // or bottom edge is skipped rather than special-cased: its pixels still travel as
    // tiles, and a hash of a 64×64 window can only ever match a full cell anyway.
    let mut cells: Vec<(u16, u16)> = Vec::new();
    for r in damage {
        let right = r.right.min(fw - 1);
        let bottom = r.bottom.min(fh - 1);
        if r.left > right || r.top > bottom {
            continue;
        }
        for cy in (r.top / CELL)..=(bottom / CELL) {
            let top = cy * CELL;
            if u32::from(top) + u32::from(CELL) > u32::from(fh) {
                break;
            }
            for cx in (r.left / CELL)..=(right / CELL) {
                let left = cx * CELL;
                if u32::from(left) + u32::from(CELL) > u32::from(fw) {
                    break;
                }
                cells.push((left, top));
            }
        }
    }
    cells.sort_unstable();
    cells.dedup();
    if cells.len() < MIN_CELLS {
        return Vec::new();
    }

    // Index the *new* content of every dirty cell. First come keeps the slot, exactly as
    // guacamole-server does: replacing on collision would let the last cell of a tiled
    // pattern shadow all the others, and keeping the first is as good as any other rule
    // once every hit is verified.
    let row_out = pow(ROW_BASE, u32::from(CELL));
    let col_out = pow(COL_BASE, u32::from(CELL));
    #[derive(Clone, Copy)]
    struct Slot {
        hash: u64,
        cell: u32,
    }
    let mut table = vec![Slot { hash: 0, cell: u32::MAX }; TABLE_SLOTS];
    for (i, &(left, top)) in cells.iter().enumerate() {
        let mut h = 0u64;
        for y in top..top + CELL {
            let row = &new[usize::from(y) * stride + usize::from(left) * 4..];
            let mut rh = 0u64;
            for x in 0..usize::from(CELL) {
                rh = rh.wrapping_mul(ROW_BASE).wrapping_add(px4(&row[x * 4..]));
            }
            h = h.wrapping_mul(COL_BASE).wrapping_add(rh);
        }
        let slot = &mut table[fold(h)];
        if slot.cell == u32::MAX {
            *slot = Slot { hash: h, cell: i as u32 };
        }
    }

    // The search region: the damage's bounding box. Everything a scroll moved is dirty at
    // both ends of the move, so the source content is inside it.
    let mut bound = damage[0];
    for r in &damage[1..] {
        bound.left = bound.left.min(r.left);
        bound.top = bound.top.min(r.top);
        bound.right = bound.right.max(r.right);
        bound.bottom = bound.bottom.max(r.bottom);
    }
    bound.right = bound.right.min(fw - 1);
    bound.bottom = bound.bottom.min(fh - 1);
    let cols = usize::from(bound.right - bound.left) + 1;
    let rows = usize::from(bound.bottom - bound.top) + 1;
    if cols < usize::from(CELL) || rows < usize::from(CELL) {
        return Vec::new();
    }

    // Slide the 64×64 window over the shadow: per-row rolling hashes, rolled again down
    // the columns, so the whole region costs O(pixels) and each window position one probe.
    let rgb = shadow.rgb();
    let sw = usize::from(fw);
    let wcols = cols - usize::from(CELL) + 1;
    let mut ring: Vec<Vec<u64>> = vec![vec![0; wcols]; usize::from(CELL)];
    let mut scratch: Vec<u64> = vec![0; wcols];
    let mut colh: Vec<u64> = vec![0; wcols];
    let mut matched: Vec<Option<(u16, u16)>> = vec![None; cells.len()];
    for row_i in 0..rows {
        let y = bound.top + row_i as u16;
        // This row's 64-wide window hashes.
        {
            let base = (usize::from(y) * sw + usize::from(bound.left)) * 3;
            let row = &rgb[base..base + cols * 3];
            let mut rh = 0u64;
            for i in 0..cols {
                rh = rh.wrapping_mul(ROW_BASE).wrapping_add(px3(&row[i * 3..]));
                if i >= usize::from(CELL) {
                    let out = px3(&row[(i - usize::from(CELL)) * 3..]);
                    rh = rh.wrapping_sub(out.wrapping_mul(row_out));
                }
                if i >= usize::from(CELL) - 1 {
                    scratch[i - (usize::from(CELL) - 1)] = rh;
                }
            }
        }
        // Roll the columns: add this row, retire the row 64 above it.
        let slot_row = row_i % usize::from(CELL);
        for x in 0..wcols {
            colh[x] = colh[x].wrapping_mul(COL_BASE).wrapping_add(scratch[x]);
            if row_i >= usize::from(CELL) {
                colh[x] = colh[x].wrapping_sub(ring[slot_row][x].wrapping_mul(col_out));
            }
        }
        std::mem::swap(&mut ring[slot_row], &mut scratch);
        if row_i < usize::from(CELL) - 1 {
            continue;
        }
        let win_top = y - (CELL - 1);
        for (xk, &h) in colh.iter().enumerate() {
            let slot = &mut table[fold(h)];
            if slot.cell == u32::MAX || slot.hash != h {
                continue;
            }
            let i = slot.cell as usize;
            let (dl, dt) = cells[i];
            let src = (bound.left + xk as u16, win_top);
            // The same place is not a move: those pixels are already on the client's
            // canvas, and the tile pass will find them unchanged for free.
            if src == (dl, dt) {
                continue;
            }
            if verified(new, stride, (dl, dt), rgb, sw, src) {
                matched[i] = Some(src);
                slot.cell = u32::MAX;
            }
        }
    }

    merge(&cells, &matched)
}

/// Merge matched cells into the fewest copies, grouped by displacement, and order each
/// group so overlapping copies read their sources before another copy overwrites them —
/// a downward move emits bottom-up, an upward one top-down, and likewise across. Order is
/// an economy like the verification: a clobbered source only costs the repaint the tile
/// pass already owes it.
fn merge(cells: &[(u16, u16)], matched: &[Option<(u16, u16)>]) -> Vec<PlannedCopy> {
    let mut by_shift: std::collections::BTreeMap<(i32, i32), Vec<(u16, u16)>> =
        std::collections::BTreeMap::new();
    for (i, m) in matched.iter().enumerate() {
        if let Some((sx, sy)) = m {
            let (dl, dt) = cells[i];
            let shift = (i32::from(dl) - i32::from(*sx), i32::from(dt) - i32::from(*sy));
            by_shift.entry(shift).or_default().push((dl, dt));
        }
    }

    let mut out = Vec::new();
    for ((dx, dy), mut group) in by_shift {
        // Horizontal runs of adjacent cells, then equal-width runs stacked vertically.
        group.sort_unstable_by_key(|&(l, t)| (t, l));
        let mut strips: Vec<Rect> = Vec::new();
        for (l, t) in group {
            match strips.last_mut() {
                Some(s) if s.top == t && s.right + 1 == l => s.right += CELL,
                _ => strips.push(Rect { left: l, top: t, right: l + CELL - 1, bottom: t + CELL - 1 }),
            }
        }
        strips.sort_unstable_by_key(|s| (s.left, s.right, s.top));
        let mut rects: Vec<Rect> = Vec::new();
        for s in strips {
            match rects.last_mut() {
                Some(r) if r.left == s.left && r.right == s.right && r.bottom + 1 == s.top => {
                    r.bottom = s.bottom;
                }
                _ => rects.push(s),
            }
        }
        rects.sort_unstable_by(|a, b| {
            use std::cmp::Ordering;
            match dy.cmp(&0) {
                Ordering::Greater => b.top.cmp(&a.top),
                Ordering::Less => a.top.cmp(&b.top),
                Ordering::Equal if dx > 0 => b.left.cmp(&a.left),
                Ordering::Equal => a.left.cmp(&b.left),
            }
        });
        out.extend(rects.into_iter().map(|dst| PlannedCopy {
            src: Rect {
                left: (i32::from(dst.left) - dx) as u16,
                top: (i32::from(dst.top) - dy) as u16,
                right: (i32::from(dst.right) - dx) as u16,
                bottom: (i32::from(dst.bottom) - dy) as u16,
            },
            dst,
        }));
    }
    out
}

/// Whether the new frame's cell at `dst` and the shadow's window at `src` hold the same
/// pixels — the check that turns a hash hit into a copy, across the two layouts (RGBX32
/// against packed RGB888) without materializing either.
fn verified(
    new: &[u8],
    stride: usize,
    dst: (u16, u16),
    rgb: &[u8],
    sw: usize,
    src: (u16, u16),
) -> bool {
    for row in 0..usize::from(CELL) {
        let n = &new[(usize::from(dst.1) + row) * stride + usize::from(dst.0) * 4..];
        let o = &rgb[((usize::from(src.1) + row) * sw + usize::from(src.0)) * 3..];
        for x in 0..usize::from(CELL) {
            if n[x * 4..x * 4 + 3] != o[x * 3..x * 3 + 3] {
                return false;
            }
        }
    }
    true
}

fn px4(bytes: &[u8]) -> u64 {
    u64::from(bytes[0]) << 16 | u64::from(bytes[1]) << 8 | u64::from(bytes[2])
}

fn px3(bytes: &[u8]) -> u64 {
    u64::from(bytes[0]) << 16 | u64::from(bytes[1]) << 8 | u64::from(bytes[2])
}

fn fold(h: u64) -> usize {
    ((h ^ (h >> 16) ^ (h >> 32) ^ (h >> 48)) & 0xFFFF) as usize
}

fn pow(base: u64, exp: u32) -> u64 {
    let mut out = 1u64;
    for _ in 0..exp {
        out = out.wrapping_mul(base);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u16 = 256;
    const H: u16 = 256;

    /// A frame whose every pixel is distinct enough that nothing matches by accident.
    fn pixel(x: u16, y: u16) -> [u8; 3] {
        let v = u32::from(x) * 7919 + u32::from(y) * 104729;
        [(v >> 16) as u8, (v >> 8) as u8, v as u8]
    }

    fn rgb_frame(shift: impl Fn(u16, u16) -> (u16, u16)) -> Vec<u8> {
        let mut out = Vec::with_capacity(usize::from(W) * usize::from(H) * 3);
        for y in 0..H {
            for x in 0..W {
                let (sx, sy) = shift(x, y);
                out.extend_from_slice(&pixel(sx, sy));
            }
        }
        out
    }

    fn rgbx_frame(shift: impl Fn(u16, u16) -> (u16, u16)) -> Vec<u8> {
        let mut out = Vec::with_capacity(usize::from(W) * usize::from(H) * 4);
        for y in 0..H {
            for x in 0..W {
                let (sx, sy) = shift(x, y);
                out.extend_from_slice(&pixel(sx, sy));
                out.push(0);
            }
        }
        out
    }

    fn known_shadow(rgb: &[u8]) -> Shadow {
        let mut shadow = Shadow::new("test", W, H);
        shadow.accept(
            Rect { left: 0, top: 0, right: W - 1, bottom: H - 1 },
            rgb,
        );
        assert!(shadow.all_known());
        shadow
    }

    fn full_damage() -> Vec<Rect> {
        vec![Rect { left: 0, top: 0, right: W - 1, bottom: H - 1 }]
    }

    /// The headline case: the frame scrolled up by 32 pixels, so every cell whose source
    /// stayed on screen becomes part of one merged copy and no image bytes are owed for it.
    #[test]
    fn an_upward_scroll_becomes_one_copy() {
        let shadow = known_shadow(&rgb_frame(|x, y| (x, y)));
        let new = rgbx_frame(|x, y| (x, y + 32)); // new row y shows old row y+32
        let plans = plan(&new, usize::from(W) * 4, W, H, &full_damage(), &shadow);
        assert_eq!(
            plans,
            vec![PlannedCopy {
                src: Rect { left: 0, top: 32, right: W - 1, bottom: 223 },
                dst: Rect { left: 0, top: 0, right: W - 1, bottom: 191 },
            }],
            "three full cell rows moved up by 32 should merge into one copy"
        );
    }

    /// A downward scroll must come out bottom-up, or the first copy executed would
    /// overwrite the second one's source on the client's canvas.
    #[test]
    fn a_downward_scroll_reads_its_sources_before_overwriting_them() {
        let shadow = known_shadow(&rgb_frame(|x, y| (x, y)));
        let new = rgbx_frame(|x, y| (x, y.saturating_sub(64)));
        let mut damage = full_damage();
        // Split the damage; the plan must still see the whole picture.
        damage.push(Rect { left: 0, top: 0, right: W - 1, bottom: 10 });
        let plans = plan(&new, usize::from(W) * 4, W, H, &damage, &shadow);
        assert!(!plans.is_empty(), "a 64-pixel downward scroll should be found");
        for pair in plans.windows(2) {
            assert!(
                pair[0].dst.top >= pair[1].dst.top,
                "downward copies must be ordered bottom-up, got {pair:?}"
            );
        }
        for p in &plans {
            assert_eq!(i32::from(p.dst.top) - i32::from(p.src.top), 64);
        }
    }

    /// Fresh content matches nothing, and the search says so rather than inventing moves.
    #[test]
    fn unrelated_content_plans_no_copies() {
        let shadow = known_shadow(&rgb_frame(|x, y| (x, y)));
        let new = rgbx_frame(|x, y| (x + 1000, y + 1000));
        assert!(plan(&new, usize::from(W) * 4, W, H, &full_damage(), &shadow).is_empty());
    }

    /// An unchanged frame is not a move: the tile pass already carries "nothing changed"
    /// for free, and a zero-displacement copy would be a record for no effect.
    #[test]
    fn an_unchanged_frame_plans_no_copies() {
        let shadow = known_shadow(&rgb_frame(|x, y| (x, y)));
        let new = rgbx_frame(|x, y| (x, y));
        assert!(plan(&new, usize::from(W) * 4, W, H, &full_damage(), &shadow).is_empty());
    }

    /// A shadow with pixels it never learned cannot be a copy source, so the whole search
    /// declines rather than reading stale bytes.
    #[test]
    fn an_incomplete_shadow_declines_the_search() {
        let shadow = Shadow::new("test", W, H);
        let new = rgbx_frame(|x, y| (x, y + 32));
        assert!(plan(&new, usize::from(W) * 4, W, H, &full_damage(), &shadow).is_empty());
    }

    /// Damage smaller than a plausible scroll is not searched: the win lives in scrolls,
    /// and a caret's flush should not pay for a desktop-wide walk.
    #[test]
    fn small_damage_is_not_searched() {
        let shadow = known_shadow(&rgb_frame(|x, y| (x, y)));
        let new = rgbx_frame(|x, y| (x, y + 32));
        let damage = vec![Rect { left: 0, top: 0, right: 100, bottom: 100 }];
        assert!(plan(&new, usize::from(W) * 4, W, H, &damage, &shadow).is_empty());
    }

    /// The shadow applying a planned copy must land exactly on the new frame's pixels —
    /// the lockstep that makes a copy safe to send.
    #[test]
    fn an_applied_plan_matches_the_new_frame() {
        let mut shadow = known_shadow(&rgb_frame(|x, y| (x, y)));
        let new = rgbx_frame(|x, y| (x, y + 32));
        let plans = plan(&new, usize::from(W) * 4, W, H, &full_damage(), &shadow);
        for p in &plans {
            assert_eq!(shadow.copy_within(p.src, p.dst), Some(true));
        }
        let copied = plans[0].dst;
        let moved = shadow.copy_out(copied).expect("the copied region is known");
        for y in copied.top..=copied.bottom {
            for x in copied.left..=copied.right {
                let at = ((usize::from(y) - usize::from(copied.top))
                    * usize::from(copied.w())
                    + (usize::from(x) - usize::from(copied.left)))
                    * 3;
                assert_eq!(
                    &moved[at..at + 3],
                    pixel(x, y + 32),
                    "shadow pixel at {x},{y} after the copy"
                );
            }
        }
    }
}

