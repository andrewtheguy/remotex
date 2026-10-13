// The smallest display a Mac is asked for, in points: the smallest window
// Apple's own Screen Sharing client allows, at 1x and at 2x alike. A Mac asked
// for less sends no picture of some sizes — a display too low for the strips its
// High Performance picture is sent in, and at 2x one it answers with its own
// minimum and then captures nothing of. Its own client never asks, so neither
// does this one: the bound is put on what the page reports, where the window
// becomes a size, and the gateway passes on what it is told. A desktop larger
// than the window scrolls, as every desktop does.
export const MAC_MINIMUM = { w: 800, h: 600 } as const;

// The size to report for a window of `size` points to a remote that is a Mac
// or is not: the window's, and for a Mac no less than `MAC_MINIMUM` on either
// side.
export function reportedSize(
  size: { w: number; h: number },
  mac: boolean,
): { w: number; h: number } {
  if (!mac) {
    return size;
  }
  return {
    w: Math.max(MAC_MINIMUM.w, size.w),
    h: Math.max(MAC_MINIMUM.h, size.h),
  };
}

// Whether a session's target is a Mac, by the `subtype` its `connected`
// carries: the two Apple ones are.
export function macSubtype(subtype: string | null): boolean {
  return subtype === "ard" || subtype === "ard-high-performance";
}
