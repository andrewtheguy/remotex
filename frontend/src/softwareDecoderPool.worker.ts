// BETA: one thread of a software decoder's pool (softwareDecoder.worker.ts): an
// instance of the module on the memory the decode worker's instance has, which
// takes a seat in the pool and decodes what the pool hands it.
//
// It answers once, before it takes its seat: null when its instance is made, or
// why it could not be. Taking the seat does not return, so nothing is heard from
// it after.
import { decoderGlue, type PoolSeat } from "./softwareDecoderModule.ts";

// The DOM lib types `self` as a Window; see desktopPainter.worker.ts.
const scope = self as unknown as {
  postMessage(message: string | null): void;
  onmessage: ((ev: MessageEvent<PoolSeat>) => void) | null;
};

scope.onmessage = ({ data }) => {
  (async () => {
    const { glue } = await decoderGlue(data.decoder);
    await glue.default({ module_or_path: data.module, memory: data.memory });
    scope.postMessage(null);
    glue.runPoolThread();
  })().catch((error: unknown) => {
    scope.postMessage(error instanceof Error ? error.message : String(error));
  });
};
