import assert from "node:assert/strict";
import { test } from "node:test";
import { desktopCanvasSize } from "./desktopCanvas.ts";

test("a Retina guest is never downsampled on a 1x host", () => {
  assert.deepEqual(desktopCanvasSize({ w: 3200, h: 1800 }, 2, 1), {
    w: 3200,
    h: 1800,
  });
});

test("matching host density presents the guest at its point size", () => {
  assert.deepEqual(desktopCanvasSize({ w: 3200, h: 1800 }, 2, 2), {
    w: 1600,
    h: 900,
  });
});

test("fractional host density preserves one framebuffer pixel per device pixel", () => {
  assert.deepEqual(desktopCanvasSize({ w: 3000, h: 1800 }, 2, 1.5), {
    w: 2000,
    h: 1200,
  });
});

test("a denser host never shrinks a 1x guest", () => {
  assert.deepEqual(desktopCanvasSize({ w: 1920, h: 1080 }, 1, 2), {
    w: 1920,
    h: 1080,
  });
});

test("invalid densities fall back to 1x", () => {
  assert.deepEqual(desktopCanvasSize({ w: 800, h: 600 }, 0, Number.NaN), {
    w: 800,
    h: 600,
  });
});
