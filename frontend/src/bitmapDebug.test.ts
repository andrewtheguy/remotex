import assert from "node:assert/strict";
import { test } from "node:test";
import { createTileHeat, FADE_MS } from "./bitmapDebug.ts";

test("a desktop is cut into 64-pixel tiles, the last ones short", () => {
  const heat = createTileHeat(1920, 1080);
  assert.equal(heat.columns, 30);
  assert.equal(heat.rows, 17);
});

test("a tile is warm as it arrives and cold once it has faded", () => {
  const heat = createTileHeat(128, 128);
  assert.equal(heat.sample(0).warm, false);
  heat.stamp([64, 0, 64, 64], 1000);
  const arrived = heat.sample(1000);
  assert.equal(arrived.warm, true);
  assert.deepEqual(Array.from(arrived.heat), [0, 255, 0, 0]);
  const halfway = heat.sample(1000 + FADE_MS / 2).heat[1];
  assert.ok(halfway > 100 && halfway < 155, `${halfway}`);
  const faded = heat.sample(1000 + FADE_MS);
  assert.equal(faded.warm, false);
  assert.deepEqual(Array.from(faded.heat), [0, 0, 0, 0]);
});

test("a rectangle warms every tile it touches, and none past the desktop", () => {
  const heat = createTileHeat(130, 70);
  // Across the line between the first two columns, and past the right edge.
  heat.stamp([60, 60, 100, 20], 0);
  assert.deepEqual(
    Array.from(heat.sample(0).heat),
    [255, 255, 255, 255, 255, 255],
  );
  const empty = createTileHeat(130, 70);
  empty.stamp([0, 0, 0, 10, 10, 10, 0, 0], 0);
  assert.equal(empty.sample(0).warm, false);
});
