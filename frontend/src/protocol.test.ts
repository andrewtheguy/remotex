// The click count the browser puts on the wire. Pure bounds logic, so it is
// checked here; what a browser actually reports in `MouseEvent.detail` is its
// own double-click policy and no unit test can stand in for it.
//
// It matters more than a bounds check looks: a value of zero is a click that
// counts as no click at all where a guest infers double-clicks from the count.
// Run with `bun test src/protocol.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  clickCount,
  mouseButtonFromEvent,
  wheelUnitFromEvent,
} from "./protocol.ts";

test("an ordinary click run is passed through as the browser counted it", () => {
  assert.equal(clickCount(1), 1);
  assert.equal(clickCount(2), 2);
  assert.equal(clickCount(3), 3);
});

test("a detail the wire cannot carry is bounded rather than dropped", () => {
  // One byte on the wire, and a held-down finger on a trackpad can run the
  // count past it. Anything over two is a triple-click to every app that cares.
  assert.equal(clickCount(255), 255);
  assert.equal(clickCount(256), 255);
  assert.equal(clickCount(9999), 255);
});

test("a detail of nothing is still one click", () => {
  // Programmatic and synthesized events arrive with a detail of 0, which would
  // otherwise inject a click state of 0 — a press macOS counts as no click.
  assert.equal(clickCount(0), 1);
  assert.equal(clickCount(-1), 1);
  assert.equal(clickCount(Number.NaN), 1);
  assert.equal(clickCount(1.7), 1);
});

test("the side buttons of a five-button mouse are named, not dropped", () => {
  // DOM 3 and 4 are back and forward, and macOS numbers them the same way.
  assert.equal(mouseButtonFromEvent(0), "left");
  assert.equal(mouseButtonFromEvent(1), "middle");
  assert.equal(mouseButtonFromEvent(2), "right");
  assert.equal(mouseButtonFromEvent(3), "back");
  assert.equal(mouseButtonFromEvent(4), "forward");
  // Past forward nothing has an agreed meaning on any platform.
  assert.equal(mouseButtonFromEvent(5), null);
});

test("a wheel delta says which unit it is in", () => {
  // The remote cannot tell a three-pixel trackpad glide from a three-line
  // notch, and guessing lines is what made trackpad scrolling jump.
  assert.equal(wheelUnitFromEvent(0), "pixel");
  assert.equal(wheelUnitFromEvent(1), "line");
  assert.equal(wheelUnitFromEvent(2), "page");
  // Anything else is what every browser on macOS actually sends.
  assert.equal(wheelUnitFromEvent(7), "pixel");
});
