// Pointer rendering for the browser SPA.
//
// Engines whose server hands the cursor shape over instead of drawing it into
// the framebuffer (VNC's Cursor pseudo-encoding — macOS Screen Sharing never
// draws one — and RDP, whose pointer updates the gateway forwards) make the
// client responsible for the pointer: the hardware pointer wears the shape as a
// CSS cursor, which is what lets it move with the mouse rather than with the
// framebuffer. Engines that composite the pointer themselves (a VNC server that
// ignores the pseudo-encoding) send no `cursor` message at all, and the hardware
// pointer wears a black X instead — see unownedCursor.

export interface CursorImage {
  url: string;
  /** Hotspot within the image, in cursor pixels. */
  hx: number;
  hy: number;
  w: number;
  h: number;
  /**
   * The image's unit, as the gateway names it: true for a density-independent
   * point-sized pixmap (Apple's cursors), sized against the desktop's points;
   * false for one cut from the desktop's own pixels (RDP, generic RFB), sized
   * against the framebuffer. See paintCursor in useRemoteDesktop.
   */
  pointSized: boolean;
}

// The engine's pointer state. `image` is null when the remote hid the pointer;
// the state as a whole is null while the remote is drawing it itself.
export interface RemoteCursor {
  image: CursorImage | null;
}

// How small the virtual pointer may get on screen, in CSS pixels across its
// longer side. Zoomed out to fit a phone, a pointer drawn at the desktop's own
// scale is a few pixels across, and the pointer is the one thing on screen that
// has to stay findable. Deliberately low: the pointer should read as part of
// the desktop it sits on, so the floor is a last resort rather than a size.
export const MIN_POINTER_CSS_PX = 14;

let arrow: CursorImage | null = null;

// A neutral arrow, standing in when the client owns the pointer but the remote
// has hidden its shape — on a remote desktop a pointer you can't see is worse
// than a generic one. Painted into a canvas rather than carried as an embedded
// blob, and PNG rather than SVG because Safari rejects SVG cursors.
export function fallbackCursor(): CursorImage {
  if (arrow) {
    return arrow;
  }
  const w = 12;
  const h = 19;
  const canvas = document.createElement("canvas");
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext("2d");
  if (ctx) {
    // The usual arrow, on half-pixel coordinates so the 1px outline lands on
    // whole pixels. The tip is the hotspot, hence (0, 0).
    ctx.beginPath();
    ctx.moveTo(0.5, 0.5);
    ctx.lineTo(0.5, 16);
    ctx.lineTo(4, 12.5);
    ctx.lineTo(6.5, 18);
    ctx.lineTo(9, 17);
    ctx.lineTo(6.5, 11.5);
    ctx.lineTo(11, 11.5);
    ctx.closePath();
    // White with a black outline, so it reads against any remote background.
    ctx.fillStyle = "#fff";
    ctx.fill();
    ctx.strokeStyle = "#000";
    ctx.lineWidth = 1;
    ctx.stroke();
  }
  // Point-sized: the stand-in should read as a normal cursor on the desktop it
  // sits on, not shrink with the density of a Retina framebuffer.
  arrow = {
    url: canvas.toDataURL("image/png"),
    hx: 0,
    hy: 0,
    w,
    h,
    pointSized: true,
  };
  return arrow;
}

let cross: CursorImage | null = null;

// A black X with a white border, worn while no engine has handed a shape over:
// before a session's first `cursor` message, between attachments, and for a
// server that sends none at all. The browser's own pointer used to be hidden
// there, on the reasoning that the remote was drawing one into its pixels, and
// over a desktop that was not — a blank or sleeping screen — that left no
// pointer at all. Painted into a canvas for the reason fallbackCursor is.
export function unownedCursor(): CursorImage {
  if (cross) {
    return cross;
  }
  const side = 15;
  const centre = 7;
  const canvas = document.createElement("canvas");
  canvas.width = side;
  canvas.height = side;
  const ctx = canvas.getContext("2d");
  if (ctx) {
    // Both strokes on half-pixel coordinates, so they cross on the centre
    // pixel, which is the hotspot.
    ctx.beginPath();
    ctx.moveTo(3.5, 3.5);
    ctx.lineTo(11.5, 11.5);
    ctx.moveTo(11.5, 3.5);
    ctx.lineTo(3.5, 11.5);
    ctx.lineCap = "round";
    // The white pass under the black one leaves a border on every side, so it
    // reads against any remote background.
    ctx.strokeStyle = "#fff";
    ctx.lineWidth = 5;
    ctx.stroke();
    ctx.strokeStyle = "#000";
    ctx.lineWidth = 2.5;
    ctx.stroke();
  }
  // Point-sized, as the arrow is.
  cross = {
    url: canvas.toDataURL("image/png"),
    hx: centre,
    hy: centre,
    w: side,
    h: side,
    pointSized: true,
  };
  return cross;
}

// A cursor image as a CSS url() token. An unquoted token ends at the first
// `)`, so quoting (and escaping what would close the quote) keeps the image
// string from spilling into the declaration. Our own base64 can't contain
// either character, but the URL is server-supplied and this is one line.
export function cssUrl(url: string): string {
  return `url("${url.replace(/["\\]/g, "\\$&")}")`;
}

// The shape the engine handed over, or null while it has handed none.
export function cursorImage(remote: RemoteCursor | null): CursorImage | null {
  if (!remote) {
    return null;
  }
  return remote.image ?? fallbackCursor();
}

// Scale a framebuffer-pixel cursor with image-set resolution. Keep a plain URL
// fallback because unsupported image-set values are rejected as a whole.
export function applyCursorCss(
  el: HTMLElement,
  image: CursorImage,
  view: number,
) {
  el.style.cursor = `${cssUrl(image.url)} ${image.hx} ${image.hy}, default`;
  if (!(view > 0) || !Number.isFinite(view) || Math.abs(view - 1) < 0.01) {
    return;
  }
  const density = (1 / view).toFixed(3);
  const hx = Math.round(image.hx * view);
  const hy = Math.round(image.hy * view);
  el.style.cursor = `image-set(${cssUrl(image.url)} ${density}x) ${hx} ${hy}, default`;
}
