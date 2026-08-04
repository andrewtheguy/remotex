// The worker half of the desktop paint path.
//
// The page's main thread used to parse, decode and paint every batch itself,
// sharing that thread with React and input; now it transfers each binary frame
// here, and this module runs the same `createTilePainter` — slot table, decoded
// bitmap cache, `VideoDecoder` table, batch draw loop — against an
// `OffscreenCanvas` the page handed over once. Nothing about the painter
// changed; what changed is which thread it costs.
//
// **Ordering is the contract this file keeps, and it is the whole of it.** On
// the main thread, draws and the control effects that must hold their place
// behind them (a resize resets the canvas; a format change drops a decoder
// that queued units still need; a clear ends the attachment) rode one promise
// chain. Here the wire's order arrives as message order — the page posts
// commands as they land on the socket — and one chain preserves it across the
// async decodes. That includes `clear`: a clear that jumped the queue would
// run *before* frames posted ahead of it had started drawing, and those draws
// would then paint the previous desktop onto the next attachment's canvas.
// Strict order needs no generation fencing — nothing from a new attachment
// can be posted before the clear that ended the old one, because the page
// stops dispatching a dead socket's frames before it clears.
import { binaryFrameKind } from "./protocol.ts";
import { createTilePainter, type TilePainter } from "./tilePainter.ts";
import type { VideoFormat } from "./videoDecoder.ts";

/** What the page sends the worker. `init` arrives exactly once, first. */
export type PainterCommand =
  | { type: "init"; canvas: OffscreenCanvas }
  | {
      type: "frame";
      data: ArrayBuffer;
      /** From the server's batch header. */
      sequence: number;
      /** Which browser WebSocket posted this command. */
      generation: number;
    }
  /**
   * The canvas bitmap in framebuffer pixels, filled black — a repaint follows.
   * Echoed back as `resized` once applied, which is what lets the page hold
   * its layout state until the bitmap that state describes is real.
   */
  | { type: "resize"; w: number; h: number; seq: number }
  | { type: "videoFormat"; stream: number; format: VideoFormat }
  /** The attachment boundary: wipe the bitmap, the caches and the decoders. */
  | { type: "clear" };

/** What the worker sends back: the painter's callbacks and the resize echo. */
export type PainterEvent =
  | { type: "cacheReset" }
  | { type: "videoError"; reason: string | null }
  | {
      type: "painted";
      sequence: number;
      generation: number;
      queuedMs: number;
      drawMs: number;
    }
  | { type: "resized"; seq: number };

export interface PainterHost {
  handle(command: PainterCommand): void;
}

/**
 * Build the worker's command handler around `post`, which delivers events back
 * to the page. `makePainter` is injectable so the ordering contract is testable
 * without an `OffscreenCanvas` runtime; the worker entry passes nothing.
 */
export function createPainterWorker(
  post: (event: PainterEvent) => void,
  makePainter: typeof createTilePainter = createTilePainter,
  now: () => number = () => performance.now(),
): PainterHost {
  let canvas: OffscreenCanvas | null = null;
  let ctx: OffscreenCanvasRenderingContext2D | null = null;
  let painter: TilePainter | null = null;

  // The draw-ordered chain. The catch keeps a garbled frame from stalling it.
  let queue: Promise<void> = Promise.resolve();
  const queued = (task: () => void | Promise<void>) => {
    queue = queue.then(task).catch(() => {});
  };

  return {
    handle(command) {
      switch (command.type) {
        case "init":
          canvas = command.canvas;
          // `alpha: false` for the same reason the element's context asked for
          // it: the framebuffer is opaque. `desynchronized` does not come
          // along — it named the element context's present path, and a
          // transferred canvas presents through the browser's commit instead.
          ctx = canvas.getContext("2d", { alpha: false });
          painter = makePainter({
            context: () => ctx,
            onCacheReset: () => post({ type: "cacheReset" }),
            onVideoError: (reason) => post({ type: "videoError", reason }),
          });
          break;
        case "frame": {
          const queuedAt = now();
          // The kind is still read rather than assumed: a batch parser handed
          // anything else would spend its way through the bytes looking for
          // tile records. Only a real batch earns an acknowledgment.
          queued(async () => {
            if (binaryFrameKind(command.data) !== "batch") {
              return;
            }
            const startedAt = now();
            await painter?.draw(command.data);
            const finishedAt = now();
            const milliseconds = (value: number) =>
              Math.min(0xffffffff, Math.max(0, Math.round(value)));
            post({
              type: "painted",
              sequence: command.sequence,
              generation: command.generation,
              queuedMs: milliseconds(startedAt - queuedAt),
              drawMs: milliseconds(finishedAt - startedAt),
            });
          });
          break;
        }
        case "resize":
          queued(() => {
            if (canvas && ctx) {
              canvas.width = command.w;
              canvas.height = command.h;
              ctx.fillStyle = "#000";
              ctx.fillRect(0, 0, command.w, command.h);
            }
            // Echoed even with no canvas: the page's layout state must not
            // wait forever on a bitmap that cannot exist.
            post({ type: "resized", seq: command.seq });
          });
          break;
        case "videoFormat":
          queued(() => painter?.setVideoFormat(command.stream, command.format));
          break;
        case "clear":
          // In the chain like everything else — see the module comment.
          // Zeroing the bitmap is what `clearDesktop` did to the element
          // directly when it could reach it.
          queued(() => {
            painter?.clear();
            if (canvas) {
              canvas.width = 0;
              canvas.height = 0;
            }
          });
          break;
      }
    },
  };
}
