// A software decoder's `VideoDecoder` shape, as far as it goes without
// its worker.
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  createSoftwareDecoder,
  resetSoftwareDecodersForTests,
  type SoftwareModule,
  type SoftwarePlanes,
  softwareModuleFor,
} from "./softwareDecoder.ts";

const HEVC = "hev1.4.10.L150.BE.8";
const VP9_444 = "vp09.01.50.08.03.06.06.06.00";
const VP9_420 = "vp09.00.40.08.01.06.06.06.00";

test("the module decodes its own stream, HEVC, and no VP9 of either profile", () => {
  assert.equal(softwareModuleFor(HEVC), "hevc");
  assert.equal(softwareModuleFor("hvc1.1.6.L93.B0"), "hevc");
  assert.equal(softwareModuleFor(VP9_444), null);
  assert.equal(softwareModuleFor(VP9_420), null);
  assert.equal(softwareModuleFor("avc1.64002a"), null);
});

test("another codec is refused as VideoDecoder refuses one: asynchronously, NotSupportedError", async () => {
  const errors: Error[] = [];
  for (const [module, other] of [
    ["hevc", VP9_444],
    ["hevc", VP9_420],
  ] as [SoftwareModule, string][]) {
    const decoder = createSoftwareDecoder(module, {
      output: () => assert.fail("no output"),
      error: (e) => errors.push(e),
    });
    decoder.configure({ codec: other });
    assert.equal(errors.length, 0, "not from inside configure");
    await Promise.resolve();
    assert.equal(errors.length, 1);
    assert.equal(errors[0].name, "NotSupportedError");
    assert.equal(decoder.state, "closed");
    assert.throws(() => decoder.configure({ codec: HEVC }));
    errors.length = 0;
  }
});

/** The decode worker, as far as the decoder talks to it: what it was posted. */
class FakeWorker {
  static made: FakeWorker[] = [];
  posted: { type: string; id: number; module?: string; serial?: number }[] = [];
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onerror: unknown = null;
  name: string;
  constructor(_url: URL, options: { name: string }) {
    this.name = options.name;
    FakeWorker.made.push(this);
  }
  postMessage(command: {
    type: string;
    id: number;
    module?: string;
    serial?: number;
  }) {
    this.posted.push(command);
  }
  terminated = false;
  terminate() {
    this.terminated = true;
  }
}

const PLANES = {
  memory: new SharedArrayBuffer(16),
  width: 2,
  height: 2,
  planes: [0, 4, 8].map((offset) => ({ offset, stride: 2, width: 2, rows: 2 })),
  row: 0,
  keyframe: true,
  changed: { picture: "1:1", since: "1:0", x: 0, y: 0, width: 2, height: 2 },
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
    const pictures: SoftwarePlanes[] = [];
    const decoder = createSoftwareDecoder("hevc", {
      output: (picture) => pictures.push(picture as SoftwarePlanes),
      error: (e) => assert.fail(e.message),
    });
    decoder.configure({ codec: HEVC });
    const worker = FakeWorker.made[0];
    const id = worker.posted[0].id;
    const types = () => worker.posted.map((command) => command.type);
    const answer = (serial: number) =>
      worker.onmessage?.({
        data: { type: "decoded", id, picture: PLANES, serial },
      });

    // The decoder holds the picture's memory until then.
    decoder.decode(chunk);
    answer(7);
    assert.equal(pictures.length, 1);
    assert.equal(pictures[0].memory, PLANES.memory);
    assert.deepEqual(types(), ["create", "decode"]);
    pictures[0].close();
    pictures[0].close();
    assert.deepEqual(types(), ["create", "decode", "release"]);
    assert.deepEqual(worker.posted[2], { type: "release", id, serial: 7 });

    // A stream that ends releases with its `destroy`: a picture closed after it
    // says nothing to a decoder that is gone.
    decoder.decode(chunk);
    answer(8);
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

test("a worker whose module did not load is ended, and the next stream starts another", () => {
  const globals = globalThis as unknown as { Worker: unknown };
  const before = globals.Worker;
  globals.Worker = FakeWorker;
  try {
    const errors: Error[] = [];
    const first = createSoftwareDecoder("hevc", {
      output: () => assert.fail("no output"),
      error: (e) => errors.push(e),
    });
    first.configure({ codec: HEVC });
    const broken = FakeWorker.made[FakeWorker.made.length - 1];
    const made = FakeWorker.made.length;
    broken.onmessage?.({ data: { type: "broken", message: "no threads" } });
    assert.equal(errors.length, 1);
    assert.equal(errors[0].name, "NotSupportedError");
    assert.equal(first.state, "closed");
    assert.equal(broken.terminated, true);

    const second = createSoftwareDecoder("hevc", {
      output: () => assert.fail("no output"),
      error: (e) => assert.fail(e.message),
    });
    second.configure({ codec: HEVC });
    assert.equal(FakeWorker.made.length, made + 1);
    // The ended worker saying so again takes nothing of the new one down.
    broken.onmessage?.({ data: { type: "broken", message: "no threads" } });
    assert.equal(second.state, "configured");
    second.close();
  } finally {
    globals.Worker = before;
  }
});

test("the module has a decode worker of its own, told which module it is", () => {
  const globals = globalThis as unknown as { Worker: unknown };
  const before = globals.Worker;
  globals.Worker = FakeWorker;
  try {
    // From nothing: a module's worker outlives the streams that opened it, as it
    // does a session's.
    resetSoftwareDecodersForTests();
    const made = FakeWorker.made.length;
    const init = {
      output: () => assert.fail("no output"),
      error: (e: Error) => assert.fail(e.message),
    };
    const ended: string[] = [];
    const ending = { ...init, error: (e: Error) => ended.push(e.message) };
    const hevc = createSoftwareDecoder("hevc", ending);
    hevc.configure({ codec: HEVC });
    // A second stream of the module is a decoder in the worker it has.
    const again = createSoftwareDecoder("hevc", ending);
    again.configure({ codec: HEVC });
    const workers = FakeWorker.made.slice(made);
    assert.deepEqual(
      workers.map((worker) => worker.name),
      ["hevc-decoder"],
    );
    assert.deepEqual(
      workers.map((worker) => worker.posted.map((command) => command.module)),
      [["hevc", "hevc"]],
    );
    // The worker ending takes every decoder in it down, and the next stream
    // starts another.
    workers[0].onmessage?.({ data: { type: "broken", message: "not served" } });
    assert.deepEqual(ended, ["not served", "not served"]);
    const after = createSoftwareDecoder("hevc", init);
    after.configure({ codec: HEVC });
    assert.equal(FakeWorker.made.length, made + 2);
    after.close();
    resetSoftwareDecodersForTests();
  } finally {
    globals.Worker = before;
  }
});
