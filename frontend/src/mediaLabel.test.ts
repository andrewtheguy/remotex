// What the "This session" card says about the sound and the video.
//
// The cases that matter are the ones the Render row cannot answer: which of the
// two audio paths a target chose, why there is no sound, and what a live video
// decoder was actually configured with.

import assert from "node:assert/strict";
import { test } from "node:test";

import { audioLabel, renderLabel, videoLabel } from "./mediaLabel.ts";

const OPUS = {
  codec: "opus",
  sampleRate: 48_000,
  channels: 2,
  packetFrames: 960,
};

test("a target with no sound says so rather than offering nothing", () => {
  assert.equal(
    audioLabel({ available: false, enabled: false, error: null, stream: null }),
    "Not offered by this target",
  );
});

test("an available stream nobody asked for is distinguished from one that failed", () => {
  assert.equal(
    audioLabel({ available: true, enabled: false, error: null, stream: null }),
    "Available, not playing",
  );
  // The one state here that is wrong rather than off.
  assert.equal(
    audioLabel({
      available: true,
      enabled: false,
      error: "AudioDecoder refused opus",
      stream: null,
    }),
    "Stopped — AudioDecoder refused opus",
  );
});

test("enabling is a click and the format is a round trip later", () => {
  assert.equal(
    audioLabel({ available: true, enabled: true, error: null, stream: null }),
    "Waiting for the audio format",
  );
});

test("an encoded stream names its codec, its shape and its packet length", () => {
  // 960 samples at 48 kHz is 20 ms, which is the figure worth reading; the frame
  // count it was derived from is not.
  assert.equal(
    audioLabel({ available: true, enabled: true, error: null, stream: OPUS }),
    "opus · 48 kHz stereo · 20 ms packets",
  );
});

test("a channel count that is neither mono nor stereo still names itself", () => {
  assert.equal(
    audioLabel({
      available: true,
      enabled: true,
      error: null,
      stream: { ...OPUS, channels: 1 },
    }),
    "opus · 48 kHz mono · 20 ms packets",
  );
  assert.equal(
    audioLabel({
      available: true,
      enabled: true,
      error: null,
      stream: { ...OPUS, channels: 6 },
    }),
    "opus · 48 kHz 6 channels · 20 ms packets",
  );
});

test("the video row waits for the format, then names it", () => {
  assert.equal(videoLabel(null, false), "Waiting for the video format");
  assert.equal(videoLabel("vp09.00.40.08", false), "vp09.00.40.08");
  assert.equal(
    videoLabel("vp09.00.40.08", true),
    "Not in use: the picture is PNG tiles",
  );
});

test("the Render row says tiles while the desktop is past what video carries", () => {
  const plan = "video q90 4:4:4 · adaptive ≥20";
  assert.equal(renderLabel("", false), "Waiting for the target");
  assert.equal(renderLabel(plan, false), plan);
  assert.equal(
    renderLabel(plan, true),
    "PNG tiles: the desktop is past what video carries",
  );
});
