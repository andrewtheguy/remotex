// Which strips a software decoder's picture holds: drawn only once all four have
// come since a keyframe.
//
// Run with `bun test src/planesPicture.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";

import { placedAfter, rowsBelow, uploadOf } from "./planesPicture.ts";

const strip = (index: number, keyframe = false) => ({ strip: index, keyframe });

test("a display is whole once every strip has come since its keyframe", () => {
  let placed = 0;
  for (const s of [strip(0, true), strip(1), strip(2)]) {
    placed = placedAfter(placed, s);
    assert.notEqual(placed, 0b1111);
  }
  placed = placedAfter(placed, strip(3));
  assert.equal(placed, 0b1111);
  // Each strip after changes its own rows, and the display stays whole.
  assert.equal(placedAfter(placed, strip(1)), 0b1111);
});

test("a keyframe starts the display over", () => {
  // What the other strips hold is from before whatever the keyframe mends.
  assert.equal(placedAfter(0b1111, strip(0, true)), 0b0001);
});

test("a picture whole is every strip", () => {
  assert.equal(placedAfter(0, { strip: undefined, keyframe: false }), 0b1111);
});

// Strip 3 of a display, its strips `pitch` rows apart, at the planes' layout.
const lastStrip = (
  height: number,
  pitch: number,
  subsampling: "4:4:4" | "4:2:0",
  fullRange: boolean,
) => {
  const row = 3 * pitch;
  const rows = Math.min(pitch, height - row);
  const chroma = subsampling === "4:2:0" ? (rows + 1) >> 1 : rows;
  return {
    height,
    subsampling,
    row,
    strip: 3,
    fullRange,
    planes: [{ rows }, { rows: chroma }, { rows: chroma }],
  };
};

test("rows past the last strip are black", () => {
  // 834 rows in strips of 208 leave the last two, black in the stream's range.
  assert.deepEqual(rowsBelow(lastStrip(834, 208, "4:4:4", false)), [
    { unit: 0, row: 832, rows: 2, value: 16 },
    { unit: 1, row: 832, rows: 2, value: 128 },
    { unit: 2, row: 832, rows: 2, value: 128 },
  ]);
  assert.equal(rowsBelow(lastStrip(834, 208, "4:4:4", true))[0].value, 0);
  // Halved chroma down the picture leaves one row of it.
  assert.deepEqual(
    rowsBelow(lastStrip(834, 208, "4:2:0", true)).map((r) => [r.row, r.rows]),
    [
      [832, 2],
      [416, 1],
      [416, 1],
    ],
  );
  // A last strip that runs past the end leaves none.
  assert.deepEqual(rowsBelow(lastStrip(1080, 272, "4:4:4", true)), []);
  // Nor do the strips before the last, or a picture whole.
  assert.deepEqual(
    rowsBelow({ ...lastStrip(834, 208, "4:4:4", true), strip: 2 }),
    [],
  );
  assert.deepEqual(
    rowsBelow({
      ...lastStrip(834, 208, "4:4:4", true),
      strip: undefined,
      row: 0,
      planes: [{ rows: 834 }, { rows: 834 }, { rows: 834 }],
    }),
    [],
  );
});

// A picture of stream 7 that says where it changed since the one before it.
const changed = (
  serial: number,
  x: number,
  y: number,
  width: number,
  height: number,
) => ({
  changed: {
    picture: `7:${serial}`,
    since: `7:${serial - 1}`,
    x,
    y,
    width,
    height,
  },
});

test("a picture after the one the textures hold uploads where it changed", () => {
  assert.deepEqual(uploadOf("7:4", changed(5, 64, 128, 192, 64)), {
    x: 64,
    y: 128,
    width: 192,
    height: 64,
  });
});

test("a picture where nothing changed uploads nothing", () => {
  assert.equal(uploadOf("7:4", changed(5, 0, 0, 0, 0)), null);
});

test("a picture after one the textures do not hold uploads whole", () => {
  // One between was dropped, the textures are new, or the picture is another stream's.
  assert.equal(uploadOf("7:3", changed(5, 64, 128, 192, 64)), "whole");
  assert.equal(uploadOf(null, changed(5, 64, 128, 192, 64)), "whole");
  assert.equal(uploadOf("6:4", changed(5, 64, 128, 192, 64)), "whole");
});

test("a picture that does not say where it changed uploads whole", () => {
  assert.equal(uploadOf("7:4", { changed: undefined }), "whole");
});
