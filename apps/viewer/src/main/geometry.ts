// Pure arithmetic between window points and remote pixels.
//
// **Resize to Display** is the only thing here, and it is local: nothing is sent for
// it, so it needs no permission from the gateway. The remote reports how many of its
// own pixels it draws per point of its own desktop — 1 for VNC, RDP and a 1× Mac,
// 2 for a Retina one — and the client presents the framebuffer at `w / scale` by
// `h / scale` CSS pixels. Fitting the window means making room for exactly that.
//
// Note what is *not* here: no fit-to-window, no zoom-to-fit, no viewport-derived
// scaling. Desktops are shown at 100% and an oversized one scrolls. Whether the
// remote renders at this window's size is the gateway's per-target `resize`
// policy, carried by the page's own viewport reports — a message, not arithmetic.

export interface Size {
  width: number;
  height: number;
}

export interface Rect extends Size {
  x: number;
  y: number;
}

/** The remote's logical size, in points, independent of this Mac's display. */
export function documentSize(remote: {
  w: number;
  h: number;
  scale: number;
}): Size {
  const scale = remote.scale > 0 ? remote.scale : 1;
  return { width: remote.w / scale, height: remote.h / scale };
}

/**
 * Fit a window around a document of `document` points.
 *
 * `content` is how much room the window currently gives the page, so the difference
 * between it and the window is the chrome, measured rather than assumed. The
 * top-left corner is preserved, the result is kept on the screen it is on, and it is
 * never smaller than the window's own minimum — a size the window manager would
 * refuse is not a size to return.
 *
 * A window mid-layout, or one whose measurements are not finite, is left exactly as
 * it is: that is the honest answer, and it is also the one that cannot move a window
 * somewhere surprising.
 */
export function windowFrameFitting(
  document: Size,
  content: Size,
  window: Rect,
  limit: Rect,
  minimum: Size,
): Rect {
  const measurements = [
    document.width,
    document.height,
    content.width,
    content.height,
    window.x,
    window.y,
    window.width,
    window.height,
    limit.x,
    limit.y,
    limit.width,
    limit.height,
    minimum.width,
    minimum.height,
  ];
  if (
    document.width < 1 ||
    document.height < 1 ||
    content.width < 1 ||
    content.height < 1 ||
    !measurements.every(Number.isFinite)
  ) {
    return window;
  }
  const width = fit(
    document.width + window.width - content.width,
    minimum.width,
    limit.width,
  );
  const height = fit(
    document.height + window.height - content.height,
    minimum.height,
    limit.height,
  );
  return {
    width,
    height,
    x: clamp(window.x, limit.x, limit.x + limit.width - width),
    y: clamp(window.y, limit.y, limit.y + limit.height - height),
  };
}

/**
 * `least` wins a fight with `most`: a window the system will not shrink past its own
 * minimum is the size that actually results, so returning anything smaller would
 * describe a window nobody gets.
 */
function fit(value: number, least: number, most: number): number {
  return Math.min(Math.max(Math.round(value), least), Math.max(most, least));
}

function clamp(value: number, least: number, most: number): number {
  return Math.max(least, Math.min(value, Math.max(least, most)));
}
