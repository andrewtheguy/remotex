// A Mac is asked for no display under its own client's smallest window, and the
// page is where that is held: what it reports is what the gateway passes on.

import assert from "node:assert/strict";
import { test } from "node:test";

import { macSubtype, reportedSize } from "./remoteMinimum.ts";

test("a Mac is never reported a window under 800×600 points", () => {
  assert.deepEqual(reportedSize({ w: 320, h: 68 }, true), { w: 800, h: 600 });
  assert.deepEqual(reportedSize({ w: 1280, h: 140 }, true), {
    w: 1280,
    h: 600,
  });
  assert.deepEqual(reportedSize({ w: 640, h: 900 }, true), { w: 800, h: 900 });
  assert.deepEqual(reportedSize({ w: 800, h: 600 }, true), { w: 800, h: 600 });
  assert.deepEqual(reportedSize({ w: 1440, h: 900 }, true), {
    w: 1440,
    h: 900,
  });
});

test("any other remote is reported the window as it is", () => {
  assert.deepEqual(reportedSize({ w: 320, h: 68 }, false), { w: 320, h: 68 });
});

test("the two Apple subtypes are the Macs", () => {
  assert.equal(macSubtype("ard"), true);
  assert.equal(macSubtype("ard-high-performance"), true);
  assert.equal(macSubtype("wlshare"), false);
  assert.equal(macSubtype(null), false);
});
