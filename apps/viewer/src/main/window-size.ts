// The window's remembered size: what the next launch opens at.
//
// Size only, no position — the OS places windows well enough, and a saved
// position is how a window comes back on a display that has since been
// unplugged.
//
// The floor here is on the *remembered* value, never on the live window: the
// window itself may still be squeezed down to `MINIMUM_SIZE` (window.ts) while
// it is up, but what is written down — and therefore what a launch opens at —
// is never smaller than 800×600. A session that ended in a sliver should not
// start the next one in one.

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

export interface WindowSize {
  width: number;
  height: number;
}

/** What a first launch opens at, before there is anything to remember. */
export const DEFAULT_WINDOW_SIZE: WindowSize = { width: 1280, height: 800 };

/** The smallest size worth writing down or opening at. */
export const REMEMBERED_MINIMUM: WindowSize = { width: 800, height: 600 };

/** Whole pixels, raised to the remembered floor. */
export function clampRemembered(size: WindowSize): WindowSize {
  return {
    width: Math.max(Math.round(size.width), REMEMBERED_MINIMUM.width),
    height: Math.max(Math.round(size.height), REMEMBERED_MINIMUM.height),
  };
}

/**
 * Parse persisted text into a remembered size, or nothing.
 *
 * Anything that is not two positive finite numbers is no memory at all — a
 * torn or hand-edited file must never become a window with `NaN` bounds. What
 * does parse is clamped on the way in as well as on the way out, so the floor
 * holds even over a file somebody edited.
 */
export function rememberedSizeFrom(text: string): WindowSize | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null) {
    return null;
  }
  const { width, height } = parsed as Record<string, unknown>;
  const finitePositive = (value: unknown): value is number =>
    typeof value === "number" && Number.isFinite(value) && value > 0;
  if (!finitePositive(width) || !finitePositive(height)) {
    return null;
  }
  return clampRemembered({ width, height });
}

export class WindowSizeStore {
  constructor(private readonly path: string) {}

  /** The remembered size, or `null` when nothing usable was remembered. */
  read(): WindowSize | null {
    try {
      return rememberedSizeFrom(readFileSync(this.path, "utf8"));
    } catch {
      return null;
    }
  }

  /**
   * Write the size down, raised to the floor.
   *
   * Best effort in both directions: a size that cannot be written costs the
   * next launch its memory, which is not worth taking a live session down for.
   */
  remember(size: WindowSize): void {
    try {
      mkdirSync(dirname(this.path), { recursive: true });
      writeFileSync(this.path, JSON.stringify(clampRemembered(size)));
    } catch {
      // The next launch opens at the default, which is where it started.
    }
  }
}
