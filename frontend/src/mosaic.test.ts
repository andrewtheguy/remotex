// Run with `bun test src/mosaic.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  mosaicDensity,
  mosaicSender,
  mosaicToFramebuffer,
  mosaicView,
} from "./mosaic.ts";
import type { ClientMsg } from "./protocol.ts";

// The measured Mac: a 1x 1280x800 screen, and a 2x 1440x900 one to its right
// whose 2880x1800 pixels start 1280 pixels into the framebuffer.
const regions = [
  {
    pixels: { x: 0, y: 0, w: 1280, h: 800 },
    points: { x: 0, y: 0, w: 1280, h: 800 },
  },
  {
    pixels: { x: 1280, y: 0, w: 2880, h: 1800 },
    points: { x: 1280, y: 0, w: 1440, h: 900 },
  },
];

test("a 1x display shows both screens at their points: the Retina one halved", () => {
  const view = mosaicView(regions, 1);
  assert.deepEqual([view.w, view.h, view.scale], [2720, 900, 1]);
  assert.deepEqual(view.draws[0], {
    sx: 0,
    sy: 0,
    sw: 1280,
    sh: 800,
    dx: 0,
    dy: 0,
    dw: 1280,
    dh: 800,
  });
  assert.deepEqual(view.draws[1], {
    sx: 1280,
    sy: 0,
    sw: 2880,
    sh: 1800,
    dx: 1280,
    dy: 0,
    dw: 1440,
    dh: 900,
  });
});

test("a 2x display doubles the 1x screen and keeps the Retina one whole", () => {
  const view = mosaicView(regions, 2);
  assert.deepEqual([view.w, view.h], [5440, 1800]);
  assert.deepEqual(
    view.draws.map((d) => [d.dx, d.dw, d.dh]),
    [
      [0, 2560, 1600],
      [2560, 2880, 1800],
    ],
  );
});

test("a pointer maps back into the screen it is over, in the Mac's pixels", () => {
  const view = mosaicView(regions, 1);
  // The Retina screen's centre, 720 points into it.
  assert.deepEqual(mosaicToFramebuffer(view, 2000, 450), { x: 2720, y: 900 });
  // The 1x screen's centre is its own pixel.
  assert.deepEqual(mosaicToFramebuffer(view, 640, 400), { x: 640, y: 400 });
  // The far edge belongs to the screen, as Apple's widened hit test has it.
  assert.deepEqual(mosaicToFramebuffer(view, 1000, 800), { x: 1000, y: 799 });
  // Below the shorter screen, beside the taller one: nothing is there.
  assert.equal(mosaicToFramebuffer(view, 100, 850), null);
});

test("a pointer between screens sends nothing, and neither do its presses", () => {
  const sent: ClientMsg[] = [];
  const view = mosaicView(regions, 1);
  const send = mosaicSender(
    (msg) => sent.push(msg),
    () => view,
  );
  send({ type: "mouseMove", x: 2000, y: 450 });
  send({ type: "mouseButton", button: "left", pressed: true, clicks: 1 });
  send({ type: "mouseMove", x: 100, y: 850 });
  send({ type: "mouseButton", button: "left", pressed: false, clicks: 1 });
  send({ type: "mouseButton", button: "left", pressed: true, clicks: 1 });
  send({ type: "wheel", dx: 0, dy: 10, unit: "pixel" });
  assert.deepEqual(sent, [
    { type: "mouseMove", x: 2720, y: 900 },
    { type: "mouseButton", button: "left", pressed: true, clicks: 1 },
    // The release after the drag left the screen, and nothing else.
    { type: "mouseButton", button: "left", pressed: false, clicks: 1 },
  ]);
});

test("without a composition every message passes through untouched", () => {
  const sent: ClientMsg[] = [];
  const send = mosaicSender(
    (msg) => sent.push(msg),
    () => null,
  );
  send({ type: "mouseMove", x: 100, y: 850 });
  assert.deepEqual(sent, [{ type: "mouseMove", x: 100, y: 850 }]);
});

test("a composition is drawn at 1x or 2x, never at a phone's 3x", () => {
  assert.deepEqual([1, 1.25, 1.5, 2, 3].map(mosaicDensity), [1, 1, 2, 2, 2]);
});
