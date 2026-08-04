import assert from "node:assert/strict";
import { test } from "node:test";
import { createRectCache } from "./pointerRect.ts";

// A target whose rect moves on every measurement, counting how often it is
// actually measured — which is the whole question here.
function movingTarget() {
  let reads = 0;
  return {
    reads: () => reads,
    target: {
      getBoundingClientRect() {
        reads += 1;
        return { left: reads * 100, top: 0, width: 50, height: 50 } as DOMRect;
      },
    } as Element,
  };
}

test("pointer events within one frame share a single measurement", () => {
  const { reads, target } = movingTarget();
  const cache = createRectCache(() => {});
  const first = cache.read(target);
  const second = cache.read(target);
  assert.equal(reads(), 1);
  assert.equal(second.left, first.left);
});

test("a geometry change between two pointer events yields a fresh rect", () => {
  const { reads, target } = movingTarget();
  const cache = createRectCache(() => {});
  const before = cache.read(target);
  // A resize message, a scroll, or a zoom/pan landed in the same frame.
  cache.invalidate();
  const after = cache.read(target);
  assert.equal(reads(), 2);
  assert.notEqual(after.left, before.left);
});

test("the frame boundary clears the cache as the backstop", () => {
  const clears: (() => void)[] = [];
  const { reads, target } = movingTarget();
  const cache = createRectCache((clear) => clears.push(clear));
  cache.read(target);
  assert.equal(clears.length, 1);
  clears[0]();
  cache.read(target);
  assert.equal(reads(), 2);
});

test("an invalidated read schedules its own clear", () => {
  const clears: (() => void)[] = [];
  const { target } = movingTarget();
  const cache = createRectCache((clear) => clears.push(clear));
  cache.read(target);
  cache.invalidate();
  cache.read(target);
  assert.equal(clears.length, 2);
});
