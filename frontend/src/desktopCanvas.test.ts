import assert from "node:assert/strict";
import { test } from "node:test";
import { desktopCanvasGeometry } from "./desktopCanvas.ts";

test("a Retina guest keeps a full-size bitmap in a point-size layout", () => {
  assert.deepEqual(desktopCanvasGeometry({ w: 3200, h: 1800 }, 2), {
    bitmap: { w: 3200, h: 1800 },
    layout: { w: 1600, h: 900 },
  });
});

test("a 1x guest has matching bitmap and layout sizes", () => {
  assert.deepEqual(desktopCanvasGeometry({ w: 1920, h: 1080 }, 1), {
    bitmap: { w: 1920, h: 1080 },
    layout: { w: 1920, h: 1080 },
  });
});

test("an invalid guest density falls back to a 1x layout", () => {
  assert.deepEqual(desktopCanvasGeometry({ w: 800, h: 600 }, Number.NaN), {
    bitmap: { w: 800, h: 600 },
    layout: { w: 800, h: 600 },
  });
});
