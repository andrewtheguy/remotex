// BETA: whether this page can run a software decoder (softwareDecoder.ts) and
// show what it decodes. Asked at load by whoever decides a stream is decoded in
// one (appleMedia.ts, videoChroma.ts), because a yes that cannot be run or
// presented is a session with no picture, where a no is one sent what the
// browser's own decoder takes.

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

const SWITCHES = ["hevc_decoder", "vp9_decoder"] as const;

/**
 * The software switches of a URL's query, as a query of their own or nothing:
 * what a page opened beside this one carries so that it decodes as this one does.
 */
export function softwareSwitches(
  search: string = globalThis.location?.search ?? "",
): string {
  const asked = new URLSearchParams(search);
  const kept = new URLSearchParams();
  for (const decoder of SWITCHES) {
    if (asked.get(decoder) === "software") {
      kept.set(decoder, "software");
    }
  }
  const query = kept.toString();
  return query && `?${query}`;
}

/** Whether the page's URL asks for `decoder` in software: `?<decoder>=software`. */
export function softwareRequested(decoder: (typeof SWITCHES)[number]): boolean {
  return (
    new URLSearchParams(globalThis.location?.search ?? "").get(decoder) ===
    "software"
  );
}
