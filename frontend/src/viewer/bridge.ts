// The contract between the macOS app and this page.
//
// The Swift side of it is `Sources/Canvas/CanvasCommand.swift`; the two are
// written together and ship in the same bundle, so there is no version
// negotiation here. (The pre-2203112 WKWebView shell had one — the SPA it hosted
// was served by a gateway that could be a different release. This page cannot.)
//
// Downstream — app to page — everything rides one ordered binary stream, which
// is what keeps a `resize` from overtaking tiles drawn in the coordinate space
// it replaces. Upstream is a script message handler: input events are small,
// JSON, and already ordered.

import type { MouseButton, WheelUnit } from "../protocol.ts";

/** Envelope kinds on the downstream stream. */
export const ENVELOPE_CONTROL = 0x00;
export const ENVELOPE_FRAME = 0x01;

/** App to page, as JSON inside an `ENVELOPE_CONTROL` envelope. */
export type CanvasCommand =
  // The remote's framebuffer size and its own pixel density. Both are needed to
  // lay the desktop out, so both arrive together — see docs/macos-viewer.md.
  | { type: "resize"; w: number; h: number; scale: number }
  // Drop the framebuffer and the slot table: the session detached, switched
  // target, or was taken over.
  | { type: "clear" }
  // `ServerMsg::Cursor` forwarded verbatim. `image` is a base64 PNG, null when
  // the remote hid the pointer; no message at all means the remote is drawing
  // its own and this page keeps the browser's hidden.
  | {
      type: "cursor";
      image: string | null;
      w: number;
      h: number;
      hx: number;
      hy: number;
    }
  | {
      type: "audioFormat";
      codec: string;
      sampleRate: number;
      channels: number;
      /** `OpusHead`, base64, exactly as the control message carried it. */
      head: string;
    }
  | { type: "audioStop" }
  // Whether input should be forwarded at all: the app's `canSendInput`, which is
  // false on the picker and while no target is connected.
  | { type: "input"; enabled: boolean };

/** Page to app, over `window.webkit.messageHandlers.remotexCanvas`. */
export type CanvasEvent =
  // First message of every stream. The app checks both: without a secure
  // context there is no WebCodecs, and silent muting is the failure this
  // catches.
  | { type: "ready"; secureContext: boolean; audioDecoder: boolean }
  | { type: "pointer"; x: number; y: number }
  // Carries no position: a press is preceded by its own `pointer`, which is how
  // the native surface reported one too, and a release lands wherever the
  // pointer already is.
  | { type: "button"; button: MouseButton; pressed: boolean; clicks: number }
  | { type: "wheel"; dx: number; dy: number; unit: WheelUnit }
  | { type: "cacheReset" }
  | { type: "audioState"; playing: boolean; error: string | null };

interface NativeHandler {
  postMessage(event: CanvasEvent): void;
}

/**
 * The app's message handler, or null when this page is open in an ordinary
 * browser.
 *
 * Null is a real case rather than an error: `bun run dev` serves this page for
 * layout work, and everything except the app round trip behaves the same.
 */
function handler(): NativeHandler | null {
  const webkit = (
    window as unknown as {
      webkit?: { messageHandlers?: { remotexCanvas?: NativeHandler } };
    }
  ).webkit;
  return webkit?.messageHandlers?.remotexCanvas ?? null;
}

export function postToApp(event: CanvasEvent): void {
  handler()?.postMessage(event);
}
