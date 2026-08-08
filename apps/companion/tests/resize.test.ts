// Resize to display, which is the only arithmetic in this tree.
//
// The clamping and the preserved corner belong to `apps/viewer/tests/geometry.test.ts`
// and are not retested here. What is here is the part that is this extension's: the
// browser-zoom conversion, and the decision to say nothing when there is nothing to
// say.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  type ViewportMetrics,
  windowBoundsForRemote,
} from "../src/shared/resize.ts";

/** A window with 16 DIPs of chrome across and 100 down, on a large screen. */
function metrics(over: Partial<ViewportMetrics> = {}): ViewportMetrics {
  return {
    innerWidth: 800,
    innerHeight: 600,
    outerWidth: 816,
    outerHeight: 700,
    availLeft: 0,
    availTop: 0,
    availWidth: 3000,
    availHeight: 2000,
    ...over,
  };
}

const current = { x: 100, y: 100, width: 816, height: 700 };

test("the window grows to hold the framebuffer, chrome measured not assumed", () => {
  assert.deepEqual(
    windowBoundsForRemote({
      remote: { w: 1920, h: 1080, scale: 1 },
      metrics: metrics(),
      zoom: 1,
      current,
    }),
    { x: 100, y: 100, width: 1936, height: 1180 },
  );
});

test("a Retina remote is half the points, and never half the pixels", () => {
  // 3840×2160 at scale 2 is a 1920×1080 desktop at full pixel fidelity. The window is
  // sized for the desktop; nothing anywhere scales the framebuffer.
  assert.deepEqual(
    windowBoundsForRemote({
      remote: { w: 3840, h: 2160, scale: 2 },
      metrics: metrics(),
      zoom: 1,
      current,
    }),
    { x: 100, y: 100, width: 1936, height: 1180 },
  );
});

test("a nonsense density is treated as 1 rather than divided by", () => {
  assert.deepEqual(
    windowBoundsForRemote({
      remote: { w: 1000, h: 800, scale: 0 },
      metrics: metrics(),
      zoom: 1,
      current,
    }),
    { x: 100, y: 100, width: 1016, height: 900 },
  );
});

test("a nonsense zoom is treated as 1 too", () => {
  assert.deepEqual(
    windowBoundsForRemote({
      remote: { w: 1000, h: 800, scale: 1 },
      metrics: metrics(),
      zoom: 0,
      current,
    }),
    { x: 100, y: 100, width: 1016, height: 900 },
  );
});

test("browser zoom is DIPs per CSS pixel, and only the page is in CSS pixels", () => {
  // At 125% the content area is 800 DIPs while `innerWidth` reports 640 CSS pixels,
  // and `outerWidth` is untouched by zoom. A 1920-CSS-pixel desktop wants 2400 DIPs.
  assert.deepEqual(
    windowBoundsForRemote({
      remote: { w: 1920, h: 1080, scale: 1 },
      metrics: metrics({ innerWidth: 640, innerHeight: 480 }),
      zoom: 1.25,
      current,
    }),
    { x: 100, y: 100, width: 2416, height: 1450 },
  );
});

test("zoomed out asks for less, by the same rule", () => {
  assert.deepEqual(
    windowBoundsForRemote({
      remote: { w: 1000, h: 800, scale: 1 },
      metrics: metrics({ innerWidth: 1000, innerHeight: 750 }),
      zoom: 0.8,
      current,
    }),
    { x: 100, y: 100, width: 816, height: 740 },
  );
});

test("an oversized remote is clamped to the work area and scrolls", () => {
  assert.deepEqual(
    windowBoundsForRemote({
      remote: { w: 3840, h: 2160, scale: 1 },
      metrics: metrics({ availWidth: 1920, availHeight: 1080 }),
      zoom: 1,
      current,
    }),
    { x: 0, y: 0, width: 1920, height: 1080 },
  );
});

test("a window already the right size is not asked to move", () => {
  // Null, not the current bounds: an update to where a window already is would be a
  // request that does nothing, and Chrome would still process it.
  assert.equal(
    windowBoundsForRemote({
      remote: { w: 800, h: 600, scale: 1 },
      metrics: metrics(),
      zoom: 1,
      current,
    }),
    null,
  );
});

test("a window mid-layout is left exactly where it is", () => {
  for (const broken of [
    { innerWidth: Number.NaN },
    { innerHeight: 0 },
    { availWidth: Number.POSITIVE_INFINITY },
  ]) {
    assert.equal(
      windowBoundsForRemote({
        remote: { w: 1920, h: 1080, scale: 1 },
        metrics: metrics(broken),
        zoom: 1,
        current,
      }),
      null,
      `${JSON.stringify(broken)} should say nothing`,
    );
  }
});
