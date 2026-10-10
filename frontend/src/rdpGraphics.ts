// Whether this page composes an RDP host's graphics pipeline: the second question
// about itself the gateway is told, beside the Mac's stream (appleMedia.ts), and
// ahead of the pipeline's H.264 (rdpH264.ts).
//
// A session started with the pipeline passed is composed here, by the gateway's own
// compositor compiled to WebAssembly (egfxCompositor.ts). That needs two things of
// the page. A memory its threads can share, which is a page that is cross-origin
// isolated: every gateway serves the two headers that make one, and a proxy that
// drops them takes it away. And a WebGL 2 canvas off the page, as the paint
// worker's is, to present the picture on (egfxPicture.ts).
//
// The answer greys the choice at the picker where it is no, and rides every session
// socket this page opens (`gateway.ts`), so a page that comes back to its session
// saying no is returned to the picker instead of being sent a stream it cannot
// compose. Nothing is rebuilt for a page that says no.

let answer: boolean | null = null;

function sharesMemory(): boolean {
  if (globalThis.crossOriginIsolated !== true) {
    return false;
  }
  const memory = new WebAssembly.Memory({
    initial: 1,
    maximum: 1,
    shared: true,
  });
  return memory.buffer instanceof SharedArrayBuffer;
}

function presentsOnWebGl2(): boolean {
  const gl = new OffscreenCanvas(1, 1).getContext("webgl2");
  if (!gl || gl.isContextLost()) {
    return false;
  }
  gl.getExtension("WEBGL_lose_context")?.loseContext();
  return true;
}

/**
 * Whether this page composes a passed pipeline. Asked once, on the first call, and
 * anything that throws reads as no.
 */
export function composesRdpGraphics(): boolean {
  if (answer === null) {
    try {
      answer = sharesMemory() && presentsOnWebGl2();
    } catch {
      answer = false;
    }
  }
  return answer;
}

/** Test seam: forget the answer so the question can be asked again. */
export function resetRdpGraphicsForTests(): void {
  answer = null;
}
