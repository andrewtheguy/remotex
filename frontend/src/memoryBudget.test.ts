// Which browsers load as little WebAssembly as they can, from what each says of
// itself. Run with `bun test src/memoryBudget.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import { memoryIsTight } from "./memoryBudget.ts";

const signs = (
  platform: string,
  maxTouchPoints: number,
  deviceMemory?: number,
) => ({ platform, maxTouchPoints, deviceMemory });

test("an iPhone or an iPad is held tight whatever its browser", () => {
  assert.equal(memoryIsTight(signs("iPhone", 5)), true);
  assert.equal(memoryIsTight(signs("iPad", 5)), true);
  // An iPad asking for desktop pages says it is a Mac, with a touchscreen.
  assert.equal(memoryIsTight(signs("MacIntel", 5)), true);
});

test("a desktop that reports no memory is not", () => {
  // Safari and Firefox on a Mac, Firefox on Linux and Windows.
  assert.equal(memoryIsTight(signs("MacIntel", 0)), false);
  assert.equal(memoryIsTight(signs("Linux x86_64", 0)), false);
  assert.equal(memoryIsTight(signs("Win32", 0)), false);
});

test("a touchscreen says nothing of memory", () => {
  // A touch laptop, and an Android phone with the memory of a desktop.
  assert.equal(memoryIsTight(signs("Win32", 10, 8)), false);
  assert.equal(memoryIsTight(signs("Linux armv81", 5, 8)), false);
});

test("a device whose browser reports 4 GB or less is held tight", () => {
  assert.equal(memoryIsTight(signs("Linux armv81", 5, 4)), true);
  assert.equal(memoryIsTight(signs("Linux armv81", 5, 2)), true);
  assert.equal(memoryIsTight(signs("Win32", 0, 4)), true);
  assert.equal(memoryIsTight(signs("Linux x86_64", 0, 8)), false);
});
