// One thread of the compositor's pool (egfxCompositor.ts): an instance of the
// module on the memory the paint worker's instance has, which takes a seat in the
// pool and decodes what the pool hands it.
//
// It answers once, before it takes its seat: whether its instance is made, or
// why it could not be. Taking the seat returns only once the pool is stopped
// (`stopEgfx`), when it says so, to be ended: a worker ended while its thread
// waits in the module is one iOS never frees.
import init, { runPoolThread } from "../wasm/egfx/pkg/zdesk_egfx.js";

/** What the paint worker's instance is, for this one to be of the same. */
export interface PoolSeat {
  module: WebAssembly.Module;
  memory: WebAssembly.Memory;
}

/**
 * What a pool worker says: seated, or failed to be; then stopped, or failed, its
 * thread out of the module either way.
 */
export type PoolEvent =
  | { type: "seated" }
  | { type: "failed"; reason: string }
  | { type: "stopped" };

// The DOM lib types `self` as a Window; see desktopPainter.worker.ts.
const scope = self as unknown as {
  postMessage(message: PoolEvent): void;
  onmessage: ((ev: MessageEvent<PoolSeat>) => void) | null;
};

const failed = (error: unknown) =>
  scope.postMessage({
    type: "failed",
    reason: error instanceof Error ? error.message : String(error),
  });

scope.onmessage = ({ data }) => {
  init({ module_or_path: data.module, memory: data.memory }).then(() => {
    scope.postMessage({ type: "seated" });
    // A thread that traps says so: thrown out of here it would be a rejection
    // nobody hears, and the paint worker would wait for its `stopped` for good.
    try {
      runPoolThread();
    } catch (error) {
      failed(error);
      return;
    }
    scope.postMessage({ type: "stopped" });
  }, failed);
};
