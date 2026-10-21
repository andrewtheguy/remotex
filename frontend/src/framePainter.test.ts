// The batch draw loop, over frames this file builds itself.
//
// Deliberately not built with `protocol.ts`'s encoder — there isn't one, and a
// test that produced its input with the same code that reads it would agree with
// itself no matter what either did. The layout below is transcribed from
// `batch` in `src/protocol.rs`, which is the contract both ends are checked
// against.
//
// Run with `bun test src/framePainter.test.ts` from frontend/.
import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import type { RelayMessage, RelayPort } from "./displayRelay.ts";
import type {
  ComposedRun,
  EgfxCompositor,
  GpuArea,
  RunPlan,
  Scanned,
} from "./egfxCompositor.ts";
import type { PicturePart } from "./egfxPicture.ts";
import type { EgfxVideo } from "./egfxVideo.ts";
import { createFramePainter, type FramePainter } from "./framePainter.ts";
import { placedAfter } from "./planesPicture.ts";

const OP_VIDEO = 0x03;

interface Unit {
  w: number;
  h: number;
  payload: number[];
  /** Defaults to true: most fixtures below are a stream's first unit. */
  keyframe?: boolean;
  /** Which strip of the picture the unit is, and whether its frame's last. */
  strip?: { index: number; ends: boolean };
}

function batchFrame(units: Unit[]): ArrayBuffer {
  const bytes: number[] = [];
  const u16 = (n: number) => bytes.push(n & 0xff, (n >> 8) & 0xff);
  const u32 = (n: number) =>
    bytes.push(n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, (n >> 24) & 0xff);
  bytes.push(0x02, 0x00);
  u16(units.length);
  u32(1); // attachment-local batch sequence
  for (const unit of units) {
    // A strip in the flags: 0x02, its number in 0x0C, and 0x10 on a frame's last.
    const strip = unit.strip
      ? 0x02 | (unit.strip.index << 2) | (unit.strip.ends ? 0x10 : 0)
      : 0;
    bytes.push(OP_VIDEO, (unit.keyframe === false ? 0 : 0x01) | strip);
    u16(unit.w);
    u16(unit.h);
    u32(unit.payload.length);
    bytes.push(...unit.payload);
  }
  return new Uint8Array(bytes).buffer;
}

// An access unit's payload. **Opaque here, deliberately**: nothing on this side of the
// wire parses a bitstream — the gateway says how to decode the stream in a `videoFormat`
// message and marks each unit's keyframe bit in the record — so these bytes only need to
// be distinguishable from each other.
const KEYFRAME = [0x82, 0x49, 0x83, 0x42, 0x00, 0x13, 0xf0];

interface FakeFrame {
  closed: boolean;
}

/**
 * The draws: where each picture's top left went, and the clip it was drawn
 * under, which is what cuts a picture that runs past the desktop.
 */
let cropped: { dx: number; dy: number; w: number; h: number }[] = [];
let decoded: FakeFrame[] = [];
let videoErrors: (string | null)[] = [];
/** Chains that were cut. The stub never goes quiet, so these are failures. */
let videoKeyframeAsks: string[] = [];

/** A canvas that records its draws, and nothing else of a context. */
const recording = (
  draw: (dx: number, dy: number, clip: { w: number; h: number }) => void,
) => {
  let clip = { w: Number.NaN, h: Number.NaN };
  const saved: (typeof clip)[] = [];
  return {
    save() {
      saved.push(clip);
    },
    restore() {
      clip = saved.pop() ?? clip;
    },
    beginPath() {},
    rect(_x: number, _y: number, w: number, h: number) {
      clip = { w, h };
    },
    clip() {},
    drawImage(_source: unknown, ...args: number[]) {
      // Three arguments: a picture is drawn whole, at its own size.
      assert.equal(args.length, 2);
      draw(args[0], args[1], clip);
    },
  } as unknown as CanvasRenderingContext2D;
};

const context = recording((dx, dy, clip) => {
  cropped.push({ dx, dy, ...clip });
});

// The WebCodecs decoder, which this runtime has none of — and which the client
// refuses to start without, so it is installed for every test here. Only its shape
// matters: what the painter does with the frames, not what a real decoder makes of
// the bitstream — that is browser QA.
let chunkTypes: string[] = [];
/** How many decoders were built — replaced on a resize. */
let decoders = 0;
let closes = 0;
/** A payload whose last byte is this makes its decoder give up. */
let poison: number | null = null;
/** Like `poison`, but the browser refusing the configuration rather than failing. */
let refused: number | null = null;
/** Like `poison`, but `decode()` itself throwing rather than the error callback. */
let rejected: number | null = null;
/** The configurations the decoders were built with. */
let configured: string[] = [];
/** Whether a decoder outputs planes, as a software decoder does. */
let planar = false;

class FakeVideoDecoder {
  private readonly output: (frame: unknown) => void;
  private readonly fail: (error: Error) => void;
  state = "unconfigured";

  constructor(init: {
    output: (frame: unknown) => void;
    error: (error: Error) => void;
  }) {
    this.output = init.output;
    this.fail = init.error;
    decoders += 1;
  }

  configure(config: { codec: string }) {
    configured.push(config.codec);
    this.state = "configured";
  }

  decode(
    chunk: { type: string; data?: Uint8Array },
    strip?: { index: number },
  ) {
    const last = chunk.data?.[chunk.data.length - 1];
    if (rejected !== null && last === rejected) {
      throw new TypeError("this chunk is not acceptable");
    }
    if (poison !== null && last === poison) {
      this.fail(new Error("this decoder gave up"));
      return;
    }
    if (refused !== null && last === refused) {
      // The name is the whole signal: it is how WebCodecs says "not this
      // configuration", which no later keyframe changes.
      const no = new Error("this configuration is not supported");
      no.name = "NotSupportedError";
      this.fail(no);
      return;
    }
    chunkTypes.push(chunk.type);
    const frame: FakeFrame = { closed: false };
    decoded.push(frame);
    this.output({
      // What marks a software decoder's picture from a `VideoFrame`.
      ...(planar
        ? { planes: [], strip: strip?.index, keyframe: chunk.type === "key" }
        : {}),
      close() {
        frame.closed = true;
      },
    });
  }

  close() {
    closes += 1;
    this.state = "closed";
  }
}

const globals = globalThis as unknown as {
  VideoDecoder: unknown;
  EncodedVideoChunk: unknown;
  createImageBitmap: unknown;
};

// The browser's image decoder, which this runtime has none of: a tile's image is
// told apart by its first byte, which also says how long it takes to decode and
// whether it decodes at all.
interface FakeBitmap {
  tag: number;
  type: string;
  closed: boolean;
  close(): void;
}
let bitmaps: FakeBitmap[] = [];
/** How long an image takes to decode, by its first byte, in ms. */
let decodeDelay = new Map<number, number>();
/** An image whose first byte is this does not decode. */
let undecodable: number | null = null;

async function fakeCreateImageBitmap(blob: Blob): Promise<FakeBitmap> {
  const tag = new Uint8Array(await blob.arrayBuffer())[0];
  await new Promise((resolve) =>
    setTimeout(resolve, decodeDelay.get(tag) ?? 0),
  );
  if (tag === undecodable) {
    throw new Error("not an image");
  }
  const bitmap: FakeBitmap = {
    tag,
    type: blob.type,
    closed: false,
    close() {
      this.closed = true;
    },
  };
  bitmaps.push(bitmap);
  return bitmap;
}

beforeEach(() => {
  uploaded = [];
  uploadedShown = [];
  reads = [];
  pictures = { made: 0, closed: 0 };
  blanked = [];
  windows = [];
  patched = [];
  shown = [];
  cropped = [];
  decoded = [];
  videoErrors = [];
  videoKeyframeAsks = [];
  chunkTypes = [];
  decoders = 0;
  closes = 0;
  poison = null;
  refused = null;
  rejected = null;
  configured = [];
  planar = false;
  planes = { made: 0, closed: 0, placed: [], drawn: [] };
  bitmaps = [];
  decodeDelay = new Map();
  undecodable = null;
  globals.createImageBitmap = fakeCreateImageBitmap;
  globals.VideoDecoder = FakeVideoDecoder;
  globals.EncodedVideoChunk = class {
    type: string;
    data: Uint8Array;
    constructor(init: { type: string; data: Uint8Array }) {
      this.type = init.type;
      this.data = init.data;
    }
  };
});

afterEach(() => {
  globals.VideoDecoder = undefined;
  globals.EncodedVideoChunk = undefined;
  globals.createImageBitmap = undefined;
});

function painter(ctx: CanvasRenderingContext2D | null = context) {
  return createFramePainter({
    context: () => ctx,
    onVideoError: (error) => {
      videoErrors.push(error);
    },
    onVideoNeedsKeyframe: (reason) => {
      videoKeyframeAsks.push(reason);
    },
  });
}

// The `videoFormat` a gateway sends before the stream's first unit. Every test goes
// through this, because a painter with no format refuses to decode — and that refusal
// has a test of its own below.
function announced(
  ctx: CanvasRenderingContext2D | null = context,
): FramePainter {
  const p = painter(ctx);
  p.setVideoFormat({ decode: "vp09.00.40.08" });
  return p;
}

test("a malformed frame is dropped whole", async () => {
  // Half an access unit is not a smaller access unit: submitting one would leave
  // the decoder's state wrong for every frame after it.
  const frame = batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]);
  await announced().draw(frame.slice(0, frame.byteLength - 1));
  assert.deepEqual(cropped, []);
  assert.deepEqual(chunkTypes, []);
});

test("a frame is cropped to the desktop, drawn at the origin", async () => {
  // An odd desktop is drawn at its own size from the origin.
  await announced().draw(batchFrame([{ w: 1599, h: 1015, payload: KEYFRAME }]));
  assert.deepEqual(cropped, [{ dx: 0, dy: 0, w: 1599, h: 1015 }]);
  assert.deepEqual(chunkTypes, ["key"]);
  assert.ok(
    decoded.every((frame) => frame.closed),
    "a VideoFrame holds decoder memory until it is closed",
  );
});

test("the browser's decoder's strips are placed by number, at their frame's last", async () => {
  // A display of 900 rows is in strips of 240, the last cut at its last row by
  // the clip. A frame is the strips that changed, and nothing of it is drawn
  // before its last.
  const p = announced();
  const strip = (index: number, ends: boolean, keyframe = false) => ({
    w: 1440,
    h: 900,
    payload: [index],
    keyframe,
    strip: { index, ends },
  });
  await p.draw(batchFrame([strip(0, false, true), strip(1, false)]));
  assert.deepEqual(cropped, []);
  assert.ok(decoded.every((frame) => !frame.closed));
  await p.draw(batchFrame([strip(2, false), strip(3, true)]));
  const at = (dy: number) => ({ dx: 0, dy, w: 1440, h: 900 });
  assert.deepEqual(cropped, [at(0), at(240), at(480), at(720)]);
  assert.ok(decoded.every((frame) => frame.closed));

  cropped = [];
  await p.draw(batchFrame([strip(2, true)]));
  assert.deepEqual(cropped, [at(480)]);
  assert.deepEqual(chunkTypes, ["key", "delta", "delta", "delta", "delta"]);
  assert.equal(decoders, 1);
});

test("strips held of an attachment that ended are closed, and never drawn", async () => {
  const p = announced();
  const first = {
    w: 64,
    h: 64,
    payload: [0],
    strip: { index: 0, ends: false },
  };
  await p.draw(batchFrame([first]));
  p.clear();
  assert.ok(decoded.every((frame) => frame.closed));
  p.setVideoFormat({ decode: "vp09.00.40.08" });
  await p.draw(batchFrame([{ ...first, strip: { index: 1, ends: true } }]));
  assert.equal(cropped.length, 1, "only the strip of the attachment that is");
});

test("a frame whose last strip the decoder gave no picture for is not painted", async () => {
  // The decoder gave up on the frame's last strip: the strips before it are of
  // a frame that will not be finished, and painting them would say video is back.
  poison = 0x66;
  const p = announced();
  const strip = (index: number, ends: boolean, payload: number[]) => ({
    w: 64,
    h: 64,
    payload,
    strip: { index, ends },
  });
  await p.draw(batchFrame([strip(0, false, [0]), strip(1, true, [0x66])]));
  assert.deepEqual(cropped, []);
  assert.ok(decoded.every((frame) => frame.closed));
  assert.equal(videoErrors.at(-1) === null, false, "the complaint stands");
});

test("strips are closed when drawing them throws", async () => {
  const throwing = recording(() => {
    throw new Error("the canvas is gone");
  });
  const p = announced(throwing);
  const last = { w: 64, h: 64, payload: [0], strip: { index: 0, ends: true } };
  await assert.rejects(p.draw(batchFrame([last])));
  assert.ok(decoded.every((frame) => frame.closed));
});

test("strips held give way to a whole picture, and to a stream announced", async () => {
  const p = announced();
  const first = {
    w: 64,
    h: 64,
    payload: [0],
    strip: { index: 0, ends: false },
  };
  await p.draw(batchFrame([first]));
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  assert.ok(decoded.every((frame) => frame.closed));
  assert.equal(cropped.length, 1, "the whole picture alone");

  await p.draw(batchFrame([first]));
  p.setVideoFormat({ decode: "vp09.00.40.08" });
  assert.ok(decoded.every((frame) => frame.closed));
});

test("a record's keyframe flag decides the chunk type, both ways", async () => {
  // The flag comes from the encoder, and VP9 — which has no parameter sets — offers a
  // client nothing to work it out from.
  await announced().draw(
    batchFrame([
      { w: 64, h: 64, payload: KEYFRAME },
      { w: 64, h: 64, payload: [1, 2, 3], keyframe: false },
    ]),
  );
  assert.deepEqual(chunkTypes, ["key", "delta"]);
});

test("a video record with an unknown flag drops the batch", async () => {
  // The same strictness the frame's own flags byte gets: a bit this client does not
  // know means a gateway newer than it, and painting half of what it meant is worse
  // than painting none of it.
  const frame = batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]);
  // Byte 8 is the op, 9 the flags.
  new Uint8Array(frame)[9] = 0x02;
  await announced().draw(frame);
  assert.deepEqual(chunkTypes, []);
  assert.deepEqual(cropped, []);
});

test("a record of an unknown op drops the batch", async () => {
  const frame = batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]);
  new Uint8Array(frame)[8] = 0x02;
  await announced().draw(frame);
  assert.deepEqual(chunkTypes, []);
});

test("units that arrive before their format are dropped, not reported", async () => {
  // The reattach: the gateway announces the stream once, to whoever was attached, so a
  // page that comes back to the session gets whatever was already in flight before the
  // repaint its attach triggers. Those units cannot be decoded here whatever happens,
  // so they are dropped in silence, and the repaint that follows carries the format
  // and a keyframe.
  const p = painter();
  await p.draw(
    batchFrame([{ w: 64, h: 64, payload: [4, 5, 6], keyframe: false }]),
  );
  assert.deepEqual(
    videoErrors,
    [null],
    "a unit before its format was reported",
  );
  assert.deepEqual(chunkTypes, []);
  assert.deepEqual(cropped, []);

  p.setVideoFormat({ decode: "vp09.00.40.08" });
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  assert.deepEqual(
    chunkTypes,
    ["key"],
    "the stream did not recover once announced",
  );
  assert.equal(cropped.length, 1);
});

test("a stream that restarts on a new size replaces its decoder", async () => {
  // The configuration string carries no resolution, so an in-band size change is not
  // something to bet two browsers on.
  const p = announced();
  await p.draw(batchFrame([{ w: 320, h: 64, payload: KEYFRAME }]));
  await p.draw(batchFrame([{ w: 640, h: 64, payload: KEYFRAME }]));
  assert.equal(
    decoders,
    2,
    "the resized desktop kept a decoder built for the old size",
  );
  assert.equal(closes, 1, "the replaced decoder was left holding memory");
});

test("a failed decoder asks for a keyframe, and the complaint goes when video paints again", async () => {
  poison = 0xbd;
  const p = announced();
  await p.draw(batchFrame([{ w: 320, h: 64, payload: [...KEYFRAME, 0xbd] }]));
  assert.equal(
    videoErrors.at(-1),
    "This browser's video decoder failed (Error: this decoder gave up).",
  );
  assert.equal(
    videoKeyframeAsks.length,
    1,
    "a failed decoder is thrown away, and only a keyframe starts another",
  );

  poison = null;
  await p.draw(batchFrame([{ w: 320, h: 64, payload: KEYFRAME }]));
  assert.equal(
    videoErrors.at(-1),
    null,
    "the banner outlived what it described",
  );
});

test("a refused stream says so, asks for nothing, and stays said", async () => {
  // The whole reason this is reported at all: the stream is all a target sends, so the
  // alternative is a desktop that never paints and never explains itself.
  refused = 0xbd;
  const p = announced();
  await p.draw(batchFrame([{ w: 64, h: 64, payload: [...KEYFRAME, 0xbd] }]));
  const said = videoErrors.filter(Boolean);
  assert.equal(said.length, 1);
  assert.match(String(said[0]), /cannot decode/);
  assert.deepEqual(
    videoKeyframeAsks,
    [],
    "a keyframe was asked for on a configuration no keyframe repairs",
  );
  assert.deepEqual(cropped, []);
});

test("clear() retracts the complaint and ends the decoder", async () => {
  // The attachment boundary. The page clears its own copy on the way back to the
  // picker only, so a reattach would otherwise inherit this sentence.
  refused = 0xbd;
  const p = announced();
  await p.draw(batchFrame([{ w: 64, h: 64, payload: [...KEYFRAME, 0xbd] }]));
  assert.notEqual(videoErrors.at(-1), null);
  p.clear();
  assert.equal(videoErrors.at(-1), null);

  refused = null;
  const q = announced();
  await q.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  q.clear();
  assert.ok(closes >= 1, "the decoder belongs to one attachment");
});

test("every decoded frame is closed, even with nowhere to draw it", async () => {
  await announced(null).draw(
    batchFrame([
      { w: 64, h: 64, payload: KEYFRAME },
      { w: 64, h: 64, payload: [1], keyframe: false },
    ]),
  );
  assert.deepEqual(cropped, []);
  assert.equal(decoded.length, 2);
  assert.ok(
    decoded.every((frame) => frame.closed),
    "a frame that is never drawn still has to be released",
  );
});

test("a malformed batch cuts the chain: the decoder restarts and a keyframe is asked for", async () => {
  // Every unit is part of one chain, so the deltas after a dropped batch name a picture
  // this decoder never made.
  const p = announced();
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  const frame = batchFrame([{ w: 64, h: 64, payload: [1], keyframe: false }]);
  await p.draw(frame.slice(0, frame.byteLength - 1));
  assert.equal(videoKeyframeAsks.length, 1);
  await p.draw(batchFrame([{ w: 64, h: 64, payload: [2], keyframe: false }]));
  assert.deepEqual(chunkTypes, ["key"], "a delta was fed past the cut");
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  assert.deepEqual(chunkTypes, ["key", "key"]);
  assert.equal(decoders, 2, "the cut chain kept its decoder");
});

test("a unit decode() throws on cuts the chain without misplacing a frame", async () => {
  // The unit was never consumed, so every delta after it is against a missing picture.
  rejected = 0xbd;
  const p = announced();
  await p.draw(
    batchFrame([
      { w: 64, h: 64, payload: KEYFRAME },
      { w: 64, h: 64, payload: [1, 0xbd], keyframe: false },
      { w: 64, h: 64, payload: [2], keyframe: false },
    ]),
  );
  assert.equal(videoKeyframeAsks.length, 1, "no keyframe was asked for");
  // Raised, then taken down again by the keyframe decoded ahead of the cut, which
  // still paints.
  assert.ok(
    videoErrors.some((error) => error?.includes("refused a frame")),
    String(videoErrors),
  );
  assert.deepEqual(chunkTypes, ["key"], "a delta was fed past the cut");
  assert.ok(decoded.every((frame) => frame.closed));

  rejected = null;
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  assert.equal(videoErrors.at(-1), null, "the stream did not recover");
});

test("a refusal gives way to a configuration the browser takes", async () => {
  // The announced VP9 level follows the desktop's size, so a browser that refused a
  // large picture may decode the smaller one a resize brings.
  refused = 0xbd;
  const p = painter();
  p.setVideoFormat({ decode: "vp09.01.51.08" });
  await p.draw(batchFrame([{ w: 64, h: 64, payload: [...KEYFRAME, 0xbd] }]));
  assert.match(String(videoErrors.at(-1)), /cannot decode/);

  // The same configuration again changes nothing.
  p.setVideoFormat({ decode: "vp09.01.51.08" });
  await p.draw(batchFrame([{ w: 64, h: 64, payload: [3], keyframe: false }]));
  assert.match(String(videoErrors.at(-1)), /cannot decode/);

  refused = null;
  p.setVideoFormat({ decode: "vp09.01.40.08" });
  assert.match(
    String(videoErrors.at(-1)),
    /cannot decode/,
    "the banner came down before anything painted",
  );
  await p.draw(batchFrame([{ w: 32, h: 32, payload: KEYFRAME }]));
  assert.equal(cropped.length, 1);
  assert.equal(
    videoErrors.at(-1),
    null,
    "the refusal outlived its configuration",
  );
  assert.equal(configured.at(-1), "vp09.01.40.08");
});

// The byte the fake compositor's messages start with, which its `unwrap` takes off:
// a record composed without being unwrapped would start with it.
const WRAPPED = 0xe0;

// A GRAPHICS record: op 0x04, a length, the message — each run wrapped as the fake
// compositor below unwraps it, but for an empty one, which stays a record of no
// length.
function graphicsFrame(runs: number[][]): ArrayBuffer {
  const bytes: number[] = [0x02, 0x00, runs.length & 0xff, runs.length >> 8];
  bytes.push(1, 0, 0, 0);
  for (const run of runs) {
    const message = run.length === 0 ? [] : [WRAPPED, ...run];
    const n = message.length;
    bytes.push(0x04, n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, n >>> 24);
    bytes.push(...message);
  }
  return new Uint8Array(bytes).buffer;
}

// A compositor that records what it was fed and paints what it is told to. What a
// real one makes of the commands is egfxCompositor.test.ts; here it is the painter's
// handling of one that is under test.
//
// A run whose first byte is `H264` carries H.264: its second byte is how many
// access units, each one byte long, and its third a surface whose stream ended.
const H264 = 200;
function h264Runs(commands: Uint8Array): Scanned[] {
  if (commands[0] !== H264) {
    return [];
  }
  const found: Scanned[] = [];
  if (commands[2] !== undefined) {
    found.push({ gone: commands[2] });
  }
  for (let number = 0; number < commands[1]; number += 1) {
    const start = 3 + number;
    found.push({
      unit: {
        surface: 1,
        start,
        end: start + 1,
        key: number === 0,
        codec: number === 0 ? "avc1.4d4020" : null,
        window: "whole",
      },
      number,
    });
  }
  return found;
}

function fakeCompositors(
  options: {
    refuse?: number;
    garbled?: number;
    fail?: boolean;
    /** What each run's plan says, by the run's first byte: none shown from the GPU else. */
    plan?: (commands: Uint8Array) => RunPlan;
  } = {},
) {
  const made: {
    /** The first byte of each message unwrapped, in order. */
    unwrapped: number[];
    fed: number[][];
    closed: boolean;
    /** Each picture supplied: its number in its run, and the unit's byte. */
    supplied: number[][];
    /** Each unit shown from the GPU instead, by its number in its run. */
    onGpu: number[];
    /** What was read back, in order with what was fed: `restore x,y` or `fed n`. */
    events: string[];
  }[] = [];
  const load = () => {
    if (options.fail) {
      return Promise.reject(new Error("no module"));
    }
    return Promise.resolve((): EgfxCompositor => {
      const record = {
        unwrapped: [] as number[],
        fed: [] as number[][],
        closed: false,
        supplied: [] as number[][],
        onGpu: [] as number[],
        events: [] as string[],
      };
      // The units this run shows from the GPU.
      let showing: number[] = [];
      made.push(record);
      // The picture: 64 by 48, every byte the last run's first byte, and of
      // nothing before the first run, as a compositor's is before its reset.
      const pixels = new Uint8ClampedArray(64 * 48 * 4);
      const picture = () =>
        record.fed.length === 0
          ? { width: 0, height: 0, pixels: new Uint8ClampedArray(0) }
          : { width: 64, height: 48, pixels };
      return {
        unwrap(wrapped) {
          assert.equal(wrapped[0], WRAPPED, "a message unwrapped twice");
          const commands = wrapped.subarray(1);
          if (commands[0] === options.garbled) {
            throw new Error("a message that does not unwrap");
          }
          record.unwrapped.push(commands[0]);
          return commands;
        },
        scan: h264Runs,
        supply(number, _window, frame) {
          assert.equal(
            record.closed,
            false,
            "a closed compositor was supplied",
          );
          record.supplied.push([
            number,
            (frame as unknown as FakePicture).unit,
          ]);
          return Promise.resolve();
        },
        plan(commands) {
          return options.plan?.(commands) ?? { gpu: [], restore: [] };
        },
        showOnGpu(number) {
          record.onGpu.push(number);
          showing.push(number);
        },
        gpuAreas: () => [],
        restore(area, read) {
          assert.equal(read.length, area.w * area.h * 4);
          record.events.push(`restore ${area.x},${area.y}`);
        },
        compose(commands): ComposedRun {
          assert.notEqual(commands[0], WRAPPED, "a message composed wrapped");
          if (commands[0] === options.refuse) {
            throw new Error("a command that does not decode");
          }
          record.fed.push([...commands]);
          record.events.push(`fed ${commands[0]}`);
          pixels.fill(commands[0]);
          // Each unit shown from the GPU draws one pixel, at its number.
          const gpu = Uint32Array.from(
            showing.flatMap((number) => [number, number, 0, 1, 1, 0, 0]),
          );
          showing = [];
          return {
            // Each run paints one rectangle named by its first byte; the first
            // run of a pipeline is the reset that lays the picture out.
            painted: new Uint32Array([commands[0], 2, 3, 4]),
            resized: record.fed.length === 1,
            gpu,
            ...picture(),
          };
        },
        picture,
        close() {
          record.closed = true;
        },
      };
    });
  };
  return { made, load };
}

/** A decoded picture: the byte of the unit it came from, and whether it is closed. */
interface FakePicture {
  unit: number;
  closed: boolean;
  close(): void;
  colorSpace: VideoColorSpaceInit;
  visibleRect: { width: number; height: number };
  displayWidth: number;
  displayHeight: number;
}

/** The colour a host's H.264 is, as a browser labels a picture it converts so. */
const AS_CODED: VideoColorSpaceInit = {
  primaries: "bt709",
  transfer: "iec61966-2-1",
  matrix: "bt709",
  fullRange: true,
};

/**
 * A pipeline's H.264 decoders, recording what they were asked. `hold` keeps each
 * decode waiting until `release` is called, and `fail` rejects every one. Their
 * pictures are labelled as a software decoder's are, BT.601, but for the units
 * whose bytes are in `coded`, labelled with the stream's own colour.
 */
function fakeVideo(
  options: { fail?: boolean; hold?: boolean; coded?: number[] } = {},
) {
  const log: string[] = [];
  const frames: FakePicture[] = [];
  const held: (() => void)[] = [];
  let made = 0;
  const make = (): EgfxVideo => {
    made += 1;
    let closed = false;
    return {
      decode(unit, data) {
        log.push(`decode ${unit.surface}:${data[0]}${unit.key ? " key" : ""}`);
        return new Promise((resolve, reject) => {
          const settle = () => {
            if (options.fail || closed) {
              reject(new Error("its H.264 decoder failed: nothing"));
              return;
            }
            const frame: FakePicture = {
              unit: data[0],
              closed: false,
              close() {
                this.closed = true;
              },
              colorSpace: options.coded?.includes(data[0])
                ? AS_CODED
                : { ...AS_CODED, matrix: "smpte170m" },
              visibleRect: { width: 64, height: 48 },
              displayWidth: 64,
              displayHeight: 48,
            };
            frames.push(frame);
            resolve(frame as unknown as VideoFrame);
          };
          if (options.hold) {
            held.push(settle);
          } else {
            settle();
          }
        });
      },
      drop(surface) {
        log.push(`drop ${surface}`);
      },
      close() {
        closed = true;
        log.push("close");
        for (const settle of held.splice(0)) {
          settle();
        }
      },
    };
  };
  return { make, log, frames, held, made: () => made };
}

/** The picture of a pipeline: what each run's upload named, painted or not. */
let uploaded: number[][] = [];
/**
 * The pictures each upload was handed to draw from the GPU: each unit's number
 * and byte, and whether it was still open.
 */
let uploadedShown: [number, number, boolean][][] = [];
/** What was read back off a picture: `x, y, w, h`. */
let reads: number[][] = [];
/** How many pictures were made, and how many closed. */
let pictures = { made: 0, closed: 0 };
/** The sizes a picture was blanked at. */
let blanked: number[][] = [];
/** The parts a picture was told to show, in order. */
let windows: (PicturePart | null)[] = [];
/** What a picture was patched with: its size, the rectangles, the bytes. */
let patched: [number, number, number[], number][] = [];
/** What the page was told about showing the picture, in order. */
let shown: boolean[] = [];

/** One end of the second display's channel, by hand: what it posted, and a way to
 * deliver what the other end says. */
function fakeRelay() {
  const posted: RelayMessage[] = [];
  let handler: (message: RelayMessage) => void = () => {};
  let closed = 0;
  const port: RelayPort = {
    post: (message) => {
      posted.push(message);
    },
    onMessage: (next) => {
      handler = next;
    },
    close: () => {
      closed += 1;
    },
  };
  return {
    port,
    posted,
    deliver: (message: RelayMessage) => handler(message),
    closed: () => closed,
  };
}

function graphicsPainter(
  load: ReturnType<typeof fakeCompositors>["load"],
  options: {
    noPicture?: boolean;
    blankFails?: boolean;
    video?: () => EgfxVideo;
    relay?: RelayPort;
  } = {},
) {
  return createFramePainter({
    makeRelay: () => options.relay ?? fakeRelay().port,
    makeGraphicsVideo:
      options.video ??
      (() => {
        throw new Error("a pipeline without H.264 made a decoder for it");
      }),
    context: () => context,
    onVideoError: (error) => {
      videoErrors.push(error);
    },
    onVideoNeedsKeyframe: (reason) => {
      videoKeyframeAsks.push(reason);
    },
    loadCompositor: load,
    makePicture: () => {
      if (options.noPicture) {
        throw new Error("WebGL 2 is not available");
      }
      pictures.made += 1;
      return {
        upload(run, frames = new Map()) {
          uploaded.push([...run.painted]);
          uploadedShown.push(
            [...frames].map(([number, frame]) => {
              const fake = frame as unknown as FakePicture;
              return [number, fake.unit, !fake.closed];
            }),
          );
        },
        read(x, y, w, h) {
          reads.push([x, y, w, h]);
          return new Uint8Array(w * h * 4);
        },
        window(part) {
          windows.push(part);
        },
        patch(w, h, rects, pixels) {
          patched.push([w, h, Array.from(rects), pixels.length]);
        },
        blank(w, h) {
          if (options.blankFails) {
            throw new Error("the GPU refused the picture");
          }
          blanked.push([w, h]);
        },
        close() {
          pictures.closed += 1;
        },
      };
    },
    onGraphicsShown: (on) => {
      shown.push(on);
    },
  });
}

test("a pipeline's runs are composed in order and their rectangles uploaded", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[7, 1], [9]]));
  await p.draw(graphicsFrame([[11, 5, 5]]));
  assert.equal(made.length, 1, "one compositor follows the whole pipeline");
  assert.deepEqual(made[0].fed, [[7, 1], [9], [11, 5, 5]]);
  assert.deepEqual(uploaded, [
    [7, 2, 3, 4],
    [9, 2, 3, 4],
    [11, 2, 3, 4],
  ]);
  // The picture is shown where it is drawn: nothing of it goes onto the
  // desktop's canvas, and the page is told to show it once, at the first run.
  assert.deepEqual(cropped, []);
  assert.deepEqual(shown, [true]);
  assert.equal(decoders, 0, "a pipeline built a video decoder");
  assert.deepEqual(videoKeyframeAsks, []);
});

test("a pipeline that starts again is composed from nothing", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  p.startGraphics();
  await p.draw(graphicsFrame([[2]]));
  assert.deepEqual(
    made.map((compositor) => [compositor.fed, compositor.closed]),
    [
      [[[1]], true],
      [[[2]], false],
    ],
  );
  assert.deepEqual(
    pictures,
    { made: 2, closed: 1 },
    "a picture each, the first given back",
  );
  assert.deepEqual(
    shown,
    [true, false, true],
    "the first hidden before the second has drawn anything",
  );
});

test("a picture is not shown before its pipeline has drawn a run", async () => {
  const { load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([]));
  assert.deepEqual(shown, []);
  p.clear();
  assert.deepEqual(shown, [], "and one never shown is not hidden");
});

test("a desktop resized under a pipeline blanks its picture", async () => {
  const { load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.blank(800, 600);
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  p.blank(1024, 768);
  assert.deepEqual(blanked, [[1024, 768]], "only a pipeline's picture");
  assert.deepEqual(
    videoErrors.filter((error) => error !== null),
    [],
  );
});

test("the part of the picture a display is, is what the picture shows", async () => {
  const { load } = fakeCompositors();
  const p = graphicsPainter(load);
  // Named ahead of the pipeline, as the gateway's `resize` and `graphicsView`
  // come ahead of its `graphicsStart`: applied to the picture when it is made.
  p.setGraphicsView({ x: 32, y: 0, w: 32, h: 48 });
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  assert.deepEqual(windows, [{ x: 32, y: 0, w: 32, h: 48 }]);
  // The picker moving to the other display: shown at once, from what the
  // picture holds, and kept by the next pipeline.
  p.setGraphicsView({ x: 0, y: 0, w: 32, h: 48 });
  p.startGraphics();
  await p.draw(graphicsFrame([[2]]));
  assert.deepEqual(windows, [
    { x: 32, y: 0, w: 32, h: 48 },
    { x: 0, y: 0, w: 32, h: 48 },
    { x: 0, y: 0, w: 32, h: 48 },
  ]);
  // The attachment boundary forgets it: the next names its own.
  p.clear();
  p.startGraphics();
  await p.draw(graphicsFrame([[3]]));
  assert.equal(windows[windows.length - 1], null);
});

test("a tab showing the second display is sent its column of the picture", async () => {
  const { load } = fakeCompositors();
  const relay = fakeRelay();
  const p = graphicsPainter(load, { relay: relay.port });
  p.startGraphics();
  assert.deepEqual(
    relay.posted,
    [{ kind: "composing" }],
    "a source that starts asks",
  );
  // The tab answers before the first run: it is owed everything once there is a
  // picture, which the first run — the reset — lays out.
  relay.deliver({
    kind: "shown",
    display: 2,
    part: { x: 32, y: 0, w: 32, h: 48 },
  });
  await p.draw(graphicsFrame([[40]]));
  assert.equal(relay.posted.length, 2);
  const first = relay.posted[1];
  assert.equal(first.kind, "paint");
  if (first.kind !== "paint") {
    return;
  }
  assert.deepEqual(
    [first.seq, first.w, first.h, first.rects],
    [1, 32, 48, [0, 0, 32, 48]],
  );
  const bytes = new Uint8Array(first.pixels);
  assert.equal(bytes.length, 32 * 48 * 4);
  assert.ok(
    bytes.every((byte) => byte === 40),
    "the column, out of the picture",
  );
  // While that is in flight, what the runs paint waits and is merged; what
  // falls outside the column is nothing to the tab.
  await p.draw(graphicsFrame([[36]]));
  await p.draw(graphicsFrame([[50]]));
  await p.draw(graphicsFrame([[7]]));
  assert.equal(relay.posted.length, 2, "one update in flight at a time");
  relay.deliver({ kind: "painted", seq: 1 });
  const second = relay.posted[2];
  assert.equal(second?.kind, "paint");
  if (second?.kind !== "paint") {
    return;
  }
  // Relative to the column, and out of the picture as it stands now.
  assert.deepEqual([second.seq, second.rects], [2, [4, 2, 3, 4, 18, 2, 3, 4]]);
  const latest = new Uint8Array(second.pixels);
  assert.equal(latest.length, 2 * 3 * 4 * 4);
  assert.ok(latest.every((byte) => byte === 7));
  // A pipeline that starts again owes nothing of the old picture; its first run
  // owes the column again.
  relay.deliver({ kind: "painted", seq: 2 });
  p.startGraphics();
  await p.draw(graphicsFrame([[9]]));
  const third = relay.posted[3];
  assert.equal(third?.kind, "paint");
  if (third?.kind === "paint") {
    assert.deepEqual(third.rects, [0, 0, 32, 48]);
  }
});

test("a tab is painted from the session page's picture, not composed", async () => {
  const { made, load } = fakeCompositors();
  const relay = fakeRelay();
  const p = graphicsPainter(load, { relay: relay.port });
  p.blank(32, 48);
  p.mirrorGraphics(2, { x: 32, y: 0, w: 32, h: 48 });
  assert.deepEqual(relay.posted, [
    { kind: "shown", display: 2, part: { x: 32, y: 0, w: 32, h: 48 } },
  ]);
  assert.deepEqual(shown, [], "nothing to show until the first update");
  relay.deliver({
    kind: "paint",
    seq: 3,
    w: 32,
    h: 48,
    rects: [0, 0, 32, 48],
    pixels: new ArrayBuffer(32 * 48 * 4),
  });
  assert.deepEqual(patched, [[32, 48, [0, 0, 32, 48], 32 * 48 * 4]]);
  assert.deepEqual(relay.posted[1], { kind: "painted", seq: 3 });
  assert.deepEqual(shown, [true]);
  // A session page that starts composing asks; the tab says again what it shows.
  relay.deliver({ kind: "composing" });
  assert.deepEqual(relay.posted[2], {
    kind: "shown",
    display: 2,
    part: { x: 32, y: 0, w: 32, h: 48 },
  });
  // A layout change: the tab's desktop blanks its picture, and the new part is
  // said on the same channel.
  p.blank(40, 48);
  p.mirrorGraphics(2, { x: 32, y: 0, w: 40, h: 48 });
  assert.deepEqual(blanked, [[40, 48]]);
  assert.equal(relay.posted.length, 4);
  assert.equal(made.length, 0, "a tab composes nothing");
  // The attachment boundary gives the picture back and hides it.
  p.clear();
  assert.deepEqual(shown, [true, false]);
  assert.deepEqual(pictures, { made: 1, closed: 1 });
  assert.equal(relay.closed(), 1);
});

test("a video format takes the display back from a tab's mirror", async () => {
  // The tab is sent a stream: it is drawn on the desktop's canvas the mirror's
  // picture covered.
  const { load } = fakeCompositors();
  const relay = fakeRelay();
  const p = graphicsPainter(load, { relay: relay.port });
  p.mirrorGraphics(2, { x: 32, y: 0, w: 32, h: 48 });
  relay.deliver({
    kind: "paint",
    seq: 1,
    w: 32,
    h: 48,
    rects: [0, 0, 32, 48],
    pixels: new ArrayBuffer(32 * 48 * 4),
  });
  assert.deepEqual(shown, [true]);
  p.setVideoFormat({ decode: "vp09.00.40.08" });
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  assert.deepEqual(chunkTypes, ["key"]);
  assert.deepEqual(shown, [true, false]);
  assert.deepEqual(pictures, { made: 1, closed: 1 });
  assert.equal(relay.closed(), 1);
  // A display passed again is mirrored afresh.
  p.mirrorGraphics(2, { x: 32, y: 0, w: 32, h: 48 });
  assert.equal(pictures.made, 2);
});

test("a picture that cannot be blanked ends its pipeline", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load, { blankFails: true });
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  p.blank(1024, 768);
  await p.draw(graphicsFrame([[2]]));
  assert.deepEqual(made[0].fed, [[1]]);
  const said = videoErrors.filter((error) => error !== null);
  assert.match(said[0] ?? "", /the GPU refused the picture/);
});

test("a run's H.264 is decoded and supplied before the run is composed", async () => {
  const { made, load } = fakeCompositors();
  const video = fakeVideo();
  const p = graphicsPainter(load, { video: video.make });
  p.startGraphics();
  await p.draw(graphicsFrame([[1], [H264, 2, 5, 31, 32], [H264, 1, 5, 33]]));
  assert.equal(video.made(), 1, "one set of decoders follows the pipeline");
  // Each unit goes through in command order, the first of a stream as its key,
  // and a surface's stream is ended before the units after it are decoded — once
  // there are decoders for one to have been in.
  assert.deepEqual(video.log, [
    "decode 1:31 key",
    "decode 1:32",
    "drop 5",
    "decode 1:33 key",
  ]);
  assert.deepEqual(
    made[0].supplied,
    [
      [0, 31],
      [1, 32],
      [0, 33],
    ],
    "each picture under its unit's number in its own run",
  );
  assert.deepEqual(
    made[0].fed.map((run) => run[0]),
    [1, H264, H264],
  );
  assert.ok(
    video.frames.every((frame) => frame.closed),
    "a picture handed over is closed",
  );
  assert.deepEqual(
    videoErrors.filter((error) => error !== null),
    [],
  );
  p.clear();
  assert.equal(video.log.at(-1), "close");
});

test("a picture the browser draws as coded is left on the GPU where the run's plan allows", async () => {
  const area: GpuArea = {
    surface: 1,
    x: 2,
    y: 3,
    w: 4,
    h: 5,
    outputX: 12,
    outputY: 13,
  };
  const { made, load } = fakeCompositors({
    // The run reads a rectangle the GPU shows, and may show its first two units
    // there.
    plan: (commands) =>
      commands[0] === H264
        ? { gpu: [true, true, false], restore: [area] }
        : { gpu: [], restore: [] },
  });
  // The first and third units' pictures are labelled with the stream's colour.
  const video = fakeVideo({ coded: [31, 33] });
  const p = graphicsPainter(load, { video: video.make });
  p.startGraphics();
  await p.draw(graphicsFrame([[1], [H264, 3, 5, 31, 32, 33]]));
  assert.deepEqual(made[0].onGpu, [0], "allowed, and drawn as coded");
  assert.deepEqual(
    made[0].supplied,
    [
      [1, 32],
      [2, 33],
    ],
    "a picture labelled otherwise, or one the plan keeps off the GPU, is supplied",
  );
  assert.deepEqual(reads, [[12, 13, 4, 5]], "read where the output has it");
  assert.deepEqual(
    made[0].events,
    ["fed 1", "restore 2,3", `fed ${H264}`],
    "what the run reads is restored before it is composed",
  );
  assert.deepEqual(
    uploadedShown.at(-1),
    [[0, 31, true]],
    "the picture is drawn from while it is open",
  );
  assert.ok(
    video.frames.every((frame) => frame.closed),
    "and closed once the run is drawn",
  );
  assert.deepEqual(
    videoErrors.filter((error) => error !== null),
    [],
  );
});

test("an H.264 unit that gives no picture ends the pipeline and says so", async () => {
  const { made, load } = fakeCompositors();
  const video = fakeVideo({ fail: true });
  const p = graphicsPainter(load, { video: video.make });
  p.startGraphics();
  await p.draw(graphicsFrame([[1], [H264, 1, 5, 31], [3]]));
  assert.deepEqual(made[0].fed, [[1]], "the run was not composed without it");
  assert.equal(made[0].closed, true);
  const said = videoErrors.filter((error) => error !== null);
  assert.equal(said.length, 1);
  assert.match(said[0] ?? "", /could not compose the host's graphics/);
  assert.match(said[0] ?? "", /H\.264 decoder failed/);
  assert.deepEqual(
    videoKeyframeAsks,
    [],
    "a host sends no keyframe on request",
  );
});

test("an attachment that ends while a unit decodes leaves its compositor alone", async () => {
  const { made, load } = fakeCompositors();
  const video = fakeVideo({ hold: true });
  const p = graphicsPainter(load, { video: video.make });
  p.startGraphics();
  const late = p.draw(graphicsFrame([[H264, 1, 5, 31]]));
  while (video.held.length === 0) {
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
  // The clear closes the decoders, which settles the unit the run waits on.
  p.clear();
  await late;
  assert.equal(made[0].closed, true);
  assert.deepEqual(made[0].supplied, []);
  assert.deepEqual(made[0].fed, []);
  assert.deepEqual(
    videoErrors.filter((error) => error !== null),
    [],
    "an ended attachment has nothing to say",
  );
});

test("a run with no pipeline started is dropped", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  await p.draw(graphicsFrame([[1]]));
  assert.deepEqual(made, []);
  assert.deepEqual(uploaded, []);
});

test("a message that does not unwrap ends the pipeline, and none after it is unwrapped", async () => {
  const { made, load } = fakeCompositors({ garbled: 66 });
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[1], [66], [3]]));
  await p.draw(graphicsFrame([[4]]));
  assert.deepEqual(made[0].unwrapped, [1]);
  assert.deepEqual(made[0].fed, [[1]], "nothing is composed after it");
  assert.equal(made[0].closed, true);
  const said = videoErrors.filter((error) => error !== null);
  assert.equal(said.length, 1);
  assert.match(said[0] ?? "", /a message that does not unwrap/);
});

test("a command that does not decode ends the pipeline and says so", async () => {
  const { made, load } = fakeCompositors({ refuse: 66 });
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[1], [66], [3]]));
  await p.draw(graphicsFrame([[4]]));
  assert.deepEqual(made[0].fed, [[1]], "nothing is composed after the refusal");
  assert.equal(made[0].closed, true);
  assert.equal(uploaded.length, 1);
  assert.deepEqual(
    [pictures.closed, shown],
    [0, [true]],
    "the picture stays, as the desktop under the sentence",
  );
  p.clear();
  assert.deepEqual([pictures.closed, shown], [1, [true, false]]);
  const said = videoErrors.filter((error) => error !== null);
  assert.equal(said.length, 1);
  assert.match(said[0] ?? "", /could not compose the host's graphics/);
  assert.deepEqual(
    videoKeyframeAsks,
    [],
    "no repaint repairs a pipeline: the host answers one out of its caches",
  );
});

test("a browser without WebGL 2 is told the compositor could not be loaded", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load, { noPicture: true });
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  const said = videoErrors.filter((error) => error !== null);
  assert.match(said[0] ?? "", /could not load the graphics compositor/);
  assert.match(said[0] ?? "", /WebGL 2 is not available/);
  assert.equal(made[0]?.closed, true, "the compositor made is given back");
  assert.deepEqual(uploaded, []);
});

test("a compositor that will not load says so, and the next pipeline tries again", async () => {
  const failing = fakeCompositors({ fail: true });
  const p = graphicsPainter(failing.load);
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  const said = videoErrors.filter((error) => error !== null);
  assert.match(said[0] ?? "", /could not load the graphics compositor/);
  assert.deepEqual(uploaded, []);
});

test("a video format takes the picture back from a pipeline", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  p.setVideoFormat({ decode: "vp09.00.40.08" });
  await p.draw(graphicsFrame([[2]]));
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  assert.deepEqual(made[0].fed, [[1]]);
  assert.equal(made[0].closed, true);
  assert.deepEqual(chunkTypes, ["key"]);
  assert.deepEqual(
    shown,
    [true, false],
    "the stream is drawn on the desktop's canvas, which the picture covered",
  );
});

test("clear() ends a pipeline, and a run of the attachment before is not painted", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  const late = p.draw(graphicsFrame([[1]]));
  p.clear();
  await late;
  assert.deepEqual(uploaded, []);
  assert.deepEqual(made, [], "the module loaded for a pipeline that was gone");
});

test("a malformed batch ends a pipeline: what it held is never composed, and no repaint is asked for", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  // A record of no length is malformed, and the batch is dropped whole.
  await p.draw(graphicsFrame([[2], []]));
  await p.draw(graphicsFrame([[3]]));
  await p.draw(graphicsFrame([[]]));
  assert.deepEqual(
    made[0].fed,
    [[1]],
    "nothing is composed after the dropped batch",
  );
  assert.equal(made[0].closed, true);
  assert.equal(uploaded.length, 1);
  const said = videoErrors.filter((error) => error !== null);
  assert.equal(said.length, 1, "the end of a pipeline is said once");
  assert.match(said[0] ?? "", /could not compose the host's graphics/);
  assert.deepEqual(
    videoKeyframeAsks,
    [],
    "no repaint repairs a pipeline: the host answers one out of its caches",
  );
});

test("a malformed batch ahead of the module's load leaves no compositor made", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[]]));
  await p.draw(graphicsFrame([[1]]));
  assert.deepEqual(made, []);
  assert.deepEqual(uploaded, []);
  assert.deepEqual(videoKeyframeAsks, []);
});

test("a pipeline that starts after one ended is composed", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[]]));
  p.startGraphics();
  await p.draw(graphicsFrame([[5]]));
  assert.deepEqual(
    made.map((compositor) => compositor.fed),
    [[[5]]],
  );
  assert.equal(videoErrors.at(-1), null, "the next pipeline retracts it");
});

/**
 * A software decoder's picture: how many were made and closed, the strips placed
 * in it (-1 for a picture whole), and the sizes drawn.
 */
let planes: {
  made: number;
  closed: number;
  placed: number[];
  drawn: number[][];
} = {
  made: 0,
  closed: 0,
  placed: [],
  drawn: [],
};

const HEVC = { decode: "hev1.4.10.L150.BE.8" };

function planesPainter(options: { refuses?: boolean } = {}) {
  planar = true;
  const p = createFramePainter({
    context: () => context,
    onVideoError: (error) => {
      videoErrors.push(error);
    },
    onVideoNeedsKeyframe: (reason) => {
      videoKeyframeAsks.push(reason);
    },
    makePlanesPicture: () => {
      planes.made += 1;
      let placed = 0;
      return {
        place(picture) {
          if (options.refuses) {
            throw new Error("the GPU refused the picture");
          }
          planes.placed.push(picture.strip ?? -1);
          placed = placedAfter(placed, picture);
        },
        draw(w, h) {
          if (placed !== 0b1111) {
            return false;
          }
          planes.drawn.push([w, h]);
          return true;
        },
        close() {
          planes.closed += 1;
        },
      };
    },
    onGraphicsShown: (on) => {
      shown.push(on);
    },
  });
  p.setVideoFormat(HEVC);
  return p;
}

test("the software decoder's pictures are drawn over the desktop's canvas, and closed", async () => {
  const p = planesPainter();
  await p.draw(batchFrame([{ w: 640, h: 480, payload: KEYFRAME }]));
  await p.draw(batchFrame([{ w: 640, h: 480, payload: [1], keyframe: false }]));
  assert.deepEqual(planes.drawn, [
    [640, 480],
    [640, 480],
  ]);
  assert.equal(planes.made, 1, "one picture follows the stream");
  assert.deepEqual(cropped, [], "nothing of it goes onto the desktop's canvas");
  assert.deepEqual(shown, [true], "shown once, at the first picture");
  // The decoder starts its next unit only once a picture is closed.
  assert.deepEqual(
    decoded.map((frame) => frame.closed),
    [true, true],
  );
});

test("the software decoder's strips are placed as they decode, and drawn at a frame's last once all have come", async () => {
  const p = planesPainter();
  const strip = (index: number, ends: boolean, keyframe = false) => ({
    w: 640,
    h: 480,
    payload: [index],
    keyframe,
    strip: { index, ends },
  });
  // A frame of two strips after the keyframe: the display is not whole yet.
  await p.draw(batchFrame([strip(0, false, true), strip(1, true)]));
  assert.deepEqual(planes.drawn, []);
  assert.deepEqual(shown, [], "nothing to show of a display not whole");
  await p.draw(batchFrame([strip(2, false), strip(3, true)]));
  // A frame is the strips that changed: the others are in the picture already.
  await p.draw(batchFrame([strip(2, true)]));
  assert.deepEqual(planes.placed, [0, 1, 2, 3, 2]);
  assert.deepEqual(planes.drawn, [
    [640, 480],
    [640, 480],
  ]);
  assert.deepEqual(shown, [true]);
  assert.ok(
    decoded.every((frame) => frame.closed),
    "every strip's picture is closed once placed",
  );
});

test("a stream encoded here takes the picture back from the software decoder's", async () => {
  const p = planesPainter();
  await p.draw(batchFrame([{ w: 640, h: 480, payload: KEYFRAME }]));
  // The Mac's stream gives way across a display change, and comes back.
  planar = false;
  p.setVideoFormat({ decode: "vp09.00.40.08" });
  await p.draw(batchFrame([{ w: 800, h: 600, payload: KEYFRAME }]));
  assert.equal(cropped.length, 1);
  assert.deepEqual(shown, [true, false]);
  planar = true;
  p.setVideoFormat(HEVC);
  await p.draw(batchFrame([{ w: 800, h: 600, payload: KEYFRAME }]));
  assert.deepEqual(shown, [true, false, true]);
  assert.deepEqual(planes.drawn, [
    [640, 480],
    [800, 600],
  ]);
  assert.deepEqual(
    [planes.made, planes.closed],
    [1, 0],
    "kept across the change",
  );
});

test("a resize hides the software decoder's picture until its next one", async () => {
  const p = planesPainter();
  await p.draw(batchFrame([{ w: 640, h: 480, payload: KEYFRAME }]));
  p.blank(800, 600);
  assert.deepEqual(shown, [true, false]);
  await p.draw(batchFrame([{ w: 800, h: 600, payload: KEYFRAME }]));
  assert.deepEqual(shown, [true, false, true]);
});

test("a picture the GPU refuses is said once, and the pictures after it dropped", async () => {
  const p = planesPainter({ refuses: true });
  await p.draw(batchFrame([{ w: 640, h: 480, payload: KEYFRAME }]));
  await p.draw(batchFrame([{ w: 640, h: 480, payload: [1], keyframe: false }]));
  const said = videoErrors.filter((error) => error !== null);
  assert.equal(said.length, 1);
  assert.match(said[0] ?? "", /the GPU refused the picture/);
  assert.deepEqual(shown, [], "nothing was drawn to show");
  assert.deepEqual([planes.made, planes.closed], [1, 1]);
  assert.deepEqual(
    decoded.map((frame) => frame.closed),
    [true, true],
    "a picture not presented is closed all the same",
  );
  assert.deepEqual(
    videoKeyframeAsks,
    [],
    "no keyframe would be presented either",
  );
});

test("the attachment's end gives the software decoder's picture back", async () => {
  const p = planesPainter();
  await p.draw(batchFrame([{ w: 640, h: 480, payload: KEYFRAME }]));
  p.clear();
  assert.deepEqual(shown, [true, false]);
  assert.deepEqual([planes.made, planes.closed], [1, 1]);
});

/** A batch of a Mac's own rectangles: op 0x05, x, y, w, h, a length, the body. */
function standardFrame(rects: { at: number[]; body: number[] }[]): ArrayBuffer {
  const bytes: number[] = [0x02, 0x00];
  const u16 = (n: number) => bytes.push(n & 0xff, (n >> 8) & 0xff);
  const u32 = (n: number) =>
    bytes.push(n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, (n >> 24) & 0xff);
  u16(rects.length);
  u32(1);
  for (const rect of rects) {
    bytes.push(0x05);
    for (const value of rect.at) {
      u16(value);
    }
    u32(rect.body.length);
    bytes.push(...rect.body);
  }
  return new Uint8Array(bytes).buffer;
}

/**
 * Decoders of a Mac's Adaptive picture that record what they are fed: a body whose first byte is 2 draws nothing, as the tables do, and one
 * whose first byte is 9 does not decode.
 */
function fakeStandard() {
  const made: {
    resized: number[][];
    fed: number[][];
    closed: boolean;
  }[] = [];
  const load = () =>
    Promise.resolve(() => {
      const picture = {
        resized: [] as number[][],
        fed: [] as number[][],
        closed: false,
      };
      made.push(picture);
      return {
        resize(w: number, h: number) {
          picture.resized.push([w, h]);
        },
        decode(x: number, y: number, w: number, h: number, body: Uint8Array) {
          if (body[0] === 9) {
            throw new Error("an update that ran past its body");
          }
          picture.fed.push([x, y, w, h, body[0]]);
          return body[0] !== 2;
        },
        pixels: () => null,
        image: () => ({}) as ImageData,
        close() {
          picture.closed = true;
        },
      };
    });
  return { made, load };
}

function standardPainter(load: ReturnType<typeof fakeStandard>["load"]) {
  const put: number[][] = [];
  const ctx = {
    putImageData(_image: unknown, dx: number, dy: number, ...dirty: number[]) {
      assert.deepEqual(
        [dx, dy],
        [0, 0],
        "the framebuffer is painted where it lies",
      );
      put.push(dirty);
    },
  } as unknown as CanvasRenderingContext2D;
  const p = createFramePainter({
    context: () => ctx,
    onVideoError: (error) => {
      videoErrors.push(error);
    },
    onVideoNeedsKeyframe: (reason) => {
      videoKeyframeAsks.push(reason);
    },
    loadStandard: load,
  });
  return { p, put };
}

test("a Mac's Adaptive rectangles are decoded in order and painted where they drew", async () => {
  const { made, load } = fakeStandard();
  const { p, put } = standardPainter(load);
  // The resize comes ahead of the picture's start, and is its framebuffer.
  p.blank(64, 32);
  p.startStandard();
  await p.draw(
    standardFrame([
      { at: [0, 0, 0, 0], body: [2] },
      { at: [8, 0, 16, 8], body: [0, 1] },
    ]),
  );
  await p.draw(standardFrame([{ at: [0, 8, 64, 24], body: [1] }]));
  assert.equal(made.length, 1, "one picture follows the whole session");
  assert.deepEqual(made[0].resized, [[64, 32]]);
  assert.deepEqual(made[0].fed, [
    [0, 0, 0, 0, 2],
    [8, 0, 16, 8, 0],
    [0, 8, 64, 24, 1],
  ]);
  assert.deepEqual(put, [
    [8, 0, 16, 8],
    [0, 8, 64, 24],
  ]);
  assert.equal(decoders, 0, "no video decoder was built");
  assert.deepEqual(videoKeyframeAsks, []);

  // A resize starts the framebuffer over at the new size.
  p.blank(32, 32);
  assert.deepEqual(made[0].resized, [
    [64, 32],
    [32, 32],
  ]);
  p.clear();
  assert.equal(made[0].closed, true);
});

test("a rectangle that does not decode ends the picture, said once", async () => {
  const { made, load } = fakeStandard();
  const { p, put } = standardPainter(load);
  p.blank(16, 8);
  p.startStandard();
  await p.draw(
    standardFrame([
      { at: [0, 0, 8, 8], body: [9] },
      { at: [8, 0, 8, 8], body: [0] },
    ]),
  );
  await p.draw(standardFrame([{ at: [8, 0, 8, 8], body: [0] }]));
  assert.deepEqual(
    made[0].fed,
    [],
    "nothing after it is drawn on what it left",
  );
  assert.deepEqual(put, []);
  const said = videoErrors.filter((error) => error !== null);
  assert.equal(said.length, 1);
  assert.match(
    said[0] ?? "",
    /could not decode the Mac's picture \(an update that ran past its body\)/,
  );
  assert.deepEqual(
    videoKeyframeAsks,
    [],
    "and nothing is asked of the gateway",
  );
});

test("a malformed batch ends a Mac's own picture", async () => {
  const { made, load } = fakeStandard();
  const { p } = standardPainter(load);
  p.blank(16, 8);
  p.startStandard();
  await p.draw(
    new Uint8Array([0x02, 0x00, 0x01, 0x00, 0x01, 0, 0, 0, 0x05]).buffer,
  );
  await p.draw(standardFrame([{ at: [0, 0, 8, 8], body: [0] }]));
  assert.ok(made.every((picture) => picture.fed.length === 0));
  assert.match(
    videoErrors.filter((error) => error !== null)[0] ?? "",
    /arrived malformed/,
  );
  assert.deepEqual(videoKeyframeAsks, []);
});

// A batch of TILE records, transcribed from `TileUnit::write_record` in
// src/protocol.rs: op 0x06, the format (1 JPEG, 2 PNG), x, y, w and h as u16, a u32
// length, the image, little-endian.
function tileFrame(
  tiles: {
    format: 1 | 2;
    at: [number, number, number, number];
    image: number[];
  }[],
): ArrayBuffer {
  const bytes: number[] = [];
  const u16 = (n: number) => bytes.push(n & 0xff, (n >> 8) & 0xff);
  const u32 = (n: number) =>
    bytes.push(n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, (n >> 24) & 0xff);
  bytes.push(0x02, 0x00);
  u16(tiles.length);
  u32(1);
  for (const tile of tiles) {
    bytes.push(0x06, tile.format);
    for (const value of tile.at) {
      u16(value);
    }
    u32(tile.image.length);
    bytes.push(...tile.image);
  }
  return new Uint8Array(bytes).buffer;
}

test("the gateway's tiles are drawn in wire order where they say, behind their start", async () => {
  const p = painter();
  p.startTiles();
  // The first decodes last, and is still drawn first: a later tile may cover it.
  decodeDelay.set(1, 20);
  await p.draw(
    tileFrame([
      { format: 1, at: [0, 0, 64, 64], image: [1, 0xff] },
      { format: 2, at: [64, 0, 128, 64], image: [2, 0x89] },
    ]),
  );
  assert.deepEqual(
    cropped.map(({ dx, dy }) => [dx, dy]),
    [
      [0, 0],
      [64, 0],
    ],
  );
  assert.deepEqual(bitmaps.map(({ type }) => type).sort(), [
    "image/jpeg",
    "image/png",
  ]);
  assert.ok(
    bitmaps.every((bitmap) => bitmap.closed),
    "a drawn image is let go",
  );
  assert.equal(decoders, 0, "no video decoder was built");
  assert.deepEqual(videoKeyframeAsks, []);
});

test("a tile is drawn only while the picture is tiles", async () => {
  const p = painter();
  const tile = () => tileFrame([{ format: 2, at: [0, 0, 8, 8], image: [5] }]);
  await p.draw(tile());
  assert.deepEqual(cropped, [], "a tile before its start");
  p.startTiles();
  await p.draw(tile());
  assert.equal(cropped.length, 1);
  // A passed stream takes the picture, and a tile late behind it is not drawn.
  p.setVideoFormat({ decode: "vp09.00.40.08.01.06.06.06.00" });
  await p.draw(tile());
  assert.equal(cropped.length, 1, "a tile drawn under a stream");
  p.startTiles();
  p.clear();
  await p.draw(tile());
  assert.equal(cropped.length, 1, "a tile drawn after the attachment ended");
});

test("a tile that does not decode asks for the whole picture, and the rest are drawn", async () => {
  const p = painter();
  p.startTiles();
  undecodable = 3;
  await p.draw(
    tileFrame([
      { format: 1, at: [0, 0, 64, 64], image: [3] },
      { format: 2, at: [64, 0, 64, 64], image: [4] },
    ]),
  );
  assert.deepEqual(
    cropped.map(({ dx }) => dx),
    [64],
  );
  assert.equal(videoKeyframeAsks.length, 1);
  assert.match(videoKeyframeAsks[0], /a tile did not decode/);
});

test("a malformed batch of tiles asks for the whole picture", async () => {
  const p = painter();
  p.startTiles();
  await p.draw(
    new Uint8Array([0x02, 0x00, 0x01, 0x00, 0x01, 0, 0, 0, 0x06, 0x01]).buffer,
  );
  assert.deepEqual(cropped, []);
  assert.match(videoKeyframeAsks[0] ?? "", /malformed/);
});

// wlshare's tiles with their moving regions (regionStreams.ts): TILE records as
// above, and REGION records transcribed from `RegionUnit::write_record` in
// src/protocol.rs: op 0x07, the stream, the flags (0x01 a keyframe), x, y, w and
// h as u16, a u32 length, the unit, little-endian.
type TilesRecord =
  | { tile: [number, number, number, number]; image: number[] }
  | {
      region: number;
      at: [number, number, number, number];
      keyframe: boolean;
      payload: number[];
    };

function regionsFrame(records: TilesRecord[]): ArrayBuffer {
  const bytes: number[] = [];
  const u16 = (n: number) => bytes.push(n & 0xff, (n >> 8) & 0xff);
  const u32 = (n: number) =>
    bytes.push(n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, (n >> 24) & 0xff);
  bytes.push(0x02, 0x00);
  u16(records.length);
  u32(1);
  for (const record of records) {
    if ("tile" in record) {
      bytes.push(0x06, 0x02);
      record.tile.forEach(u16);
      u32(record.image.length);
      bytes.push(...record.image);
    } else {
      bytes.push(0x07, record.region, record.keyframe ? 0x01 : 0x00);
      record.at.forEach(u16);
      u32(record.payload.length);
      bytes.push(...record.payload);
    }
  }
  return new Uint8Array(bytes).buffer;
}

/** A desktop's context that records each draw's source, place and clip. */
function regionContext() {
  const draws: string[] = [];
  let clip = "";
  const saved: string[] = [];
  const ctx = {
    save() {
      saved.push(clip);
    },
    restore() {
      clip = saved.pop() ?? "";
    },
    beginPath() {},
    rect(x: number, y: number, w: number, h: number) {
      clip = ` clip ${x},${y} ${w}x${h}`;
    },
    clip() {},
    drawImage(source: { tag?: number }, ...args: number[]) {
      // Drawn whole at its place: the clip is what cuts it.
      assert.equal(args.length, 2);
      const what = source.tag === undefined ? "frame" : `tile${source.tag}`;
      draws.push(`${what} ${args[0]},${args[1]}${clip}`);
    },
  };
  return { ctx: ctx as unknown as CanvasRenderingContext2D, draws };
}

function regionPainter(ctx: CanvasRenderingContext2D, regionRetireMs = 0) {
  return createFramePainter({
    context: () => ctx,
    onVideoError: (error) => {
      videoErrors.push(error);
    },
    onVideoNeedsKeyframe: (reason) => {
      videoKeyframeAsks.push(reason);
    },
    regionRetireMs,
  });
}

const REGION = "vp09.00.21.08.01.06.06.06.00";
const waited = (ms: number) =>
  new Promise((resolve) => setTimeout(resolve, ms));

test("wlshare's regions are decoded by their own decoders and drawn cut to their rectangles, in wire order among the tiles", async () => {
  const { ctx, draws } = regionContext();
  const p = regionPainter(ctx);
  p.startTiles();
  p.setRegionFormat(1, { decode: REGION });
  p.setRegionFormat(2, { decode: "vp09.00.10.08.01.06.06.06.00" });
  // The first tile decodes last, and is still drawn first.
  decodeDelay.set(1, 20);
  await p.draw(
    regionsFrame([
      { tile: [0, 0, 64, 64], image: [1] },
      { region: 1, at: [10, 20, 33, 17], keyframe: true, payload: KEYFRAME },
      { tile: [64, 0, 64, 64], image: [2] },
      { region: 2, at: [100, 100, 16, 16], keyframe: true, payload: KEYFRAME },
    ]),
  );
  assert.deepEqual(draws, [
    "tile1 0,0",
    "frame 10,20 clip 10,20 33x17",
    "tile2 64,0",
    "frame 100,100 clip 100,100 16x16",
  ]);
  assert.deepEqual(configured, [REGION, "vp09.00.10.08.01.06.06.06.00"]);
  // A stream's units after its keyframe go to its own decoder, in its chain.
  await p.draw(
    regionsFrame([
      { region: 1, at: [10, 20, 33, 17], keyframe: false, payload: [0x01] },
    ]),
  );
  assert.equal(draws.at(-1), "frame 10,20 clip 10,20 33x17");
  assert.equal(decoders, 2);
  assert.deepEqual(chunkTypes, ["key", "key", "delta"]);
  assert.ok(
    decoded.every((frame) => frame.closed),
    "a drawn frame is let go",
  );
  assert.deepEqual(videoKeyframeAsks, []);
});

test("a region of no stream this page was told of, or of no picture of tiles, is dropped", async () => {
  const { ctx, draws } = regionContext();
  const p = regionPainter(ctx);
  const region = (stream: number) =>
    regionsFrame([
      { region: stream, at: [0, 0, 16, 16], keyframe: true, payload: KEYFRAME },
    ]);
  // Before any `tilesStart`: the format and its unit are of a picture this page
  // is not on.
  p.setRegionFormat(1, { decode: REGION });
  await p.draw(region(1));
  p.startTiles();
  await p.draw(region(1));
  // And a stream never announced: from before the `tilesStart` of a reattach.
  p.setRegionFormat(2, { decode: REGION });
  await p.draw(region(3));
  assert.deepEqual(draws, []);
  assert.equal(decoders, 0);
  assert.deepEqual(videoKeyframeAsks, []);
  assert.deepEqual(
    videoErrors.filter((error) => error !== null),
    [],
  );
});

test("a region that ended draws nothing more, and its decoder serves its id started again or retires", async () => {
  const { ctx, draws } = regionContext();
  const p = regionPainter(ctx, 10);
  const region = (keyframe: boolean) =>
    regionsFrame([
      { region: 1, at: [0, 0, 16, 16], keyframe, payload: KEYFRAME },
    ]);
  p.startTiles();
  p.setRegionFormat(1, { decode: REGION });
  await p.draw(region(true));
  p.endRegion(1);
  await p.draw(region(false));
  assert.equal(draws.length, 1, "a unit after its stream's end");
  // The id started again at the same size before the decoder retired: the
  // same decoder, from the new stream's keyframe.
  p.setRegionFormat(1, { decode: REGION });
  await p.draw(region(true));
  assert.equal(draws.length, 2);
  assert.deepEqual([decoders, closes], [1, 0]);
  p.endRegion(1);
  await waited(20);
  assert.equal(closes, 1, "the decoder retired");
  // Started again after that, it is built again.
  p.setRegionFormat(1, { decode: REGION });
  await p.draw(region(true));
  assert.deepEqual([decoders, draws.length], [2, 3]);
});

test("a tiles picture started over ends every region of the one before, and a video format ends them too", async () => {
  const { ctx, draws } = regionContext();
  const p = regionPainter(ctx);
  const region = (stream: number) =>
    regionsFrame([
      { region: stream, at: [0, 0, 16, 16], keyframe: true, payload: KEYFRAME },
    ]);
  p.startTiles();
  p.setRegionFormat(1, { decode: REGION });
  p.setRegionFormat(2, { decode: REGION });
  await p.draw(region(1));
  await p.draw(region(2));
  assert.deepEqual([decoders, closes], [2, 0]);
  p.startTiles();
  assert.equal(closes, 2);
  await p.draw(region(1));
  assert.equal(draws.length, 2, "a region of the picture before");
  p.setRegionFormat(3, { decode: REGION });
  await p.draw(region(3));
  assert.equal(draws.length, 3);
  p.setVideoFormat({ decode: "vp09.00.40.08.01.06.06.06.00" });
  assert.equal(closes, 3);
  p.setRegionFormat(3, { decode: REGION });
  await p.draw(region(3));
  assert.equal(draws.length, 3, "a region under a stream");
  // And the attachment's end.
  p.startTiles();
  p.setRegionFormat(4, { decode: REGION });
  await p.draw(region(4));
  p.clear();
  assert.equal(closes, decoders);
});

test("a malformed batch cuts every region's chain and asks for the whole picture", async () => {
  const { ctx, draws } = regionContext();
  const p = regionPainter(ctx);
  p.startTiles();
  p.setRegionFormat(1, { decode: REGION });
  await p.draw(
    regionsFrame([
      { region: 1, at: [0, 0, 16, 16], keyframe: true, payload: KEYFRAME },
    ]),
  );
  // A REGION record with a flag this client does not know.
  const bad = new Uint8Array(
    regionsFrame([
      { region: 1, at: [0, 0, 16, 16], keyframe: true, payload: KEYFRAME },
    ]),
  );
  bad[10] = 0x02;
  await p.draw(bad.buffer);
  assert.match(videoKeyframeAsks[0] ?? "", /malformed/);
  // The delta after it names a picture no decoder has now.
  await p.draw(
    regionsFrame([
      { region: 1, at: [0, 0, 16, 16], keyframe: false, payload: [0x01] },
    ]),
  );
  assert.equal(draws.length, 1);
  assert.deepEqual(chunkTypes, ["key"]);
});
