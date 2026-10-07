// BETA: one thread of the software HEVC decoder's pool (hevcWasm.worker.ts): an
// instance of the module on the memory the decode worker's instance has, which
// takes a seat in the pool and decodes the rows the pool hands it.
//
// It answers once, before it takes its seat: null when its instance is made, or
// why it could not be. Taking the seat does not return, so nothing is heard from
// it after.
import { hevcDecoderUrl } from "./gateway.ts";

/**
 * The module's memory, which its threads share: a `WebAssembly.Memory` whose
 * buffer is a `SharedArrayBuffer`, as the DOM's types do not say.
 */
export interface SharedMemory extends Omit<WebAssembly.Memory, "buffer"> {
  readonly buffer: SharedArrayBuffer;
}

/** What the decode worker's instance is, for this one to be of the same. */
export interface PoolSeat {
  module: WebAssembly.Module;
  memory: SharedMemory;
}

/** The glue's exports a thread uses. */
interface ThreadGlue {
  default(options: {
    module_or_path: WebAssembly.Module;
    memory: SharedMemory;
  }): Promise<unknown>;
  runPoolThread(): void;
}

// The DOM lib types `self` as a Window; see desktopPainter.worker.ts.
const scope = self as unknown as {
  postMessage(message: string | null): void;
  onmessage: ((ev: MessageEvent<PoolSeat>) => void) | null;
};

scope.onmessage = ({ data }) => {
  (async () => {
    const glue = (await import(
      /* @vite-ignore */ hevcDecoderUrl("hevc.js")
    )) as ThreadGlue;
    await glue.default({ module_or_path: data.module, memory: data.memory });
    scope.postMessage(null);
    glue.runPoolThread();
  })().catch((error: unknown) => {
    scope.postMessage(error instanceof Error ? error.message : String(error));
  });
};
