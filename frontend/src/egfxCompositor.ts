// The page's compositor for an RDP host's graphics pipeline.
//
// In a session started with the pipeline passed the gateway does not compose the
// desktop and encode it: it passes the host's drawing commands on, and this page
// composes them.
// What does the composing is the gateway's own compositor and codecs, compiled to
// WebAssembly (frontend/wasm/egfx, around the crate the gateway composes with), so
// there is one reading of the protocol and not two.
//
// A compositor is right only for a pipeline it has followed from its first
// command: the host draws against what its client already holds — surfaces, cache
// slots, each codec's own caches. So one is made at every `graphicsStart` and never
// carried past the next, and one that has refused a command is not fed again.
//
// Progressive's tiles are decoded side by side, on threads: workers this one
// starts, each an instance of the module on the one memory (egfxPool.worker.ts).
// A memory that is shared needs a page that is cross-origin isolated, which the
// gateway's two headers make of every page it serves (src/assets.rs).
//
// H.264 is the one codec the module does not decode (EXPERIMENTAL, rdpH264.ts): a
// run's access units are found by `scan`, decoded by the browser (egfxVideo.ts),
// and each picture's samples copied into the module's memory by `supply`, before
// the run is composed and paints them.
//
// The framebuffer stays in the module's memory, and a canvas takes no image data
// out of a shared one; a WebGL texture takes an upload from one. So `pixels` is a
// view on the framebuffer where it is, good until the next `compose`, and the
// painted rectangles are uploaded out of it into the pipeline's picture
// (egfxPicture.ts).
import init, {
  module as compiled,
  Egfx,
  type InitInput,
  startPool,
} from "../wasm/egfx/pkg/remotex_egfx.js";
import type { PoolSeat } from "./egfxPool.worker.ts";

/**
 * The most threads the tiles are decoded on. Past four the captured desktop this
 * was measured with composed no sooner: what is left is what one thread does.
 */
const MOST_THREADS = 4;

/** What one run of commands did to the picture. */
export interface ComposedRun {
  /**
   * The rectangles the run painted: `x, y, width, height` for each, those that
   * share a row band and touch along it merged into one.
   */
  painted: Uint32Array;
  /** The framebuffer's size, in its own pixels. Zero before the first reset. */
  width: number;
  height: number;
  /**
   * Whether the run reset the output: the framebuffer is blank but for what the
   * run painted after, at a size that may be the same.
   */
  resized: boolean;
  /**
   * The whole picture, RGBX with the fourth byte unused, top row first: a view on
   * the module's shared memory, good until the next `compose`.
   */
  pixels: Uint8ClampedArray;
}

/** The part of a picture, in its own pixels, right and bottom exclusive. */
export interface PictureWindow {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

/** One H.264 access unit in a run of commands. */
export interface H264Unit {
  /** The surface whose stream it belongs to. */
  surface: number;
  /** Where its bytes are in the commands, Annex B. */
  start: number;
  end: number;
  /** Whether a decoder can start at it. */
  key: boolean;
  /**
   * The codec string its parameter sets name, `avc1.` and their profile,
   * constraint and level bytes; null for a unit that carries none.
   */
  codec: string | null;
  /**
   * How much of its picture the compositor paints from: a part, the whole, or
   * null for none. A unit that shows nothing is still part of its stream.
   */
  window: PictureWindow | "whole" | null;
}

/** What a scan finds in a run, in command order. */
export type Scanned =
  /** An access unit, and its number in the run, which `supply` takes. */
  | { unit: H264Unit; number: number }
  /** A surface was created or deleted: its stream is over. */
  | { gone: number };

/**
 * What `supply` needs of a decoded picture, which is a `VideoFrame`'s own: the
 * samples' layout, where the picture lies in what was coded, and a copy of a
 * part of it.
 */
export type DecodedFrame = Pick<
  VideoFrame,
  "format" | "visibleRect" | "allocationSize" | "copyTo"
>;

export interface EgfxCompositor {
  /**
   * Find the H.264 access units in one GRAPHICS record's commands, and the
   * surfaces whose streams end in it. Throws for a command that does not decode,
   * as `compose` would.
   */
  scan(commands: Uint8Array): Scanned[];
  /**
   * Hand over the decoded picture of unit `number` of the record composed next:
   * the samples its window names, copied into the module's memory. Throws for a
   * picture laid out in a way the compositor does not read.
   */
  supply(
    number: number,
    window: H264Unit["window"],
    frame: DecodedFrame,
  ): Promise<void>;
  /**
   * Compose one GRAPHICS record's commands. Throws for a command that does not
   * decode, after which this compositor is no longer the host's picture of its
   * client.
   */
  compose(commands: Uint8Array): ComposedRun;
  /**
   * The picture as composed so far: a view on the framebuffer where it is, good
   * until the next `compose`, and of nothing before the first reset or after
   * `close`.
   */
  picture(): Picture;
  /** Give the compositor's memory back. */
  close(): void;
}

/** The whole picture: `width` by `height` RGBX pixels, top row first. */
export interface Picture {
  width: number;
  height: number;
  pixels: Uint8ClampedArray;
}

/** Makes a compositor with nothing in it, for a pipeline that is starting. */
export type EgfxFactory = () => EgfxCompositor;

let loaded: Promise<EgfxFactory> | null = null;

/** The pool's workers, held for as long as the pool is: the page's lifetime. */
const workers: Worker[] = [];

/**
 * Start the pool: `threads` workers, each told what this instance is and heard
 * from once its own is made, and then the pool made of them. The pool is made last
 * because making it waits for its threads, and a worker does not start while the
 * one that made it waits.
 */
async function startThreads(
  memory: WebAssembly.Memory,
  threads: number,
): Promise<void> {
  const seat: PoolSeat = { module: compiled(), memory };
  try {
    await Promise.all(
      Array.from(
        { length: threads },
        () =>
          new Promise<void>((resolve, reject) => {
            const worker = new Worker(
              new URL("./egfxPool.worker.ts", import.meta.url),
              { type: "module", name: "egfx-pool" },
            );
            workers.push(worker);
            worker.onmessage = ({ data }: MessageEvent<string | null>) => {
              if (data === null) {
                resolve();
              } else {
                reject(new Error(data));
              }
            };
            worker.onerror = (event) => {
              reject(new Error(event.message || "a thread did not start"));
            };
            // Throws for a memory that cannot be shared, which is a page that is
            // not cross-origin isolated.
            worker.postMessage(seat);
          }),
      ),
    );
  } catch (error) {
    for (const worker of workers.splice(0)) {
      worker.terminate();
    }
    throw error;
  }
  startPool(threads);
}

/**
 * The rectangles a run painted, with those that share a row band and touch or
 * overlap along it merged into one. A Progressive frame is reported tile by tile,
 * and each rectangle costs an upload and a draw, whatever its width.
 */
export function coalesce(painted: Uint32Array): Uint32Array {
  const rects: number[][] = [];
  for (let i = 0; i + 3 < painted.length; i += 4) {
    rects.push([painted[i], painted[i + 1], painted[i + 2], painted[i + 3]]);
  }
  rects.sort((a, b) => a[1] - b[1] || a[3] - b[3] || a[0] - b[0]);
  const merged: number[][] = [];
  for (const rect of rects) {
    const last = merged[merged.length - 1];
    if (
      last &&
      last[1] === rect[1] &&
      last[3] === rect[3] &&
      rect[0] <= last[0] + last[2]
    ) {
      last[2] = Math.max(last[0] + last[2], rect[0] + rect[2]) - last[0];
    } else {
      merged.push(rect);
    }
  }
  return Uint32Array.from(merged.flat());
}

/** How many numbers the module says each thing a scan found in. */
const SCAN_FIELDS = 10;
/** The window the module names for the whole of a picture. */
const WHOLE = 0xffffffff;

/** What the module's scan found, read out of its numbers (`Egfx::units`). */
function scanned(fields: Uint32Array): Scanned[] {
  const found: Scanned[] = [];
  let number = 0;
  for (let at = 0; at + SCAN_FIELDS <= fields.length; at += SCAN_FIELDS) {
    const [kind, surface, start, end, key, profile, left, top, right, bottom] =
      fields.subarray(at, at + SCAN_FIELDS);
    if (kind === 1) {
      found.push({ gone: surface });
      continue;
    }
    let window: H264Unit["window"] = { left, top, right, bottom };
    if (left === WHOLE) {
      window = "whole";
    } else if (right === 0) {
      window = null;
    }
    // `0x01PPCCLL`: the marker byte, then the three a codec string is made of.
    const codec =
      profile === 0
        ? null
        : `avc1.${(profile & 0xffffff).toString(16).padStart(6, "0")}`;
    found.push({
      unit: { surface, start, end, key: key === 1, codec, window },
      number,
    });
    number += 1;
  }
  return found;
}

/**
 * The part of a decoded picture a window names, widened to whole chroma samples
 * and kept to the picture; null when nothing of it is inside.
 */
function windowRect(
  window: PictureWindow | "whole",
  visible: DOMRectReadOnly,
): PictureWindow | null {
  if (window === "whole") {
    return { left: 0, top: 0, right: visible.width, bottom: visible.height };
  }
  const left = window.left & ~1;
  const top = window.top & ~1;
  const right = Math.min(window.right + (window.right & 1), visible.width);
  const bottom = Math.min(window.bottom + (window.bottom & 1), visible.height);
  return left < right && top < bottom ? { left, top, right, bottom } : null;
}

/**
 * The module, fetched and compiled once for the page, its threads started, and
 * what makes compositors out of it. `source` is the module's bytes for a runtime
 * with nothing to fetch from, which is a test; so is a `threads` of zero, for one
 * with no workers to start, where the module composes on the thread that calls it.
 */
export function loadEgfx(
  source?: InitInput,
  threads = Math.min(navigator.hardwareConcurrency, MOST_THREADS),
): Promise<EgfxFactory> {
  loaded ??= init(source === undefined ? undefined : { module_or_path: source })
    .then(async ({ memory }) => {
      if (threads > 0) {
        await startThreads(memory, threads);
      }
      return () => {
        const egfx = new Egfx();
        // The module's memory is given back by `close`, and not from under a copy
        // the browser is still writing into it.
        let closed = false;
        let copying: Promise<unknown> | null = null;
        const picture = (): Picture => {
          if (closed) {
            return { width: 0, height: 0, pixels: new Uint8ClampedArray(0) };
          }
          const width = egfx.width();
          const height = egfx.height();
          return {
            width,
            height,
            pixels: new Uint8ClampedArray(
              memory.buffer,
              egfx.pixels(),
              width * height * 4,
            ),
          };
        };
        return {
          scan(commands: Uint8Array): Scanned[] {
            return scanned(egfx.units(commands));
          },
          async supply(
            number: number,
            window: H264Unit["window"],
            frame: DecodedFrame,
          ): Promise<void> {
            const visible = frame.visibleRect;
            const part = window && visible && windowRect(window, visible);
            if (!part) {
              return;
            }
            const format = frame.format;
            if (format !== "I420" && format !== "NV12") {
              throw new Error(`its H.264 decoder's pictures are ${format}`);
            }
            const rect = {
              x: visible.x + part.left,
              y: visible.y + part.top,
              width: part.right - part.left,
              height: part.bottom - part.top,
            };
            const size = frame.allocationSize({ rect });
            // The room first: making it may grow the memory, and the buffer read
            // before that is the memory as it was.
            const at = egfx.reserve(size);
            const room = new Uint8Array(memory.buffer, at, size);
            const copy = frame.copyTo(room, { rect });
            copying = copy.catch(() => {});
            const layout = await copy;
            copying = null;
            if (closed) {
              return;
            }
            egfx.supply(
              number,
              part.left,
              part.top,
              rect.width,
              rect.height,
              Uint32Array.from(
                layout.flatMap((plane) => [plane.offset, plane.stride]),
              ),
            );
          },
          compose(commands: Uint8Array): ComposedRun {
            egfx.compose(commands);
            return {
              painted: coalesce(egfx.painted()),
              resized: egfx.resized(),
              ...picture(),
            };
          },
          picture,
          close() {
            closed = true;
            if (copying) {
              void copying.then(() => egfx.free());
            } else {
              egfx.free();
            }
          },
        };
      };
    })
    .catch((error: unknown) => {
      // A load that failed is tried again by the next pipeline: the page may have
      // been offline for the one fetch, and nothing else would ever retry it.
      loaded = null;
      throw error;
    });
  return loaded;
}
