import assert from "node:assert/strict";
import { test } from "node:test";
import { tileGridLines } from "./tileGrid.ts";

test("the lattice is the gateway's 320x64 cells, interior lines only", () => {
  // A desktop three cells wide and two tall: two vertical boundaries inside it
  // and one horizontal. The framebuffer's own edges are cell boundaries too and
  // are deliberately absent — the desktop's edge already draws them, and half of
  // such a line would fall outside the canvas.
  assert.deepEqual(tileGridLines({ w: 960, h: 128 }, { w: 320, h: 64 }), {
    xs: [320, 640],
    ys: [64],
  });
});

test("a desktop that does not divide evenly keeps every whole boundary", () => {
  // 1920x1080 is 6 whole columns and 16 whole rows plus a 56px remainder: the
  // partial cell at the bottom has a boundary above it and none below.
  const { xs, ys } = tileGridLines({ w: 1920, h: 1080 }, { w: 320, h: 64 });
  assert.deepEqual(xs, [320, 640, 960, 1280, 1600]);
  assert.equal(ys.length, 16);
  assert.equal(ys.at(-1), 1024);
});

test("a desktop smaller than one cell has no interior boundary to draw", () => {
  assert.deepEqual(tileGridLines({ w: 200, h: 40 }, { w: 320, h: 64 }), {
    xs: [],
    ys: [],
  });
});

test("a nonsense pitch draws nothing rather than looping on it", () => {
  // The pitch comes off the wire, so a zero, a negative or a NaN is reachable
  // from a malformed message and must terminate — a fractional one under 1 too,
  // which would otherwise fill the canvas one sub-pixel at a time.
  for (const bad of [0, -320, 0.5, Number.NaN, Number.POSITIVE_INFINITY]) {
    assert.deepEqual(
      tileGridLines({ w: 1920, h: 1080 }, { w: bad, h: bad }),
      { xs: [], ys: [] },
      `pitch ${bad}`,
    );
  }
});
