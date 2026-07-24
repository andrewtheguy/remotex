// Phase 8 (mobile gestures): remotex's touch controls, ported onto the input
// overlay. The gesture model is a trackpad, not a touchscreen: the cursor is
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
//   two-finger pinch      zoom (1x-4x, anchored at the finger midpoint)
//   two-finger drag       pan the zoomed view (when not in drag mode)
//   three-finger swipe    scroll, axis-locked (vertical or horizontal wheel)
//
// The state machine and every threshold are ported faithfully from
// ../remotex/src/useRemoteDesktop.ts — they are battle-tested there. Only the
// output layer differs: rdpweb ClientMsg JSON instead of RFB pointer masks
// (a scroll tick is one wheel message; the server turns any nonzero delta
// into one notch), and the view transform is owned by useRemoteDesktop's
// applyCanvasCss, reached through GestureDeps.

import type { ClientMsg } from "./protocol.ts";

export const MIN_ZOOM = 1;
export const MAX_ZOOM = 4;
const TAP_MAX_MOVE_PX = 4;
const TAP_MAX_DURATION_MS = 200;
const PAN_ACTIVATION_THRESHOLD_PX = 12;
const PAN_CURSOR_SPEED = 1.5;
const FORCE_TAP_THRESHOLD = 0.15;
const DOUBLE_TAP_WINDOW_MS = 300;
const TWO_FINGER_TAP_MAX_MOVE_PX = 12;
const TWO_FINGER_TAP_MAX_DURATION_MS = 260;
const THREE_FINGER_SCROLL_AXIS_LOCK_PX = 10;
const THREE_FINGER_SCROLL_STEP_PX = 32;

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

interface TwoFingerTapGesture {
  startTime: number;
  firstId: number;
  secondId: number;
  firstStartX: number;
  firstStartY: number;
  secondStartX: number;
  secondStartY: number;
  valid: boolean;
}

interface ThreeFingerScrollGesture {
  touchIds: [number, number, number];
  startMidX: number;
  startMidY: number;
  lastMidX: number;
  lastMidY: number;
  axis: "x" | "y" | null;
  carryX: number;
  carryY: number;
}

interface PinchGesture {
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

// Only midpoint deltas matter for the scroll gesture, so raw client
// coordinates are fine here.
function getThreeTouchMidpoint(
  first: Touch,
  second: Touch,
  third: Touch,
): Point {
  return {
    x: (first.clientX + second.clientX + third.clientX) / 3,
    y: (first.clientY + second.clientY + third.clientY) / 3,
  };
}

function getScrollTouchSet(
  touches: TouchList,
  ids: [number, number, number],
): [Touch, Touch, Touch] | null {
  const first = getTouchById(touches, ids[0]);
  const second = getTouchById(touches, ids[1]);
  const third = getTouchById(touches, ids[2]);
  return first && second && third ? [first, second, third] : null;
}

// Drain accumulated finger travel into wheel ticks, one per 32px step, and
// return the leftover carry.
function drainScrollCarry(carry: number, tick: (dir: 1 | -1) => void): number {
  let rest = carry;
  while (Math.abs(rest) >= THREE_FINGER_SCROLL_STEP_PX) {
    const dir = rest > 0 ? 1 : -1;
    tick(dir);
    rest -= dir * THREE_FINGER_SCROLL_STEP_PX;
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
  let pinchGesture: PinchGesture | null = null;
  let mouseGesture: MouseGesture | null = null;
  let dragAssist: DragAssistGesture | null = null;
  let twoFingerTap: TwoFingerTapGesture | null = null;
  let threeFingerScroll: ThreeFingerScrollGesture | null = null;
  // A gesture that broke down (e.g. a finger of a three-finger swipe lifted)
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
    return {
      width: Math.max(1, doc.clientWidth),
      height: Math.max(1, doc.clientHeight),
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

  function trackCursor(x: number, y: number): void {
    cursor = { x, y };
    hasCursor = true;
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
  // state changes (the remotex equivalent sent position + mask atomically).
  function movePointer(x: number, y: number, left: boolean): void {
    const clamped = clampCursorToRemote(x, y);
    trackCursor(clamped.x, clamped.y);
    deps.send({ type: "mouseMove", x: clamped.x, y: clamped.y });
    if (left !== leftHeld) {
      leftHeld = left;
      deps.send({ type: "mouseButton", button: "left", pressed: left });
    }
  }

  function sendTapClick(): void {
    const c = currentCursor();
    movePointer(c.x, c.y, true);
    movePointer(c.x, c.y, false);
  }

  function sendRightClick(): void {
    const c = currentCursor();
    deps.send({ type: "mouseMove", x: c.x, y: c.y });
    deps.send({ type: "mouseButton", button: "right", pressed: true });
    deps.send({ type: "mouseButton", button: "right", pressed: false });
  }

  // One scroll notch at the cursor; the server maps any nonzero delta to one
  // wheel tick, so only the sign carries meaning.
  function sendScrollTick(dx: number, dy: number): void {
    const c = currentCursor();
    deps.send({ type: "mouseMove", x: c.x, y: c.y });
    deps.send({ type: "wheel", dx, dy });
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
    threeFingerScroll = null;

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
    const isForceTap =
      !gesture.moved &&
      duration <= TAP_MAX_DURATION_MS &&
      gesture.maxForce >= FORCE_TAP_THRESHOLD;

    if (gesture.mode === "drag") {
      const c = currentCursor();
      movePointer(c.x, c.y, false);
      // A quick forceful second tap after the drag release doubles up into a
      // double-click.
      if (isForceTap) {
        sendTapClick();
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

  function beginTwoFingerTapGesture(first: Touch, second: Touch): void {
    twoFingerTap = {
      startTime: Date.now(),
      firstId: first.identifier,
      secondId: second.identifier,
      firstStartX: first.clientX,
      firstStartY: first.clientY,
      secondStartX: second.clientX,
      secondStartY: second.clientY,
      valid: true,
    };
  }

  // Still a right-click candidate? Any lifted/swapped finger or real movement
  // invalidates it for good.
  function updateTwoFingerTapGesture(touches: TouchList): boolean {
    if (!twoFingerTap || !twoFingerTap.valid) {
      return false;
    }
    if (touches.length !== 2) {
      twoFingerTap.valid = false;
      return false;
    }
    const first = getTouchById(touches, twoFingerTap.firstId);
    const second = getTouchById(touches, twoFingerTap.secondId);
    if (!first || !second) {
      twoFingerTap.valid = false;
      return false;
    }
    const firstMoved = Math.hypot(
      first.clientX - twoFingerTap.firstStartX,
      first.clientY - twoFingerTap.firstStartY,
    );
    const secondMoved = Math.hypot(
      second.clientX - twoFingerTap.secondStartX,
      second.clientY - twoFingerTap.secondStartY,
    );
    if (
      firstMoved > TWO_FINGER_TAP_MAX_MOVE_PX ||
      secondMoved > TWO_FINGER_TAP_MAX_MOVE_PX
    ) {
      twoFingerTap.valid = false;
      return false;
    }
    return true;
  }

  function startPinchGesture(first: Touch, second: Touch): void {
    const initialDistance = getTouchDistance(first, second);
    if (initialDistance <= 0) {
      return;
    }
    const midpoint = getTouchMidpoint(first, second);
    const view = deps.view();
    const scale = Math.max(0.0001, view.fit * view.zoom);
    pinchGesture = {
      initialDistance,
      initialZoom: view.zoom,
      // The remote point under the finger midpoint, kept there while zooming.
      anchorX: (midpoint.x - view.pan.x) / scale,
      anchorY: (midpoint.y - view.pan.y) / scale,
    };
  }

  // One pinch/pan frame: zoom from the distance ratio, pan from the midpoint
  // drift (so a constant-distance two-finger drag is a pure pan).
  function applyPinchMove(first: Touch, second: Touch): void {
    if (!pinchGesture) {
      return;
    }
    const distance = getTouchDistance(first, second);
    if (distance <= 0) {
      return;
    }
    const midpoint = getTouchMidpoint(first, second);
    const nextZoom = clampValue(
      pinchGesture.initialZoom * (distance / pinchGesture.initialDistance),
      MIN_ZOOM,
      MAX_ZOOM,
    );
    const scale = deps.view().fit * nextZoom;
    deps.applyView(nextZoom, {
      x: midpoint.x - pinchGesture.anchorX * scale,
      y: midpoint.y - pinchGesture.anchorY * scale,
    });
  }

  function startThreeFingerScrollGesture(touches: TouchList): void {
    if (touches.length < 3) {
      return;
    }
    const midpoint = getThreeTouchMidpoint(touches[0], touches[1], touches[2]);
    threeFingerScroll = {
      touchIds: [
        touches[0].identifier,
        touches[1].identifier,
        touches[2].identifier,
      ],
      startMidX: midpoint.x,
      startMidY: midpoint.y,
      lastMidX: midpoint.x,
      lastMidY: midpoint.y,
      axis: null,
      carryX: 0,
      carryY: 0,
    };
  }

  // Feed a movement of the three-finger midpoint into the scroll: the axis
  // locks after 10px of total travel, then every 32px of movement drains
  // into one wheel tick. Returns false when the touch set fell apart.
  function handleThreeFingerScrollMove(touches: TouchList): boolean {
    const scroll = threeFingerScroll;
    if (!scroll || touches.length < 3) {
      return false;
    }
    const touchSet = getScrollTouchSet(touches, scroll.touchIds);
    if (!touchSet) {
      threeFingerScroll = null;
      return false;
    }

    const midpoint = getThreeTouchMidpoint(...touchSet);
    const stepX = midpoint.x - scroll.lastMidX;
    const stepY = midpoint.y - scroll.lastMidY;
    scroll.lastMidX = midpoint.x;
    scroll.lastMidY = midpoint.y;

    if (!scroll.axis && !lockScrollAxis(scroll, midpoint)) {
      return true;
    }

    if (scroll.axis === "x") {
      scroll.carryX = drainScrollCarry(scroll.carryX + stepX, (dir) =>
        sendScrollTick(dir, 0),
      );
    } else {
      scroll.carryY = drainScrollCarry(scroll.carryY + stepY, (dir) =>
        sendScrollTick(0, dir),
      );
    }
    return true;
  }

  // Pick the scroll axis once the midpoint traveled far enough from its
  // start; returns false while still within the lock threshold.
  function lockScrollAxis(
    scroll: ThreeFingerScrollGesture,
    midpoint: Point,
  ): boolean {
    const totalX = midpoint.x - scroll.startMidX;
    const totalY = midpoint.y - scroll.startMidY;
    if (
      Math.abs(totalX) < THREE_FINGER_SCROLL_AXIS_LOCK_PX &&
      Math.abs(totalY) < THREE_FINGER_SCROLL_AXIS_LOCK_PX
    ) {
      return false;
    }
    scroll.axis = Math.abs(totalX) >= Math.abs(totalY) ? "x" : "y";
    return true;
  }

  // Shared prologue for all touch events: while a three-finger scroll is
  // active it owns every touch; once its touch set breaks, the leftover
  // fingers are swallowed until fully released. Returns true when the event
  // was consumed here.
  function continueThreeFingerScroll(e: TouchEvent): boolean {
    if (!threeFingerScroll) {
      return false;
    }
    if (e.touches.length === 3 && handleThreeFingerScrollMove(e.touches)) {
      consumeTouchEvent(e);
      return true;
    }
    threeFingerScroll = null;
    dragAssist = null;
    twoFingerTap = null;
    pinchGesture = null;
    ignoreSingleTouch = e.touches.length > 0;
    consumeTouchEvent(e);
    return true;
  }

  function finalizeMouseFromTouches(e: TouchEvent, suppressTap: boolean): void {
    if (!mouseGesture) {
      return;
    }
    const active =
      getTouchById(e.touches, mouseGesture.touchId) || e.touches[0] || null;
    finalizeMouseGesture(active, suppressTap);
  }

  // A third finger landed (or was noticed mid-move) outside a hold-drag: end
  // any mouse gesture without a click and start scrolling.
  function beginThreeFingerScroll(e: TouchEvent): void {
    finalizeMouseFromTouches(e, true);
    dragAssist = null;
    twoFingerTap = null;
    pinchGesture = null;
    startThreeFingerScrollGesture(e.touches);
    ignoreSingleTouch = true;
    consumeTouchEvent(e);
  }

  function handleTouchStart(e: TouchEvent): void {
    cancelPendingTap();
    if (continueThreeFingerScroll(e)) {
      return;
    }

    if (e.touches.length === 3 && mouseGesture?.mode !== "drag") {
      beginThreeFingerScroll(e);
      return;
    }

    if (e.touches.length >= 2) {
      handleMultiTouchStart(e);
      return;
    }

    twoFingerTap = null;
    if (ignoreSingleTouch) {
      consumeTouchEvent(e);
      return;
    }
    pinchGesture = null;
    beginMouseGesture(e.touches[0]);
    consumeTouchEvent(e);
  }

  function handleMultiTouchStart(e: TouchEvent): void {
    if (mouseGesture?.mode === "drag") {
      // Extra fingers during a hold-drag assist the cursor, they never
      // zoom/scroll.
      ignoreSingleTouch = false;
      twoFingerTap = null;
      pinchGesture = null;
      threeFingerScroll = null;
      const assist = getDragAssistTouch(e.touches);
      if (assist) {
        handleDragAssistMove(assist);
      }
      consumeTouchEvent(e);
      return;
    }
    finalizeMouseFromTouches(e, true);
    ignoreSingleTouch = true;
    if (e.touches.length === 2) {
      beginTwoFingerTapGesture(e.touches[0], e.touches[1]);
      startPinchGesture(e.touches[0], e.touches[1]);
    } else {
      twoFingerTap = null;
      pinchGesture = null;
    }
    consumeTouchEvent(e);
  }

  // Two-finger move outside a hold-drag: keep the right-click candidate alive
  // while the fingers stay put, otherwise pinch/pan.
  function handleTwoFingerMove(e: TouchEvent): void {
    finalizeMouseFromTouches(e, true);
    ignoreSingleTouch = true;
    if (e.touches.length !== 2) {
      twoFingerTap = null;
    } else {
      if (updateTwoFingerTapGesture(e.touches)) {
        consumeTouchEvent(e);
        return;
      }
      twoFingerTap = null;
    }
    const first = e.touches[0];
    const second = e.touches[1];
    if (!pinchGesture) {
      startPinchGesture(first, second);
    } else {
      applyPinchMove(first, second);
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
      twoFingerTap = null;
      pinchGesture = null;
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
    twoFingerTap = null;
    pinchGesture = null;
    threeFingerScroll = null;
    ignoreSingleTouch = false;
    consumeTouchEvent(e);
  }

  function handleTouchMove(e: TouchEvent): void {
    if (continueThreeFingerScroll(e)) {
      return;
    }

    if (e.touches.length === 3 && mouseGesture?.mode !== "drag") {
      beginThreeFingerScroll(e);
      return;
    }

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
    threeFingerScroll = null;
    if (
      twoFingerTap?.valid &&
      Date.now() - twoFingerTap.startTime <= TWO_FINGER_TAP_MAX_DURATION_MS
    ) {
      sendRightClick();
    }
    twoFingerTap = null;
    pinchGesture = null;
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
    twoFingerTap = null;
    pinchGesture = null;
    threeFingerScroll = null;
    consumeTouchEvent(e);
  }

  function handleTouchEnd(e: TouchEvent): void {
    if (continueThreeFingerScroll(e)) {
      return;
    }

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
    if (e.touches.length === 2) {
      updateTwoFingerTapGesture(e.touches);
      startPinchGesture(e.touches[0], e.touches[1]);
    } else {
      twoFingerTap = null;
      pinchGesture = null;
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
    updateTwoFingerTapGesture(e.touches);
    pinchGesture = null;
    consumeTouchEvent(e);
  }

  function release(): void {
    cancelPendingTap();
    mouseGesture = null;
    dragAssist = null;
    twoFingerTap = null;
    pinchGesture = null;
    threeFingerScroll = null;
    ignoreSingleTouch = false;
    if (leftHeld) {
      leftHeld = false;
      deps.send({ type: "mouseButton", button: "left", pressed: false });
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
      trackCursor(x, y);
    },
    release,
  };
}
