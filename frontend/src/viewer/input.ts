// Pointer input, reported to the app in remote pixels.
//
// The page holds no protocol state: it never decides what a `ClientMsg` is, and
// it tracks no pressed keys. Buttons it does track, but only so a press it
// reported cannot be left unreleased when the pointer leaves the window — the
// app's `PressedInput` is still the one that answers for a target switch, a
// takeover or a dropped socket.
//
// Keyboard is absent on purpose. The app takes keys with an AppKit local event
// monitor, ahead of the menu bar, so that ⌘Q and ⌘W reach the guest instead of
// this application; those events never arrive here.

import {
  clickCount,
  type MouseButton,
  mouseButtonFromEvent,
  wheelUnitFromEvent,
} from "../protocol.ts";
import { type CanvasEvent, postToApp } from "./bridge.ts";

export interface InputSurface {
  /** The element that receives events: the transparent overlay. */
  overlay: HTMLElement;
  /** The canvas, whose rect is what remote coordinates are measured against. */
  canvas: HTMLCanvasElement;
  /** The remote's framebuffer size, or null before the first `resize`. */
  size: () => { w: number; h: number } | null;
  /** Whether the app is currently accepting input at all. */
  enabled: () => boolean;
}

/**
 * A point on screen as a remote framebuffer pixel.
 *
 * Measured against the canvas rect rather than the overlay's: the canvas is the
 * displayed framebuffer, and the two coincide only while nothing has resized
 * between layout passes. Clamped to the framebuffer, which is what keeps a drag
 * that runs off the edge reporting positions the gateway will accept instead of
 * ones it rejects; rounded rather than truncated so the pixel under the pointer
 * is the nearest one.
 *
 * Exported for its tests — this used to be `RemoteGeometry.remotePoint` on the
 * Swift side, and it is the one piece of geometry that came across with the
 * canvas.
 */
export function remotePoint(
  client: { x: number; y: number },
  rect: { left: number; top: number; width: number; height: number },
  remote: { w: number; h: number } | null,
): { x: number; y: number } {
  const scaleX = remote && rect.width > 0 ? remote.w / rect.width : 1;
  const scaleY = remote && rect.height > 0 ? remote.h / rect.height : 1;
  const clamp = (value: number, limit: number | undefined) => {
    // Checked before the clamp, not after: a rect read mid-layout can be
    // non-finite, and clamping an infinity would report the far corner — a
    // specific claim about where the pointer is, made from a number that says
    // nothing about it. The origin is the honest answer.
    if (!Number.isFinite(value)) {
      return 0;
    }
    return limit === undefined
      ? Math.round(value)
      : Math.min(Math.max(Math.round(value), 0), limit - 1);
  };
  return {
    x: clamp((client.x - rect.left) * scaleX, remote?.w),
    y: clamp((client.y - rect.top) * scaleY, remote?.h),
  };
}

export function attachInput(surface: InputSurface): void {
  const pressed = new Set<MouseButton>();

  const send = (event: CanvasEvent) => {
    if (!surface.enabled()) {
      return;
    }
    postToApp(event);
  };

  const toRemote = (e: MouseEvent) =>
    remotePoint(
      { x: e.clientX, y: e.clientY },
      surface.canvas.getBoundingClientRect(),
      surface.size(),
    );

  surface.overlay.addEventListener("mousemove", (e) => {
    const { x, y } = toRemote(e);
    send({ type: "pointer", x, y });
  });

  surface.overlay.addEventListener("mousedown", (e) => {
    const button = mouseButtonFromEvent(e.button);
    if (!button) {
      return;
    }
    // The position first, then the press: a press at a stale position is a
    // click in the wrong place, and the native surface ordered these the same
    // way for the same reason.
    const { x, y } = toRemote(e);
    pressed.add(button);
    send({ type: "pointer", x, y });
    send({
      type: "button",
      button,
      pressed: true,
      clicks: clickCount(e.detail),
    });
  });

  // Release on the window so a press that ends outside the overlay still
  // reports the button up. Only buttons seen pressed on the surface count.
  window.addEventListener("mouseup", (e) => {
    const button = mouseButtonFromEvent(e.button);
    if (!button || !pressed.delete(button)) {
      return;
    }
    send({
      type: "button",
      button,
      pressed: false,
      clicks: clickCount(e.detail),
    });
  });

  surface.overlay.addEventListener(
    "wheel",
    (e) => {
      // Always, even when input is off: an unprevented wheel scrolls the page,
      // and a desktop that scrolled itself while the remote heard nothing is
      // worse than one that ignored the gesture. Scrolling an oversized desktop
      // is what the scrollbars are for.
      e.preventDefault();
      send({
        type: "wheel",
        dx: e.deltaX,
        dy: e.deltaY,
        unit: wheelUnitFromEvent(e.deltaMode),
      });
    },
    { passive: false },
  );

  surface.overlay.addEventListener("contextmenu", (e) => e.preventDefault());

  // Anything still held when the window loses the pointer is released here as
  // well as by the app: this one is immediate and knows the position, the app's
  // is the backstop that also covers a socket that simply went away.
  const releaseAll = () => {
    for (const button of pressed) {
      // A sweep, not a gesture: one click is what an unclick is worth.
      send({ type: "button", button, pressed: false, clicks: 1 });
    }
    pressed.clear();
  };
  window.addEventListener("blur", releaseAll);
}
