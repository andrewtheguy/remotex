import assert from "node:assert/strict";
import { test } from "node:test";
import { desktopCanvasGeometry } from "./desktopCanvas.ts";

test("a 2x host lays the full-size bitmap out at half its pixel size", () => {
  assert.deepEqual(desktopCanvasGeometry({ w: 3200, h: 1800 }, 2), {
    bitmap: { w: 3200, h: 1800 },
    layout: { w: 1600, h: 900 },
  });
});

test("a 1x host has matching bitmap and layout sizes", () => {
  assert.deepEqual(desktopCanvasGeometry({ w: 1920, h: 1080 }, 1), {
    bitmap: { w: 1920, h: 1080 },
    layout: { w: 1920, h: 1080 },
  });
});

test("a fractional host density still lands every pixel on a device pixel", () => {
  assert.deepEqual(desktopCanvasGeometry({ w: 1920, h: 1080 }, 1.5), {
    bitmap: { w: 1920, h: 1080 },
    layout: { w: 1280, h: 720 },
  });
});

test("an invalid host density falls back to a 1x layout", () => {
  assert.deepEqual(desktopCanvasGeometry({ w: 800, h: 600 }, Number.NaN), {
    bitmap: { w: 800, h: 600 },
    layout: { w: 800, h: 600 },
  });
});
