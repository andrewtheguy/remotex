// The slot table and the batch draw loop, over frames this file builds itself.
//
// Deliberately not built with `protocol.ts`'s encoder — there isn't one, and a
// test that produced its input with the same code that reads it would agree with
// itself no matter what either did. The layout below is transcribed from
// `batch` in `src/protocol.rs`, which is the contract both ends are checked
// against (the Swift viewer's `TileFrameTests` does the same on its side).
//
// Run with `bun test src/tilePainter.test.ts` from frontend/.
import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { NO_SLOT } from "./protocol.ts";
import { createTilePainter } from "./tilePainter.ts";

const OP_TILE = 0x01;
const OP_TILE_REF = 0x02;
const FORMAT_PNG = 1;

type Record =
  | { op: "tile"; slot: number; x: number; y: number; payload: number[] }
  | { op: "ref"; slot: number; x: number; y: number };

function batchFrame(records: Record[]): ArrayBuffer {
  const bytes: number[] = [];
  const u16 = (n: number) => bytes.push(n & 0xff, (n >> 8) & 0xff);
  const u32 = (n: number) =>
    bytes.push(n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, (n >> 24) & 0xff);
  bytes.push(0x02, 0x00);
  u16(records.length);
  for (const record of records) {
    if (record.op === "ref") {
      bytes.push(OP_TILE_REF);
      u16(record.slot);
      u16(record.x);
      u16(record.y);
      continue;
    }
    bytes.push(OP_TILE, FORMAT_PNG);
    u16(record.slot);
    u16(record.x);
    u16(record.y);
    u16(1); // w
    u16(1); // h
    u32(record.payload.length);
    bytes.push(...record.payload);
  }
  return new Uint8Array(bytes).buffer;
}

// A decoded tile stands for its first payload byte, so a draw can be named.
interface FakeBitmap {
  tag: number;
  closed: boolean;
}

let drawn: { tag: number; x: number; y: number }[] = [];
let decoded: FakeBitmap[] = [];
let resets = 0;
/** Payload first bytes the stubbed decoder refuses. */
let undecodable = new Set<number>();

const context = {
  drawImage(bitmap: FakeBitmap, x: number, y: number) {
    drawn.push({ tag: bitmap.tag, x, y });
  },
} as unknown as CanvasRenderingContext2D;

const globals = globalThis as unknown as {
  createImageBitmap: (blob: Blob) => Promise<unknown>;
};
const realCreateImageBitmap = globals.createImageBitmap;

beforeEach(() => {
  drawn = [];
  decoded = [];
  resets = 0;
  undecodable = new Set();
  globals.createImageBitmap = async (blob: Blob) => {
    const tag = new Uint8Array(await blob.arrayBuffer())[0];
    if (undecodable.has(tag)) {
      throw new Error("undecodable");
    }
    const bitmap: FakeBitmap = { tag, closed: false };
    decoded.push(bitmap);
    return {
      ...bitmap,
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
});

function painter(ctx: CanvasRenderingContext2D | null = context) {
  return createTilePainter({
    context: () => ctx,
    onCacheReset: () => {
      resets += 1;
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
