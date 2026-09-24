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
import { createFramePainter, type FramePainter } from "./framePainter.ts";

const OP_VIDEO = 0x03;

interface Unit {
  w: number;
  h: number;
  payload: number[];
  /** Defaults to true: most fixtures below are a stream's first unit. */
  keyframe?: boolean;
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
    bytes.push(OP_VIDEO, unit.keyframe === false ? 0 : 0x01);
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

/** Nine-argument draws: the source rectangle is what crops a padded frame. */
let cropped: {
  sx: number;
  sy: number;
  sw: number;
  sh: number;
  dx: number;
  dy: number;
  dw: number;
  dh: number;
}[] = [];
let decoded: FakeFrame[] = [];
let videoErrors: (string | null)[] = [];
/** Chains that were cut. The stub never goes quiet, so these are failures. */
let videoKeyframeAsks: string[] = [];

const context = {
  drawImage(_source: unknown, ...args: number[]) {
    cropped.push({
      sx: args[0],
      sy: args[1],
      sw: args[2],
      sh: args[3],
      dx: args[4],
      dy: args[5],
      dw: args[6],
      dh: args[7],
    });
  },
} as unknown as CanvasRenderingContext2D;

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

  decode(chunk: { type: string; data?: Uint8Array }) {
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
};

beforeEach(() => {
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
  // The encoder is held to even sides and an odd desktop does not have them, so the
  // decoded picture can be a pixel wider or taller than the desktop.
  await announced().draw(batchFrame([{ w: 1599, h: 1015, payload: KEYFRAME }]));
  assert.deepEqual(cropped, [
    { sx: 0, sy: 0, sw: 1599, sh: 1015, dx: 0, dy: 0, dw: 1599, dh: 1015 },
  ]);
  assert.deepEqual(chunkTypes, ["key"]);
  assert.ok(
    decoded.every((frame) => frame.closed),
    "a VideoFrame holds decoder memory until it is closed",
  );
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

test("a record that is not a VIDEO record drops the batch", async () => {
  const frame = batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]);
  new Uint8Array(frame)[8] = 0x01;
  await announced().draw(frame);
  assert.deepEqual(chunkTypes, []);
});

test("units that arrive before their format are dropped, not reported", async () => {
  // The takeover: the gateway announces the stream once, to whoever was attached, so a
  // browser that takes the session over gets whatever was already in flight before the
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
  // picker only, so a reattach or a takeover would otherwise inherit this sentence.
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
