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
  passthrough: false,
};

test("a session without sound says so rather than offering nothing", () => {
  assert.equal(
    audioLabel({ available: false, enabled: false, error: null, stream: null }),
    "None in this session",
  );
});

test("a muted session is distinguished from one whose sound failed", () => {
  assert.equal(
    audioLabel({ available: true, enabled: false, error: null, stream: null }),
    "Muted",
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

test("an encoded stream names its codec, its shape and whose it is", () => {
  assert.equal(
    audioLabel({ available: true, enabled: true, error: null, stream: OPUS }),
    "opus · 48 kHz stereo · encoded by the gateway",
  );
});

test("a lossless stream says whether it is the remote's own or coded here", () => {
  const row = (stream: typeof OPUS) =>
    audioLabel({ available: true, enabled: true, error: null, stream });
  // wlshare's frames, passed as they came.
  assert.equal(
    row({ ...OPUS, codec: "flac", passthrough: true }),
    "flac · 48 kHz stereo · passthrough from the remote",
  );
  // An RDP host's PCM, coded as FLAC by the gateway at the host's own rate.
  assert.equal(
    row({ ...OPUS, codec: "flac", sampleRate: 44_100 }),
    "flac · 44.1 kHz stereo · encoded by the gateway",
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
    "opus · 48 kHz mono · encoded by the gateway",
  );
  assert.equal(
    audioLabel({
      available: true,
      enabled: true,
      error: null,
      stream: { ...OPUS, channels: 6 },
    }),
    "opus · 48 kHz 6 channels · encoded by the gateway",
  );
});

test("the video row waits for the format, then names it and whose stream it is", () => {
  assert.equal(videoLabel(null, null), "Waiting for the video format");
  assert.equal(
    videoLabel({ decode: "vp09.00.40.08", passthrough: false }, null),
    "vp09.00.40.08 · encoded by the gateway · decoded by the browser",
  );
  assert.equal(
    videoLabel({ decode: "hev1.4.10.L150.BE.8", passthrough: true }, null),
    "hev1.4.10.L150.BE.8 · passthrough from the remote · decoded by the browser",
  );
  assert.equal(
    videoLabel(
      { decode: "vp09.01.40.08", passthrough: false, software: true },
      null,
    ),
    "vp09.01.40.08 · encoded by the gateway · decoded in WebAssembly by this page",
  );
  assert.equal(
    videoLabel({ decode: "vp09.00.40.08", passthrough: true }, "size"),
    "Not in use: the desktop is past what video carries",
  );
  assert.equal(
    videoLabel({ decode: "vp09.00.40.08", passthrough: true }, "screens"),
    "Not in use: Combined Display spans more than two screens",
  );
});

test("the Render row waits for the target, then names its dial", () => {
  const plan = "video q90 4:4:4 · adaptive";
  assert.equal(renderLabel(""), "Waiting for the target");
  assert.equal(renderLabel(plan), plan);
});
