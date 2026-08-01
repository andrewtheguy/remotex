export interface CanvasSize {
  w: number;
  h: number;
}

// Size a pointer desktop without discarding framebuffer pixels the host screen
// could not represent. A 2x guest may use its point size on a 2x host, but on a
// 1x host it stays at its full pixel size and native scrolling exposes the rest.
export function desktopCanvasSize(
  framebuffer: CanvasSize,
  guestDensity: number,
  hostDensity: number,
): CanvasSize {
  const usable = (density: number) =>
    Number.isFinite(density) && density > 0 ? density : 1;
  const density = Math.min(usable(guestDensity), usable(hostDensity));
  return {
    w: framebuffer.w / density,
    h: framebuffer.h / density,
  };
}
