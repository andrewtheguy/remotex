// Where a finger is on the soft keyboard, from a table of the cells' boxes.
//
// A geometry table rather than `elementFromPoint`: hit-testing the DOM flushes
// layout on every pointer move of every finger, answers with the preview bubble
// or a cosmetic keycap's gap instead of the key, cannot clamp a finger that has
// slid past the end of a row back onto its last key, and cannot be unit-tested.
// The table is measured once per layout (softKeyPressHost in the panel) and read
// here with the rules a keyboard wants: every point inside the key area belongs
// to some key, a spacer belongs to its neighbour, and a key already under the
// finger keeps it until the finger is clearly past the boundary.
import type { CellId } from "./softKeyboard.ts";
import type { HitTester } from "./softKeyPress.ts";

export interface Rect {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

export interface GeometryCell {
  id: CellId;
  rect: Rect;
  // A spacer is inert: a finger on it goes to the nearest real neighbour.
  spacer: boolean;
}

export interface GeometryRow {
  rect: Rect;
  // In visual order, left to right.
  cells: GeometryCell[];
}

export interface GeometryOptions {
  // How far past a key's edge the finger may be before the key it is already on
  // lets go of it. Keeps the active key from flickering on a boundary.
  slopPx?: number;
}

const DEFAULT_SLOP_PX = 3;

function inside(r: Rect, x: number, y: number, pad = 0): boolean {
  return (
    x >= r.left - pad &&
    x <= r.right + pad &&
    y >= r.top - pad &&
    y <= r.bottom + pad
  );
}

// How far a point is from a box, zero inside it.
function distance(r: Rect, x: number, y: number): number {
  const dx = x < r.left ? r.left - x : x > r.right ? x - r.right : 0;
  const dy = y < r.top ? r.top - y : y > r.bottom ? y - r.bottom : 0;
  return Math.hypot(dx, dy);
}

function neighbourOf(
  cells: GeometryCell[],
  index: number,
  x: number,
): CellId | null {
  const spacer = cells[index];
  const mid = (spacer.rect.left + spacer.rect.right) / 2;
  const order = x < mid ? [-1, 1] : [1, -1];
  for (const dir of order) {
    for (let i = index + dir; i >= 0 && i < cells.length; i += dir) {
      if (!cells[i].spacer) {
        return cells[i].id;
      }
    }
  }
  return null;
}

// The row a point is in, or the closest when it is in the padding or in the
// gap between the PC grid's main block and its side cluster.
function nearestRow(
  rows: readonly GeometryRow[],
  x: number,
  y: number,
): GeometryRow {
  let row = rows[0];
  let best = distance(row.rect, x, y);
  for (const r of rows) {
    const d = distance(r.rect, x, y);
    if (d < best) {
      best = d;
      row = r;
    }
  }
  return row;
}

// The key at `x` in a row, the end keys taking what lies beyond them and a
// spacer handing over to its neighbour.
function cellAt(row: GeometryRow, x: number): CellId | null {
  const cx = Math.min(Math.max(x, row.rect.left), row.rect.right);
  let index = row.cells.findIndex((cell) => cx < cell.rect.right);
  if (index < 0) {
    index = row.cells.length - 1;
  }
  const cell = row.cells[index];
  return cell.spacer ? neighbourOf(row.cells, index, cx) : cell.id;
}

// `bounds` is the key area a finger may be in at all: outside it the answer is
// null, which is how a finger that leaves the keyboard commits nothing. Within
// it a point above, below or beside every row still resolves to the nearest row
// and the nearest key in it — the panel's padding is not dead space.
export function createHitTester(
  rows: readonly GeometryRow[],
  bounds: Rect,
  options: GeometryOptions = {},
): HitTester {
  const slop = options.slopPx ?? DEFAULT_SLOP_PX;
  const byId = new Map<CellId, GeometryCell>();
  for (const r of rows) {
    for (const cell of r.cells) {
      byId.set(cell.id, cell);
    }
  }
  return (x, y, prefer) => {
    if (rows.length === 0 || !inside(bounds, x, y)) {
      return null;
    }
    // A spacer is never kept: it was never the answer, and would commit nothing.
    if (prefer !== null) {
      const held = byId.get(prefer);
      if (held && !held.spacer && inside(held.rect, x, y, slop)) {
        return prefer;
      }
    }
    return cellAt(nearestRow(rows, x, y), x);
  };
}
