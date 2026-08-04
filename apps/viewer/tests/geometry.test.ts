import { describe, expect, test } from "bun:test";
import { documentSize, windowFrameFitting } from "../src/main/geometry.ts";

const screen = { x: 0, y: 0, width: 2560, height: 1440 };
const minimum = { width: 720, height: 480 };

describe("the remote's logical size", () => {
  test("a 1× guest draws one pixel per point", () => {
    expect(documentSize({ w: 1920, h: 1080, scale: 1 })).toEqual({
      width: 1920,
      height: 1080,
    });
  });

  test("a Retina guest is half the pixels it sends", () => {
    // 3840×2160 at scale 2 is a 1920×1080 desktop at full pixel fidelity, which is
    // the thing the window has to make room for.
    expect(documentSize({ w: 3840, h: 2160, scale: 2 })).toEqual({
      width: 1920,
      height: 1080,
    });
  });

  test("a nonsense scale is treated as 1 rather than dividing by zero", () => {
    expect(documentSize({ w: 800, h: 600, scale: 0 })).toEqual({
      width: 800,
      height: 600,
    });
  });
});

describe("fitting the window", () => {
  const window = { x: 100, y: 100, width: 1000, height: 800 };
  // 28 points of title bar: the chrome is measured, never assumed.
  const content = { width: 1000, height: 772 };

  test("the document gets exactly its own size, plus the chrome", () => {
    const frame = windowFrameFitting(
      { width: 1280, height: 800 },
      content,
      window,
      screen,
      minimum,
    );
    expect(frame.width).toBe(1280);
    expect(frame.height).toBe(828);
  });

  test("the top-left corner stays where it was", () => {
    const frame = windowFrameFitting(
      { width: 1280, height: 800 },
      content,
      window,
      screen,
      minimum,
    );
    expect(frame.x).toBe(100);
    expect(frame.y).toBe(100);
  });

  test("a desktop bigger than this screen gets the screen, and scrolls the rest", () => {
    // Never a scaled-down picture: pointer clients show desktops at 100%.
    const frame = windowFrameFitting(
      { width: 3840, height: 2160 },
      content,
      window,
      screen,
      minimum,
    );
    expect(frame.width).toBe(screen.width);
    expect(frame.height).toBe(screen.height);
    expect(frame.x).toBe(0);
    expect(frame.y).toBe(0);
  });

  test("a tiny desktop still gets a usable window", () => {
    // A size the window manager would refuse is not a size to return.
    const frame = windowFrameFitting(
      { width: 320, height: 200 },
      content,
      window,
      screen,
      minimum,
    );
    expect(frame.width).toBe(minimum.width);
    expect(frame.height).toBe(minimum.height);
  });

  test("a window that would fall off the bottom is pulled back on", () => {
    const low = { x: 2000, y: 1300, width: 1000, height: 800 };
    const frame = windowFrameFitting(
      { width: 1280, height: 800 },
      content,
      low,
      screen,
      minimum,
    );
    expect(frame.x + frame.width).toBeLessThanOrEqual(screen.width);
    expect(frame.y + frame.height).toBeLessThanOrEqual(screen.height);
  });

  test("a window mid-layout is left exactly as it is", () => {
    // No room measured yet, or a measurement that is not a number: leaving it alone
    // is the honest answer, and the one that cannot move a window somewhere odd.
    expect(
      windowFrameFitting(
        { width: 1280, height: 800 },
        { width: 0, height: 0 },
        window,
        screen,
        minimum,
      ),
    ).toEqual(window);
    expect(
      windowFrameFitting(
        { width: Number.NaN, height: 800 },
        content,
        window,
        screen,
        minimum,
      ),
    ).toEqual(window);
  });
});
