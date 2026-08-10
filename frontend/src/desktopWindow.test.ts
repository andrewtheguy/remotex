import assert from "node:assert/strict";
import { test } from "node:test";
import {
  desktopViewportSize,
  outerSizeForViewport,
  type ResizableWindow,
  sizeWindowToDesktop,
} from "./desktopWindow.ts";

test("a Retina framebuffer requests its logical desktop size", () => {
  assert.deepEqual(desktopViewportSize({ w: 3840, h: 2160 }, 2), {
    w: 1920,
    h: 1080,
  });
});

test("a fractional remote point is rounded outward rather than clipped", () => {
  assert.deepEqual(desktopViewportSize({ w: 1366, h: 768 }, 1.25), {
    w: 1093,
    h: 615,
  });
});

test("the requested outer size preserves the live browser frame", () => {
  const target: ResizableWindow = {
    innerWidth: 1200,
    innerHeight: 700,
    outerWidth: 1216,
    outerHeight: 788,
    resizeTo: () => {},
  };

  assert.deepEqual(outerSizeForViewport({ w: 1920, h: 1080 }, target), {
    w: 1936,
    h: 1168,
  });
});

test("sizing a window requests the remote logical viewport plus its frame", () => {
  let requested: { w: number; h: number } | null = null;
  const target: ResizableWindow = {
    innerWidth: 800,
    innerHeight: 600,
    outerWidth: 816,
    outerHeight: 688,
    resizeTo: (w, h) => {
      requested = { w, h };
    },
  };

  sizeWindowToDesktop({ w: 3200, h: 1800 }, 2, target);

  assert.deepEqual(requested, { w: 1616, h: 988 });
});
