// The microphone sender's pure halves: the Opus configuration, the downmix,
// and the frame the gateway parses. Capture needs a microphone and a gateway,
// which is browser QA's business.
//
// Run with `bun test src/micSender.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  downmix,
  MIC_BITRATE,
  MIC_FRAME_MICROSECONDS,
  opusConfig,
} from "./micSender";
import { encodeMicFrame } from "./protocol";

test("the microphone is mono speech at a low bitrate, whatever it captures at", () => {
  const config = opusConfig(44_100);
  assert.equal(config.codec, "opus");
  assert.equal(config.sampleRate, 44_100);
  assert.equal(config.numberOfChannels, 1);
  assert.equal(config.bitrate, MIC_BITRATE);
  assert.equal(MIC_BITRATE, 16_000);
  assert.equal(config.opus?.application, "voip");
  assert.equal(config.opus?.signal, "voice");
  assert.equal(config.opus?.frameDuration, MIC_FRAME_MICROSECONDS);
});

test("channels are averaged into one", () => {
  const mono = downmix([
    new Float32Array([1, 0.5, -1]),
    new Float32Array([0, 0.5, 1]),
  ]);
  assert.deepEqual(Array.from(mono), [0.5, 0.5, 0]);
  assert.equal(downmix([]).length, 0);
});

// The layout mirrors `mic` in src/protocol.rs, whose parser has its own tests
// over the same bytes.
test("a microphone frame is the kind byte then the packet", () => {
  assert.deepEqual(
    Array.from(encodeMicFrame(new Uint8Array([0xf8, 0xff])) ?? []),
    [0x05, 0xf8, 0xff],
  );
  assert.equal(encodeMicFrame(new Uint8Array()), null);
});
