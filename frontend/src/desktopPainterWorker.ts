// The worker half of the desktop paint path.
//
// The page transfers each binary frame here, and this module runs
// `createTilePainter` — slot table, decoded bitmap cache, `VideoDecoder` table,
// batch draw loop — against an `OffscreenCanvas` handed over once.
//
// **What the boundary buys is not the decoding.** `createImageBitmap` and
// `VideoDecoder` hand their work to the browser's own threads wherever they are
// called from, so moving them here makes them no faster and takes nothing off the
// main thread that was ever really on it. What does move is the batch parse, the
// ordering glue, and a batch's worth of `drawImage` calls — and, the part that
// earns the boundary, presentation: a transferred canvas commits from this thread,
// so a frame reaches the screen without the main thread being scheduled at all.
// That thread carries input and React, and a remote desktop is largely how quickly
// those two answer.
//
// **Ordering is the contract this file keeps, and it is nearly the whole of it.**
// On the main thread, draws and the control effects that must hold their place
// behind them (a resize resets the canvas; a format change drops a decoder
// that queued units still need) rode one promise chain. Here the wire's order
// arrives as message order — the page posts commands as they land on the socket —
// and one chain preserves it across the async decodes.
//
// **`clear` is the exception, and it has to be.** It is the attachment boundary,
// and the one thing it must survive is the chain itself being stuck: a draw that
// never finishes is exactly when a session needs ending, and a clear queued behind
// one never runs at all. The next target then arrives connected and waiting forever
// on a resize echo that is parked behind the same stuck draw. So a clear runs at
// once and starts a fresh chain, and two fences keep the attachment it ended from
// reaching the next one's canvas: `epoch` here drops commands queued before it, and
// the painter's own generation drops the decodes already in flight inside it. It is
// also the cure and not only the escape — closing the decoders settles every access
// unit the stuck draw is holding, so the chain it abandoned unwedges behind it.
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
  /** A stream's decoder was reset or thrown away, and needs a keyframe to resume. */
  | { type: "videoNeedsKeyframe"; reason: string }
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
  // Which attachment the queued commands belong to, bumped by `clear`. A command
  // queued before the boundary is not merely late by the time its turn comes, it is
  // for a desktop that is gone — and after a clear it may be sharing the canvas with
  // the next attachment's commands, since the clear did not wait for it.
  let epoch = 0;
  const queued = (task: () => void | Promise<void>) => {
    const born = epoch;
    queue = queue
      .then(() => {
        if (born === epoch) {
          return task();
        }
      })
      .catch(() => {});
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
            onVideoNeedsKeyframe: (reason) =>
              post({ type: "videoNeedsKeyframe", reason }),
          });
          break;
        case "frame": {
          const queuedAt = now();
          // Read here rather than after the draw: what decides whether this batch
          // still means anything is the attachment it arrived on.
          const born = epoch;
          // The kind is still read rather than assumed: a batch parser handed
          // anything else would spend its way through the bytes looking for
          // tile records. Only a real batch earns an acknowledgment.
          queued(async () => {
            if (binaryFrameKind(command.data) !== "batch") {
              return;
            }
            const startedAt = now();
            await painter?.draw(command.data);
            if (born !== epoch) {
              // A draw the clear did not wait for, landing after it. The painter's
              // own generation already kept its pixels off the new canvas; what is
              // left is not to acknowledge a batch on behalf of a session that has
              // ended.
              return;
            }
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
          // Out of the chain, and starting a new one — see the module comment.
          // Zeroing the bitmap is what `clearDesktop` did to the element
          // directly when it could reach it.
          epoch += 1;
          queue = Promise.resolve();
          painter?.clear();
          if (canvas) {
            canvas.width = 0;
            canvas.height = 0;
          }
          break;
      }
    },
  };
}
