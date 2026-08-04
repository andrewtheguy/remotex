// The paint worker's entry point: bind `createPainterWorker` to this worker's
// message port. Everything with behaviour lives in desktopPainterWorker.ts,
// where it can be tested; this file exists because a worker has to start at a
// module of its own (see desktopPainter.ts, which names this file to `new
// Worker`).
import {
  createPainterWorker,
  type PainterCommand,
  type PainterEvent,
} from "./desktopPainterWorker.ts";

// The DOM lib types `self` as a Window, whose `postMessage` wants a target
// origin; this is a dedicated worker, whose `postMessage` takes none. Narrowed
// by hand rather than by `lib: ["webworker"]`, which cannot share a program
// with the DOM lib the rest of the client needs.
const scope = self as unknown as {
  postMessage(message: PainterEvent): void;
  onmessage: ((ev: MessageEvent<PainterCommand>) => void) | null;
};

const host = createPainterWorker((event) => scope.postMessage(event));
scope.onmessage = (ev) => host.handle(ev.data);
