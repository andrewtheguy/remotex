// Which strips a software decoder's picture holds: drawn only once all four have
// come since a keyframe.
//
// Run with `bun test src/planesPicture.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";

import { placedAfter } from "./planesPicture.ts";

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
