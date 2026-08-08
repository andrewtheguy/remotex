// Resize to display, as arithmetic.
//
// Pure, and three lines over `shared/geometry.ts`, so the clamping, the preserved
// top-left corner and the "a window mid-layout is left alone" guard are the viewer's
// rather than a second implementation of them.
//
// The one thing that is not the viewer's problem is **browser zoom**, and it is the
// only subtle part of this file. `innerWidth` is CSS pixels *at the current zoom*,
// while `outerWidth` and `chrome.windows.update` are device-independent pixels, which
// zoom does not touch. So a framebuffer presented at `w/scale` CSS pixels needs
// `w/scale * zoom` DIPs of content, and the window's chrome is
// `outerWidth - innerWidth * zoom`. Everything below is in DIPs once it has been
// through that.
//
// At any zoom but 1 the desktop is not at 100% however the window is sized. Nothing
// here corrects for that silently and nothing may: the popup says so, and an app
// window delivers Ctrl+0 to the page, which is the user's own way back.

import type { HostRemoteSize } from "./contract.ts";
import {
  documentSize,
  type Rect,
  type Size,
  windowFrameFitting,
} from "./geometry.ts";

/** What the content script measures. Nothing here is asked of the page. */
export interface ViewportMetrics {
  innerWidth: number;
  innerHeight: number;
  outerWidth: number;
  outerHeight: number;
  availLeft: number;
  availTop: number;
  availWidth: number;
  availHeight: number;
}

/**
 * A window Chrome will refuse to make smaller, so asking for less would describe a
 * window nobody gets. Matches the viewer's own floor.
 */
export const MINIMUM: Size = { width: 500, height: 300 };

/**
 * The bounds to give `chrome.windows.update`, or null if there is nothing to say.
 *
 * Null rather than the current bounds when a measurement is missing or not finite: the
 * caller skips the update entirely, which is quieter than asking Chrome to move a
 * window to where it already is.
 */
export function windowBoundsForRemote(input: {
  remote: HostRemoteSize;
  metrics: ViewportMetrics;
  zoom: number;
  current: Rect;
}): Rect | null {
  const { remote, metrics, current } = input;
  const zoom = input.zoom > 0 ? input.zoom : 1;
  const logical = documentSize(remote);
  const document: Size = {
    width: logical.width * zoom,
    height: logical.height * zoom,
  };
  const content: Size = {
    width: metrics.innerWidth * zoom,
    height: metrics.innerHeight * zoom,
  };
  const limit: Rect = {
    x: metrics.availLeft,
    y: metrics.availTop,
    width: metrics.availWidth,
    height: metrics.availHeight,
  };
  const fitted = windowFrameFitting(document, content, current, limit, MINIMUM);
  // `windowFrameFitting` returns the window unchanged when it cannot answer, which is
  // the right thing for a window manager and the wrong thing for a message: an update
  // to the current bounds is a request that does nothing.
  return same(fitted, current) ? null : fitted;
}

function same(a: Rect, b: Rect): boolean {
  return (
    a.x === b.x && a.y === b.y && a.width === b.width && a.height === b.height
  );
}
