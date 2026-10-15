// The picture of an RDP host's graphics pipeline, kept on the GPU and shown from
// there.
//
// The compositor's framebuffer is in the module's memory, which its threads share
// (egfxCompositor.ts), and the desktop's 2D canvas takes no image data out of a
// shared memory. A WebGL texture takes an upload from one as it is. So the
// pipeline's picture is a texture on the WebGL canvas the page shows over the
// desktop's (glPicture.ts), drawn whole after each upload. Drawing the painted
// rectangles from it onto the desktop's canvas instead made the GPU copy the whole
// picture again for every run, which on an integrated GPU was more work than
// everything else presenting it.
//
// The picture is the whole output the host draws, which over two virtual displays
// is both of them as the host placed them; the canvas shows the part of it the gateway named
// (`graphicsView`), one display's column. A tab showing the second display holds a
// picture of its own, patched from the session page's rather than composed
// (displayRelay.ts), and shows it whole.
//
// TESTING ONLY: a debug session of passed bitmap updates has the tile grid and
// each tile's warmth drawn over the picture here (bitmapDebug.ts), by the shader,
// from a texture of one texel a tile: the picture's own texture is left exact.
import { TILE } from "./bitmapDebug.ts";
import type { ComposedRun } from "./egfxCompositor.ts";
import { openPictureCanvas } from "./glPicture.ts";

/** A part of the picture, in its pixels. */
export interface PicturePart {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface GraphicsPicture {
  /**
   * Take a run's painted rectangles into the picture, and draw it. Throws once
   * the browser has taken the GPU away (a lost context), after which nothing more
   * is drawn.
   */
  upload(run: ComposedRun): void;
  /**
   * Show this part of the picture, the display this page is out of the span the
   * host draws, or the whole picture for null. Drawn at once from what the
   * picture holds, and kept for every draw after.
   */
  window(part: PicturePart | null): void;
  /**
   * Take rectangles into a picture of `w` by `h` — made at that size where it is
   * another — and draw it. `rects` is `x, y, width, height` for each, and
   * `pixels` holds each one's RGBX rows in turn, packed at its own width. The
   * tab's side of displayRelay.ts.
   */
  patch(
    w: number,
    h: number,
    rects: ArrayLike<number>,
    pixels: Uint8Array,
  ): void;
  /**
   * The canvas at this size, black: the desktop's canvas was replaced at it. The
   * picture is kept, since the picker's switch between displays replaces the
   * canvas too and shows another part of the same picture; the `window` that
   * follows draws it. A host that lays the span out again redraws it in a run
   * whose reset resizes the picture.
   */
  blank(width: number, height: number): void;
  /**
   * TESTING ONLY: draw the tile grid over the picture, each tile tinted by its
   * warmth in `heat` (one byte a tile, `columns` across, row by row from the
   * top), and draw it; or, for null, draw the picture alone again.
   */
  tiles(heat: Uint8Array | null, columns: number, rows: number): void;
  /**
   * Give the GPU its memory back, and leave the canvas black: what it shows next
   * is another pipeline's.
   */
  close(): void;
}

// `view` is the part of the texture shown, normalized: its origin and its size. The
// texture's top row at the top: the framebuffer is top row first, where the
// canvas's rows go up. Texel coordinates at full precision: at half precision a
// wide desktop's neighbouring columns share a value, and NEAREST then repeats or
// drops one.
//
// With `debug`, the pixel of the picture `at` names is found in its tile: the
// first row and column of every tile are drawn as the grid, and the whole tile
// is tinted by its texel in `heat`. Without it neither is read.
const FRAGMENT = `#version 300 es
precision highp float;
uniform sampler2D picture;
uniform sampler2D heat;
uniform vec4 view;
uniform vec2 size;
uniform bool debug;
in vec2 uv;
out vec4 color;
void main() {
  vec2 at = view.xy + vec2(uv.x, 1.0 - uv.y) * view.zw;
  vec3 rgb = texture(picture, at).rgb;
  if (debug) {
    vec2 pixel = floor(at * size);
    vec2 tile = floor(pixel / ${TILE}.0);
    float warm = texelFetch(heat, ivec2(tile), 0).r;
    rgb = mix(rgb, vec3(1.0, 0.3, 0.1), warm * 0.45);
    vec2 inside = pixel - tile * ${TILE}.0;
    if (inside.x < 1.0 || inside.y < 1.0) {
      rgb = mix(rgb, vec3(0.0, 0.8, 1.0), 0.6);
    }
  }
  color = vec4(rgb, 1.0);
}`;

/** `part` within a picture of `w` by `h`, or null for none of it inside. */
function clipped(part: PicturePart, w: number, h: number): PicturePart | null {
  const x = Math.max(part.x, 0);
  const y = Math.max(part.y, 0);
  const right = Math.min(part.x + part.w, w);
  const bottom = Math.min(part.y + part.h, h);
  return x < right && y < bottom ? { x, y, w: right - x, h: bottom - y } : null;
}

/** A pipeline's picture on `canvas`, the one the page shows it on. */
export function createGraphicsPicture(
  canvas: OffscreenCanvas,
): GraphicsPicture {
  const surface = openPictureCanvas(canvas, FRAGMENT);
  const { gl } = surface;
  const viewAt = gl.getUniformLocation(surface.program, "view");
  const sizeAt = gl.getUniformLocation(surface.program, "size");
  const debugAt = gl.getUniformLocation(surface.program, "debug");
  // The heat on the second texture unit, the picture on the first.
  gl.uniform1i(gl.getUniformLocation(surface.program, "heat"), 1);
  let texture: WebGLTexture | null = null;
  // TESTING ONLY: the tiles' warmth, while the tiles are drawn, and its size.
  let heat: WebGLTexture | null = null;
  let heatSize = { columns: 0, rows: 0 };
  // The picture's size: the texture's.
  let width = 0;
  let height = 0;
  // The part shown, or the whole picture.
  let part: PicturePart | null = null;

  // A texture of the picture's size, blank: WebGL zero-fills what it makes.
  // WebGL reports a refused allocation as an error flag, not an exception, and a
  // draw from the texture it did not make is blank: so it is checked here, and
  // ends the pipeline rather than acknowledging a blank.
  const resize = (w: number, h: number) => {
    if (texture) {
      gl.deleteTexture(texture);
    }
    texture = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, texture);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    gl.texStorage2D(gl.TEXTURE_2D, 1, gl.RGBA8, w, h);
    const error = gl.getError();
    if (error !== gl.NO_ERROR) {
      throw new Error(
        `the GPU refused a ${w}x${h} picture (WebGL error 0x${error.toString(16)})`,
      );
    }
    width = w;
    height = h;
  };

  // The canvas at the shown part's size — the display's, which is what the
  // desktop's canvas under it is — and the part drawn where it lands on it. A
  // part the picture has not reached yet (the reset that lays the span out is
  // still to be composed) leaves the canvas black, as the desktop's canvas is.
  const present = () => {
    const shown = part ?? { x: 0, y: 0, w: width, h: height };
    if (shown.w === 0 || shown.h === 0) {
      return;
    }
    if (canvas.width !== shown.w || canvas.height !== shown.h) {
      surface.resize(shown.w, shown.h);
    }
    gl.clearColor(0, 0, 0, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    const inside = clipped(shown, width, height);
    if (!inside) {
      return;
    }
    // Whole, every time: the canvas's drawing buffer is not kept from one frame
    // the browser shows to the next. The viewport is where the part inside the
    // picture lands on the canvas, from the bottom as WebGL counts rows.
    gl.viewport(
      inside.x - shown.x,
      shown.h - (inside.y - shown.y) - inside.h,
      inside.w,
      inside.h,
    );
    gl.uniform4f(
      viewAt,
      inside.x / width,
      inside.y / height,
      inside.w / width,
      inside.h / height,
    );
    gl.uniform2f(sizeAt, width, height);
    gl.uniform1i(debugAt, heat ? 1 : 0);
    surface.draw();
  };

  return {
    upload(run) {
      surface.usable();
      if (run.width === 0 || run.height === 0) {
        return;
      }
      if (run.resized || run.width !== width || run.height !== height) {
        resize(run.width, run.height);
      }
      // Each rectangle straight out of the framebuffer, at its stride.
      gl.pixelStorei(gl.UNPACK_ROW_LENGTH, width);
      const rects = run.painted;
      for (let i = 0; i + 3 < rects.length; i += 4) {
        gl.pixelStorei(gl.UNPACK_SKIP_PIXELS, rects[i]);
        gl.pixelStorei(gl.UNPACK_SKIP_ROWS, rects[i + 1]);
        gl.texSubImage2D(
          gl.TEXTURE_2D,
          0,
          rects[i],
          rects[i + 1],
          rects[i + 2],
          rects[i + 3],
          gl.RGBA,
          gl.UNSIGNED_BYTE,
          run.pixels,
          0,
        );
      }
      present();
    },
    window(next) {
      part = next;
      if (width > 0 && height > 0) {
        surface.usable();
        present();
      }
    },
    patch(w, h, rects, pixels) {
      surface.usable();
      if (w === 0 || h === 0) {
        return;
      }
      if (w !== width || h !== height) {
        resize(w, h);
      }
      // Each rectangle packed at its own width, one after the other.
      gl.pixelStorei(gl.UNPACK_ROW_LENGTH, 0);
      gl.pixelStorei(gl.UNPACK_SKIP_PIXELS, 0);
      gl.pixelStorei(gl.UNPACK_SKIP_ROWS, 0);
      let at = 0;
      for (let i = 0; i + 3 < rects.length; i += 4) {
        const [x, y, rw, rh] = [
          rects[i],
          rects[i + 1],
          rects[i + 2],
          rects[i + 3],
        ];
        gl.texSubImage2D(
          gl.TEXTURE_2D,
          0,
          x,
          y,
          rw,
          rh,
          gl.RGBA,
          gl.UNSIGNED_BYTE,
          pixels,
          at,
        );
        at += rw * rh * 4;
      }
      present();
    },
    blank(w, h) {
      surface.usable();
      if (w === 0 || h === 0) {
        return;
      }
      // Not the texture: a resize to a display's size is, over a span, a switch
      // between its displays, and the span is what the window after it is of.
      if (canvas.width !== w || canvas.height !== h) {
        surface.resize(w, h);
      }
      gl.clearColor(0, 0, 0, 1);
      gl.clear(gl.COLOR_BUFFER_BIT);
    },
    tiles(values, columns, rows) {
      surface.usable();
      if (!values) {
        gl.deleteTexture(heat);
        heat = null;
        heatSize = { columns: 0, rows: 0 };
        present();
        return;
      }
      gl.activeTexture(gl.TEXTURE1);
      if (!heat || heatSize.columns !== columns || heatSize.rows !== rows) {
        gl.deleteTexture(heat);
        heat = gl.createTexture();
        gl.bindTexture(gl.TEXTURE_2D, heat);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
        gl.texStorage2D(gl.TEXTURE_2D, 1, gl.R8, columns, rows);
        heatSize = { columns, rows };
      } else {
        gl.bindTexture(gl.TEXTURE_2D, heat);
      }
      // A byte a texel, packed: none of what the picture's uploads set.
      gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
      gl.pixelStorei(gl.UNPACK_ROW_LENGTH, 0);
      gl.pixelStorei(gl.UNPACK_SKIP_PIXELS, 0);
      gl.pixelStorei(gl.UNPACK_SKIP_ROWS, 0);
      gl.texSubImage2D(
        gl.TEXTURE_2D,
        0,
        0,
        0,
        columns,
        rows,
        gl.RED,
        gl.UNSIGNED_BYTE,
        values,
      );
      gl.pixelStorei(gl.UNPACK_ALIGNMENT, 4);
      gl.activeTexture(gl.TEXTURE0);
      if (width > 0 && height > 0) {
        present();
      }
    },
    close() {
      gl.deleteTexture(texture);
      texture = null;
      gl.deleteTexture(heat);
      heat = null;
      surface.close();
    },
  };
}
