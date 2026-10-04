import assert from "node:assert/strict";
import { test } from "node:test";
import { remotePoint } from "./remotePoint.ts";

// A 1600×1000 display shown at half size, 100 points in from the page's corner.
const rect = { left: 100, top: 100, width: 800, height: 500 };
const remote = { w: 1600, h: 1000 };

test("a point on the canvas maps through its rect to a remote pixel", () => {
  assert.deepEqual(remotePoint(100, 100, rect, remote, false), { x: 0, y: 0 });
  assert.deepEqual(remotePoint(500, 350, rect, remote, false), {
    x: 800,
    y: 500,
  });
  // Without a remote size the offset into the canvas is the answer.
  assert.deepEqual(remotePoint(150, 120, rect, null, false), { x: 50, y: 20 });
});

test("a drag held past an edge is held at the framebuffer's edge", () => {
  assert.deepEqual(remotePoint(0, 0, rect, remote, false), { x: 0, y: 0 });
  assert.deepEqual(remotePoint(2000, 2000, rect, remote, false), {
    x: 1599,
    y: 999,
  });
});

test("while another display is shown beside this one, it is let through", () => {
  // Whichever edge the other display is against is the gateway's to know: a
  // position past any edge is where the pointer is, and is held there.
  assert.deepEqual(remotePoint(1000, 350, rect, remote, true), {
    x: 1800,
    y: 500,
  });
  assert.deepEqual(remotePoint(0, 350, rect, remote, true), {
    x: -200,
    y: 500,
  });
  assert.deepEqual(remotePoint(500, 0, rect, remote, true), {
    x: 800,
    y: -200,
  });
  assert.deepEqual(remotePoint(500, 700, rect, remote, true), {
    x: 800,
    y: 1200,
  });
});
