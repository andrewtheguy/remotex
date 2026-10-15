// The software decoders' pictures (softwareDecoder.ts), shown from the
// GPU.
//
// A picture is three 8-bit planes in the decoder's memory. Each is uploaded into
// a texture of its own from where the decoder left it, and one draw on the WebGL
// canvas the page shows over the desktop's (glPicture.ts) turns them into the
// colors they stand for. The planes are read only inside `place`, which is what
// lets the decoder have their memory back when the picture is closed.
//
// A display sent in strips is placed a strip at a time, each into its rows of
// textures of the display's size, and drawn at its frame's last: a frame carries
// the strips that changed, and the others are in the textures already. It is
// drawn once every strip has come since its keyframe, or since the textures were
// made, and not before: what was in a strip's rows then is no picture of it. A
// display one to three rows past a multiple of 64 has strips that end short of its
// last rows, which are black, as the gateway leaves them.
//
// A picture whose decoder says where it may differ from the one before it in its
// rows (`changed`), the picture whole's or the strip's, is uploaded only there
// when the textures hold that one, and not at all where nothing changed: a quiet
// desktop's frame is a few of its blocks, and the whole picture is 25 MB at 4K.
import { openPictureCanvas } from "./glPicture.ts";
import type { DecodedPlanes } from "./softwareDecoder.ts";

export interface PlanesPicture {
  /**
   * Upload `planes` into the picture, at their rows. Throws where the GPU will
   * not hold the picture, and once the browser has taken the GPU away.
   */
  place(planes: DecodedPlanes): void;
  /**
   * Draw the picture placed as the desktop's, `w` by `h`: the decoded picture
   * can be a pixel or so larger, and what is past the desktop is not shown.
   * False, and nothing drawn, while a strip has not come since its keyframe.
   * Throws once the browser has taken the GPU away.
   */
  draw(w: number, h: number): boolean;
  /**
   * Give the GPU its memory back, and leave the canvas black: what it shows next
   * is another session's.
   */
  close(): void;
}

/** Every strip of a picture sent in four. */
const WHOLE = 0b1111;

/**
 * The strips in the textures once `planes` is placed, a bit each, from those
 * placed before: a picture whole is every strip, and a keyframe's strip the only
 * one of its frame.
 */
export function placedAfter(
  placed: number,
  planes: Pick<DecodedPlanes, "strip" | "keyframe">,
): number {
  if (planes.strip === undefined) {
    return WHOLE;
  }
  return (planes.keyframe ? 0 : placed) | (1 << planes.strip);
}

// Each fragment is one pixel of the desktop, counted from the top row, and reads
// the texel that holds it in each plane, the three being the picture's size.
// Coordinates at full precision, as a wide desktop needs (egfxPicture.ts).
const FRAGMENT = `#version 300 es
precision highp float;
uniform sampler2D luma;
uniform sampler2D cb;
uniform sampler2D cr;
uniform float top;
uniform vec2 extent;
uniform mat3 toRgb;
uniform vec3 zero;
out vec4 color;
void main() {
  vec2 at = vec2(gl_FragCoord.x, top - gl_FragCoord.y) / extent;
  vec3 ycbcr = vec3(
    texture(luma, at).r,
    texture(cb, at).r,
    texture(cr, at).r);
  color = vec4(toRgb * (ycbcr - zero), 1.0);
}`;

// The luma's share of red and of blue, by who defined the coefficients.
const LUMA = {
  bt709: [0.2126, 0.0722],
  smpte170m: [0.299, 0.114],
} as const;

/**
 * Y'CbCr samples, as a texture gives them (0 to 1), to R'G'B': what is taken off
 * each first, and the matrix after, its columns in the order WebGL takes them.
 */
function conversion(matrix: DecodedPlanes["matrix"], fullRange: boolean) {
  const [kr, kb] = LUMA[matrix];
  const kg = 1 - kr - kb;
  // A limited-range luma runs 16 to 235 and its chroma 16 to 240, about 128.
  const y = fullRange ? 1 : 255 / 219;
  const c = fullRange ? 1 : 255 / 224;
  return {
    zero: [fullRange ? 0 : 16 / 255, 128 / 255, 128 / 255],
    toRgb: [
      ...[y, y, y],
      ...[0, (-2 * kb * (1 - kb) * c) / kg, 2 * (1 - kb) * c],
      ...[2 * (1 - kr) * c, (-2 * kr * (1 - kr) * c) / kg, 0],
    ],
  };
}

/**
 * The rows of each plane's texture below `planes` that no strip holds, and the
 * value that is black there: none but past the last strip, which can end short of
 * the display's last rows.
 */
export function rowsBelow(
  planes: Pick<DecodedPlanes, "height" | "row" | "strip" | "fullRange"> & {
    planes: { rows: number }[];
  },
): { unit: number; row: number; rows: number; value: number }[] {
  if (planes.strip !== undefined && planes.strip < 3) {
    return [];
  }
  return planes.planes.flatMap((plane, unit) => {
    const end = planes.row + plane.rows;
    const rows = planes.height - end;
    if (rows <= 0) {
      return [];
    }
    const value = unit > 0 ? 128 : planes.fullRange ? 0 : 16;
    return [{ unit, row: end, rows, value }];
  });
}

/** A rectangle of a picture's planes, in samples. */
export interface PlanesRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/**
 * What of `planes` to upload into textures whose rows of it hold the picture
 * `held` (a `PictureChange`'s `picture`, or null for none known): where it may
 * differ from the one before it, when the textures hold that one, and of a
 * last strip only its rows of the display; nothing, where nothing does; and
 * otherwise the whole picture.
 */
export function uploadOf(
  held: string | null,
  planes: Pick<DecodedPlanes, "changed"> & { planes: { rows: number }[] },
): PlanesRect | "whole" | null {
  const changed = planes.changed;
  if (held === null || changed.since !== held) {
    return "whole";
  }
  const { x, y, width } = changed;
  const height = Math.min(changed.height, planes.planes[0].rows - y);
  if (width <= 0 || height <= 0) {
    return null;
  }
  return { x, y, width, height };
}

/** The decoder's pictures on `canvas`, the one the page shows them on. */
export function createPlanesPicture(canvas: OffscreenCanvas): PlanesPicture {
  const surface = openPictureCanvas(canvas, FRAGMENT);
  const { gl, program } = surface;
  const uniform = (name: string) => gl.getUniformLocation(program, name);
  const textures: (WebGLTexture | null)[] = [null, null, null];
  ["luma", "cb", "cr"].forEach((name, unit) => {
    gl.uniform1i(uniform(name), unit);
  });
  // Rows at the planes' strides, which are whatever the decoder aligned them to.
  gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
  gl.pixelStorei(gl.UNPACK_SKIP_PIXELS, 0);
  gl.pixelStorei(gl.UNPACK_SKIP_ROWS, 0);
  // What the canvas, the textures and the uniforms were last made for.
  let size = "";
  let extent = "";
  let colors = "";
  // The strips in the textures (`placedAfter`).
  let placed = 0;
  // The picture the textures hold, as its `changed.picture` (`uploadOf`), in
  // each strip's rows, or the first's for a picture whole: null where that is
  // not known, or the textures were made since.
  const held: (string | null)[] = [null, null, null, null];

  // A texture for each plane, at the picture's size; a pixel reads its own
  // texel. WebGL reports a refused allocation as an error flag, not an
  // exception, and a draw from a texture it did not make is black: so it is
  // checked here.
  const allocate = (w: number, h: number) => {
    textures.forEach((texture, unit) => {
      gl.deleteTexture(texture);
      textures[unit] = gl.createTexture();
      gl.activeTexture(gl.TEXTURE0 + unit);
      gl.bindTexture(gl.TEXTURE_2D, textures[unit]);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
      gl.texStorage2D(gl.TEXTURE_2D, 1, gl.R8, w, h);
    });
    const error = gl.getError();
    if (error !== gl.NO_ERROR) {
      throw new Error(
        `the GPU refused a ${w}x${h} picture (WebGL error 0x${error.toString(16)})`,
      );
    }
    gl.uniform2f(uniform("extent"), w, h);
  };

  // Every plane of `planes` into its rows of the textures, and the rows past a
  // last strip black.
  const uploadWhole = (planes: DecodedPlanes, memory: Uint8Array) => {
    planes.planes.forEach((plane, unit) => {
      gl.activeTexture(gl.TEXTURE0 + unit);
      gl.pixelStorei(gl.UNPACK_ROW_LENGTH, plane.stride);
      gl.texSubImage2D(
        gl.TEXTURE_2D,
        0,
        0,
        planes.row,
        plane.width,
        plane.rows,
        gl.RED,
        gl.UNSIGNED_BYTE,
        memory,
        plane.offset,
      );
    });
    for (const { unit, row, rows, value } of rowsBelow(planes)) {
      const width = planes.planes[unit].width;
      gl.activeTexture(gl.TEXTURE0 + unit);
      gl.pixelStorei(gl.UNPACK_ROW_LENGTH, 0);
      gl.texSubImage2D(
        gl.TEXTURE_2D,
        0,
        0,
        row,
        width,
        rows,
        gl.RED,
        gl.UNSIGNED_BYTE,
        new Uint8Array(width * rows).fill(value),
      );
    }
  };

  // The same rectangle of each plane, in the picture's rows. Its first sample
  // is given as the offset, not by UNPACK_SKIP_PIXELS and UNPACK_SKIP_ROWS:
  // WebKit took 0.5 ms for three 128×128 rectangles given by those, and 0.13
  // by the offset, where a whole 3840×2160 picture took 1.6.
  const uploadRect = (
    planes: DecodedPlanes,
    memory: Uint8Array,
    rect: PlanesRect,
  ) => {
    planes.planes.forEach((plane, unit) => {
      gl.activeTexture(gl.TEXTURE0 + unit);
      gl.pixelStorei(gl.UNPACK_ROW_LENGTH, plane.stride);
      gl.texSubImage2D(
        gl.TEXTURE_2D,
        0,
        rect.x,
        planes.row + rect.y,
        rect.width,
        rect.height,
        gl.RED,
        gl.UNSIGNED_BYTE,
        memory,
        plane.offset + rect.y * plane.stride + rect.x,
      );
    });
  };

  // What of `planes` the textures do not hold already (`uploadOf`), into its
  // rows of them.
  const upload = (planes: DecodedPlanes) => {
    const memory = new Uint8Array(planes.memory);
    const strip = planes.strip ?? 0;
    const what = uploadOf(held[strip], planes);
    // Unknown until the upload is done: one that throws leaves the textures
    // holding no picture whole. A picture whole is in every strip's rows.
    if (planes.strip === undefined) {
      held.fill(null);
    }
    held[strip] = null;
    if (what === "whole") {
      uploadWhole(planes, memory);
    } else if (what) {
      uploadRect(planes, memory, what);
    }
    held[strip] = planes.changed.picture;
  };

  return {
    place(planes) {
      surface.usable();
      const wanted = `${planes.width}x${planes.height}`;
      if (wanted !== extent) {
        // Not remembered until made: a refused allocation is tried again.
        extent = "";
        placed = 0;
        held.fill(null);
        allocate(planes.width, planes.height);
        extent = wanted;
      }
      const stated = `${planes.matrix} ${planes.fullRange} ${planes.colorSpace}`;
      if (stated !== colors) {
        const { zero, toRgb } = conversion(planes.matrix, planes.fullRange);
        gl.uniform3fv(uniform("zero"), zero);
        gl.uniformMatrix3fv(uniform("toRgb"), false, toRgb);
        // The primaries are the canvas's to state: the browser takes its pixels
        // to the display's from there, as it takes a video frame's.
        if ("drawingBufferColorSpace" in gl) {
          gl.drawingBufferColorSpace = planes.colorSpace;
        } else if (planes.colorSpace !== "srgb") {
          throw new Error(
            `this browser's WebGL canvas shows no ${planes.colorSpace} picture`,
          );
        }
        colors = stated;
      }
      upload(planes);
      placed = placedAfter(placed, planes);
    },
    draw(w, h) {
      surface.usable();
      if (placed !== WHOLE) {
        return false;
      }
      if (`${w}x${h}` !== size) {
        surface.resize(w, h);
        gl.uniform1f(uniform("top"), h);
        size = `${w}x${h}`;
      }
      // Whole, every time: the canvas's drawing buffer is not kept from one frame
      // the browser shows to the next.
      surface.draw();
      return true;
    },
    close() {
      for (const texture of textures) {
        gl.deleteTexture(texture);
      }
      surface.close();
    },
  };
}
