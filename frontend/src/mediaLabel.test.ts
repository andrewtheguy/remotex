// What the "This session" card says about the sound and the video.
//
// The cases that matter are the ones the Render row cannot answer: which of the
// two audio paths a target chose, why there is no sound, and what a live video
// decoder was actually configured with.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  audioBlockCopy,
  audioLabel,
  hostPictureLabel,
  renderLabel,
  videoLabel,
} from "./mediaLabel.ts";

const OPUS = {
  codec: "opus",
  sampleRate: 48_000,
  channels: 2,
  passthrough: false,
};

test("a session without sound says so rather than offering nothing", () => {
  assert.equal(
    audioLabel({
      blocked: null,
      available: false,
      enabled: false,
      error: null,
      stream: null,
    }),
    "None in this session",
  );
});

test("a browser that cannot play the sound says why, whatever the session carries", () => {
  // A High Performance Mac's session carries sound whatever was chosen, and the
  // reason there is none here is the browser, not the session.
  for (const available of [false, true]) {
    const row = { available, enabled: false, error: null, stream: null };
    assert.equal(
      audioLabel({ ...row, blocked: "decoder" }),
      "Not supported by this browser",
    );
    assert.equal(
      audioLabel({ ...row, blocked: "aac-eld" }),
      "AAC-ELD, not supported by this browser",
    );
  }
  // And the menu's greyed button names the same reason, short enough to fit.
  assert.equal(audioBlockCopy("decoder").button, "Audio not supported");
  assert.equal(audioBlockCopy("aac-eld").button, "AAC-ELD not supported");
  assert.match(audioBlockCopy("decoder").reason, /AudioDecoder/);
  assert.match(audioBlockCopy("aac-eld").reason, /AAC-ELD/);
});

test("a muted session is distinguished from one whose sound failed", () => {
  assert.equal(
    audioLabel({
      blocked: null,
      available: true,
      enabled: false,
      error: null,
      stream: null,
    }),
    "Muted",
  );
  // The one state here that is wrong rather than off.
  assert.equal(
    audioLabel({
      blocked: null,
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
    audioLabel({
      blocked: null,
      available: true,
      enabled: true,
      error: null,
      stream: null,
    }),
    "Waiting for the audio format",
  );
});

test("an encoded stream names its codec, its shape and whose it is", () => {
  assert.equal(
    audioLabel({
      blocked: null,
      available: true,
      enabled: true,
      error: null,
      stream: OPUS,
    }),
    "opus · 48 kHz stereo · encoded by the gateway",
  );
});

test("a lossless stream says whether it is the remote's own or coded here", () => {
  const row = (stream: typeof OPUS) =>
    audioLabel({
      blocked: null,
      available: true,
      enabled: true,
      error: null,
      stream,
    });
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
      blocked: null,
      available: true,
      enabled: true,
      error: null,
      stream: { ...OPUS, channels: 1 },
    }),
    "opus · 48 kHz mono · encoded by the gateway",
  );
  assert.equal(
    audioLabel({
      blocked: null,
      available: true,
      enabled: true,
      error: null,
      stream: { ...OPUS, channels: 6 },
    }),
    "opus · 48 kHz 6 channels · encoded by the gateway",
  );
});

test("the video row waits for the format, then names it and its decoder", () => {
  assert.equal(videoLabel(null, null), "Waiting for the video format");
  assert.equal(
    videoLabel({ decode: "vp09.00.40.08" }, null),
    "vp09.00.40.08 · passthrough from the remote · decoded by the browser's native decoder",
  );
  assert.equal(
    videoLabel({ decode: "hev1.4.10.L150.BE.8" }, null),
    "hev1.4.10.L150.BE.8 · passthrough from the remote · decoded by the browser's native decoder",
  );
  // A High Performance Mac's pictures come in strips, and the row says how many.
  assert.equal(
    videoLabel({ decode: "hev1.4.10.L153.BE.8", strips: 4 }, null),
    "hev1.4.10.L153.BE.8 · passthrough from the remote in 4 strips · decoded by the browser's native decoder",
  );
  assert.equal(
    videoLabel({ decode: "hev1.4.10.L153.BE.8", strips: 1 }, null),
    "hev1.4.10.L153.BE.8 · passthrough from the remote · decoded by the browser's native decoder",
  );
  assert.equal(
    videoLabel({ decode: "hev1.4.10.L153.BE.8", software: true }, null),
    "hev1.4.10.L153.BE.8 · passthrough from the remote · decoded by this page's WebAssembly decoder",
  );
  assert.equal(
    videoLabel({ decode: "", standard: true }, null),
    "Not in use: the Mac's Adaptive picture is decoded by this page",
  );
  // Tiles are no stream: the gateway's ever, and wlshare's until a region moves.
  assert.equal(
    videoLabel({ decode: "", tiles: true }, null),
    "Not in use: the tiles are images this page decodes",
  );
  assert.equal(
    videoLabel({ decode: "vp09.00.40.08" }, "size"),
    "Not in use: the desktop is past what its video stream carries",
  );
  // A Mac's own picture has no ceiling, and is decoded whatever is said of a hold.
  assert.equal(
    videoLabel({ decode: "", standard: true }, "size"),
    "Not in use: the Mac's Adaptive picture is decoded by this page",
  );
});

test("wlshare's tiles say what is drawn where once a region has moved, and which decoder takes the motion", () => {
  assert.equal(
    videoLabel(
      { decode: "", tiles: true, regions: "vp09.00.21.08.01.06.06.06.00" },
      null,
    ),
    "VP9 regions where the desktop moves (vp09.00.21.08.01.06.06.06.00), PNG tiles where it is still · passthrough from the remote · decoded by the browser's native decoder",
  );
});

test("the Render row waits for the target, then names its dial", () => {
  assert.equal(renderLabel(""), "Waiting for the target");
  // The gateway's tiles, and wlshare's: the plan is said as it came.
  for (const plan of ["tiles q90", "tiles · VP9 regions q90 · 4:2:0"]) {
    assert.equal(renderLabel(plan), plan);
  }
});

test("an RDP host's picture says whether it is lossless, and with what", () => {
  assert.equal(
    hostPictureLabel({ exact: true, codecs: [] }),
    "Nothing drawn yet",
  );
  assert.equal(
    hostPictureLabel({ exact: true, codecs: ["Uncompressed", "Planar"] }),
    "Lossless (Uncompressed, Planar)",
  );
  assert.equal(
    hostPictureLabel({
      exact: false,
      codecs: ["ClearCodec with NSCodec", "RemoteFX Progressive"],
    }),
    "Lossy (ClearCodec with NSCodec, RemoteFX Progressive): the host's image quality policy is not Lossless",
  );
});
