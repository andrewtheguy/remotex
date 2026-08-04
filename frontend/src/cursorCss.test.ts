// Which pointer a surface with no cursor image of its own should show. Two lines of
// code, and a test because getting them the wrong way round is invisible to every
// other check here: the page still runs, the desktop still draws, and the only
// symptom is a missing pointer on a screen this file never sees.
import assert from "node:assert/strict";
import { test } from "node:test";

import { bareCursorCss, cursorImage } from "./cursorCss.ts";

test("a desktop with no cursor image hides the local pointer", () => {
  // A VNC server that ignores the Cursor pseudo-encoding draws its pointer into
  // the framebuffer; showing the local one too would draw two.
  assert.equal(bareCursorCss(true), "none");
});

test("no desktop gives the pointer back", () => {
  // The regression this exists for: the canvas stays mounted under the target
  // picker, so a page still claiming `none` after `clear` left the picker with no
  // pointer at all. Hiding it is a property of a desktop being on screen, not of
  // the page.
  assert.equal(bareCursorCss(false), "default");
});

test("no remote cursor is what bareCursorCss then answers for", () => {
  // The two are a pair: `cursorImage` returning null is exactly the case
  // `paintCursor` hands to `bareCursorCss`, and getting that wiring backwards is
  // how the picker lost its pointer. (A `cursor` message carrying no image is the
  // opposite case — the remote saying it will *not* draw one — and it builds a
  // fallback, which needs a DOM and belongs to the browser tests.)
  assert.equal(cursorImage(null), null);
});
