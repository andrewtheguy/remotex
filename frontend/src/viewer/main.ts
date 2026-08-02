// The macOS viewer's remote surface.
//
// Everything this page does is downstream of one stream and upstream of one
// message handler; it owns no session, opens no gateway socket and holds no
// protocol state beyond the tile slot table the wire tells it to keep.

import {
  applyCursorCss,
  cursorImage,
  type RemoteCursor,
} from "../cursorCss.ts";
import { desktopCanvasGeometry } from "../desktopCanvas.ts";
import { binaryFrameKind } from "../protocol.ts";
import { createTilePainter } from "../tilePainter.ts";
import { createViewerAudio } from "./audio.ts";
import {
  type CanvasCommand,
  ENVELOPE_CONTROL,
  ENVELOPE_FRAME,
  postToApp,
} from "./bridge.ts";
import { attachInput } from "./input.ts";
import { readStream } from "./stream.ts";
import "./viewer.css";

interface RemoteSize {
  w: number;
  h: number;
  /**
   * How many framebuffer pixels the remote draws per point of its own desktop:
   * 1 for VNC, RDP and a 1x Mac, 2 for a Retina one. Here rather than alongside
   * because neither number presents the desktop without the other.
   */
  scale: number;
}

const canvas = document.getElementById("framebuffer") as HTMLCanvasElement;
const overlay = document.getElementById("overlay") as HTMLElement;

let size: RemoteSize | null = null;
let context: CanvasRenderingContext2D | null = null;
let cursor: RemoteCursor | null = null;
let inputEnabled = false;

const painter = createTilePainter({
  context: () => context,
  onCacheReset: () => postToApp({ type: "cacheReset" }),
});

const audio = createViewerAudio({
  onState: (playing, error) =>
    postToApp({ type: "audioState", playing, error }),
});

attachInput({
  overlay,
  canvas,
  size: () => size,
  enabled: () => inputEnabled,
});

// Push the pointer state to the DOM. The overlay wears the shape as a CSS
// cursor, which tracks the hardware pointer with no lag; there is no virtual
// pointer here, because there is no touch input to drive one.
function paintCursor() {
  const image = cursorImage(cursor);
  if (!image) {
    overlay.style.cursor = "none";
    return;
  }
  const rect = canvas.getBoundingClientRect();
  // The desktop's on-screen scale, framebuffer pixels to CSS pixels: the
  // pointer is sized through it. 1:1 until the first resize names a size.
  const view = size && size.w > 0 && rect.width > 0 ? rect.width / size.w : 1;
  applyCursorCss(overlay, image, view);
}

function resize(w: number, h: number, scale: number) {
  const next: RemoteSize = { w, h, scale: scale > 0 ? scale : 1 };
  const { bitmap, layout } = desktopCanvasGeometry(next, next.scale);
  canvas.width = bitmap.w;
  canvas.height = bitmap.h;
  canvas.style.width = `${layout.w}px`;
  canvas.style.height = `${layout.h}px`;
  // A fresh bitmap starts transparent, and the window behind it is not black on
  // every macOS appearance.
  context = canvas.getContext("2d");
  if (context) {
    context.fillStyle = "#000";
    context.fillRect(0, 0, bitmap.w, bitmap.h);
  }
  size = next;
  paintCursor();
  // The desktop's size just changed, so the room may have too — a bar that was
  // needed a moment ago may not be now, or the other way round.
  reportGeometry();
}

// Drop any retained framebuffer. The desktop is always rebuilt from a full
// repaint on the next attachment, so holding stale pixels would only flash on
// the way back.
function clear() {
  size = null;
  canvas.width = 0;
  canvas.height = 0;
  canvas.style.width = "0px";
  canvas.style.height = "0px";
  context = null;
  // Pointer ownership is per-engine: the next target may composite its own
  // cursor, so drop back to hiding the browser's until it says otherwise.
  cursor = null;
  painter.clear();
  audio.stop();
  paintCursor();
}

function apply(command: CanvasCommand) {
  switch (command.type) {
    case "resize":
      resize(command.w, command.h, command.scale);
      break;
    case "clear":
      clear();
      break;
    case "cursor":
      cursor = {
        image: command.image
          ? {
              url: `data:image/png;base64,${command.image}`,
              hx: command.hx,
              hy: command.hy,
              w: command.w,
              h: command.h,
            }
          : null,
      };
      paintCursor();
      break;
    case "audioFormat":
      audio.start(command);
      break;
    case "audioStop":
      audio.stop();
      break;
    case "input":
      inputEnabled = command.enabled;
      break;
  }
}

// Tiles decode asynchronously (`createImageBitmap`), so every message is chained
// through one promise queue: draws land in arrival order, and a resize cannot
// jump ahead of tiles drawn in the coordinate space it replaces. The catch keeps
// a garbled frame from stalling the chain.
let queue: Promise<void> = Promise.resolve();

// Where the app writes. Always relative to wherever this page was served from,
// which under the app's own listener is the stream beside it.
//
// `?frames=` overrides it in a development build only. It exists for
// `REMOTEX_VIEWER_DEV_URL`, where Vite serves the page and the stream is on
// another origin, and a shipped page has no business taking the address it
// reads frames from out of its own URL.
const framesUrl =
  (import.meta.env.DEV
    ? new URLSearchParams(location.search).get("frames")
    : null) ?? "./frames";

const screen = document.querySelector(".screen") as HTMLElement;

/**
 * Make the scroll bars decide again from nothing.
 *
 * They take their width out of the content box, so once both are up the box is
 * 15px smaller each way and a desktop that would now fit exactly still
 * overflows *that* — each bar is the reason the other is needed, and neither
 * lets go. Resizing the window to the desktop's own size is precisely the case
 * that lands there, so "Resize to Display" left the bars up and the desktop
 * clipped, while the same size reached from a larger window was fine.
 *
 * Dropping to `overflow: hidden` and forcing a layout removes both bars, so the
 * measurement that follows is of the full box; going back to the stylesheet's
 * `auto` then asks the honest question — does the content fit in the box with
 * no bars — and answers it correctly in both directions.
 */
function settleScrollbars() {
  screen.style.overflow = "hidden";
  void screen.offsetWidth; // forces the layout that drops the bars
  screen.style.overflow = "";
}

// What the app cannot measure for itself: the room left for the desktop once
// the scroll bars have taken their width, and what the desktop is actually
// laid out at. Sent on every attachment and after every layout change, because
// "Resize to Display" fits to the first and is wrong whenever the two differ.
function reportGeometry() {
  settleScrollbars();
  postToApp({
    type: "ready",
    secureContext: window.isSecureContext,
    audioDecoder: typeof AudioDecoder !== "undefined",
    room: { w: screen.clientWidth, h: screen.clientHeight },
    content: { w: screen.scrollWidth, h: screen.scrollHeight },
  });
}

readStream(framesUrl, {
  onOpen: reportGeometry,
  onEnvelope: (envelope) => {
    queue = queue
      .then(async () => {
        if (envelope.kind === ENVELOPE_CONTROL) {
          apply(JSON.parse(new TextDecoder().decode(envelope.payload)));
          return;
        }
        if (envelope.kind !== ENVELOPE_FRAME) {
          return;
        }
        // A copy already, from the reader — so its buffer is exactly this frame
        // and can be handed to the parsers as one.
        const frame = envelope.payload.buffer as ArrayBuffer;
        switch (binaryFrameKind(frame)) {
          case "batch":
            await painter.draw(frame);
            break;
          case "audio":
            // Not awaited inside: the packets go to WebCodecs, which decodes
            // off-thread, so what queues here is a copy rather than a decode.
            audio.play(frame);
            break;
          default:
            break;
        }
      })
      .catch(() => {});
  },
});

// The desktop's on-screen scale changes with the window's display, and the
// pointer is sized through it. The app also wants the new room: it fits the
// window to what is left for the desktop, which only this side can measure.
window.addEventListener("resize", () => {
  paintCursor();
  reportGeometry();
});
