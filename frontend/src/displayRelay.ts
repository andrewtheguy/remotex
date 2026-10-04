// The second display's picture, in a session started with an RDP host's graphics
// pipeline passed.
//
// The host draws its two virtual displays as one output, the second against the first, through one
// pipeline whose state they share: the cache slots and the copies between surfaces
// cross from one to the other, so the pipeline cannot be dealt out by display. The
// page that holds the session composes the whole of it, once, and shows its own
// display's column (egfxPicture.ts). The tab showing the second display
// (`/display/2`) composes nothing. Its display socket tells it which column of the
// picture it is (`graphicsView`), and this module hands that column across the
// browser, from the session page's paint worker to the tab's, over a
// BroadcastChannel of the gateway's origin: a copy of the pixels that changed, and
// nothing decoded twice. The gateway holds nothing of a passed pipeline to paint
// the tab from, and a compositor in each tab fed the same commands would decode
// everything twice.
//
// Two ends. The source is the session page's: told what each run painted, it
// keeps what the tab has not been sent yet and sends it once the tab has painted
// the last — one update in flight at a time, out of the picture as it stands when
// it is sent, so a tab slower than the host sees the latest picture and never a
// queue of old ones. The sink is the tab's: it says which part it shows, paints
// what it is sent, and says when it has. Either end may be there first: a source
// that starts asks, and a sink answers with what it shows.
import { coalesce, type Picture } from "./egfxCompositor.ts";
import type { PicturePart } from "./egfxPicture.ts";

/** The channel both ends open, named for the one thing it carries. */
export const RELAY_CHANNEL = "remotex-display-relay";

/**
 * How many rectangles an update names at most: past this they are sent as the one
 * rectangle around them all, a bound on what waits for a slow tab.
 */
const MOST_RECTS = 64;

/** What the two ends say to each other. */
export type RelayMessage =
  /** The source: a pipeline is composed here; a tab showing part of it says so. */
  | { kind: "composing" }
  /** The sink: this tab shows display `display`, which is `part` of the picture. */
  | { kind: "shown"; display: number; part: PicturePart }
  /**
   * The source: rectangles of the part, `x, y, width, height` each and relative to
   * it, with each one's RGBX rows in turn in `pixels`; `w` by `h` is the part's
   * size. `seq` is answered with a `painted`.
   */
  | {
      kind: "paint";
      seq: number;
      w: number;
      h: number;
      rects: number[];
      pixels: ArrayBuffer;
    }
  /** The sink: the update `seq` is on screen, and the next may come. */
  | { kind: "painted"; seq: number };

/** One end of the channel. */
export interface RelayPort {
  post(message: RelayMessage): void;
  /** Hand each message from the other end to `handler`; one handler at a time. */
  onMessage(handler: (message: RelayMessage) => void): void;
  close(): void;
}

/** The channel itself, which a page's workers reach as the page does. */
export function openRelayPort(): RelayPort {
  const channel = new BroadcastChannel(RELAY_CHANNEL);
  return {
    post: (message) => channel.postMessage(message),
    onMessage: (handler) => {
      channel.onmessage = (event: MessageEvent<RelayMessage>) =>
        handler(event.data);
    },
    close: () => channel.close(),
  };
}

/** The session page's end: what its pipeline paints, for the tab. */
export interface RelaySource {
  /**
   * A run was composed: the rectangles it painted, or all of the picture where
   * it reset the output. Whatever of them the tab shows is owed to it.
   */
  painted(run: {
    painted: ArrayLike<number>;
    width: number;
    height: number;
    resized: boolean;
  }): void;
  /**
   * The picture is gone: the pipeline ended or another starts. Nothing is owed
   * of it any more; the next pipeline's first run says what the tab is owed.
   */
  reset(): void;
  close(): void;
}

/** `rect` within `part`, or null for nothing of it inside. */
function within(rect: PicturePart, part: PicturePart): PicturePart | null {
  const x = Math.max(rect.x, part.x);
  const y = Math.max(rect.y, part.y);
  const right = Math.min(rect.x + rect.w, part.x + part.w);
  const bottom = Math.min(rect.y + rect.h, part.y + part.h);
  return x < right && y < bottom ? { x, y, w: right - x, h: bottom - y } : null;
}

/** Whether `outer` holds the whole of `inner`. */
function contains(outer: PicturePart, inner: PicturePart): boolean {
  return (
    inner.x >= outer.x &&
    inner.y >= outer.y &&
    inner.x + inner.w <= outer.x + outer.w &&
    inner.y + inner.h <= outer.y + outer.h
  );
}

/** The one rectangle around `rects`. */
function around(rects: PicturePart[]): PicturePart {
  let left = Number.POSITIVE_INFINITY;
  let top = Number.POSITIVE_INFINITY;
  let right = 0;
  let bottom = 0;
  for (const r of rects) {
    left = Math.min(left, r.x);
    top = Math.min(top, r.y);
    right = Math.max(right, r.x + r.w);
    bottom = Math.max(bottom, r.y + r.h);
  }
  return { x: left, y: top, w: right - left, h: bottom - top };
}

/**
 * The session page's end, on `port`. `picture` is the pipeline's picture as it
 * stands, read when an update is sent — the pixels are copied then, out of the
 * compositor's memory — or null while there is none.
 */
export function createRelaySource(
  port: RelayPort,
  picture: () => Picture | null,
): RelaySource {
  // The part the tab shows, in the picture, from its last word.
  let part: PicturePart | null = null;
  // Rectangles of the picture the tab has not been sent, each within `part`.
  let owed: PicturePart[] = [];
  // The update the tab has not said it painted, or null.
  let inFlight: number | null = null;
  let seq = 0;

  // A rectangle inside one already owed adds nothing: the pixels are read when
  // the update goes out, not now.
  const owe = (rects: PicturePart[]) => {
    for (const rect of rects) {
      if (!owed.some((held) => contains(held, rect))) {
        owed.push(rect);
      }
    }
    if (owed.length > MOST_RECTS) {
      owed = [around(owed)];
    }
  };

  // Send what is owed, from the picture as it is now.
  const flush = () => {
    if (inFlight !== null || !part || owed.length === 0) {
      return;
    }
    const shown = part;
    const current = picture();
    if (!current || current.width === 0 || current.height === 0) {
      // Nothing to send it from; the first run of a picture owes it all again.
      owed = [];
      return;
    }
    const inside = {
      x: 0,
      y: 0,
      w: current.width,
      h: current.height,
    };
    const rects = owed
      .map((rect) => within(rect, inside))
      .filter((rect): rect is PicturePart => rect !== null);
    owed = [];
    if (rects.length === 0) {
      return;
    }
    const merged = coalesce(
      Uint32Array.from(rects.flatMap((r) => [r.x, r.y, r.w, r.h])),
    );
    let bytes = 0;
    for (let i = 0; i + 3 < merged.length; i += 4) {
      bytes += merged[i + 2] * merged[i + 3] * 4;
    }
    const pixels = new Uint8Array(bytes);
    const relative: number[] = [];
    let at = 0;
    const stride = current.width * 4;
    for (let i = 0; i + 3 < merged.length; i += 4) {
      const [x, y, w, h] = [
        merged[i],
        merged[i + 1],
        merged[i + 2],
        merged[i + 3],
      ];
      for (let row = y; row < y + h; row += 1) {
        const from = row * stride + x * 4;
        pixels.set(current.pixels.subarray(from, from + w * 4), at);
        at += w * 4;
      }
      relative.push(x - shown.x, y - shown.y, w, h);
    }
    seq += 1;
    inFlight = seq;
    port.post({
      kind: "paint",
      seq,
      w: shown.w,
      h: shown.h,
      rects: relative,
      pixels: pixels.buffer,
    });
  };

  port.onMessage((message) => {
    switch (message.kind) {
      case "shown":
        // A tab that says what it shows holds nothing of it yet: whatever was in
        // flight was for the part it showed before, if any.
        part = message.part;
        inFlight = null;
        owed = [part];
        flush();
        break;
      case "painted":
        if (message.seq === inFlight) {
          inFlight = null;
          flush();
        }
        break;
      default:
        break;
    }
  });
  port.post({ kind: "composing" });

  return {
    painted(run) {
      if (!part) {
        return;
      }
      if (run.resized) {
        owe([part]);
      } else {
        const rects: PicturePart[] = [];
        const painted = run.painted;
        for (let i = 0; i + 3 < painted.length; i += 4) {
          const rect = within(
            {
              x: painted[i],
              y: painted[i + 1],
              w: painted[i + 2],
              h: painted[i + 3],
            },
            part,
          );
          if (rect) {
            rects.push(rect);
          }
        }
        owe(rects);
      }
      flush();
    },
    reset() {
      owed = [];
      inFlight = null;
    },
    close() {
      port.close();
    },
  };
}

/** The tab's end: the part of the session page's picture this tab shows. */
export interface RelaySink {
  /** This tab shows display `display`, which is `part` of the picture. */
  show(display: number, part: PicturePart): void;
  close(): void;
}

/**
 * The tab's end, on `port`, painting what it is sent into `picture`. `onPainted`
 * is told after each update is on it, and `onError` why one could not be, after
 * which nothing more is painted.
 */
export function createRelaySink(
  port: RelayPort,
  picture: {
    patch(
      w: number,
      h: number,
      rects: ArrayLike<number>,
      pixels: Uint8Array,
    ): void;
  },
  onPainted: () => void,
  onError: (why: string) => void,
): RelaySink {
  let shown: { display: number; part: PicturePart } | null = null;
  let broken = false;

  const announce = () => {
    if (shown) {
      port.post({ kind: "shown", ...shown });
    }
  };

  port.onMessage((message) => {
    switch (message.kind) {
      case "composing":
        announce();
        break;
      case "paint": {
        if (broken) {
          return;
        }
        try {
          picture.patch(
            message.w,
            message.h,
            message.rects,
            new Uint8Array(message.pixels),
          );
        } catch (error) {
          broken = true;
          onError(error instanceof Error ? error.message : String(error));
          return;
        }
        port.post({ kind: "painted", seq: message.seq });
        onPainted();
        break;
      }
      default:
        break;
    }
  });

  return {
    show(display, part) {
      shown = { display, part };
      announce();
    },
    close() {
      port.close();
    },
  };
}
