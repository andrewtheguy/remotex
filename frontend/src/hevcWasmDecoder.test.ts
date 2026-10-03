// BETA: the software HEVC decoder's `VideoDecoder` shape, as far as it
// goes without its worker.
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  createWasmHevcDecoder,
  type HevcPlanes,
  isHevc,
} from "./hevcWasmDecoder.ts";

test("only HEVC configurations are the software decoder's", () => {
  assert.equal(isHevc("hev1.4.10.L150.BE.8"), true);
  assert.equal(isHevc("hvc1.1.6.L93.B0"), true);
  assert.equal(isHevc("vp09.01.50.08.03.06.06.06.00"), false);
});

test("another codec is refused as VideoDecoder refuses one: asynchronously, NotSupportedError", async () => {
  const errors: Error[] = [];
  const decoder = createWasmHevcDecoder({
    output: () => assert.fail("no output"),
    error: (e) => errors.push(e),
  });
  decoder.configure({ codec: "vp09.00.40.08" });
  assert.equal(errors.length, 0, "not from inside configure");
  await Promise.resolve();
  assert.equal(errors.length, 1);
  assert.equal(errors[0].name, "NotSupportedError");
  assert.equal(decoder.state, "closed");
  assert.throws(() => decoder.configure({ codec: "hev1.4.10.L150.BE.8" }));
});

/** The decode worker, as far as the decoder talks to it: what it was posted. */
class FakeWorker {
  static made: FakeWorker[] = [];
  posted: { type: string; id: number }[] = [];
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onerror: unknown = null;
  constructor() {
    FakeWorker.made.push(this);
  }
  postMessage(command: { type: string; id: number }) {
    this.posted.push(command);
  }
  terminate() {}
}

const PLANES = {
  memory: new SharedArrayBuffer(16),
  width: 2,
  height: 2,
  planes: [0, 4, 8].map((offset) => ({ offset, stride: 2, width: 2, rows: 2 })),
  fullRange: true,
  matrix: "bt709",
  colorSpace: "display-p3",
};

const chunk = {
  type: "key",
  timestamp: 0,
  byteLength: 1,
  copyTo() {},
} as unknown as EncodedVideoChunk;

test("a picture is released to the decode worker when it is closed, once", () => {
  const globals = globalThis as unknown as { Worker: unknown };
  const before = globals.Worker;
  globals.Worker = FakeWorker;
  try {
    const pictures: HevcPlanes[] = [];
    const decoder = createWasmHevcDecoder({
      output: (picture) => pictures.push(picture as HevcPlanes),
      error: (e) => assert.fail(e.message),
    });
    decoder.configure({ codec: "hev1.4.10.L150.BE.8" });
    const worker = FakeWorker.made[0];
    const id = worker.posted[0].id;
    const types = () => worker.posted.map((command) => command.type);
    const answer = () =>
      worker.onmessage?.({ data: { type: "decoded", id, picture: PLANES } });

    // The decoder holds the picture's memory until then, and decodes no further.
    decoder.decode(chunk);
    answer();
    assert.equal(pictures.length, 1);
    assert.equal(pictures[0].memory, PLANES.memory);
    assert.deepEqual(types(), ["create", "decode"]);
    pictures[0].close();
    pictures[0].close();
    assert.deepEqual(types(), ["create", "decode", "release"]);

    // A stream that ends releases with its `destroy`: a picture closed after it
    // says nothing to a decoder that is gone.
    decoder.decode(chunk);
    answer();
    decoder.close();
    pictures[1].close();
    assert.deepEqual(types(), [
      "create",
      "decode",
      "release",
      "decode",
      "destroy",
    ]);
  } finally {
    globals.Worker = before;
  }
});
