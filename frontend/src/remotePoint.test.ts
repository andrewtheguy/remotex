import assert from "node:assert/strict";
import { test } from "node:test";
import { remotePoint } from "./remotePoint.ts";

// A 1600×1000 display shown at half size, 100 points in from the page's corner.
const rect = { left: 100, top: 100, width: 800, height: 500 };
const remote = { w: 1600, h: 1000 };

test("a point on the canvas maps through its rect to a remote pixel", () => {
  assert.deepEqual(remotePoint(100, 100, rect, remote, null), { x: 0, y: 0 });
  assert.deepEqual(remotePoint(500, 350, rect, remote, null), {
    x: 800,
    y: 500,
  });
  // Without a remote size the offset into the canvas is the answer.
  assert.deepEqual(remotePoint(150, 120, rect, null, null), { x: 50, y: 20 });
});

test("a drag held past an edge is held at the framebuffer's edge", () => {
  assert.deepEqual(remotePoint(0, 0, rect, remote, null), { x: 0, y: 0 });
  assert.deepEqual(remotePoint(2000, 2000, rect, remote, null), {
    x: 1599,
    y: 999,
  });
});

test("towards the display shown beside this one, it is let through", () => {
  // The session's page, the second display to its right: past the right edge
  // the position is where the pointer is on that display, and every other edge
  // still holds.
  assert.deepEqual(remotePoint(1000, 350, rect, remote, "right"), {
    x: 1800,
    y: 500,
  });
  assert.deepEqual(remotePoint(0, 2000, rect, remote, "right"), {
    x: 0,
    y: 999,
  });
  // The second display's tab, the first to its left.
  assert.deepEqual(remotePoint(0, 350, rect, remote, "left"), {
    x: -200,
    y: 500,
  });
  assert.deepEqual(remotePoint(2000, 0, rect, remote, "left"), {
    x: 1599,
    y: 0,
  });
});
