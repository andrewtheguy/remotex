// Touchscreen mode: fingers go to the remote as the touch contacts they are,
// and the *guest* recognises the gestures — a Windows host's tap, drag,
// press-and-hold, pinch, two-finger scroll and edge swipes are all its own
// (MS-RDPEI contacts, injected by the RDP engine). This file decides nothing
// about what fingers mean; it names them and forwards them.
//
// The opposite of touchGestures.ts, which is a trackpad: there, the fingers
// drive a virtual cursor and the page interprets them. The two are never
// attached together — useRemoteDesktop picks one per the Touchscreen toggle,
// and offers the toggle only after the gateway's `touchReady`.
//
// Ids are assigned here rather than forwarded: a DOM `Touch.identifier` is any
// 32-bit number (WebKit hands out large ones), while the engine's contact table
// is ten slots named by small ids. A finger gets the lowest free slot on its
// down and gives it back on its up or cancel; an eleventh finger is ignored
// entirely, down to up, rather than sent to a table that cannot hold it.

import type { ClientMsg, TouchPhase } from "./protocol.ts";

// MAX_CONTACTS in FreeRDP's rdpei plugin, which is MS-RDPEI's own ceiling.
export const MAX_CONTACTS = 10;

export interface Point {
  x: number;
  y: number;
}

// The three fields of a DOM Touch this needs — a type of its own so the
// forwarder can be driven without a document.
export interface TouchPoint {
  identifier: number;
  clientX: number;
  clientY: number;
}

export interface PassthroughDeps {
  send(msg: ClientMsg): void;
  // Client point to remote framebuffer pixels, clamped to the framebuffer —
  // the same mapping the mouse uses. `null` before the first resize, when
  // there is no framebuffer to map onto and the finger is dropped.
  toRemote(clientX: number, clientY: number): Point | null;
}

export interface TouchForwarder {
  down(touches: ArrayLike<TouchPoint>): void;
  move(touches: ArrayLike<TouchPoint>): void;
  up(touches: ArrayLike<TouchPoint>): void;
  cancel(touches: ArrayLike<TouchPoint>): void;
  // Cancel every contact still down — blur, detach, mode switch. A cancel
  // rather than an up: nobody lifted these fingers, and an up where they were
  // would be a tap.
  release(): void;
  // How many fingers are down, for tests and for nothing else.
  held(): number;
}

export function createTouchForwarder(deps: PassthroughDeps): TouchForwarder {
  // DOM identifier -> slot id, with the slot's last remote position so a
  // release can cancel where the finger was.
  const slots = new Map<number, { id: number; at: Point }>();

  const freeSlot = (): number | null => {
    const taken = new Set<number>();
    for (const { id } of slots.values()) {
      taken.add(id);
    }
    for (let id = 1; id <= MAX_CONTACTS; id += 1) {
      if (!taken.has(id)) {
        return id;
      }
    }
    return null;
  };

  const forward = (
    phase: Exclude<TouchPhase, "down">,
    touches: ArrayLike<TouchPoint>,
  ) => {
    for (let i = 0; i < touches.length; i += 1) {
      const touch = touches[i];
      const slot = slots.get(touch.identifier);
      if (!slot) {
        continue; // never went down here (an eleventh finger, or pre-resize)
      }
      // A finger that leaves the framebuffer keeps reporting: the mapping
      // clamps, so a drag past the edge stays at the edge, as with the mouse.
      const at = deps.toRemote(touch.clientX, touch.clientY) ?? slot.at;
      slot.at = at;
      deps.send({ type: "touch", id: slot.id, phase, x: at.x, y: at.y });
      if (phase !== "move") {
        slots.delete(touch.identifier);
      }
    }
  };

  return {
    down(touches) {
      for (let i = 0; i < touches.length; i += 1) {
        const touch = touches[i];
        if (slots.has(touch.identifier)) {
          continue; // a down for a finger already down: the DOM does not do this
        }
        const at = deps.toRemote(touch.clientX, touch.clientY);
        if (!at) {
          continue;
        }
        const id = freeSlot();
        if (id === null) {
          continue;
        }
        slots.set(touch.identifier, { id, at });
        deps.send({ type: "touch", id, phase: "down", x: at.x, y: at.y });
      }
    },
    move: (touches) => forward("move", touches),
    up: (touches) => forward("up", touches),
    cancel: (touches) => forward("cancel", touches),
    release() {
      for (const { id, at } of slots.values()) {
        deps.send({ type: "touch", id, phase: "cancel", x: at.x, y: at.y });
      }
      slots.clear();
    },
    held: () => slots.size,
  };
}

export interface TouchPassthrough {
  detach(): void;
  release(): void;
}

// Listen on the overlay and forward `changedTouches` — the fingers this event
// is about, not every finger down. Every event is preventDefaulted: that is
// what keeps the browser from synthesising mouse events for a tap, scrolling
// the page under a drag, or zooming on a double-tap, all of which are the
// guest's to decide now. Passive false for the same reason.
export function attachTouchPassthrough(
  el: HTMLElement,
  deps: PassthroughDeps,
): TouchPassthrough {
  const forwarder = createTouchForwarder(deps);
  const handle =
    (phase: TouchPhase) =>
    (e: TouchEvent): void => {
      e.preventDefault();
      forwarder[phase](e.changedTouches);
    };
  const onStart = handle("down");
  const onMove = handle("move");
  const onEnd = handle("up");
  const onCancel = handle("cancel");
  const options: AddEventListenerOptions = { passive: false };
  el.addEventListener("touchstart", onStart, options);
  el.addEventListener("touchmove", onMove, options);
  el.addEventListener("touchend", onEnd, options);
  el.addEventListener("touchcancel", onCancel, options);
  return {
    release: () => forwarder.release(),
    detach() {
      // What is still down is cancelled first: the mode is going away, and
      // the listener that would have sent the up with it.
      forwarder.release();
      el.removeEventListener("touchstart", onStart);
      el.removeEventListener("touchmove", onMove);
      el.removeEventListener("touchend", onEnd);
      el.removeEventListener("touchcancel", onCancel);
    },
  };
}
