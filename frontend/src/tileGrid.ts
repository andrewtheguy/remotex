// The gateway's tile lattice, drawn over the desktop for `render_grid_debug`.
//
// This is the one render debug aid the client draws. The other two —
// `render_motion_debug` and `render_classify_debug` — outline a *decision* the
// encoder made about one tile, so they belong in that tile's own pixels and
// travel with it. The lattice is not a decision: it is fixed to the framebuffer
// while the pixels are not. A COPY record slides pixels sideways, a cached tile
// is one bitmap redrawn wherever the server names, and a corner nothing has
// touched since the last repaint is never sent again — so lines baked into tiles
// would shear off with the first scroll and never reach the still parts at all.
// Drawn on a canvas of its own, over the framebuffer, the grid is exact
// everywhere, complete from the first frame, and costs the encoders nothing.
//
// The canvas keeps the framebuffer's own bitmap size, so nothing here is redrawn
// when the CSS box changes: a pinch zoom or a density difference scales the
// finished lines the same way it scales the desktop under them, and the lines
// stay on the boundaries they mark.

import type { CanvasSize } from "./desktopCanvas.ts";

/** The lattice pitch in framebuffer pixels, as `connected` states it. */
export interface GridPitch {
  w: number;
  h: number;
}

// Dash and gap in framebuffer pixels at density 1. Long enough to read as a
// deliberate line rather than as noise, short enough that a dash and the gap
// beside it both fall inside one 320x64 cell edge.
const DASH = 8;

// Every line is drawn twice, dark then light, each in the other's gaps, so one
// pattern is legible over a white document and a black terminal alike — the
// contrast is between the two dashes rather than between a dash and whatever
// happens to be behind it. Kept below full opacity: this is drawn over the
// desktop being inspected, not instead of it.
const INK = ["rgba(0, 0, 0, 0.65)", "rgba(255, 255, 255, 0.75)"] as const;

/**
 * Where the lattice lines fall inside `size`, in framebuffer pixels.
 *
 * Interior lines only: the framebuffer's own edges are cell boundaries too, but
 * they are already drawn by the edge of the desktop, and half of such a line
 * would fall outside the canvas. A pitch that is not a positive finite number
 * draws nothing rather than looping forever on it.
 */
export function tileGridLines(
  size: CanvasSize,
  pitch: GridPitch,
): { xs: number[]; ys: number[] } {
  const interior = (extent: number, step: number) => {
    const out: number[] = [];
    if (!(Number.isFinite(step) && step >= 1)) {
      return out;
    }
    for (let at = step; at < extent; at += step) {
      out.push(at);
    }
    return out;
  };
  return { xs: interior(size.w, pitch.w), ys: interior(size.h, pitch.h) };
}

/**
 * Paint the lattice on `canvas`, sized to the framebuffer `size` describes.
 *
 * `scale` is the remote's pixel density, so a 2x desktop — presented at half its
 * bitmap size in CSS pixels — gets lines twice as thick and dashes twice as long,
 * and both come out the same size on screen as they do on a 1x one.
 */
export function drawTileGrid(
  canvas: HTMLCanvasElement,
  size: CanvasSize & { scale: number },
  pitch: GridPitch,
): void {
  // Assigning either dimension resets the bitmap and the context state with it,
  // so this both clears the previous grid and is why every context property
  // below is set after it rather than once.
  canvas.width = size.w;
  canvas.height = size.h;
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return;
  }
  const density = size.scale > 0 ? size.scale : 1;
  const { xs, ys } = tileGridLines(size, pitch);
  ctx.lineWidth = density;
  const period = DASH * density;
  ctx.setLineDash([period, period]);
  for (const [i, ink] of INK.entries()) {
    // The second pass starts one dash along, so it lands in the first's gaps.
    ctx.lineDashOffset = i * period;
    ctx.strokeStyle = ink;
    ctx.beginPath();
    for (const x of xs) {
      // Centred half a line-width into the cell the boundary opens, so the line
      // covers whole pixels instead of straddling the boundary and blurring
      // across the two cells it separates.
      ctx.moveTo(x + density / 2, 0);
      ctx.lineTo(x + density / 2, size.h);
    }
    for (const y of ys) {
      ctx.moveTo(0, y + density / 2);
      ctx.lineTo(size.w, y + density / 2);
    }
    ctx.stroke();
  }
}

/** Drop the lattice: a target without `render_grid_debug` shows no overlay. */
export function clearTileGrid(canvas: HTMLCanvasElement): void {
  canvas.width = 0;
  canvas.height = 0;
}
