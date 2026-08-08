// The slot table and the batch draw loop, over frames this file builds itself.
//
// Deliberately not built with `protocol.ts`'s encoder — there isn't one, and a
// test that produced its input with the same code that reads it would agree with
// itself no matter what either did. The layout below is transcribed from
// `batch` in `src/protocol.rs`, which is the contract both ends are checked
// against.
//
// Run with `bun test src/tilePainter.test.ts` from frontend/.
import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { NO_SLOT } from "./protocol.ts";
import { createTilePainter, type TilePainter } from "./tilePainter.ts";

const OP_TILE = 0x01;
const OP_TILE_REF = 0x02;
const OP_VIDEO = 0x03;
const OP_COPY = 0x04;
const FORMAT_PNG = 1;

type Record =
  | {
      op: "tile";
      slot: number;
      x: number;
      y: number;
      payload: number[];
      format?: number;
      w?: number;
      h?: number;
    }
  | { op: "ref"; slot: number; x: number; y: number }
  | {
      op: "copy";
      sx: number;
      sy: number;
      x: number;
      y: number;
      w: number;
      h: number;
    }
  | {
      op: "video";
      stream: number;
      x: number;
      y: number;
      w: number;
      h: number;
      payload: number[];
      /** Defaults to true: most fixtures below are a stream's first unit. */
      keyframe?: boolean;
    };

function batchFrame(records: Record[]): ArrayBuffer {
  const bytes: number[] = [];
  const u16 = (n: number) => bytes.push(n & 0xff, (n >> 8) & 0xff);
  const u32 = (n: number) =>
    bytes.push(n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, (n >> 24) & 0xff);
  bytes.push(0x02, 0x00);
  u16(records.length);
  u32(1); // attachment-local batch sequence
  for (const record of records) {
    if (record.op === "ref") {
      bytes.push(OP_TILE_REF);
      u16(record.slot);
      u16(record.x);
      u16(record.y);
      continue;
    }
    if (record.op === "copy") {
      bytes.push(OP_COPY);
      u16(record.sx);
      u16(record.sy);
      u16(record.x);
      u16(record.y);
      u16(record.w);
      u16(record.h);
      continue;
    }
    if (record.op === "video") {
      bytes.push(OP_VIDEO, record.stream, record.keyframe === false ? 0 : 0x01);
      u16(record.x);
      u16(record.y);
      u16(record.w);
      u16(record.h);
      u32(record.payload.length);
      bytes.push(...record.payload);
      continue;
    }
    bytes.push(OP_TILE, record.format ?? FORMAT_PNG);
    u16(record.slot);
    u16(record.x);
    u16(record.y);
    u16(record.w ?? 1);
    u16(record.h ?? 1);
    u32(record.payload.length);
    bytes.push(...record.payload);
  }
  return new Uint8Array(bytes).buffer;
}

// An access unit's payload. **Opaque here, deliberately**: nothing on this side of the
// wire parses a bitstream any more — the gateway says how to decode a stream in a
// `videoFormat` message and marks each unit's keyframe bit in the record — so these
// bytes only need to be distinguishable from each other. The shape is a plausible
// bitstream rather than a run of zeros because it reads better in a failure.
const KEYFRAME = [
  0, 0, 0, 1, 7, 0x42, 0xc0, 0x1e, 0, 0, 0, 1, 8, 0xce, 0, 0, 1, 5, 0x88,
];

// A decoded tile stands for its first payload byte, so a draw can be named.
interface FakeBitmap {
  tag: number;
  closed: boolean;
}

let drawn: { tag: number; x: number; y: number }[] = [];
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
/** Nine-argument draws whose source was the canvas itself: a COPY record. */
let blitted: {
  sx: number;
  sy: number;
  sw: number;
  sh: number;
  dx: number;
  dy: number;
  dw: number;
  dh: number;
}[] = [];
let decoded: FakeBitmap[] = [];
let resets = 0;
let videoErrors: (string | null)[] = [];
/** Streams whose chain was cut. The stub never goes quiet, so these are failures. */
let videoKeyframeAsks: string[] = [];
/** Payload first bytes the stubbed decoder refuses. */
let undecodable = new Set<number>();
/** Dimensions the stub reports, so a test can spend the decoded byte budget. */
let bitmapDims = { width: 1, height: 1 };
/** Payload first bytes whose decode waits until `releaseDecodes` runs. */
let stalled = new Set<number>();
let releaseDecodes = () => {};

// The surface a copy names as its own source. Identity is the whole test: a
// nine-argument draw off the canvas is a blit, and off a decoded picture it is a
// padded video frame being cropped.
const canvas = { id: "the canvas itself" };

const context = {
  canvas,
  drawImage(source: FakeBitmap | typeof canvas, ...args: number[]) {
    if (args.length === 8) {
      (source === canvas ? blitted : cropped).push({
        sx: args[0],
        sy: args[1],
        sw: args[2],
        sh: args[3],
        dx: args[4],
        dy: args[5],
        dw: args[6],
        dh: args[7],
      });
      return;
    }
    drawn.push({ tag: (source as FakeBitmap).tag, x: args[0], y: args[1] });
  },
} as unknown as CanvasRenderingContext2D;

// The WebCodecs decoder, which this runtime has none of — and which the client
// refuses to start without, so it is installed for every test here rather than taken
// away by any of them. Only its shape matters: what the painter does with the frames,
// not what a real decoder makes of the bitstream — that is browser QA.
let videoClosed = false;
let chunkTypes: string[] = [];
/** How many decoders were built — one per live stream, replaced on a resize. */
let decoders = 0;
/** How many were closed, so "the other one survived" is checkable. */
let closes = 0;
/** A payload whose first byte is this makes its decoder give up. */
let poison: number | null = null;
/** Like `poison`, but the browser refusing the configuration rather than failing. */
let refused: number | null = null;

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

  configure() {
    this.state = "configured";
  }

  decode(chunk: { type: string; data?: Uint8Array }) {
    if (poison !== null && chunk.data?.[chunk.data.length - 1] === poison) {
      this.fail(new Error("this decoder gave up"));
      return;
    }
    if (refused !== null && chunk.data?.[chunk.data.length - 1] === refused) {
      // The name is the whole signal: it is how WebCodecs says "not this
      // configuration", which no later keyframe and no other region changes.
      const no = new Error("this configuration is not supported");
      no.name = "NotSupportedError";
      this.fail(no);
      return;
    }
    chunkTypes.push(chunk.type);
    const frame: FakeBitmap = { tag: 0xf7, closed: false };
    decoded.push(frame);
    this.output({
      close() {
        frame.closed = true;
      },
      get tag() {
        return frame.tag;
      },
    });
  }

  close() {
    videoClosed = true;
    closes += 1;
    this.state = "closed";
  }
}

const globals = globalThis as unknown as {
  createImageBitmap: (blob: Blob) => Promise<unknown>;
  VideoDecoder: unknown;
  EncodedVideoChunk: unknown;
};
const realCreateImageBitmap = globals.createImageBitmap;

beforeEach(() => {
  drawn = [];
  cropped = [];
  blitted = [];
  decoded = [];
  resets = 0;
  videoErrors = [];
  videoKeyframeAsks = [];
  videoClosed = false;
  chunkTypes = [];
  decoders = 0;
  closes = 0;
  poison = null;
  refused = null;
  undecodable = new Set();
  globals.VideoDecoder = FakeVideoDecoder;
  globals.EncodedVideoChunk = class {
    type: string;
    data: Uint8Array;
    constructor(init: { type: string; data: Uint8Array }) {
      this.type = init.type;
      this.data = init.data;
    }
  };
  bitmapDims = { width: 1, height: 1 };
  stalled = new Set();
  releaseDecodes = () => {};
  globals.createImageBitmap = async (blob: Blob) => {
    const tag = new Uint8Array(await blob.arrayBuffer())[0];
    if (stalled.has(tag)) {
      await new Promise<void>((resolve) => {
        const previous = releaseDecodes;
        releaseDecodes = () => {
          previous();
          resolve();
        };
      });
    }
    if (undecodable.has(tag)) {
      throw new Error("undecodable");
    }
    const bitmap: FakeBitmap = { tag, closed: false };
    decoded.push(bitmap);
    return {
      ...bitmapDims,
      close() {
        bitmap.closed = true;
      },
      get tag() {
        return bitmap.tag;
      },
    };
  };
});

afterEach(() => {
  globals.createImageBitmap = realCreateImageBitmap;
  globals.VideoDecoder = undefined;
  globals.EncodedVideoChunk = undefined;
});

function painter(ctx: CanvasRenderingContext2D | null = context) {
  return createTilePainter({
    context: () => ctx,
    onCacheReset: () => {
      resets += 1;
    },
    onVideoError: (error) => {
      videoErrors.push(error);
    },
    onVideoNeedsKeyframe: (reason) => {
      videoKeyframeAsks.push(reason);
    },
  });
}

test("tiles are drawn in wire order at their own coordinates", async () => {
  // Order is the whole correctness requirement: a later tile has to overwrite an
  // earlier one covering the same pixels, and decodes finish out of order.
  await painter().draw(
    batchFrame([
      { op: "tile", slot: NO_SLOT, x: 10, y: 20, payload: [1] },
      { op: "tile", slot: NO_SLOT, x: 30, y: 40, payload: [2] },
      { op: "tile", slot: NO_SLOT, x: 10, y: 20, payload: [3] },
    ]),
  );
  assert.deepEqual(drawn, [
    { tag: 1, x: 10, y: 20 },
    { tag: 2, x: 30, y: 40 },
    { tag: 3, x: 10, y: 20 },
  ]);
});

test("a copy blits the canvas onto itself, source rectangle to destination", async () => {
  await painter().draw(
    batchFrame([{ op: "copy", sx: 0, sy: 64, x: 0, y: 0, w: 1920, h: 936 }]),
  );
  assert.deepEqual(blitted, [
    { sx: 0, sy: 64, sw: 1920, sh: 936, dx: 0, dy: 0, dw: 1920, dh: 936 },
  ]);
  assert.deepEqual(drawn, [], "a copy decodes nothing");
  assert.deepEqual(cropped, [], "and is not a video frame being cropped");
  assert.equal(resets, 0);
});

test("a copy reads the canvas the records before it left behind", async () => {
  // The ordering claim the whole record rests on. Its source is drawn by a tile in
  // the same batch, and tiles decode asynchronously — so a copy that did not keep
  // its place in the wire order would blit pixels that were not there yet.
  stalled = new Set([1]);
  const p = painter();
  const pending = p.draw(
    batchFrame([
      { op: "tile", slot: NO_SLOT, x: 0, y: 0, payload: [1] },
      { op: "copy", sx: 0, sy: 0, x: 64, y: 0, w: 32, h: 32 },
      { op: "tile", slot: NO_SLOT, x: 128, y: 0, payload: [2] },
    ]),
  );
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(
    blitted,
    [],
    "the copy went first past a decode it must wait for",
  );
  releaseDecodes();
  await pending;
  assert.deepEqual(drawn, [
    { tag: 1, x: 0, y: 0 },
    { tag: 2, x: 128, y: 0 },
  ]);
  assert.equal(blitted.length, 1);
  assert.deepEqual(blitted[0], {
    sx: 0,
    sy: 0,
    sw: 32,
    sh: 32,
    dx: 64,
    dy: 0,
    dw: 32,
    dh: 32,
  });
});

test("a copy neither claims a slot nor asks for a cache reset", async () => {
  // It is an instruction, not a picture: nothing to remember, and a client that
  // has drawn it holds no bytes the server could later reference.
  const p = painter();
  await p.draw(batchFrame([{ op: "tile", slot: 0, x: 0, y: 0, payload: [9] }]));
  await p.draw(
    batchFrame([{ op: "copy", sx: 0, sy: 0, x: 8, y: 8, w: 4, h: 4 }]),
  );
  await p.draw(batchFrame([{ op: "ref", slot: 0, x: 16, y: 16 }]));
  assert.deepEqual(drawn, [
    { tag: 9, x: 0, y: 0 },
    { tag: 9, x: 16, y: 16 },
  ]);
  assert.equal(resets, 0, "the copy did not disturb the slot table");
});

test("a copy that arrives after the attachment ended is not painted", async () => {
  // Same rule as a stale tile, and for a sharper reason: a blit onto the next
  // attachment's canvas moves *its* pixels, not the ones the record was about.
  stalled = new Set([1]);
  const p = painter();
  const pending = p.draw(
    batchFrame([
      { op: "tile", slot: NO_SLOT, x: 0, y: 0, payload: [1] },
      { op: "copy", sx: 0, sy: 0, x: 64, y: 0, w: 32, h: 32 },
    ]),
  );
  await new Promise((resolve) => setTimeout(resolve, 0));
  p.clear();
  releaseDecodes();
  await pending;
  assert.deepEqual(blitted, []);
  assert.deepEqual(drawn, []);
});

test("a reference redraws its slot's payload at its own position", async () => {
  const p = painter();
  await p.draw(batchFrame([{ op: "tile", slot: 7, x: 0, y: 0, payload: [9] }]));
  await p.draw(batchFrame([{ op: "ref", slot: 7, x: 64, y: 128 }]));
  assert.deepEqual(drawn, [
    { tag: 9, x: 0, y: 0 },
    { tag: 9, x: 64, y: 128 },
  ]);
  assert.equal(resets, 0);
});

test("a reference may name a slot filled earlier in its own batch", async () => {
  // The gateway does emit this, so clearing the table mid-pass would drop a
  // legal record for company.
  await painter().draw(
    batchFrame([
      { op: "tile", slot: 3, x: 0, y: 0, payload: [5] },
      { op: "ref", slot: 3, x: 8, y: 8 },
    ]),
  );
  assert.deepEqual(drawn, [
    { tag: 5, x: 0, y: 0 },
    { tag: 5, x: 8, y: 8 },
  ]);
});

test("a reference reuses the slot's decoded bitmap rather than decoding again", async () => {
  // The decoded cache's whole point: a TILE_REF was a Blob, a decode and a GPU
  // upload per reference, for pixels this client had already decoded.
  const p = painter();
  await p.draw(batchFrame([{ op: "tile", slot: 7, x: 0, y: 0, payload: [9] }]));
  await p.draw(batchFrame([{ op: "ref", slot: 7, x: 64, y: 128 }]));
  assert.equal(decoded.length, 1, "the reference decoded a second copy");
  assert.deepEqual(drawn, [
    { tag: 9, x: 0, y: 0 },
    { tag: 9, x: 64, y: 128 },
  ]);
  assert.ok(!decoded[0].closed, "a cached bitmap must stay drawable");
});

test("overwriting a slot closes its old bitmap; later references get the new one", async () => {
  const p = painter();
  await p.draw(batchFrame([{ op: "tile", slot: 7, x: 0, y: 0, payload: [1] }]));
  await p.draw(batchFrame([{ op: "tile", slot: 7, x: 0, y: 0, payload: [2] }]));
  assert.ok(decoded[0].closed, "the stale bitmap was left holding GPU memory");
  await p.draw(batchFrame([{ op: "ref", slot: 7, x: 8, y: 8 }]));
  assert.deepEqual(drawn.at(-1), { tag: 2, x: 8, y: 8 });
});

test("an in-batch overwrite draws each reference against the bytes in force", async () => {
  // The overwrite drops the decoded bitmap at resolve time and the new one is
  // adopted in wire order at draw time, so neither reference can see the wrong
  // side of the overwrite however the decodes interleave.
  await painter().draw(
    batchFrame([
      { op: "tile", slot: 3, x: 0, y: 0, payload: [1] },
      { op: "ref", slot: 3, x: 1, y: 0 },
      { op: "tile", slot: 3, x: 2, y: 0, payload: [2] },
      { op: "ref", slot: 3, x: 3, y: 0 },
    ]),
  );
  assert.deepEqual(
    drawn.map((d) => d.tag),
    [1, 1, 2, 2],
  );
});

test("the decoded cache evicts its least recently used past the byte budget", async () => {
  bitmapDims = { width: 2048, height: 1024 }; // 8 MiB decoded apiece
  const p = painter();
  await p.draw(
    batchFrame([
      { op: "tile", slot: 1, x: 0, y: 0, payload: [1] },
      { op: "tile", slot: 2, x: 0, y: 0, payload: [2] },
      { op: "tile", slot: 3, x: 0, y: 0, payload: [3] },
    ]),
  );
  assert.ok(decoded[0].closed, "the oldest entry was kept past the budget");
  assert.ok(!decoded[2].closed, "the newest entry went instead of the oldest");
  const before = decoded.length;
  await p.draw(batchFrame([{ op: "ref", slot: 1, x: 0, y: 0 }]));
  assert.equal(
    decoded.length,
    before + 1,
    "an evicted slot re-decodes from its encoded bytes",
  );
  assert.deepEqual(drawn.at(-1), { tag: 1, x: 0, y: 0 });
  await p.draw(batchFrame([{ op: "ref", slot: 3, x: 0, y: 0 }]));
  assert.equal(decoded.length, before + 1, "a kept slot decoded again");
});

test("a slow decode holds back the tiles after it and none before it", async () => {
  // The old shape awaited the whole batch before painting anything, so one
  // slow decode gated every tile and the batch's decoded images were all alive
  // at once. Wire order still holds: the stalled tile paints before the one
  // behind it, however early that one decoded.
  stalled = new Set([2]);
  const p = painter();
  const done = p.draw(
    batchFrame([
      { op: "tile", slot: NO_SLOT, x: 0, y: 0, payload: [1] },
      { op: "tile", slot: NO_SLOT, x: 1, y: 0, payload: [2] },
      { op: "tile", slot: NO_SLOT, x: 2, y: 0, payload: [3] },
    ]),
  );
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(
    drawn.map((d) => d.tag),
    [1],
    "the tile before the stall waited for it",
  );
  releaseDecodes();
  await done;
  assert.deepEqual(
    drawn.map((d) => d.tag),
    [1, 2, 3],
  );
});

test("clear() during a stalled decode fences the rest of the batch", async () => {
  // `clear()` is the attachment boundary and is not queued behind draws — an
  // eviction closes the socket from under a batch mid-decode. What resumes
  // afterwards must not paint the previous desktop onto the next attachment's
  // canvas, must not seed its caches, and must still settle its images.
  stalled = new Set([2]);
  const p = painter();
  const done = p.draw(
    batchFrame([
      { op: "tile", slot: 1, x: 0, y: 0, payload: [1] },
      { op: "tile", slot: 2, x: 1, y: 0, payload: [2] },
    ]),
  );
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(
    drawn.map((d) => d.tag),
    [1],
  );
  p.clear();
  releaseDecodes();
  await done;
  assert.deepEqual(
    drawn.map((d) => d.tag),
    [1],
    "a stale tile painted onto the next attachment",
  );
  assert.ok(
    decoded.every((bitmap) => bitmap.closed),
    "the fenced batch left a bitmap alive",
  );
  await p.draw(batchFrame([{ op: "ref", slot: 2, x: 0, y: 0 }]));
  assert.equal(
    resets,
    1,
    "the fenced batch left slot state behind for the next attachment",
  );
});

test("a cache reset closes the decoded bitmaps with the table", async () => {
  const p = painter();
  await p.draw(batchFrame([{ op: "tile", slot: 5, x: 0, y: 0, payload: [1] }]));
  await p.draw(batchFrame([{ op: "ref", slot: 200, x: 0, y: 0 }]));
  assert.ok(decoded[0].closed, "a reset left a cached bitmap alive");
});

test("clear() closes the decoded bitmaps, not only the slot table", async () => {
  const p = painter();
  await p.draw(batchFrame([{ op: "tile", slot: 5, x: 0, y: 0, payload: [1] }]));
  p.clear();
  assert.ok(decoded[0].closed, "the cache belongs to one attachment");
});

test("a batch of misses asks for one reset, not one per miss", async () => {
  await painter().draw(
    batchFrame([
      { op: "ref", slot: 1, x: 0, y: 0 },
      { op: "ref", slot: 2, x: 0, y: 0 },
      { op: "ref", slot: 3, x: 0, y: 0 },
    ]),
  );
  assert.equal(resets, 1, "one disagreement, not three");
  assert.deepEqual(
    drawn,
    [],
    "a miss draws nothing rather than inventing pixels",
  );
});

test("a reset empties the table, so a slot filled before it misses after", async () => {
  const p = painter();
  await p.draw(
    batchFrame([
      { op: "tile", slot: 4, x: 0, y: 0, payload: [1] },
      { op: "ref", slot: 200, x: 0, y: 0 },
    ]),
  );
  assert.equal(resets, 1);
  await p.draw(batchFrame([{ op: "ref", slot: 4, x: 0, y: 0 }]));
  assert.equal(
    resets,
    2,
    "the slot the reset cleared is now a miss of its own",
  );
});

test("clear() forgets every slot", async () => {
  const p = painter();
  await p.draw(batchFrame([{ op: "tile", slot: 5, x: 0, y: 0, payload: [1] }]));
  p.clear();
  await p.draw(batchFrame([{ op: "ref", slot: 5, x: 0, y: 0 }]));
  assert.equal(resets, 1);
});

test("an undecodable tile costs a reset only when the server is keeping it", async () => {
  undecodable = new Set([1, 2]);
  const p = painter();
  await p.draw(
    batchFrame([{ op: "tile", slot: NO_SLOT, x: 0, y: 0, payload: [1] }]),
  );
  assert.equal(
    resets,
    0,
    "one dropped tile, and nothing will refer to it again",
  );
  await p.draw(batchFrame([{ op: "tile", slot: 6, x: 0, y: 0, payload: [2] }]));
  assert.equal(
    resets,
    1,
    "every later reference to that slot would fail the same way",
  );
});

test("a malformed frame is dropped whole", async () => {
  const frame = batchFrame([
    { op: "tile", slot: NO_SLOT, x: 0, y: 0, payload: [1] },
  ]);
  await painter().draw(frame.slice(0, frame.byteLength - 1));
  assert.deepEqual(drawn, []);
  assert.equal(resets, 0);
});

test("a stream id the wire does not allow drops the batch", async () => {
  // The same bound as the slot table's, for the same reason: a client's decoder
  // table is a function of the protocol, not of what a gateway chooses to send.
  await painter().draw(
    batchFrame([
      { op: "video", stream: 16, x: 0, y: 0, w: 64, h: 64, payload: KEYFRAME },
    ]),
  );
  assert.deepEqual(cropped, []);
  assert.equal(
    decoders,
    0,
    "a decoder was built for a stream that cannot exist",
  );
});

test("a truncated access unit drops the batch rather than decoding half of it", async () => {
  // Half an access unit is not a smaller access unit: submitting one would leave
  // the decoder's state wrong for every frame after it.
  const frame = batchFrame([
    { op: "video", stream: 0, x: 0, y: 0, w: 64, h: 64, payload: KEYFRAME },
  ]);
  await painter().draw(frame.slice(0, frame.byteLength - 1));
  assert.deepEqual(cropped, []);
  assert.deepEqual(chunkTypes, []);
});

// The `videoFormat` a gateway sends before a stream's first unit. Every video test goes
// through this, because a painter with no format for a stream refuses to decode it — and
// that refusal has a test of its own below.
function announced(streams: number[] = [0]): TilePainter {
  const p = painter();
  for (const stream of streams) {
    p.setVideoFormat(stream, { decode: "vp09.00.40.08" });
  }
  return p;
}

test("a video frame is cropped to its region and drawn where it belongs", async () => {
  // The encoders are held to even sides and a region at the edge of an odd desktop
  // does not have them, so the decoded picture can be a pixel wider or taller than
  // the rectangle. The record carries the true rectangle, which is where it goes.
  await announced().draw(
    batchFrame([
      {
        op: "video",
        stream: 0,
        x: 320,
        y: 64,
        w: 1599,
        h: 1015,
        payload: KEYFRAME,
      },
    ]),
  );
  // sx/sy are 0 and the destination is the record's own rectangle: the padding a
  // stream may carry is at its right and bottom edges, so the crop starts at the
  // picture's origin and not at the region's position on the desktop.
  assert.deepEqual(cropped, [
    { sx: 0, sy: 0, sw: 1599, sh: 1015, dx: 320, dy: 64, dw: 1599, dh: 1015 },
  ]);
  assert.deepEqual(drawn, [], "a padded frame was drawn at its own size");
  assert.deepEqual(
    chunkTypes,
    ["key"],
    "the record's keyframe flag did not reach the chunk",
  );
  assert.ok(
    decoded.every((frame) => frame.closed),
    "a VideoFrame holds decoder memory until it is closed",
  );
});

test("a record's keyframe flag decides the chunk type, both ways", async () => {
  // The flag is the whole reason the record grew a byte: it comes from the encoder, and
  // VP9 — which has no parameter sets — offers a client nothing to work it out from.
  await announced().draw(
    batchFrame([
      { op: "video", stream: 0, x: 0, y: 0, w: 64, h: 64, payload: KEYFRAME },
      {
        op: "video",
        stream: 0,
        x: 0,
        y: 0,
        w: 64,
        h: 64,
        payload: [1, 2, 3],
        keyframe: false,
      },
    ]),
  );
  assert.deepEqual(chunkTypes, ["key", "delta"]);
});

test("a video record with an unknown flag drops the batch", async () => {
  // The same strictness the frame's own flags byte gets, and for the same reason: a bit
  // this client does not know means a gateway newer than it, and painting half of what it
  // meant is worse than painting none of it.
  const frame = batchFrame([
    { op: "video", stream: 0, x: 0, y: 0, w: 64, h: 64, payload: KEYFRAME },
  ]);
  // Byte 8 is the op, 9 the stream, 10 the flags.
  new Uint8Array(frame)[10] = 0x02;
  await announced().draw(frame);
  assert.deepEqual(
    chunkTypes,
    [],
    "a record with an unknown flag was decoded anyway",
  );
  assert.deepEqual(cropped, []);
});

test("units that arrive before their format are dropped, not reported", async () => {
  // The takeover: the gateway announces a stream once, to whoever was attached, so a
  // browser that takes the session over gets whatever was already in flight before the
  // repaint its attach triggers. Those units cannot be decoded here whatever happens —
  // a decoder built now can only start at a keyframe — so they are dropped in silence,
  // and the repaint that follows carries the format and a keyframe.
  //
  // It used to raise "the gateway sent video before saying how to decode it", which
  // named a contract violation for an ordinary race and left the banner up over a
  // session that had already recovered.
  const p = painter();
  await p.draw(
    batchFrame([
      {
        op: "video",
        stream: 0,
        x: 0,
        y: 0,
        w: 64,
        h: 64,
        payload: [4, 5, 6],
        keyframe: false,
      },
    ]),
  );
  assert.deepEqual(
    videoErrors,
    [null],
    "a unit before its format was reported",
  );
  assert.deepEqual(chunkTypes, [], "a unit with no format reached a decoder");
  assert.deepEqual(cropped, []);

  // And the recovery is the ordinary path: the format lands, the keyframe after it
  // decodes, and nothing had to be reset by hand.
  p.setVideoFormat(0, { decode: "vp09.00.40.08" });
  await p.draw(
    batchFrame([
      { op: "video", stream: 0, x: 0, y: 0, w: 64, h: 64, payload: KEYFRAME },
    ]),
  );
  assert.deepEqual(
    chunkTypes,
    ["key"],
    "the stream did not recover once announced",
  );
  assert.equal(cropped.length, 1);
});

test("each stream id gets its own decoder", async () => {
  // A target on `render_motion_subtype = "stream"` runs one per moving region, and
  // they are separate chains: a unit decoded against the wrong region's history
  // is corruption, not a misplaced picture.
  await announced([0, 1]).draw(
    batchFrame([
      { op: "video", stream: 0, x: 0, y: 0, w: 320, h: 64, payload: KEYFRAME },
      {
        op: "video",
        stream: 1,
        x: 640,
        y: 128,
        w: 320,
        h: 64,
        payload: KEYFRAME,
      },
    ]),
  );
  assert.equal(decoders, 2, "two regions shared one decoder");
  assert.deepEqual(
    cropped.map((c) => [c.dx, c.dy]),
    [
      [0, 0],
      [640, 128],
    ],
  );
});

test("a region that restarts on a new size replaces its decoder", async () => {
  // The configuration string carries no resolution, so an in-band size
  // change is not something to bet two browsers on. A region that grew is a new
  // picture — and the gateway re-announces its format, which is the other half of the
  // same statement and has its own test below.
  const p = announced();
  await p.draw(
    batchFrame([
      { op: "video", stream: 0, x: 0, y: 0, w: 320, h: 64, payload: KEYFRAME },
    ]),
  );
  await p.draw(
    batchFrame([
      { op: "video", stream: 0, x: 0, y: 0, w: 640, h: 64, payload: KEYFRAME },
    ]),
  );
  assert.equal(
    decoders,
    2,
    "the grown region kept a decoder built for the old size",
  );
  assert.ok(videoClosed, "the replaced decoder was left holding memory");
});

test("one region's decoder giving up does not take the others down", async () => {
  // Under `render_motion_subtype = "stream"` the rest of the desktop is arriving as
  // still tiles and the other regions are chains of their own, so a decoder that
  // fails is one region that stops — not the session.
  poison = 0xbd;
  const p = announced([0, 1]);
  await p.draw(
    batchFrame([
      {
        op: "video",
        stream: 0,
        x: 0,
        y: 0,
        w: 320,
        h: 64,
        payload: [...KEYFRAME, 0xbd],
      },
      {
        op: "video",
        stream: 1,
        x: 640,
        y: 0,
        w: 320,
        h: 64,
        payload: KEYFRAME,
      },
    ]),
  );
  assert.equal(
    videoErrors.filter(Boolean).length,
    1,
    "the failure was not reported",
  );
  assert.equal(
    closes,
    0,
    "a working decoder was closed because another failed",
  );
  assert.equal(
    videoKeyframeAsks.length,
    1,
    "a failed decoder is thrown away, and only a keyframe starts another",
  );

  // The surviving region keeps decoding on the decoder it already had.
  const before = decoders;
  await p.draw(
    batchFrame([
      {
        op: "video",
        stream: 1,
        x: 640,
        y: 0,
        w: 320,
        h: 64,
        payload: KEYFRAME,
      },
    ]),
  );
  assert.equal(
    decoders,
    before,
    "the surviving region was handed a new decoder",
  );
  assert.equal(cropped.length, 2, "the surviving region stopped painting");
});

test("the video complaint goes when video paints again", async () => {
  // A decoder giving up is a warning, not a verdict: the region comes back on the
  // next keyframe. Leaving the banner up would be a permanent notice about something
  // that stopped being true — and it has no dismiss button, because a statement
  // about the present should not need one.
  poison = 0xbd;
  const p = announced();
  await p.draw(
    batchFrame([
      {
        op: "video",
        stream: 0,
        x: 0,
        y: 0,
        w: 320,
        h: 64,
        payload: [...KEYFRAME, 0xbd],
      },
    ]),
  );
  assert.equal(
    videoErrors.at(-1),
    "This browser's video decoder failed (Error: this decoder gave up).",
  );

  poison = null;
  await p.draw(
    batchFrame([
      { op: "video", stream: 0, x: 0, y: 0, w: 320, h: 64, payload: KEYFRAME },
    ]),
  );
  assert.equal(
    videoErrors.at(-1),
    null,
    "the banner outlived what it described",
  );
});

test("a refused configuration stays up while another region paints", async () => {
  // Two regions, two codec strings: the level a stream announces follows its picture
  // size (`codec_string` in src/vp9.rs), so a browser can refuse one region's and
  // decode its neighbour's happily. The neighbour's frames say nothing about the
  // refused region, which is still showing nothing and still owes the explanation.
  refused = 0xbd;
  const p = announced([0, 1]);
  await p.draw(
    batchFrame([
      {
        op: "video",
        stream: 0,
        x: 0,
        y: 0,
        w: 320,
        h: 64,
        payload: [...KEYFRAME, 0xbd],
      },
      {
        op: "video",
        stream: 1,
        x: 640,
        y: 0,
        w: 320,
        h: 64,
        payload: KEYFRAME,
      },
    ]),
  );
  const said = videoErrors.at(-1);
  assert.match(
    String(said),
    /cannot decode/,
    "the refusal was not what the banner ended up saying",
  );
  assert.deepEqual(
    videoKeyframeAsks,
    [],
    "a keyframe was asked for on a configuration no keyframe repairs",
  );

  // And it keeps painting, which must not be read as the refused region recovering.
  await p.draw(
    batchFrame([
      {
        op: "video",
        stream: 1,
        x: 640,
        y: 0,
        w: 320,
        h: 64,
        payload: KEYFRAME,
      },
    ]),
  );
  assert.equal(
    videoErrors.at(-1),
    said,
    "one region painting took down another region's standing refusal",
  );
});

test("clear() retracts the complaint as well as the decoders", async () => {
  // The attachment boundary. The page clears its own copy on the way back to the
  // picker only, so a reattach or a takeover would otherwise inherit this sentence —
  // and a refusal is the kind that no later frame can retract on its own.
  refused = 0xbd;
  const p = announced();
  await p.draw(
    batchFrame([
      {
        op: "video",
        stream: 0,
        x: 0,
        y: 0,
        w: 64,
        h: 64,
        payload: [...KEYFRAME, 0xbd],
      },
    ]),
  );
  assert.notEqual(videoErrors.at(-1), null);
  p.clear();
  assert.equal(videoErrors.at(-1), null);
});

test("clear() ends the video decoders, not only the slot table", async () => {
  const p = announced();
  await p.draw(
    batchFrame([
      { op: "video", stream: 0, x: 0, y: 0, w: 64, h: 64, payload: KEYFRAME },
    ]),
  );
  p.clear();
  assert.ok(videoClosed, "the decoders belong to one attachment");
});

test("a refused stream says so rather than showing nothing", async () => {
  // The whole reason this is reported at all: a video target sends no still tiles, so
  // the alternative is a desktop that never paints and never explains itself.
  refused = 0xbd;
  await announced().draw(
    batchFrame([
      {
        op: "video",
        stream: 0,
        x: 0,
        y: 0,
        w: 64,
        h: 64,
        payload: [...KEYFRAME, 0xbd],
      },
    ]),
  );
  // Filtered because building the table retracts whatever the last attachment said,
  // which is a null of its own ahead of this one.
  const said = videoErrors.filter(Boolean);
  assert.equal(said.length, 1);
  assert.match(String(said[0]), /decode/i);
  assert.deepEqual(cropped, []);
});

test("every decoded bitmap is closed, even with nowhere to draw it", async () => {
  await painter(null).draw(
    batchFrame([
      { op: "tile", slot: NO_SLOT, x: 0, y: 0, payload: [1] },
      { op: "tile", slot: NO_SLOT, x: 0, y: 0, payload: [2] },
    ]),
  );
  assert.deepEqual(drawn, []);
  assert.equal(decoded.length, 2);
  assert.ok(
    decoded.every((bitmap) => bitmap.closed),
    "a bitmap that is never drawn still has to be released",
  );
});
