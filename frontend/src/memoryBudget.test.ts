// Which browsers load as little WebAssembly as they can, from what each says of
// itself. Run with `bun test src/memoryBudget.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import { memoryIsTight, saysOutOfMemory } from "./memoryBudget.ts";

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

test("each engine's words for running out of memory are read as that", () => {
  for (const reason of [
    // JavaScriptCore, and SpiderMonkey in lower case.
    "This browser could not load the graphics compositor (Out of memory).",
    "This browser could not show the display's picture (out of memory).",
    // V8, for a buffer and for a WebAssembly memory.
    "This browser could not present the decoded picture (Array buffer allocation failed).",
    "its decoder did not load: WebAssembly.Memory(): could not allocate memory",
    "WebAssembly.instantiate(): Out of memory: Cannot allocate Wasm memory for new instance",
    // Emscripten's abort.
    "its decoder did not load: Aborted(OOM)",
    "Cannot enlarge memory arrays to size 2147483648 bytes",
  ]) {
    assert.equal(saysOutOfMemory(reason), true, reason);
  }
});

test("other failures are not", () => {
  for (const reason of [
    "This browser could not load the graphics compositor (Out of bounds memory access (evaluating 'T.startPool(t)')).",
    "This browser could not show the display's picture (unreachable). Reload the page to start it over.",
    "This browser's own decoder does not take the gateway's 4:4:4 VP9, and this page cannot run its own: that needs WebGL 2 and a cross-origin isolated page.",
    "the ROOMS were full",
  ]) {
    assert.equal(saysOutOfMemory(reason), false, reason);
  }
});
