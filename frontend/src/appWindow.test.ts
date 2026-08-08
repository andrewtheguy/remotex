// Which windows count as app windows, which is the switch under three behaviours: the
// Command chord table (macKeys.ts), the companion seam being live at all
// (companion.ts), and what the Immersive button claims it will do.
import assert from "node:assert/strict";
import { test } from "node:test";
import { isAppWindow } from "./appWindow.ts";

/** A window whose display mode is exactly one of these. */
function inMode(mode: string) {
  return (query: string) => ({ matches: query.includes(`: ${mode})`) });
}

test("the three app display modes are app windows", () => {
  for (const mode of ["standalone", "minimal-ui", "window-controls-overlay"]) {
    assert.equal(isAppWindow(inMode(mode)), true, mode);
  }
});

test("a tab is not, in a window or full screen", () => {
  assert.equal(isAppWindow(inMode("browser")), false);
  // The trap this is written as an allow-list to avoid. A plain tab reports
  // `display-mode: fullscreen` the moment it goes full screen, and that window is given
  // no chords at all — it is the keyboard lock beneath it that changes a tab's answer,
  // not the fullscreen. Read as "not browser", this would have said yes.
  assert.equal(isAppWindow(inMode("fullscreen")), false);
});

test("a browser that answers nothing is not an app window", () => {
  assert.equal(
    isAppWindow(() => ({ matches: false })),
    false,
  );
});
