import assert from "node:assert/strict";
import { test } from "node:test";
import { tabletGuestSize } from "./tabletGuestSize.ts";

// An iPad Pro 13" in CSS pixels, as iPadOS reports its screen.
const IPAD = { w: 1032, h: 1376 };
// Its status bar, the band an installed page is shown under.
const STATUS_BAR = 24;

test("a phone asks for the target default", () => {
  assert.equal(tabletGuestSize({ w: 390, h: 844 }, { w: 390, h: 700 }), null);
});

test("a full-screen page asks for the whole landscape screen", () => {
  // The page and the screen are the same rectangle, so nothing is deducted.
  assert.deepEqual(tabletGuestSize(IPAD, { w: 1376, h: 1032 }), {
    w: 1376,
    h: 1032,
  });
});

test("a landscape page under a status bar asks for the height it has", () => {
  assert.deepEqual(tabletGuestSize(IPAD, { w: 1376, h: 1032 - STATUS_BAR }), {
    w: 1376,
    h: 1032 - STATUS_BAR,
  });
});

test("a portrait page under a status bar deducts the same band from landscape", () => {
  // The bar is the same height either way round, so a reading taken before the
  // rotation stands for the one after it.
  assert.deepEqual(tabletGuestSize(IPAD, { w: 1032, h: 1376 - STATUS_BAR }), {
    w: 1376,
    h: 1032 - STATUS_BAR,
  });
});

test("Safari's bar above a tab is deducted like the status bar", () => {
  assert.deepEqual(tabletGuestSize(IPAD, { w: 1376, h: 1032 - 94 }), {
    w: 1376,
    h: 1032 - 94,
  });
});

test("a windowed page says nothing about the bar and keeps the full height", () => {
  // Stage Manager, split view: short on both axes, so the remainder is not the
  // browser's and the screen's landscape height is asked for as before.
  assert.deepEqual(tabletGuestSize(IPAD, { w: 980, h: 700 }), {
    w: 1376,
    h: 1032,
  });
});

test("a one-pixel rounding of the width still counts as spanning the screen", () => {
  assert.deepEqual(tabletGuestSize(IPAD, { w: 1375, h: 1032 - STATUS_BAR }), {
    w: 1376,
    h: 1032 - STATUS_BAR,
  });
});

test("a page taller than its screen deducts nothing", () => {
  assert.deepEqual(tabletGuestSize(IPAD, { w: 1376, h: 1040 }), {
    w: 1376,
    h: 1032,
  });
});
