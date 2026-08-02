// Where a click lands on the remote.
//
// This arithmetic used to be `RemoteGeometry.remotePoint` in the Swift viewer and
// came across with the canvas; the cases below are its cases, because they are
// the ones that were wrong at some point.
//
// Run with `bun test src/viewer/input.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import { remotePoint } from "./input.ts";

/** A canvas rect at the origin, `w`x`h` CSS pixels. */
function rect(w: number, h: number, left = 0, top = 0) {
  return { left, top, width: w, height: h };
}

test("a point maps to the remote pixel under it", () => {
  // A 1000x500 desktop drawn at half size: every CSS pixel is two remote ones.
  const remote = { w: 1000, h: 500 };
  assert.deepEqual(remotePoint({ x: 0, y: 0 }, rect(500, 250), remote), {
    x: 0,
    y: 0,
  });
  assert.deepEqual(remotePoint({ x: 250, y: 125 }, rect(500, 250), remote), {
    x: 500,
    y: 250,
  });
});

test("the canvas offset is subtracted, not ignored", () => {
  // The canvas is not at the window origin once the desktop is scrolled or
  // centred, and a click read in window coordinates would land elsewhere.
  const remote = { w: 100, h: 100 };
  assert.deepEqual(
    remotePoint({ x: 40, y: 70 }, rect(100, 100, 30, 50), remote),
    { x: 10, y: 20 },
  );
});

test("the far corners clamp inside the framebuffer", () => {
  // The bottom-right pixel is w-1, h-1, not w, h — and this is what a drag that
  // runs off the edge lands on. Unclamped, those coordinates would be off the
  // framebuffer and the gateway would refuse them.
  const remote = { w: 800, h: 600 };
  const box = rect(800, 600);
  assert.deepEqual(remotePoint({ x: 800, y: 600 }, box, remote), {
    x: 799,
    y: 599,
  });
  assert.deepEqual(remotePoint({ x: 5000, y: 5000 }, box, remote), {
    x: 799,
    y: 599,
  });
  // A drag that leaves the top-left has to keep reporting, too.
  assert.deepEqual(remotePoint({ x: -40, y: -40 }, box, remote), {
    x: 0,
    y: 0,
  });
});

test("a one-pixel remote has exactly one addressable pixel", () => {
  const remote = { w: 1, h: 1 };
  for (const point of [
    { x: 0, y: 0 },
    { x: 50, y: 50 },
    { x: 100, y: 100 },
  ]) {
    assert.deepEqual(remotePoint(point, rect(100, 100), remote), {
      x: 0,
      y: 0,
    });
  }
});

test("a surface that has not been laid out yet maps 1:1", () => {
  // No scale to divide by, and no size to clamp to: report the point itself
  // rather than dividing by zero.
  assert.deepEqual(remotePoint({ x: 10, y: 10 }, rect(0, 0), null), {
    x: 10,
    y: 10,
  });
});

test("non-finite geometry reports the origin rather than NaN", () => {
  // A rect read mid-layout can be non-finite, and `{"x":null}` is a message the
  // gateway refuses rather than a position.
  const remote = { w: 100, h: 100 };
  assert.deepEqual(
    remotePoint(
      { x: Number.NaN, y: Number.POSITIVE_INFINITY },
      rect(100, 100),
      remote,
    ),
    { x: 0, y: 0 },
  );
  // And from the other side: the point is a real one and the *rect* is what came
  // back unmeasured, which is the case that actually happens — a pointer event
  // over a canvas whose layout has not settled.
  assert.deepEqual(
    remotePoint(
      { x: 40, y: 70 },
      rect(Number.NaN, Number.POSITIVE_INFINITY, Number.NaN, 0),
      remote,
    ),
    { x: 0, y: 0 },
  );
});
