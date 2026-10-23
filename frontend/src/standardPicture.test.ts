// The page's decoder for a Mac's Adaptive picture, through its binding: the
// decoder's own tests (wasm/apple-adaptive) check what each tile command draws,
// and these that a rectangle reaches the framebuffer the canvas is painted from,
// and that one that does not decode says so and keeps what it drew.
//
// Run with `bun run test` from frontend/, which builds the module first.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { loadStandard } from "./standardPicture.ts";

const module = readFileSync(
  new URL(
    "../wasm/apple-adaptive/pkg/zdesk_apple_adaptive_bg.wasm",
    import.meta.url,
  ),
);

// The quantization tables, each entry 1: the rectangle of no area the Mac sends
// before its first picture.
const TABLES = Uint8Array.from([2, ...new Array(128).fill(1)]);

// A full update of one white tile: its precisions, the data stream's offset, a
// command stream of the bit the viewer skips, command 0 for one tile and the
// mark, and a data stream of its mark alone.
const WHITE_TILE = Uint8Array.from([0, 3, 5, 0, 0, 8, 0x03, 0x68, 0x6d]);

test("a rectangle draws on the framebuffer, where it is and nowhere else", async () => {
  const make = await loadStandard(module);
  const picture = make();
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
  const make = await loadStandard(module);
  const picture = make();
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
