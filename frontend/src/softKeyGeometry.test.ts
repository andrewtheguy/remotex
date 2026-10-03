// Which key a point belongs to, given the keys' boxes. Run with
// `bun test src/softKeyGeometry.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  createHitTester,
  type GeometryRow,
  type Rect,
} from "./softKeyGeometry.ts";

const box = (left: number, top: number, w: number, h: number): Rect => ({
  left,
  top,
  right: left + w,
  bottom: top + h,
});

// Two rows of 40px keys under a 30px shortcut row the table leaves out. The
// second row has half a key of spacer at each end, like the home row.
const ROW_H = 40;
const rows: GeometryRow[] = [
  {
    rect: box(0, 30, 400, ROW_H),
    cells: ["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"].map((id, i) => ({
      id,
      rect: box(i * 40, 30, 40, ROW_H),
      spacer: false,
    })),
  },
  {
    rect: box(0, 70, 400, ROW_H),
    cells: [
      { id: "gapL", rect: box(0, 70, 20, ROW_H), spacer: true },
      ...["a", "s", "d", "f", "g", "h", "j", "k", "l"].map((id, i) => ({
        id,
        rect: box(20 + i * 40, 70, 40, ROW_H),
        spacer: false,
      })),
      { id: "gapR", rect: box(380, 70, 20, ROW_H), spacer: true },
    ],
  },
];
// The key area: side padding of 4px, 10px below the last row, nothing above the
// first row (the shortcut row sits there).
const bounds = box(-4, 30, 408, 90);
const hit = createHitTester(rows, bounds);

test("a point on a key is that key", () => {
  assert.equal(hit(60, 50, null), "w");
  assert.equal(hit(60, 90, null), "s");
});

test("a point in the side padding is the end key of its row", () => {
  assert.equal(hit(-3, 50, null), "q");
  assert.equal(hit(403, 50, null), "p");
});

test("a spacer belongs to its neighbour", () => {
  assert.equal(hit(10, 90, null), "a");
  assert.equal(hit(390, 90, null), "l");
  assert.equal(hit(-3, 90, null), "a");
});

test("a point below the last row is still the last row; above the area is nothing", () => {
  assert.equal(hit(60, 118, null), "s");
  assert.equal(hit(60, 20, null), null);
  assert.equal(hit(60, 130, null), null);
});

test("a key already under the finger keeps it within the slop", () => {
  assert.equal(hit(81, 50, "w"), "w");
  assert.equal(hit(84, 50, "w"), "e");
  assert.equal(hit(81, 50, null), "e");
});

test("the slop never holds a key across a row", () => {
  assert.equal(hit(60, 71, "w"), "w");
  assert.equal(hit(60, 74, "w"), "s");
});

test("beside every row, the nearest row takes the point", () => {
  const side: GeometryRow[] = [
    ...rows,
    {
      rect: box(420, 30, 120, ROW_H),
      cells: ["ins", "home", "pgup"].map((id, i) => ({
        id,
        rect: box(420 + i * 40, 30, 40, ROW_H),
        spacer: false,
      })),
    },
  ];
  const wide = createHitTester(side, box(-4, 30, 548, 90));
  // In the gap between the blocks, level with both rows: an end key of one.
  assert.ok(["p", "ins"].includes(wide(410, 50, null) ?? ""));
  // Well below the side cluster and just right of the home row: the home row.
  assert.equal(wide(430, 110, null), "l");
});

test("an empty table answers nothing", () => {
  assert.equal(createHitTester([], bounds)(60, 50, null), null);
});
