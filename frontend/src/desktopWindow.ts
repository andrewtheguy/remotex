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
 * The whole-CSS-pixel viewport that shows every framebuffer pixel on one device
 * pixel, at this host's density.
 *
 * A framebuffer can divide into fractional CSS pixels at an unusual density. A
 * native window only accepts integer dimensions, so round outward: one spare
 * fraction is preferable to clipping the last remote pixel.
 */
export function desktopViewportSize(
  framebuffer: CanvasSize,
  hostDensity: number,
): CanvasSize {
  const { layout } = desktopCanvasGeometry(framebuffer, hostDensity);
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

/** Request a window whose content viewport is the framebuffer at one device pixel per remote pixel. */
export function sizeWindowToDesktop(
  framebuffer: CanvasSize,
  hostDensity: number,
  target: ResizableWindow = window,
): void {
  const outer = outerSizeForViewport(
    desktopViewportSize(framebuffer, hostDensity),
    target,
  );
  target.resizeTo(outer.w, outer.h);
}
