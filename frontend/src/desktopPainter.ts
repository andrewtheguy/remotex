// The page's handle on the desktop paint worker.
//
// The parse→decode→paint path runs in a worker (desktopPainterWorker.ts)
// drawing on an `OffscreenCanvas`, so a decode backlog costs the worker's
// thread rather than React's and input's. This module is the main-thread side:
// it transfers each binary frame over — `postMessage` with a transfer list
// moves the buffer, it does not copy it — and carries the painter's callbacks
// and ordered completion feedback back as messages.
//
// One worker per canvas *element*, held at module level like the pointer rect
// cache, and for a harder reason than tidiness: `transferControlToOffscreen`
// works exactly once per element, and the connection effect that wants a
// painter runs more than once per element — StrictMode reruns it on the same
// canvas. So the worker outlives the effect, the effect binds and unbinds its
// handlers, and only a *new* canvas element (a full remount) replaces the
// worker — at which point the old one holds the bitmap of an element that is
// gone, and is terminated rather than left decoding for it.
import type { PainterCommand, PainterEvent } from "./desktopPainterWorker.ts";
import { batchFrameSequence } from "./protocol.ts";
import type { VideoFormat } from "./videoDecoder.ts";

export interface PainterHandlers {
  /** The worker found a slot this client does not hold; ask the server to reset. */
  onCacheReset: () => void;
  /** Why a video target shows nothing, or null once it shows something. */
  onVideoError: (reason: string | null) => void;
  /**
   * A stream's decoder was reset out of a stall. The region it was carrying is
   * frozen until a keyframe arrives, and asking for one is the page's job because
   * only it holds the socket.
   */
  onVideoStall: (reason: string) => void;
  /** One ordered screen batch finished in the worker. */
  onPainted: (
    sequence: number,
    generation: number,
    queuedMs: number,
    drawMs: number,
  ) => void;
  /**
   * A `resize` command has been applied — the bitmap it named is on screen.
   * The page defers the layout state that presents the bitmap (the CSS box,
   * the size the pointer maps through, the status overlay's "is there a
   * desktop yet") to this echo, because applying it on the control message's
   * arrival would drop the overlay while the worker was still painting the
   * previous desktop's backlog onto the previous bitmap.
   */
  onResized: (seq: number) => void;
}

export interface DesktopPainter {
  /** Point the worker's events at this attachment's handlers. */
  bind(handlers: PainterHandlers): void;
  /** Drop the handlers; the worker idles until the next bind. */
  unbind(): void;
  /** Hand one binary socket frame to the worker. Transfers the buffer. */
  draw(frame: ArrayBuffer, generation: number): void;
  /** Set the canvas bitmap (framebuffer pixels) and fill it black. */
  resize(bitmap: { w: number; h: number }, seq: number): void;
  setVideoFormat(stream: number, format: VideoFormat): void;
  /** The attachment boundary: wipe the bitmap, the caches and the decoders. */
  clear(): void;
}

let current: {
  canvas: HTMLCanvasElement;
  worker: Worker;
  painter: DesktopPainter;
} | null = null;

/** The painter for the page's one desktop canvas, built on first ask. */
export function desktopPainterFor(canvas: HTMLCanvasElement): DesktopPainter {
  if (current?.canvas === canvas) {
    return current.painter;
  }
  current?.worker.terminate();
  const worker = new Worker(
    new URL("./desktopPainter.worker.ts", import.meta.url),
    { type: "module", name: "desktop-painter" },
  );
  // Most events carry no attachment tag: for a stale event to reach a
  // *new* binding, this worker would need two binds with the first having
  // produced worker activity — and two binds on one worker only happen under
  // StrictMode's synchronous remount, whose first effect run is disposed
  // before its session claim resolves and so never posts a frame, a resize or
  // a format. (A real remount is a new canvas element, which replaces the
  // worker above; its events die with it.) The one event with per-attachment
  // meaning, `resized`, is matched against the binding's own pending map
  // besides. `painted` is the exception: the socket generation it echoes is
  // what prevents an old completion from acknowledging a new attachment.
  let handlers: PainterHandlers | null = null;
  worker.onmessage = (ev: MessageEvent<PainterEvent>) => {
    const event = ev.data;
    if (event.type === "cacheReset") {
      handlers?.onCacheReset();
    } else if (event.type === "videoError") {
      handlers?.onVideoError(event.reason);
    } else if (event.type === "videoStall") {
      handlers?.onVideoStall(event.reason);
    } else if (event.type === "resized") {
      handlers?.onResized(event.seq);
    } else {
      handlers?.onPainted(
        event.sequence,
        event.generation,
        event.queuedMs,
        event.drawMs,
      );
    }
  };
  const post = (command: PainterCommand, transfer: Transferable[] = []) =>
    worker.postMessage(command, transfer);
  const offscreen = canvas.transferControlToOffscreen();
  post({ type: "init", canvas: offscreen }, [offscreen]);
  const painter: DesktopPainter = {
    bind(next) {
      handlers = next;
    },
    unbind() {
      handlers = null;
    },
    draw(frame, generation) {
      const sequence = batchFrameSequence(frame);
      if (sequence === null) {
        return;
      }
      post({ type: "frame", data: frame, sequence, generation }, [frame]);
    },
    resize(bitmap, seq) {
      post({ type: "resize", w: bitmap.w, h: bitmap.h, seq });
    },
    setVideoFormat(stream, format) {
      post({ type: "videoFormat", stream, format });
    },
    clear() {
      post({ type: "clear" });
    },
  };
  current = { canvas, worker, painter };
  return painter;
}
