// What two fingers mean. The gesture layer decides once per two-finger drag
// whether the fingers are pinching or scrolling, and the decision holds for the
// rest of the gesture; a two-finger tap still has to survive both.
//
// Driven through a stand-in element and document, since the decision is
// arithmetic on touch coordinates and needs no layout.
//
// Run with `bun test src/touchGestures.test.ts` from frontend/.
import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import type { ClientMsg } from "./protocol.ts";
import {
  attachTouchGestures,
  type GestureView,
  type TouchGestures,
} from "./touchGestures.ts";

type Handler = (e: TouchEvent) => void;

const REMOTE = { w: 2000, h: 1000 };

let sent: ClientMsg[] = [];
let view: GestureView;
let gestures: TouchGestures;
const handlers = new Map<string, Handler>();

const element = {
  addEventListener(type: string, handler: Handler) {
    handlers.set(type, handler);
  },
  removeEventListener(type: string) {
    handlers.delete(type);
  },
  getBoundingClientRect: () => ({ left: 0, top: 0 }),
} as unknown as HTMLElement;

// The overlay reads the viewport through the document; nothing else does.
Object.assign(globalThis, {
  document: { documentElement: { clientWidth: 400, clientHeight: 800 } },
});

const touch = (identifier: number, clientX: number, clientY: number) =>
  ({ identifier, clientX, clientY, force: 0 }) as unknown as Touch;

function dispatch(
  type: "touchstart" | "touchmove" | "touchend",
  touches: Touch[],
  changed: Touch[] = touches,
): void {
  handlers.get(type)?.({
    touches: touches as unknown as TouchList,
    changedTouches: changed as unknown as TouchList,
    preventDefault() {},
    stopImmediatePropagation() {},
  } as unknown as TouchEvent);
}

const wheels = () => sent.filter((msg) => msg.type === "wheel");

// Two fingers, 50px apart, moved together by the given offsets in turn.
function twoFingerDrag(steps: readonly { dx: number; dy: number }[]): void {
  dispatch("touchstart", [touch(1, 100, 400), touch(2, 150, 400)]);
  for (const step of steps) {
    dispatch("touchmove", [
      touch(1, 100 + step.dx, 400 + step.dy),
      touch(2, 150 + step.dx, 400 + step.dy),
    ]);
  }
}

beforeEach(() => {
  sent = [];
  view = { fit: 0.2, zoom: 1, pan: { x: 0, y: 0 } };
  handlers.clear();
  gestures = attachTouchGestures(element, {
    send: (msg) => sent.push(msg),
    remoteSize: () => REMOTE,
    view: () => view,
    applyView: (zoom, pan) => {
      view = { fit: view.fit, zoom, pan };
    },
  });
  gestures.notePointer(1000, 500);
});

test("fingers moving in parallel scroll, in the natural direction", () => {
  // 12px classifies the gesture as a scroll and names its axis; the 32px that
  // follow are the first tick's worth of travel.
  twoFingerDrag([
    { dx: 0, dy: 12 },
    { dx: 0, dy: 44 },
  ]);
  assert.deepEqual(wheels(), [
    { type: "wheel", dx: 0, dy: -32, unit: "pixel" },
  ]);
  assert.equal(view.zoom, 1, "a scroll never zooms");
});

test("a sideways drag scrolls sideways", () => {
  twoFingerDrag([
    { dx: 12, dy: 0 },
    { dx: 44, dy: 0 },
  ]);
  assert.deepEqual(wheels(), [
    { type: "wheel", dx: -32, dy: 0, unit: "pixel" },
  ]);
});

test("a diagonal drag locks onto the axis it was classified on", () => {
  // Mostly downwards, then hard to the right: the sideways travel arrives after
  // the lock and never reaches the wire as a horizontal tick.
  twoFingerDrag([
    { dx: 4, dy: 12 },
    { dx: 60, dy: 44 },
  ]);
  assert.deepEqual(wheels(), [
    { type: "wheel", dx: 0, dy: -32, unit: "pixel" },
  ]);
});

test("fingers changing their distance pinch, and never scroll", () => {
  dispatch("touchstart", [touch(1, 100, 400), touch(2, 150, 400)]);
  dispatch("touchmove", [touch(1, 80, 400), touch(2, 170, 400)]);
  dispatch("touchmove", [touch(1, 50, 400), touch(2, 200, 400)]);
  assert.ok(view.zoom > 1, `expected a zoom, got ${view.zoom}`);
  assert.deepEqual(wheels(), []);
});

test("a scroll that drifts apart stays a scroll", () => {
  dispatch("touchstart", [touch(1, 100, 400), touch(2, 150, 400)]);
  dispatch("touchmove", [touch(1, 100, 412), touch(2, 150, 412)]);
  // The same downward travel, with the fingers spreading as they go.
  dispatch("touchmove", [touch(1, 60, 444), touch(2, 190, 444)]);
  assert.deepEqual(wheels(), [
    { type: "wheel", dx: 0, dy: -32, unit: "pixel" },
  ]);
  assert.equal(view.zoom, 1);
});

test("two fingers that stay put are still a right-click", () => {
  dispatch("touchstart", [touch(1, 100, 400), touch(2, 150, 400)]);
  dispatch("touchmove", [touch(1, 102, 401), touch(2, 151, 400)]);
  dispatch("touchend", [touch(2, 151, 400)], [touch(1, 102, 401)]);
  dispatch("touchend", [], [touch(2, 151, 400)]);
  assert.deepEqual(
    sent.filter((msg) => msg.type === "mouseButton"),
    [
      { type: "mouseButton", button: "right", pressed: true, clicks: 1 },
      { type: "mouseButton", button: "right", pressed: false, clicks: 1 },
    ],
  );
});

test("a scroll releases without clicking anything", () => {
  twoFingerDrag([
    { dx: 0, dy: 12 },
    { dx: 0, dy: 44 },
  ]);
  dispatch("touchend", [touch(2, 150, 444)], [touch(1, 100, 444)]);
  dispatch("touchend", [], [touch(2, 150, 444)]);
  assert.deepEqual(
    sent.filter((msg) => msg.type === "mouseButton"),
    [],
  );
});
