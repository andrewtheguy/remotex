export interface CanvasSize {
  w: number;
  h: number;
}

export interface CanvasGeometry {
  // The canvas bitmap: one canvas pixel for every remote framebuffer pixel.
  bitmap: CanvasSize;
  // The element's CSS box: the remote desktop's own logical size.
  layout: CanvasSize;
}

// Keep the high-density bitmap separate from the canvas's on-screen size. The
// browser rasterizes the remote's logical points for whichever host display the
// window occupies; host density never changes the desktop's layout.
export function desktopCanvasGeometry(
  framebuffer: CanvasSize,
  guestDensity: number,
): CanvasGeometry {
  const density =
    Number.isFinite(guestDensity) && guestDensity > 0 ? guestDensity : 1;
  return {
    bitmap: { w: framebuffer.w, h: framebuffer.h },
    layout: {
      w: framebuffer.w / density,
      h: framebuffer.h / density,
    },
  };
}
