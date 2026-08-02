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

// Where the app writes. Relative when this page is served by the app's own
// listener; absolute via `?frames=` when it is served by Vite for layout work.
const framesUrl =
  new URLSearchParams(location.search).get("frames") ?? "./frames";

readStream(framesUrl, {
  onOpen: () => {
    postToApp({
      type: "ready",
      secureContext: window.isSecureContext,
      audioDecoder: typeof AudioDecoder !== "undefined",
    });
  },
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
// pointer is sized through it.
window.addEventListener("resize", paintCursor);
