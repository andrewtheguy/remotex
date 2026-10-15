// The region decoders of a picture sent for motion: one for each stream id, kept
// past a `regionEnd` for the stream that reuses the id, and retired after.
//
// Run with `bun test src/motionPicture.test.ts` from frontend/.
import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { createMotionPicture } from "./motionPicture.ts";
import type { RegionMsg } from "./protocol.ts";

let built = 0;
let closed = 0;

class FakeVideoDecoder {
  private readonly output: (frame: unknown) => void;
  state = "unconfigured";

  constructor(init: { output: (frame: unknown) => void }) {
    this.output = init.output;
    built += 1;
  }

  configure() {
    this.state = "configured";
  }

  decode() {
    this.output({ close() {} });
  }

  close() {
    closed += 1;
    this.state = "closed";
  }
}

const globals = globalThis as unknown as {
  VideoDecoder: unknown;
  EncodedVideoChunk: unknown;
};

beforeEach(() => {
  built = 0;
  closed = 0;
  globals.VideoDecoder = FakeVideoDecoder;
  globals.EncodedVideoChunk = class {};
});

afterEach(() => {
  globals.VideoDecoder = undefined;
  globals.EncodedVideoChunk = undefined;
});

const unit = (stream: number, w = 64): RegionMsg => ({
  kind: "region",
  stream,
  x: 0,
  y: 0,
  w,
  h: 32,
  keyframe: true,
  data: new Uint8Array([1]),
});

const wait = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

function motion() {
  return createMotionPicture({
    handlers: { onError: () => {}, onNeedsKeyframe: () => {} },
    onBroken: () => {},
    retireMs: 20,
  });
}

test("a stream that reuses its id before its decoder retires keeps the decoder", async () => {
  const m = motion();
  const VP9 = { decode: "vp09.01.10.08" };
  m.setFormat(1, VP9);
  (await m.decode(unit(1)))?.close();
  m.end(1);
  m.setFormat(1, VP9);
  await wait(40);
  (await m.decode(unit(1)))?.close();
  assert.equal(built, 1);
  assert.equal(closed, 0);
  // At another size it is a new stream, and a new decoder.
  (await m.decode(unit(1, 48)))?.close();
  assert.equal(built, 2);
  m.close();
});

test("a stream that ends is retired, and a unit on its id after is dropped", async () => {
  const m = motion();
  m.setFormat(2, { decode: "vp09.01.10.08" });
  (await m.decode(unit(2)))?.close();
  m.end(2);
  await wait(40);
  assert.equal(closed, 1);
  assert.equal(await m.decode(unit(2)), null);
  assert.equal(built, 1);
});

test("close gives every decoder back, retiring or not", async () => {
  const m = motion();
  m.setFormat(1, { decode: "vp09.01.10.08" });
  m.setFormat(2, { decode: "vp09.01.10.08" });
  (await m.decode(unit(1)))?.close();
  (await m.decode(unit(2)))?.close();
  m.end(2);
  m.close();
  assert.equal(closed, 2);
  await wait(40);
  assert.equal(closed, 2);
});
