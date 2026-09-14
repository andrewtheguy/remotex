export interface CanvasSize {
  w: number;
  h: number;
}

export interface CanvasGeometry {
  // The canvas bitmap: one canvas pixel for every remote framebuffer pixel.
  bitmap: CanvasSize;
  // The element's CSS box: the bitmap at one device pixel per remote pixel.
  layout: CanvasSize;
}

// The bitmap is the framebuffer and the CSS box is that bitmap divided by the
// host's density, so every remote pixel lands on exactly one device pixel and
// nothing is resampled on the way to the screen. The remote's own density
// (`Resize.scale`) is a label: it says how many of these pixels the remote
// draws per point of its desktop, and it never enters the layout. A 1x
// framebuffer on a 2x screen is therefore half the CSS size it would be on a
// 1x one, and sharp on both.
export function desktopCanvasGeometry(
  framebuffer: CanvasSize,
  hostDensity: number,
): CanvasGeometry {
  const density =
    Number.isFinite(hostDensity) && hostDensity > 0 ? hostDensity : 1;
  return {
    bitmap: { w: framebuffer.w, h: framebuffer.h },
    layout: {
      w: framebuffer.w / density,
      h: framebuffer.h / density,
    },
  };
}
