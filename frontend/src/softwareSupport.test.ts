// Which of the page's software decoders a page can run and present: both
// need WebCodecs' chunks, shared-memory WebAssembly on an isolated page and a
// WebGL 2 canvas off the page, and a Mac's HEVC a canvas that can be given its
// primaries besides.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  resetRunnableDecodersForTests,
  runnableDecoders,
} from "./softwareSupport.ts";

/**
 * A page, isolated or not, with or without WebCodecs, whose canvas has this
 * WebGL 2 context. Returns the undo.
 */
function page(
  isolated: boolean,
  webgl: "whole" | "srgb only" | "none",
  webCodecs = true,
): () => void {
  const scope = globalThis as unknown as Record<string, unknown>;
  const saved = [
    "crossOriginIsolated",
    "OffscreenCanvas",
    "EncodedVideoChunk",
  ].map((key) => [key, Object.getOwnPropertyDescriptor(scope, key)] as const);
  Object.defineProperty(scope, "crossOriginIsolated", {
    value: isolated,
    configurable: true,
  });
  if (webCodecs) {
    scope.EncodedVideoChunk = class {};
  } else {
    delete scope.EncodedVideoChunk;
  }
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
    [true, "whole", { hevc: true }],
    // No color space to set: sRGB alone, which the Mac's HEVC is not.
    [true, "srgb only", { hevc: false }],
    [true, "none", { hevc: false }],
    // A proxy that dropped the isolation headers: no shared memory.
    [false, "whole", { hevc: false }],
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

test("a browser without WebCodecs runs no software decoder", () => {
  // Everything else in place: the stream reaches the module as chunks it lacks.
  const undo = page(true, "whole", false);
  try {
    assert.deepEqual(runnableDecoders(), { hevc: false });
  } finally {
    undo();
  }
});
