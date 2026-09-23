// A Mac's combined view of screens at different densities, composed here.
//
// Apple Screen Sharing sends that view as one framebuffer holding each screen's
// own pixels: a 1x screen beside a 2x one arrives as 1280 + 2880 pixels across.
// No single density presents it, and the Mac cannot render one framebuffer at
// two, so the gateway sends a `mosaic` naming each screen's pixels and its
// points, and the page draws every region at its points at this browser
// display's own density — what Apple's viewer does with the same view. It is
// the one place remote pixels are rescaled in the browser (AGENTS.md).
//
// The composed canvas is then presented like any framebuffer: `w`/`h` in
// pixels at `scale` of them per point. Everything downstream — the CSS box,
// the pointer's rect mapping, the cursor's size — reads that, so only the last
// step of a pointer position, back into the Mac's pixels, knows about regions.
import type { ClientMsg, MosaicRegion } from "./protocol.ts";

/** One region's copy: from framebuffer pixels to composed-canvas pixels. */
export interface MosaicDraw {
  sx: number;
  sy: number;
  sw: number;
  sh: number;
  dx: number;
  dy: number;
  dw: number;
  dh: number;
}

/** The composed canvas for one set of regions at one browser density. */
export interface MosaicView {
  w: number;
  h: number;
  /** Composed pixels per point: the browser display's density. */
  scale: number;
  draws: MosaicDraw[];
}

export function mosaicView(
  regions: MosaicRegion[],
  density: number,
): MosaicView {
  const scale = Number.isFinite(density) && density > 0 ? density : 1;
  const at = (points: number) => Math.round(points * scale);
  const draws = regions.map(({ pixels, points }) => {
    const dx = at(points.x);
    const dy = at(points.y);
    return {
      sx: pixels.x,
      sy: pixels.y,
      sw: pixels.w,
      sh: pixels.h,
      dx,
      dy,
      // Edges from rounded corners, so neighbours share their boundary pixel.
      dw: at(points.x + points.w) - dx,
      dh: at(points.y + points.h) - dy,
    };
  });
  return {
    w: Math.max(1, ...draws.map((d) => d.dx + d.dw)),
    h: Math.max(1, ...draws.map((d) => d.dy + d.dh)),
    scale,
    draws,
  };
}

/**
 * A composed-canvas position as the Mac addresses it: a pixel of its combined
 * framebuffer, or null between screens. Apple's viewer hit-tests the screens in
 * order, each rect widened by one pixel on its far edges, and sends nothing for
 * a point outside all of them (`frameBufferCoordinatesFromWindowCoordinates:`).
 */
export function mosaicToFramebuffer(
  view: MosaicView,
  x: number,
  y: number,
): { x: number; y: number } | null {
  const d = view.draws.find(
    (d) =>
      d.dw > 0 &&
      d.dh > 0 &&
      x >= d.dx &&
      x <= d.dx + d.dw &&
      y >= d.dy &&
      y <= d.dy + d.dh,
  );
  if (!d) {
    return null;
  }
  const clamp = (v: number, lo: number, hi: number) =>
    Math.min(Math.max(v, lo), hi);
  return {
    x: clamp(
      Math.round(d.sx + ((x - d.dx) * d.sw) / d.dw),
      d.sx,
      d.sx + d.sw - 1,
    ),
    y: clamp(
      Math.round(d.sy + ((y - d.dy) * d.sh) / d.dh),
      d.sy,
      d.sy + d.sh - 1,
    ),
  };
}

/**
 * Wrap `send` so pointer input taken on a composed canvas reaches the Mac in
 * its framebuffer's pixels. Without a composition everything passes through.
 *
 * Between screens Apple's viewer sends nothing, and a press or a wheel there
 * has no screen to act on either. A release still goes, so a drag that ends in
 * a gap does not leave a button held on the Mac.
 */
export function mosaicSender(
  send: (msg: ClientMsg) => void,
  currentView: () => MosaicView | null,
): (msg: ClientMsg) => void {
  let offScreen = false;
  const blocked = (msg: ClientMsg) =>
    (msg.type === "mouseButton" && msg.pressed) || msg.type === "wheel";
  return (msg) => {
    const view = currentView();
    if (!view) {
      offScreen = false;
      send(msg);
    } else if (msg.type === "mouseMove") {
      const at = mosaicToFramebuffer(view, msg.x, msg.y);
      offScreen = at === null;
      if (at) {
        send({ ...msg, ...at });
      }
    } else if (!(offScreen && blocked(msg))) {
      send(msg);
    }
  };
}
