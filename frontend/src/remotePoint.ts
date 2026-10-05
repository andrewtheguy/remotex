// A pointer position on the canvas, in the pixels of the display it shows.
//
// Mapped through the canvas rect rather than the overlay's: it reflects the
// displayed framebuffer under the current touch zoom and pan, and on desktop the
// two coincide. Clamped to the framebuffer, so a drag held past an edge stays in
// range — except while another display is shown beside this one, in a tab of its
// own, where the position is let through as it is. A browser keeps delivering a
// held drag's positions to the window it began in, past that window's edge and
// onto the next screen, so a position past the display's edge is where the
// pointer is on the display beyond it, and the gateway places it there: its
// engine knows which edge the other display is against, offsets the position
// into the remote's arrangement and holds it on a display, so one past any other
// edge, or beyond a neighbour shorter or narrower than this display, is held
// there instead of here. A window dragged over the edge between the two windows
// arrives on the other display, however they are arranged: the position is the
// distance past this canvas, so it lands where it was dragged to when the two
// canvases' edges meet, as with each full screen on a display of its own, and
// short by whatever lies between them otherwise. The pointer itself is sent by
// whichever window it is over, so it needs nothing of this.

/** The canvas's client rect: where the displayed framebuffer is on the page. */
export interface CanvasRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** The displayed framebuffer's size in pixels. */
export interface RemotePixels {
  w: number;
  h: number;
}

/**
 * The remote pixel under client point `clientX`, `clientY`. Without a remote
 * size the point is the canvas offset as is. `beside` is whether another display
 * is shown in a tab of its own: the session's page while *All Displays* is
 * chosen, and the tab showing the second display for as long as it is shown.
 */
export function remotePoint(
  clientX: number,
  clientY: number,
  rect: CanvasRect,
  remote: RemotePixels | null,
  beside: boolean,
): { x: number; y: number } {
  const scaleX = remote && rect.width > 0 ? remote.w / rect.width : 1;
  const scaleY = remote && rect.height > 0 ? remote.h / rect.height : 1;
  let x = Math.round((clientX - rect.left) * scaleX);
  let y = Math.round((clientY - rect.top) * scaleY);
  if (remote && !beside) {
    x = Math.min(Math.max(x, 0), remote.w - 1);
    y = Math.min(Math.max(y, 0), remote.h - 1);
  }
  return { x, y };
}
