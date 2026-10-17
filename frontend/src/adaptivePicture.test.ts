// The page's decoder for a Mac's own picture, through its binding: the
// decoders' own tests (wasm/adaptive) check what each tile command draws and how
// the zlib stream runs across rectangles, and these that a rectangle of either
// quality reaches the framebuffer the canvas is painted from, and that one that
// does not decode says so and keeps what it drew.
//
// Run with `bun run test` from frontend/, which builds the module first.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { constants, deflateSync } from "node:zlib";
import { loadAdaptive } from "./adaptivePicture.ts";

const module = readFileSync(
  new URL("../wasm/adaptive/pkg/remotex_adaptive_bg.wasm", import.meta.url),
);

// The quantization tables, each entry 1: the rectangle of no area the Mac sends
// before its first picture.
const TABLES = Uint8Array.from([2, ...new Array(128).fill(1)]);

// A full update of one white tile: its precisions, the data stream's offset, a
// command stream of the bit the viewer skips, command 0 for one tile and the
// mark, and a data stream of its mark alone.
const WHITE_TILE = Uint8Array.from([0, 3, 5, 0, 0, 8, 0x03, 0x68, 0x6d]);

test("a rectangle draws on the framebuffer, where it is and nowhere else", async () => {
  const make = await loadAdaptive(module);
  const picture = make("adaptive");
  assert.equal(
    picture.pixels(),
    null,
    "no framebuffer before the first resize",
  );
  picture.resize(16, 8);
  assert.equal(
    picture.decode(0, 0, 0, 0, TABLES),
    false,
    "the tables draw nothing",
  );
  assert.equal(picture.decode(8, 0, 8, 8, WHITE_TILE), true);
  const pixels = picture.pixels();
  assert.ok(pixels);
  assert.equal(pixels.length, 16 * 8 * 4);
  for (let y = 0; y < 8; y += 1) {
    for (let x = 0; x < 16; x += 1) {
      const at = (y * 16 + x) * 4;
      const want = x < 8 ? [0, 0, 0, 255] : [255, 255, 255, 255];
      assert.deepEqual([...pixels.subarray(at, at + 4)], want, `${x},${y}`);
    }
  }
  picture.close();
});

test("a rectangle that does not decode is refused with the reason", async () => {
  const make = await loadAdaptive(module);
  const picture = make("adaptive");
  picture.resize(8, 8);
  assert.throws(
    () => picture.decode(0, 0, 8, 8, WHITE_TILE),
    /before the quantization tables/,
  );
  picture.decode(0, 0, 0, 0, TABLES);
  assert.throws(
    () => picture.decode(0, 0, 16, 8, WHITE_TILE),
    /on a 8x8 framebuffer/,
  );
  picture.close();
});

// A Full-quality rectangle as the Mac deflates it: its pixels in the gateway's
// format, each blue, green, red and an unused byte, the stream flushed and not
// finished.
function zlibRect(pixels: [number, number, number][]): Uint8Array {
  const raw = Uint8Array.from(pixels.flatMap(([r, g, b]) => [b, g, r, 0]));
  return deflateSync(raw, { finishFlush: constants.Z_SYNC_FLUSH });
}

test("a Full-quality rectangle inflates onto the framebuffer in R, G, B, A", async () => {
  const make = await loadAdaptive(module);
  const picture = make("full");
  picture.resize(4, 2);
  const rect = zlibRect([
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [10, 20, 30],
  ]);
  assert.equal(picture.decode(2, 0, 2, 2, rect), true);
  const pixels = picture.pixels();
  assert.ok(pixels);
  const at = (x: number, y: number) => {
    const i = (y * 4 + x) * 4;
    return [...pixels.subarray(i, i + 4)];
  };
  assert.deepEqual(at(2, 0), [255, 0, 0, 255]);
  assert.deepEqual(at(3, 0), [0, 255, 0, 255]);
  assert.deepEqual(at(2, 1), [0, 0, 255, 255]);
  assert.deepEqual(at(3, 1), [10, 20, 30, 255]);
  assert.deepEqual(at(0, 0), [0, 0, 0, 255], "nothing drawn beside it");
  picture.close();
});

test("a Full-quality body that is not its rectangle's pixels is refused", async () => {
  const make = await loadAdaptive(module);
  const picture = make("full");
  picture.resize(4, 4);
  assert.throws(
    () => picture.decode(0, 0, 2, 2, zlibRect([[1, 2, 3]])),
    /inflated to 4 bytes, where its pixels are 16/,
  );
  picture.close();
});
