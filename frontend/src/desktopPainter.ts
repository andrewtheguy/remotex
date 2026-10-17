// The page's handle on the desktop paint worker.
//
// The parse→decode→paint path runs in a worker (desktopPainterWorker.ts)
// drawing on an `OffscreenCanvas`, which keeps painting and its presentation off
// the thread input and React are on — that module says what the boundary does and
// does not buy, since it is less than the word "decode" suggests. This module is
// the main-thread side:
// it transfers each binary frame over — `postMessage` with a transfer list
// moves the buffer, it does not copy it — and carries the painter's callbacks
// and ordered completion feedback back as messages.
//
// One worker per canvas *element*, held at module level like the pointer rect
// cache, and for a harder reason than tidiness: `transferControlToOffscreen`
// works exactly once per element, and the connection effect that wants a
// painter runs more than once per element — StrictMode reruns it on the same
// canvas. So the worker outlives the effect, the effect binds and unbinds its
// handlers, and only a *new* canvas element replaces the worker — at which point
// the old one holds the bitmap of an element that is gone, and is terminated
// rather than left decoding for it.
//
// A session's end is where what its picture alone loaded is let go, since a
// WebAssembly memory grows and never shrinks and only ending the worker that holds
// it gives it back (`endSession`). The compositor of an RDP host's pipeline and the
// decoder of a Mac's own picture are modules of this worker's own, so a
// session that loaded either ends the worker, and the page mounts new canvases for
// the next one. Otherwise the worker is kept, and its decode worker, the HEVC's,
// goes.
import { decodesAppleMedia } from "./appleMedia.ts";
import type { PainterCommand, PainterEvent } from "./desktopPainterWorker.ts";
import type { PicturePart } from "./egfxPicture.ts";
import type { MosaicView } from "./mosaic.ts";
import { batchFrameSequence, type StandardQuality } from "./protocol.ts";
import { runnableDecoders } from "./softwareSupport.ts";
import type { VideoFormat } from "./videoDecoder.ts";

export interface PainterHandlers {
  /** Why the desktop shows nothing, or null once it shows something. */
  onVideoError: (reason: string | null) => void;
  /**
   * The decoder was reset out of a stall, or failed and was thrown away. Either
   * way the desktop is frozen until a keyframe arrives, and
   * asking for one is the page's job because only it holds the socket.
   */
  onVideoNeedsKeyframe: (reason: string) => void;
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
  /** Everything posted before the `mark` of this number has been drawn. */
  onReached: (seq: number) => void;
}

export interface DesktopPainter {
  /** Point the worker's events at this attachment's handlers. */
  bind(handlers: PainterHandlers): void;
  /** Drop the handlers; the worker idles until the next bind. */
  unbind(): void;
  /** Hand one binary socket frame to the worker. Transfers the buffer. */
  draw(frame: ArrayBuffer, generation: number): void;
  /**
   * Set the framebuffer (its pixels) and fill it black. With a `view`, the
   * framebuffer is held off screen and the canvas shows that composition.
   */
  resize(
    bitmap: { w: number; h: number },
    seq: number,
    view: MosaicView | null,
  ): void;
  /** Recompose what is already painted under `view`, or show it whole. */
  setView(view: MosaicView | null, seq: number): void;
  /** Ask for `onReached(seq)` once every frame handed over so far is drawn. */
  mark(seq: number): void;
  setVideoFormat(format: VideoFormat): void;
  /** An RDP host's graphics pipeline starts, for the worker to compose. */
  startGraphics(): void;
  /** A Mac's own picture starts, in `quality`, for the worker to decode. */
  startStandard(quality: StandardQuality): void;
  /** The part of the pipeline's picture this page's display is (`graphicsView`). */
  setGraphicsView(part: PicturePart): void;
  /**
   * This page shows display `display` in a tab of its own, `part` of the picture
   * the session's page composes, and is painted from there (displayRelay.ts).
   */
  mirrorGraphics(display: number, part: PicturePart): void;
  /** The attachment boundary: wipe the bitmap and the decoder. */
  clear(): void;
  /**
   * The session is over, after its `clear`: let go of what its picture alone
   * loaded. True when that was the worker itself, whose canvases go with it: the
   * page mounts new ones before it paints again, and this painter takes nothing
   * more.
   */
  endSession(): boolean;
}

let current: {
  canvas: HTMLCanvasElement;
  worker: Worker;
  painter: DesktopPainter;
} | null = null;

// Canvases whose worker a session's end took: transferred, so no other worker
// can draw on them.
const spent = new WeakSet<HTMLCanvasElement>();

/**
 * The painter for the page's one desktop canvas, built on first ask. `graphics`
 * is the canvas laid over it for what is drawn on the GPU, an RDP host's graphics
 * pipeline and a software decoder's pictures: the worker draws them there,
 * and says when it is to be shown. Null for a canvas a session's end has spent,
 * until the page mounts its replacement.
 */
export function desktopPainterFor(
  canvas: HTMLCanvasElement,
  graphics: HTMLCanvasElement,
): DesktopPainter | null {
  if (current?.canvas === canvas) {
    return current.painter;
  }
  if (spent.has(canvas)) {
    return null;
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
  // How many `clear`s have been posted, which is the worker's epoch once it has
  // taken them all. The GPU's canvas is hidden where a clear is posted, not
  // where the worker answers it, and a `graphicsShown` the worker said before it
  // took that clear is for the attachment the clear ended.
  let clears = 0;
  const showGraphics = (shown: boolean) => {
    graphics.style.display = shown ? "block" : "";
  };
  worker.onmessage = (ev: MessageEvent<PainterEvent>) => {
    const event = ev.data;
    if (event.type === "graphicsShown") {
      if (event.epoch === clears) {
        showGraphics(event.shown);
      }
    } else if (event.type === "videoError") {
      handlers?.onVideoError(event.reason);
    } else if (event.type === "videoNeedsKeyframe") {
      handlers?.onVideoNeedsKeyframe(event.reason);
    } else if (event.type === "resized") {
      handlers?.onResized(event.seq);
    } else if (event.type === "reached") {
      handlers?.onReached(event.seq);
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
  // Whether a session has loaded a module of this worker's own: the pipeline's
  // compositor or the decoder of a Mac's own picture.
  let ownModule = false;
  const offscreen = canvas.transferControlToOffscreen();
  const offscreenGraphics = graphics.transferControlToOffscreen();
  post(
    {
      type: "init",
      canvas: offscreen,
      graphics: offscreenGraphics,
      decoding: {
        runs: runnableDecoders(),
        nativeHevc: decodesAppleMedia(),
      },
    },
    [offscreen, offscreenGraphics],
  );
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
    resize(bitmap, seq, view) {
      post({ type: "resize", w: bitmap.w, h: bitmap.h, seq, view });
    },
    setView(view, seq) {
      post({ type: "view", view, seq });
    },
    mark(seq) {
      post({ type: "mark", seq });
    },
    setVideoFormat(format) {
      post({ type: "videoFormat", format });
    },
    startGraphics() {
      ownModule = true;
      post({ type: "graphicsStart" });
    },
    startStandard(quality) {
      ownModule = true;
      post({ type: "standardStart", quality });
    },
    setGraphicsView(part) {
      post({ type: "graphicsView", part });
    },
    mirrorGraphics(display, part) {
      post({ type: "graphicsMirror", display, part });
    },
    clear() {
      clears += 1;
      showGraphics(false);
      post({ type: "clear" });
    },
    endSession() {
      if (!ownModule) {
        post({ type: "sessionEnded" });
        return false;
      }
      // Its decode workers and the compositor's threads are its own, and end
      // with it.
      worker.terminate();
      handlers = null;
      spent.add(canvas);
      spent.add(graphics);
      if (current?.worker === worker) {
        current = null;
      }
      return true;
    },
  };
  current = { canvas, worker, painter };
  return painter;
}
