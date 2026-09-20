// Mobile gestures for the input overlay. The gesture model is a trackpad, not
// a touchscreen: the cursor is
// a persistent position that fingers nudge around, and taps click wherever
// the cursor currently is (the server renders the cursor into the
// framebuffer, so it is always visible).
//
//   one-finger tap        left-click at the cursor
//   one-finger drag       move the cursor (1.5x speed), panning the view when
//                         the cursor pushes past the visible edge
//   double-tap-and-hold   hold the left button (drag mode); a second finger
//                         then moves the cursor while the first keeps holding
//   two-finger tap        right-click at the cursor
//   two-finger drag       one gesture, classified by what the fingers do:
//                         changing the distance between them pinches (zoom
//                         1x-8x, anchored at the midpoint, which pans the
//                         zoomed view with it), moving in parallel scrolls,
//                         axis-locked (vertical or horizontal wheel) in the
//                         natural direction, where content follows the fingers
//
// The state machine keeps its thresholds local to this file. The output layer
// sends remotex ClientMsg JSON (a scroll tick is one wheel message carrying the
// finger travel it stands for), and the view transform is owned by
// useRemoteDesktop's applyCanvasCss, reached through GestureDeps.

import type { ClientMsg } from "./protocol.ts";

export const MIN_ZOOM = 1;
// The pinch ceiling, as a multiple of the fit-to-width base scale — so the most a
// remote pixel can occupy is `MAX_ZOOM × clientWidth / framebufferWidth` CSS px,
// which on a wide desktop is still well under 1:1.
//
// Eight rather than four so a desktop that arrives *larger than it should be* stays
// legible on a phone. That is not hypothetical: a Mac's combined framebuffer spans
// every screen at its own density (4480 px wide on the two-screen test Mac), so on a
// 390 px phone fit-to-width is 0.087 and a ceiling of four capped a point at 0.7 CSS
// px — readable only just. Eight makes it 1.4. The same headroom covers any future
// engine that reports a density wrong in the direction that magnifies, which is the
// failure this is insurance against: a phone is where you find out, and it must not
// also be where you are stuck.
//
// Not conditional on the engine or the subtype, deliberately. A ceiling exists only
// to stop a pinch running away, so a higher one costs nothing to a session that does
// not need it — and an escape hatch fitted to the one bug already found would be no
// use for the next one. The client is not told the target's subtype anyway.
export const MAX_ZOOM = 8;
const TAP_MAX_MOVE_PX = 4;
const TAP_MAX_DURATION_MS = 200;
const PAN_ACTIVATION_THRESHOLD_PX = 12;
const PAN_CURSOR_SPEED = 1.5;
const FORCE_TAP_THRESHOLD = 0.15;
const DOUBLE_TAP_WINDOW_MS = 300;
const TWO_FINGER_TAP_MAX_MOVE_PX = 12;
const TWO_FINGER_TAP_MAX_DURATION_MS = 260;
// How far two fingers must work before their gesture commits to pinching or
// scrolling. Both measurements are taken at that moment and the larger one
// decides: how much the distance between the fingers changed, against how far
// their midpoint travelled. Until then nothing moves, which is also what holds
// a two-finger tap still. That same travel names the scroll axis and counts as
// the scroll's own first movement, so recognising a scroll costs it nothing.
const TWO_FINGER_CLASSIFY_PX = 12;
const SCROLL_STEP_PX = 32;

export interface Point {
  x: number;
  y: number;
}

// Snapshot of the canvas view transform: the fit-to-width base scale, the
// pinch zoom on top of it, and the pan offset in CSS pixels (≤ 0 per axis).
export interface GestureView {
  fit: number;
  zoom: number;
  pan: Point;
}

export interface GestureDeps {
  send(msg: ClientMsg): void;
  // The remote framebuffer size; null before the first resize message.
  remoteSize(): { w: number; h: number } | null;
  // The current view transform, after clamping.
  view(): GestureView;
  // Clamp the requested zoom/pan and restyle the canvas.
  applyView(zoom: number, pan: Point): void;
  // CSS pixels along the bottom edge that are covered by chrome (the docked
  // soft keyboard) and so excluded from the visible region — the cursor won't
  // pan under them, and content can pan up above them. 0 when nothing covers
  // the canvas. Optional: absent means no inset.
  bottomInset?(): number;
  // Reports where the virtual cursor now sits, in remote coordinates, so the
  // caller can draw a pointer there when the remote isn't drawing one itself.
  // `real` marks a position that came from a hardware mouse (notePointer) —
  // the browser's own CSS cursor already tracks that one, with no lag.
  // Optional: absent means nothing is drawn.
  onCursor?(x: number, y: number, real: boolean): void;
}

export interface TouchGestures {
  detach(): void;
  // Keeps the gesture cursor in sync with real mouse input (hybrid devices).
  notePointer(x: number, y: number): void;
  // Drops all gesture state and releases a held drag button, so nothing
  // sticks on the remote (blur/logout path).
  release(): void;
}

interface MouseGesture {
  touchId: number;
  startClientX: number;
  startClientY: number;
  lastClientX: number;
  lastClientY: number;
  maxForce: number;
  startTime: number;
  mode: "pending" | "pan" | "drag";
  moved: boolean;
}

interface DragAssistGesture {
  touchId: number;
  lastClientX: number;
  lastClientY: number;
}

// The single two-finger gesture, tracked on the pair of fingers it started
// with. It begins undecided — a right-click candidate while the fingers stay
// put — and once it commits to a pinch or a scroll it stays that for life, so
// the drift of a scroll never creeps into the zoom and back.
interface TwoFingerGesture {
  firstId: number;
  secondId: number;
  startTime: number;
  firstStartX: number;
  firstStartY: number;
  secondStartX: number;
  secondStartY: number;
  // What the classification measures against. The midpoint is re-based when a
  // scroll starts, so the axis lock measures from there rather than from the
  // travel that bought the decision.
  startDistance: number;
  startMidX: number;
  startMidY: number;
  mode: "undecided" | "pinch" | "scroll";
  // Still a right-click, if every finger lifts soon enough.
  tapCandidate: boolean;
  // The zoom and finger distance a pinch scales from, and the remote point
  // under the midpoint, which is held there. Null until the pinch starts.
  pinch: PinchAnchor | null;
  // The axis a scroll locked onto (null until it does), the midpoint it has
  // spent — where the gesture began, until the scroll moves it on — and what it
  // still owes the wire.
  axis: ScrollAxis | null;
  lastMidX: number;
  lastMidY: number;
  carryX: number;
  carryY: number;
}

type ScrollAxis = "x" | "y";

interface PinchAnchor {
  initialDistance: number;
  initialZoom: number;
  anchorX: number;
  anchorY: number;
}

function clampValue(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function getTouchDistance(first: Touch, second: Touch): number {
  return Math.hypot(
    second.clientX - first.clientX,
    second.clientY - first.clientY,
  );
}

function getTouchById(touches: TouchList, touchId: number): Touch | null {
  for (let i = 0; i < touches.length; i += 1) {
    if (touches[i].identifier === touchId) {
      return touches[i];
    }
  }
  return null;
}

// Drain accumulated finger travel into wheel ticks, one per 32px step, and
// return the leftover carry.
function drainScrollCarry(carry: number, tick: (dir: 1 | -1) => void): number {
  let rest = carry;
  while (Math.abs(rest) >= SCROLL_STEP_PX) {
    const dir = rest > 0 ? 1 : -1;
    tick(dir);
    rest -= dir * SCROLL_STEP_PX;
  }
  return rest;
}

function consumeTouchEvent(e: TouchEvent): void {
  e.preventDefault();
  e.stopImmediatePropagation();
}

export function attachTouchGestures(
  el: HTMLElement,
  deps: GestureDeps,
): TouchGestures {
  let mouseGesture: MouseGesture | null = null;
  let dragAssist: DragAssistGesture | null = null;
  let twoFinger: TwoFingerGesture | null = null;
  // A gesture that broke down (e.g. a finger of a two-finger swipe lifted)
  // swallows the leftover touches so they can't turn into stray clicks.
  let ignoreSingleTouch = false;
  let lastTapTime = 0;
  let pendingTapTimer: ReturnType<typeof setTimeout> | null = null;
  // The virtual trackpad cursor, in remote framebuffer coordinates.
  let cursor: Point = { x: 0, y: 0 };
  let hasCursor = false;
  // Whether the gesture layer is holding the remote left button down.
  let leftHeld = false;

  function remoteSize(): { w: number; h: number } {
    return deps.remoteSize() ?? { w: 1, h: 1 };
  }

  function effectiveScale(): number {
    const view = deps.view();
    return Math.max(0.0001, view.fit * view.zoom);
  }

  function viewportSize(): { width: number; height: number } {
    const doc = document.documentElement;
    const inset = deps.bottomInset?.() ?? 0;
    return {
      width: Math.max(1, doc.clientWidth),
      height: Math.max(1, doc.clientHeight - inset),
    };
  }

  // The overlay is fixed to the viewport, so this offset is normally zero;
  // mapping through the rect keeps the math honest anyway.
  function getTouchMidpoint(first: Touch, second: Touch): Point {
    const rect = el.getBoundingClientRect();
    return {
      x: (first.clientX + second.clientX) / 2 - rect.left,
      y: (first.clientY + second.clientY) / 2 - rect.top,
    };
  }

  function clampCursorToRemote(x: number, y: number): Point {
    const size = remoteSize();
    return {
      x: clampValue(Math.round(x), 0, Math.max(0, size.w - 1)),
      y: clampValue(Math.round(y), 0, Math.max(0, size.h - 1)),
    };
  }

  // The remote-coordinate rectangle currently visible through the viewport.
  function visibleRemoteBounds(scale: number): {
    left: number;
    right: number;
    top: number;
    bottom: number;
  } {
    const size = remoteSize();
    const { width, height } = viewportSize();
    const { pan } = deps.view();
    const maxX = Math.max(0, size.w - 1);
    const maxY = Math.max(0, size.h - 1);
    const left = clampValue(-pan.x / scale, 0, maxX);
    const top = clampValue(-pan.y / scale, 0, maxY);
    const right = clampValue(left + width / scale - 1, left, maxX);
    const bottom = clampValue(top + height / scale - 1, top, maxY);
    return { left, right, top, bottom };
  }

  function trackCursor(x: number, y: number, real = false): void {
    cursor = { x, y };
    hasCursor = true;
    deps.onCursor?.(x, y, real);
  }

  function currentCursor(): Point {
    if (hasCursor) {
      return cursor;
    }
    const size = remoteSize();
    const fallback = clampCursorToRemote(size.w / 2, size.h / 2);
    trackCursor(fallback.x, fallback.y);
    return fallback;
  }

  // Move the remote pointer, transitioning the left button when the held
  // state changes.
  function movePointer(x: number, y: number, left: boolean, clicks = 1): void {
    const clamped = clampCursorToRemote(x, y);
    trackCursor(clamped.x, clamped.y);
    deps.send({ type: "mouseMove", x: clamped.x, y: clamped.y });
    if (left !== leftHeld) {
      leftHeld = left;
      deps.send({ type: "mouseButton", button: "left", pressed: left, clicks });
    }
  }

  // A tap is a click, and the count says which one of a run it is — a touch
  // screen has no MouseEvent.detail to inherit, so the gesture layer is the only
  // thing that knows a second tap was meant as a double-click.
  function sendTapClick(clicks = 1): void {
    const c = currentCursor();
    movePointer(c.x, c.y, true, clicks);
    movePointer(c.x, c.y, false, clicks);
  }

  function sendRightClick(): void {
    const c = currentCursor();
    deps.send({ type: "mouseMove", x: c.x, y: c.y });
    deps.send({
      type: "mouseButton",
      button: "right",
      pressed: true,
      clicks: 1,
    });
    deps.send({
      type: "mouseButton",
      button: "right",
      pressed: false,
      clicks: 1,
    });
  }

  // One scroll step at the cursor, carrying the finger travel it stands for.
  //
  // Pixels, because that is what the deltas are: a step is a step's worth of
  // finger movement. RDP spends the distance as proportional wheel rotation and
  // an Apple VNC target as as many wheel pulses as it is worth there; generic
  // VNC reads only the sign, so a step is a notch there.
  function sendScrollTick(dx: number, dy: number): void {
    const c = currentCursor();
    deps.send({ type: "mouseMove", x: c.x, y: c.y });
    deps.send({ type: "wheel", dx, dy, unit: "pixel" });
  }

  // Move the cursor by a finger step (screen px -> remote px through the
  // effective scale); when the cursor would leave the visible rectangle, pan
  // the view by the overflow instead so the cursor drags the viewport along.
  function moveCursorWithPan(
    stepX: number,
    stepY: number,
    leftDown: boolean,
    base: Point,
  ): void {
    const view = deps.view();
    const scale = Math.max(0.0001, view.fit * view.zoom);
    const speed = leftDown ? 1 : PAN_CURSOR_SPEED;
    const desired = clampCursorToRemote(
      base.x + (stepX * speed) / scale,
      base.y + (stepY * speed) / scale,
    );
    const visible = visibleRemoteBounds(scale);
    const constrained = {
      x: clampValue(desired.x, visible.left, visible.right),
      y: clampValue(desired.y, visible.top, visible.bottom),
    };
    movePointer(constrained.x, constrained.y, leftDown);

    const overflowX = desired.x - constrained.x;
    const overflowY = desired.y - constrained.y;
    if (overflowX !== 0 || overflowY !== 0) {
      deps.applyView(view.zoom, {
        x: view.pan.x - overflowX * scale,
        y: view.pan.y - overflowY * scale,
      });
    }
  }

  function cancelPendingTap(): void {
    if (pendingTapTimer !== null) {
      clearTimeout(pendingTapTimer);
      pendingTapTimer = null;
    }
  }

  function beginMouseGesture(touch: Touch): void {
    const now = Date.now();
    // A touch landing right after a tap is the second half of a double-tap:
    // it holds the left button down (drag) instead of waiting to click.
    const isSecondTap = now - lastTapTime <= DOUBLE_TAP_WINDOW_MS;

    mouseGesture = {
      touchId: touch.identifier,
      startClientX: touch.clientX,
      startClientY: touch.clientY,
      lastClientX: touch.clientX,
      lastClientY: touch.clientY,
      maxForce: touch.force ?? 0,
      startTime: now,
      mode: isSecondTap ? "drag" : "pending",
      moved: false,
    };
    dragAssist = null;
    twoFinger = null;

    if (isSecondTap) {
      cancelPendingTap();
      lastTapTime = 0;
      const c = currentCursor();
      movePointer(c.x, c.y, true);
    }
  }

  function finalizeMouseGesture(
    touch: Touch | null,
    suppressTap: boolean,
  ): void {
    if (!mouseGesture) {
      return;
    }
    const gesture = mouseGesture;
    mouseGesture = null;
    dragAssist = null;

    if (touch) {
      gesture.maxForce = Math.max(gesture.maxForce, touch.force ?? 0);
    }
    const duration = Date.now() - gesture.startTime;
    // Devices that report pressure must clear FORCE_TAP_THRESHOLD (a resting
    // finger isn't a tap); devices that don't report it leave maxForce at 0,
    // so fall back to the move/duration bounds alone.
    const isForceTap =
      !gesture.moved &&
      duration <= TAP_MAX_DURATION_MS &&
      (gesture.maxForce === 0 || gesture.maxForce >= FORCE_TAP_THRESHOLD);

    if (gesture.mode === "drag") {
      const c = currentCursor();
      movePointer(c.x, c.y, false);
      // A quick forceful second tap after the drag release doubles up into a
      // double-click — which it only is if it says so, since the remote counts
      // nothing itself.
      if (isForceTap) {
        sendTapClick(2);
      }
      return;
    }

    if (gesture.mode === "pending" && !suppressTap && isForceTap) {
      // Delay the click by the double-tap window: if a second tap lands in
      // time it becomes a drag (and cancels this), otherwise the click fires.
      lastTapTime = Date.now();
      cancelPendingTap();
      pendingTapTimer = setTimeout(() => {
        pendingTapTimer = null;
        sendTapClick();
      }, DOUBLE_TAP_WINDOW_MS);
    }
  }

  function handleOneFingerMove(touch: Touch): void {
    if (!mouseGesture) {
      return;
    }
    const gesture = mouseGesture;
    const stepX = touch.clientX - gesture.lastClientX;
    const stepY = touch.clientY - gesture.lastClientY;
    gesture.lastClientX = touch.clientX;
    gesture.lastClientY = touch.clientY;
    gesture.maxForce = Math.max(gesture.maxForce, touch.force ?? 0);

    const totalMove = Math.hypot(
      touch.clientX - gesture.startClientX,
      touch.clientY - gesture.startClientY,
    );
    if (!gesture.moved && totalMove >= TAP_MAX_MOVE_PX) {
      gesture.moved = true;
    }
    if (
      gesture.mode === "pending" &&
      totalMove >= PAN_ACTIVATION_THRESHOLD_PX
    ) {
      gesture.mode = "pan";
    }

    if (gesture.mode === "pan") {
      handleTrackpadMove(stepX, stepY);
      return;
    }
    if (gesture.mode === "drag") {
      moveCursorWithPan(stepX, stepY, true, currentCursor());
    }
  }

  // Trackpad move: start from the cursor pulled into the visible area so the
  // pointer never crawls along off-screen.
  function handleTrackpadMove(stepX: number, stepY: number): void {
    const scale = effectiveScale();
    const visible = visibleRemoteBounds(scale);
    const size = remoteSize();
    const raw = hasCursor ? cursor : { x: size.w / 2, y: size.h / 2 };
    moveCursorWithPan(stepX, stepY, false, {
      x: clampValue(raw.x, visible.left, visible.right),
      y: clampValue(raw.y, visible.top, visible.bottom),
    });
  }

  // During a hold-drag, any other finger works the cursor while the primary
  // finger keeps the button held.
  function getDragAssistTouch(touches: TouchList): Touch | null {
    if (!mouseGesture || mouseGesture.mode !== "drag") {
      dragAssist = null;
      return null;
    }
    if (dragAssist) {
      const existing = getTouchById(touches, dragAssist.touchId);
      if (existing && existing.identifier !== mouseGesture.touchId) {
        return existing;
      }
      dragAssist = null;
    }
    for (let i = 0; i < touches.length; i += 1) {
      const touch = touches[i];
      if (touch.identifier === mouseGesture.touchId) {
        continue;
      }
      dragAssist = {
        touchId: touch.identifier,
        lastClientX: touch.clientX,
        lastClientY: touch.clientY,
      };
      return touch;
    }
    return null;
  }

  function handleDragAssistMove(touch: Touch): void {
    if (!dragAssist || dragAssist.touchId !== touch.identifier) {
      dragAssist = {
        touchId: touch.identifier,
        lastClientX: touch.clientX,
        lastClientY: touch.clientY,
      };
      return;
    }
    const stepX = touch.clientX - dragAssist.lastClientX;
    const stepY = touch.clientY - dragAssist.lastClientY;
    dragAssist.lastClientX = touch.clientX;
    dragAssist.lastClientY = touch.clientY;
    if (stepX === 0 && stepY === 0) {
      return;
    }
    moveCursorWithPan(stepX, stepY, true, currentCursor());
  }

  // The pair the gesture started on, or null once either finger has left.
  function trackedPair(
    gesture: TwoFingerGesture,
    touches: TouchList,
  ): [Touch, Touch] | null {
    const first = getTouchById(touches, gesture.firstId);
    const second = getTouchById(touches, gesture.secondId);
    return first && second ? [first, second] : null;
  }

  function beginTwoFingerGesture(
    first: Touch,
    second: Touch,
    tapCandidate: boolean,
  ): void {
    const midpoint = getTouchMidpoint(first, second);
    twoFinger = {
      firstId: first.identifier,
      secondId: second.identifier,
      startTime: Date.now(),
      firstStartX: first.clientX,
      firstStartY: first.clientY,
      secondStartX: second.clientX,
      secondStartY: second.clientY,
      startDistance: getTouchDistance(first, second),
      startMidX: midpoint.x,
      startMidY: midpoint.y,
      mode: "undecided",
      tapCandidate,
      pinch: null,
      axis: null,
      lastMidX: midpoint.x,
      lastMidY: midpoint.y,
      carryX: 0,
      carryY: 0,
    };
  }

  // A right-click candidate survives only while both fingers stay where they
  // landed — including a rotation, which moves neither the midpoint nor the
  // distance and so would otherwise never be classified out of the running.
  function updateTapCandidate(
    gesture: TwoFingerGesture,
    first: Touch,
    second: Touch,
  ): void {
    if (!gesture.tapCandidate) {
      return;
    }
    const firstMoved = Math.hypot(
      first.clientX - gesture.firstStartX,
      first.clientY - gesture.firstStartY,
    );
    const secondMoved = Math.hypot(
      second.clientX - gesture.secondStartX,
      second.clientY - gesture.secondStartY,
    );
    if (
      firstMoved > TWO_FINGER_TAP_MAX_MOVE_PX ||
      secondMoved > TWO_FINGER_TAP_MAX_MOVE_PX
    ) {
      gesture.tapCandidate = false;
    }
  }

  // Which of the two things two fingers do is this? Distance between them
  // changing is a pinch; the pair travelling while that distance holds is a
  // scroll. Nothing happens until one of the measurements is worth a decision,
  // and the decision is final: a scroll that lets the fingers drift apart is
  // still a scroll, and a pinch that slides is still a pinch.
  function classifyTwoFingerGesture(
    gesture: TwoFingerGesture,
    first: Touch,
    second: Touch,
  ): void {
    const spread = Math.abs(
      getTouchDistance(first, second) - gesture.startDistance,
    );
    const midpoint = getTouchMidpoint(first, second);
    const travel = Math.hypot(
      midpoint.x - gesture.startMidX,
      midpoint.y - gesture.startMidY,
    );
    if (Math.max(spread, travel) < TWO_FINGER_CLASSIFY_PX) {
      return;
    }
    gesture.tapCandidate = false;
    if (spread >= travel) {
      gesture.mode = "pinch";
      startPinch(gesture, first, second);
      return;
    }
    gesture.mode = "scroll";
    // The travel that bought the decision also says which way it was going, and
    // that axis holds for the rest of the gesture — a diagonal drag never sends
    // a stray tick sideways. The midpoint is left where the gesture began, so
    // that travel is also the scroll's first movement: a swipe the browser
    // delivers as one coalesced move scrolls by all of it, not by what is left
    // after the threshold.
    gesture.axis =
      Math.abs(midpoint.x - gesture.startMidX) >=
      Math.abs(midpoint.y - gesture.startMidY)
        ? "x"
        : "y";
  }

  // Anchor the pinch on the fingers as they are now, so committing to it — or
  // carrying it over to a new pair of fingers — never jumps the zoom.
  function startPinch(
    gesture: TwoFingerGesture,
    first: Touch,
    second: Touch,
  ): void {
    const initialDistance = getTouchDistance(first, second);
    if (initialDistance <= 0) {
      return;
    }
    const midpoint = getTouchMidpoint(first, second);
    const view = deps.view();
    const scale = Math.max(0.0001, view.fit * view.zoom);
    gesture.pinch = {
      initialDistance,
      initialZoom: view.zoom,
      // The remote point under the finger midpoint, kept there while zooming.
      anchorX: (midpoint.x - view.pan.x) / scale,
      anchorY: (midpoint.y - view.pan.y) / scale,
    };
  }

  // One pinch frame: zoom from the distance ratio, pan from the midpoint drift
  // (so the zoomed view follows the fingers as they spread).
  function applyPinchMove(
    gesture: TwoFingerGesture,
    first: Touch,
    second: Touch,
  ): void {
    const pinch = gesture.pinch;
    if (!pinch) {
      startPinch(gesture, first, second);
      return;
    }
    const distance = getTouchDistance(first, second);
    if (distance <= 0) {
      return;
    }
    const midpoint = getTouchMidpoint(first, second);
    const nextZoom = clampValue(
      pinch.initialZoom * (distance / pinch.initialDistance),
      MIN_ZOOM,
      MAX_ZOOM,
    );
    const scale = deps.view().fit * nextZoom;
    deps.applyView(nextZoom, {
      x: midpoint.x - pinch.anchorX * scale,
      y: midpoint.y - pinch.anchorY * scale,
    });
  }

  // One scroll frame: every 32px the midpoint travels along the locked axis
  // drains into one wheel tick.
  function applyScrollMove(
    gesture: TwoFingerGesture,
    axis: ScrollAxis,
    first: Touch,
    second: Touch,
  ): void {
    const midpoint = getTouchMidpoint(first, second);
    const stepX = midpoint.x - gesture.lastMidX;
    const stepY = midpoint.y - gesture.lastMidY;
    gesture.lastMidX = midpoint.x;
    gesture.lastMidY = midpoint.y;

    // Negated: natural (touch) direction, where content follows the fingers —
    // swiping up scrolls the content up, i.e. a wheel-down tick.
    if (axis === "x") {
      gesture.carryX = drainScrollCarry(gesture.carryX + stepX, (dir) =>
        sendScrollTick(-dir * SCROLL_STEP_PX, 0),
      );
    } else {
      gesture.carryY = drainScrollCarry(gesture.carryY + stepY, (dir) =>
        sendScrollTick(0, -dir * SCROLL_STEP_PX),
      );
    }
  }

  function finalizeMouseFromTouches(e: TouchEvent, suppressTap: boolean): void {
    if (!mouseGesture) {
      return;
    }
    const active =
      getTouchById(e.touches, mouseGesture.touchId) || e.touches[0] || null;
    finalizeMouseGesture(active, suppressTap);
  }

  function handleTouchStart(e: TouchEvent): void {
    cancelPendingTap();
    if (e.touches.length >= 2) {
      handleMultiTouchStart(e);
      return;
    }

    twoFinger = null;
    if (ignoreSingleTouch) {
      consumeTouchEvent(e);
      return;
    }
    beginMouseGesture(e.touches[0]);
    consumeTouchEvent(e);
  }

  function handleMultiTouchStart(e: TouchEvent): void {
    if (mouseGesture?.mode === "drag") {
      // Extra fingers during a hold-drag assist the cursor, they never
      // zoom/scroll.
      ignoreSingleTouch = false;
      twoFinger = null;
      const assist = getDragAssistTouch(e.touches);
      if (assist) {
        handleDragAssistMove(assist);
      }
      consumeTouchEvent(e);
      return;
    }
    finalizeMouseFromTouches(e, true);
    ignoreSingleTouch = true;
    if (twoFinger && trackedPair(twoFinger, e.touches)) {
      // A further finger joined a gesture already under way: it keeps running
      // on the pair it started with, but it is no longer a two-finger tap.
      twoFinger.tapCandidate = false;
    } else {
      beginTwoFingerGesture(e.touches[0], e.touches[1], e.touches.length === 2);
    }
    consumeTouchEvent(e);
  }

  // Two-finger move outside a hold-drag: classify the gesture once the fingers
  // have worked far enough, then pinch or scroll for the rest of its life.
  function handleTwoFingerMove(e: TouchEvent): void {
    finalizeMouseFromTouches(e, true);
    ignoreSingleTouch = true;
    const gesture = twoFinger;
    const pair = gesture ? trackedPair(gesture, e.touches) : null;
    if (!gesture || !pair) {
      // The pair it started on is gone while fingers are still down: what is
      // left starts a gesture of its own, never a right-click.
      beginTwoFingerGesture(e.touches[0], e.touches[1], false);
      consumeTouchEvent(e);
      return;
    }
    const [first, second] = pair;
    updateTapCandidate(gesture, first, second);
    if (gesture.mode === "undecided") {
      classifyTwoFingerGesture(gesture, first, second);
    }
    if (gesture.mode === "pinch") {
      applyPinchMove(gesture, first, second);
    } else if (gesture.mode === "scroll" && gesture.axis) {
      applyScrollMove(gesture, gesture.axis, first, second);
    }
    consumeTouchEvent(e);
  }

  // Multi-finger move while a hold-drag is active: the primary finger keeps
  // holding, the assist finger moves the cursor.
  function handleDragMultiTouchMove(
    e: TouchEvent,
    gesture: MouseGesture,
  ): void {
    const primary = getTouchById(e.touches, gesture.touchId);
    if (!primary) {
      finalizeMouseGesture(
        getTouchById(e.changedTouches, gesture.touchId) || null,
        false,
      );
      twoFinger = null;
      ignoreSingleTouch = true;
      consumeTouchEvent(e);
      return;
    }
    gesture.lastClientX = primary.clientX;
    gesture.lastClientY = primary.clientY;
    const assist = getDragAssistTouch(e.touches);
    if (assist) {
      handleDragAssistMove(assist);
    } else {
      dragAssist = null;
    }
    twoFinger = null;
    ignoreSingleTouch = false;
    consumeTouchEvent(e);
  }

  function handleTouchMove(e: TouchEvent): void {
    if (e.touches.length >= 2) {
      if (mouseGesture?.mode === "drag") {
        handleDragMultiTouchMove(e, mouseGesture);
        return;
      }
      handleTwoFingerMove(e);
      return;
    }

    if (e.touches.length !== 1) {
      return;
    }
    handleSingleTouchMove(e);
  }

  function handleSingleTouchMove(e: TouchEvent): void {
    if (ignoreSingleTouch) {
      consumeTouchEvent(e);
      return;
    }
    const active = mouseGesture
      ? getTouchById(e.touches, mouseGesture.touchId) || e.touches[0]
      : e.touches[0];
    if (!mouseGesture) {
      beginMouseGesture(active);
    }
    handleOneFingerMove(active);
    consumeTouchEvent(e);
  }

  function handleAllTouchesEnded(e: TouchEvent): void {
    if (mouseGesture) {
      finalizeMouseGesture(
        getTouchById(e.changedTouches, mouseGesture.touchId) || null,
        false,
      );
    }
    dragAssist = null;
    const gesture = twoFinger;
    twoFinger = null;
    if (
      gesture?.tapCandidate &&
      Date.now() - gesture.startTime <= TWO_FINGER_TAP_MAX_DURATION_MS
    ) {
      sendRightClick();
    }
    ignoreSingleTouch = false;
    consumeTouchEvent(e);
  }

  // A finger lifted while a hold-drag is active: the drag survives as long
  // as its primary finger is still down.
  function handleDragTouchEnd(e: TouchEvent, gesture: MouseGesture): void {
    const primary = getTouchById(e.touches, gesture.touchId);
    if (!primary) {
      finalizeMouseGesture(
        getTouchById(e.changedTouches, gesture.touchId) || null,
        false,
      );
      ignoreSingleTouch = true;
    } else {
      gesture.lastClientX = primary.clientX;
      gesture.lastClientY = primary.clientY;
      const assist = getDragAssistTouch(e.touches);
      dragAssist = assist
        ? {
            touchId: assist.identifier,
            lastClientX: assist.clientX,
            lastClientY: assist.clientY,
          }
        : null;
      ignoreSingleTouch = false;
    }
    twoFinger = null;
    consumeTouchEvent(e);
  }

  function handleTouchEnd(e: TouchEvent): void {
    if (e.touches.length === 0) {
      handleAllTouchesEnded(e);
      return;
    }

    if (mouseGesture?.mode === "drag") {
      handleDragTouchEnd(e, mouseGesture);
      return;
    }

    if (e.touches.length >= 2) {
      handleMultiTouchEnd(e);
      return;
    }
    handleSingleTouchEnd(e);
  }

  function handleMultiTouchEnd(e: TouchEvent): void {
    if (mouseGesture) {
      const released =
        getTouchById(e.changedTouches, mouseGesture.touchId) ||
        getTouchById(e.touches, mouseGesture.touchId) ||
        e.changedTouches[0] ||
        null;
      finalizeMouseGesture(released, true);
    }
    ignoreSingleTouch = true;
    if (!twoFinger || !trackedPair(twoFinger, e.touches)) {
      // A finger of the pair left while others are still down: the remaining
      // fingers carry on as a fresh gesture, which the next move classifies.
      beginTwoFingerGesture(e.touches[0], e.touches[1], false);
    }
    consumeTouchEvent(e);
  }

  function handleSingleTouchEnd(e: TouchEvent): void {
    if (ignoreSingleTouch) {
      consumeTouchEvent(e);
      return;
    }
    if (mouseGesture && !getTouchById(e.touches, mouseGesture.touchId)) {
      const released =
        getTouchById(e.changedTouches, mouseGesture.touchId) ||
        e.changedTouches[0] ||
        null;
      finalizeMouseGesture(released, false);
    }
    twoFinger = null;
    consumeTouchEvent(e);
  }

  function release(): void {
    cancelPendingTap();
    mouseGesture = null;
    dragAssist = null;
    twoFinger = null;
    ignoreSingleTouch = false;
    if (leftHeld) {
      leftHeld = false;
      deps.send({
        type: "mouseButton",
        button: "left",
        pressed: false,
        clicks: 1,
      });
    }
  }

  el.addEventListener("touchstart", handleTouchStart, { passive: false });
  el.addEventListener("touchmove", handleTouchMove, { passive: false });
  el.addEventListener("touchend", handleTouchEnd, { passive: false });
  el.addEventListener("touchcancel", handleTouchEnd, { passive: false });

  return {
    detach() {
      release();
      el.removeEventListener("touchstart", handleTouchStart);
      el.removeEventListener("touchmove", handleTouchMove);
      el.removeEventListener("touchend", handleTouchEnd);
      el.removeEventListener("touchcancel", handleTouchEnd);
    },
    notePointer(x: number, y: number) {
      trackCursor(x, y, true);
    },
    release,
  };
}
