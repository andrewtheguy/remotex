// BETA: whether this page can run a software decoder (softwareDecoder.ts) and
// show what it decodes. The picker asks before it offers a session decoded in the
// page (targetChoices.ts), and the paint worker is told, since a page told to
// decode a stream it cannot says so and decodes nothing (videoDecoder.ts).

import type { SoftwareModule } from "./softwareDecoder.ts";

/** A function returning a SIMD128 value, which only a SIMD engine validates. */
const SIMD_PROBE = Uint8Array.of(
  ...[0, 97, 115, 109, 1, 0, 0, 0],
  ...[1, 5, 1, 96, 0, 1, 123],
  ...[3, 2, 1, 0],
  ...[10, 10, 1, 8, 0, 65, 0, 253, 15, 253, 98, 11],
);

/**
 * Whether this browser presents a software decoder's pictures (planesPicture.ts):
 * on a WebGL 2 canvas off the page, as the paint worker's is, and for
 * `widePrimaries` one that can be given primaries other than sRGB's.
 */
function presentsPlanes(widePrimaries: boolean): boolean {
  const gl = new OffscreenCanvas(1, 1).getContext("webgl2");
  if (!gl || gl.isContextLost()) {
    return false;
  }
  const takesPrimaries = "drawingBufferColorSpace" in gl;
  gl.getExtension("WEBGL_lose_context")?.loseContext();
  return takesPrimaries || !widePrimaries;
}

/**
 * Whether the page runs shared-memory SIMD WebAssembly, which is a module and
 * its threads, and presents the pictures. `widePrimaries` for a stream that may
 * state primaries other than sRGB's, as a Mac's does.
 */
export function runsSoftwareDecoder(options: {
  widePrimaries: boolean;
}): boolean {
  try {
    // Every gateway isolates the page; a proxy that drops the headers does not,
    // and says so first.
    if (globalThis.crossOriginIsolated !== true) {
      return false;
    }
    if (!WebAssembly.validate(SIMD_PROBE)) {
      return false;
    }
    const memory = new WebAssembly.Memory({
      initial: 1,
      maximum: 1,
      shared: true,
    });
    if (!(memory.buffer instanceof SharedArrayBuffer)) {
      return false;
    }
    return presentsPlanes(options.widePrimaries);
  } catch {
    return false;
  }
}

/** Which of the page's software decoders this page can run and present. */
export type RunnableDecoders = Record<SoftwareModule, boolean>;

let runnable: RunnableDecoders | null = null;

/**
 * Which of them this page runs, asked once: a Mac's HEVC needs the canvas given
 * its primaries, Display P3, and the gateway's VP9 is presented in sRGB's.
 */
export function runnableDecoders(): RunnableDecoders {
  runnable ??= {
    hevc: runsSoftwareDecoder({ widePrimaries: true }),
    vp9: runsSoftwareDecoder({ widePrimaries: false }),
  };
  return runnable;
}

/** Test seam: forget the answer so the question can be asked again. */
export function resetRunnableDecodersForTests(): void {
  runnable = null;
}
