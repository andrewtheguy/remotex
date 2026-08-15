// The camera sender's pure halves: the H.264 configuration chosen for a capture
// geometry, and the rational frame rate the wire carries. The capture and
// socket halves need a camera and a gateway, which is browser QA's business.
//
// Run with `bun test src/cameraSender.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import { h264Config, rationalFps } from "./cameraSender";
import { encodeCameraFrame } from "./protocol";

test("a 720p30 camera fits level 3.1", () => {
  // 80x45 macroblocks at 30 fps = 108,000/s, exactly level 3.1's limit.
  assert.equal(h264Config(1280, 720, 30).codec, "avc1.42e01f");
});

test("1080p30 needs level 4.0 and 4K needs 5.0", () => {
  assert.equal(h264Config(1920, 1080, 30).codec, "avc1.42e028");
  assert.equal(h264Config(3840, 2160, 30).codec, "avc1.42e032");
});

test("the bitrate is a tenth of a bit per pixel per frame, clamped", () => {
  // 1280*720*30*0.1 = 2.76 Mbit/s — inside the clamp, so exactly that.
  assert.equal(h264Config(1280, 720, 30).bitrate, 2_764_800);
  // A tiny capture stays at the floor, a 4K60 one at the ceiling.
  assert.equal(h264Config(160, 120, 15).bitrate, 300_000);
  assert.equal(h264Config(3840, 2160, 60).bitrate, 8_000_000);
});

test("integer frame rates stay {fps, 1}", () => {
  assert.deepEqual(rationalFps(30), { numerator: 30, denominator: 1 });
  assert.deepEqual(rationalFps(60), { numerator: 60, denominator: 1 });
});

test("fractional frame rates keep their thousandths, reduced", () => {
  assert.deepEqual(rationalFps(29.97), { numerator: 2997, denominator: 100 });
  assert.deepEqual(rationalFps(23.976), { numerator: 2997, denominator: 125 });
});

// The layout mirrors `camera` in src/protocol.rs; the Rust side's parser has
// its own tests over the same bytes, which is the two-ends check the audio
// frame gets between decodeAudioFrame and `audio::frame`.
test("a camera frame is kind then flags then the unit, keyframe in bit zero", () => {
  assert.deepEqual(
    Array.from(encodeCameraFrame(new Uint8Array([9, 8]), true)),
    [0x04, 0x01, 9, 8],
  );
  assert.deepEqual(
    Array.from(encodeCameraFrame(new Uint8Array([7]), false)),
    [0x04, 0x00, 7],
  );
});
