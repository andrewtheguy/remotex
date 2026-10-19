// Which failures the page shows are the browser saying it ran out of memory. Run
// with `bun test src/memoryBudget.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import { reloadFreesMemory, saysOutOfMemory } from "./memoryBudget.ts";

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
    "This browser's own decoder does not take the Mac's HEVC, and this page cannot run its own: that needs WebGL 2 and a cross-origin isolated page.",
    "the ROOMS were full",
  ]) {
    assert.equal(saysOutOfMemory(reason), false, reason);
  }
});

test("a reload frees the memory everywhere but an iPhone or an iPad", () => {
  const iphone =
    "Mozilla/5.0 (iPhone; CPU iPhone OS 18_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.6.1 Mobile/15E148 Safari/604.1";
  const mac =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.0 Safari/605.1.15";
  const chrome =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36";
  assert.equal(
    reloadFreesMemory({
      userAgent: iphone,
      platform: "iPhone",
      maxTouchPoints: 5,
    }),
    false,
  );
  // An iPad asking for desktop pages.
  assert.equal(
    reloadFreesMemory({
      userAgent: mac,
      platform: "MacIntel",
      maxTouchPoints: 5,
    }),
    false,
  );
  assert.equal(
    reloadFreesMemory({
      userAgent: mac,
      platform: "MacIntel",
      maxTouchPoints: 0,
    }),
    true,
  );
  assert.equal(
    reloadFreesMemory({
      userAgent: chrome,
      platform: "Win32",
      maxTouchPoints: 0,
    }),
    true,
  );
});
