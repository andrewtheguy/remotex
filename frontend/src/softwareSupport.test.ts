// BETA: which of the page's software decoders a page can run and present: both
// need shared-memory WebAssembly on an isolated page and a WebGL 2 canvas off the
// page, and a Mac's HEVC a canvas that can be given its primaries besides.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  resetRunnableDecodersForTests,
  runnableDecoders,
} from "./softwareSupport.ts";

/** A page, isolated or not, whose canvas has this WebGL 2 context. Returns the undo. */
function page(
  isolated: boolean,
  webgl: "whole" | "srgb only" | "none",
): () => void {
  const scope = globalThis as unknown as Record<string, unknown>;
  const saved = ["crossOriginIsolated", "OffscreenCanvas"].map(
    (key) => [key, Object.getOwnPropertyDescriptor(scope, key)] as const,
  );
  Object.defineProperty(scope, "crossOriginIsolated", {
    value: isolated,
    configurable: true,
  });
  scope.OffscreenCanvas = class {
    getContext(kind: string) {
      assert.equal(kind, "webgl2");
      if (webgl === "none") {
        return null;
      }
      return {
        isContextLost: () => false,
        getExtension: () => null,
        ...(webgl === "whole" ? { drawingBufferColorSpace: "srgb" } : {}),
      };
    }
  };
  resetRunnableDecodersForTests();
  return () => {
    resetRunnableDecodersForTests();
    for (const [key, descriptor] of saved) {
      if (descriptor) {
        Object.defineProperty(scope, key, descriptor);
      } else {
        delete scope[key];
      }
    }
  };
}

test("a page runs the decoders it can present the pictures of", () => {
  const cases = [
    [true, "whole", { hevc: true, vp9: true }],
    // No color space to set: sRGB alone, which is the gateway's VP9.
    [true, "srgb only", { hevc: false, vp9: true }],
    [true, "none", { hevc: false, vp9: false }],
    // A proxy that dropped the isolation headers: no shared memory.
    [false, "whole", { hevc: false, vp9: false }],
  ] as const;
  for (const [isolated, webgl, runs] of cases) {
    const undo = page(isolated, webgl);
    try {
      assert.deepEqual(runnableDecoders(), runs, `${isolated} ${webgl}`);
    } finally {
      undo();
    }
  }
});
