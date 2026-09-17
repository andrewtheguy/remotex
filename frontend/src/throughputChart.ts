// Drawing one direction of the throughput meter: the points of a range on a
// canvas, an area under a line, a dot on the newest second, four grid lines with
// their rate at the right edge, and a gap where a point was not read. The rendering
// is the panel's (ThroughputPanel.tsx); this is the geometry and the strokes.

import { formatRate } from "./throughput.ts";

/// Room at the right edge for the scale's labels, in CSS pixels.
export const SCALE_WIDTH = 68;

const PAD_TOP = 6;
const PAD_BOTTOM = 4;

export interface ChartInk {
  line: string;
  fill: string;
  grid: string;
  label: string;
  surface: string;
}

export interface ChartLayout {
  width: number;
  height: number;
  /** A rate per point, oldest first; `null` for one no sample covers. */
  points: readonly (number | null)[];
  /** The rate at the top of the plot. */
  top: number;
}

/** The runs of read seconds in `points`, each as `[from, to)`. */
export function runs(points: readonly (number | null)[]): [number, number][] {
  const found: [number, number][] = [];
  let start = -1;
  for (let i = 0; i <= points.length; i++) {
    const read = i < points.length && points[i] !== null;
    if (read && start < 0) {
      start = i;
    } else if (!read && start >= 0) {
      found.push([start, i]);
      start = -1;
    }
  }
  return found;
}

/** The plot's x for the point at `index`, over `width` less the scale. */
export function plotX(index: number, count: number, width: number): number {
  return count > 1 ? (index / (count - 1)) * (width - SCALE_WIDTH) : 0;
}

function plotY(value: number, layout: ChartLayout): number {
  const plotHeight = layout.height - PAD_TOP - PAD_BOTTOM;
  return PAD_TOP + plotHeight - (value / layout.top) * plotHeight;
}

function drawGrid(
  ctx: CanvasRenderingContext2D,
  layout: ChartLayout,
  ink: ChartInk,
) {
  const plotWidth = layout.width - SCALE_WIDTH;
  ctx.strokeStyle = ink.grid;
  ctx.lineWidth = 1;
  ctx.fillStyle = ink.label;
  ctx.font = "11px ui-monospace, SFMono-Regular, Menlo, monospace";
  ctx.textAlign = "left";
  ctx.textBaseline = "middle";
  for (let g = 0; g <= 4; g++) {
    const value = (layout.top * g) / 4;
    const y = Math.round(plotY(value, layout)) + 0.5;
    ctx.beginPath();
    ctx.moveTo(0, y);
    ctx.lineTo(plotWidth, y);
    ctx.stroke();
    if (g > 0) {
      ctx.fillText(formatRate(value), plotWidth + 6, y);
    }
  }
}

/// The line through the read seconds `from` up to `to`, as the current path.
function trace(
  ctx: CanvasRenderingContext2D,
  layout: ChartLayout,
  from: number,
  to: number,
) {
  ctx.beginPath();
  for (let i = from; i < to; i++) {
    const x = plotX(i, layout.points.length, layout.width);
    const y = plotY(layout.points[i] as number, layout);
    if (i === from) {
      ctx.moveTo(x, y);
    } else {
      ctx.lineTo(x, y);
    }
  }
}

/// One run of read seconds: its area, then its line over it.
function drawRun(
  ctx: CanvasRenderingContext2D,
  layout: ChartLayout,
  ink: ChartInk,
  from: number,
  to: number,
) {
  const count = layout.points.length;
  trace(ctx, layout, from, to);
  ctx.lineTo(plotX(to - 1, count, layout.width), plotY(0, layout));
  ctx.lineTo(plotX(from, count, layout.width), plotY(0, layout));
  ctx.closePath();
  ctx.fillStyle = ink.fill;
  ctx.fill();
  trace(ctx, layout, from, to);
  ctx.strokeStyle = ink.line;
  ctx.lineWidth = 2;
  ctx.lineJoin = "round";
  ctx.stroke();
}

/// The newest second, when it was read: a dot ringed in the surface.
function drawNewest(
  ctx: CanvasRenderingContext2D,
  layout: ChartLayout,
  ink: ChartInk,
) {
  const count = layout.points.length;
  const last = layout.points[count - 1];
  if (last === null || last === undefined) {
    return;
  }
  ctx.beginPath();
  ctx.arc(
    plotX(count - 1, count, layout.width),
    plotY(last, layout),
    4,
    0,
    Math.PI * 2,
  );
  ctx.fillStyle = ink.line;
  ctx.fill();
  ctx.lineWidth = 2;
  ctx.strokeStyle = ink.surface;
  ctx.stroke();
}

/** Draws the chart over a cleared `ctx` scaled to CSS pixels. */
export function drawChart(
  ctx: CanvasRenderingContext2D,
  layout: ChartLayout,
  ink: ChartInk,
) {
  ctx.clearRect(0, 0, layout.width, layout.height);
  drawGrid(ctx, layout, ink);
  for (const [from, to] of runs(layout.points)) {
    drawRun(ctx, layout, ink, from, to);
  }
  drawNewest(ctx, layout, ink);
}

/** The point under a pointer `px` from the plot's left, or `null` beside it. */
export function pointedIndex(
  px: number,
  count: number,
  width: number,
): number | null {
  const plotWidth = width - SCALE_WIDTH;
  if (px < 0 || px > plotWidth) {
    return null;
  }
  return Math.round((px / plotWidth) * (count - 1));
}

/**
 * Where the tooltip for the point at `index` sits: over it, kept inside the plot,
 * `half` being the room each side of its middle its text takes.
 */
export function tipLeft(
  index: number,
  count: number,
  width: number,
  half = 60,
): number {
  const plotWidth = width - SCALE_WIDTH;
  return Math.min(Math.max(plotX(index, count, width), half), plotWidth - half);
}
