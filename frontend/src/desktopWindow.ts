import { type CanvasSize, desktopCanvasGeometry } from "./desktopCanvas.ts";

/** The window measurements needed to turn a desired viewport into an outer size. */
export interface ResizableWindow {
  readonly innerWidth: number;
  readonly innerHeight: number;
  readonly outerWidth: number;
  readonly outerHeight: number;
  resizeTo(width: number, height: number): void;
}

/**
 * The whole-CSS-pixel viewport that presents every remote point without scaling.
 *
 * A framebuffer can divide into fractional points at an unusual density. A native
 * window only accepts integer dimensions, so round outward: one spare fraction is
 * preferable to clipping the last fraction of a remote point.
 */
export function desktopViewportSize(
  framebuffer: CanvasSize,
  guestDensity: number,
): CanvasSize {
  const { layout } = desktopCanvasGeometry(framebuffer, guestDensity);
  return {
    w: Math.ceil(layout.w),
    h: Math.ceil(layout.h),
  };
}

/**
 * Add the browser frame currently surrounding `target` to a desired inner size.
 *
 * `resizeTo` speaks in outer-window dimensions. Measuring the live difference keeps
 * this independent of the OS, title-bar height, browser theme, and whether Chrome is
 * using its ordinary app title bar or Window Controls Overlay.
 */
export function outerSizeForViewport(
  viewport: CanvasSize,
  target: ResizableWindow,
): CanvasSize {
  return {
    w: target.outerWidth + viewport.w - target.innerWidth,
    h: target.outerHeight + viewport.h - target.innerHeight,
  };
}

/** Request a window whose content viewport is the remote desktop's logical size. */
export function sizeWindowToDesktop(
  framebuffer: CanvasSize,
  guestDensity: number,
  target: ResizableWindow = window,
): void {
  const outer = outerSizeForViewport(
    desktopViewportSize(framebuffer, guestDensity),
    target,
  );
  target.resizeTo(outer.w, outer.h);
}
