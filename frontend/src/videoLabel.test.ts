// What the "This session" card says about video.
//
// Its three states are genuinely different rather than degrees of one, and each is a
// question somebody asks while looking at a desktop that is behaving oddly: is this
// target streaming at all, has it connected but not yet produced a frame, and *which*
// codec and configuration is the decoder actually running. A wrong answer here sends
// the reader looking in the wrong half of the system, which is the whole reason the row
// exists.

import assert from "node:assert/strict";
import { test } from "node:test";

// Stubbed before the import, not after: `FloatingMenu` reaches `gateway.ts`, which
// derives the gateway's origin from `window.location` at module load — and this runtime
// is not a browser.
const globals = globalThis as unknown as {
  window?: unknown;
  screen?: unknown;
};
globals.window = {
  location: { origin: "https://gateway.test" },
  isSecureContext: true,
};
// `useRemoteDesktop` reads `screen` at module load too, to decide a mobile guest size.
globals.screen = { width: 1920, height: 1080 };
const { videoLabel } = await import("./FloatingMenu.tsx");

test("a target that streams no video says so rather than showing nothing", () => {
  assert.equal(videoLabel(null, []), "None — this target sends tiles");
  // A stale string cannot outlive the family: returning to the picker clears both, and
  // a target with no video must never read as one with some.
  assert.equal(
    videoLabel(null, ["vp09.00.40.08"]),
    "None — this target sends tiles",
  );
});

test("a connected target with no frame yet says which codec it will be", () => {
  // The gap between `connected` and the first access unit. The family is known — it is
  // the target's, and it arrived on `connected` — and the configuration is not, because
  // only the encoder knows the picture size.
  assert.equal(videoLabel("vp9", []), "VP9 — waiting for the first frame");
  assert.equal(videoLabel("h264", []), "H264 — waiting for the first frame");
});

test("a streaming target names the exact configuration its decoder was built with", () => {
  assert.equal(videoLabel("vp9", ["vp09.00.40.08"]), "VP9 — vp09.00.40.08");
  assert.equal(videoLabel("h264", ["avc1.42c01e"]), "H264 — avc1.42c01e");
  // A `motion` target runs a stream per moving region and the regions differ in size,
  // so one family can have several configurations live at once — the level is derived
  // from the picture. All of them are shown: "which decoder refused" is the question
  // this row is here to answer, and an elided one cannot answer it.
  assert.equal(
    videoLabel("vp9", ["vp09.00.31.08", "vp09.00.40.08"]),
    "VP9 — vp09.00.31.08, vp09.00.40.08",
  );
});
