// BETA: the software decoders' pictures (softwareDecoder.ts), shown from the
// GPU.
//
// A picture is three 8-bit planes in the decoder's memory. Each is uploaded as a
// texture of its own from where the decoder left it, and one draw on the WebGL
// canvas the page shows over the desktop's (glPicture.ts) turns them into the
// colors they stand for. The planes are read only inside `draw`, which is what
// lets the decoder have their memory back when the picture is closed.
import { openPictureCanvas } from "./glPicture.ts";
import type { DecodedPlanes } from "./softwareDecoder.ts";

export interface PlanesPicture {
  /**
   * Draw `planes` as the desktop's picture, `w` by `h`: the decoded picture can
   * be a pixel or so larger, and what is past the desktop is not shown. Throws
   * where the GPU will not hold the picture, and once the browser has taken the
   * GPU away.
   */
  draw(planes: DecodedPlanes, w: number, h: number): void;
  /**
   * Give the GPU its memory back, and leave the canvas black: what it shows next
   * is another session's.
   */
  close(): void;
}

// Each fragment is one pixel of the desktop, counted from the top row, and reads
// the texel that holds it in each plane: `extent` is a plane's size in the
// picture's pixels, which is twice its texels where the chroma is halved.
// Coordinates at full precision, as a wide desktop needs (egfxPicture.ts).
const FRAGMENT = `#version 300 es
precision highp float;
uniform sampler2D luma;
uniform sampler2D cb;
uniform sampler2D cr;
uniform float top;
uniform vec2 lumaExtent;
uniform vec2 chromaExtent;
uniform mat3 toRgb;
uniform vec3 zero;
out vec4 color;
void main() {
  vec2 at = vec2(gl_FragCoord.x, top - gl_FragCoord.y);
  vec3 ycbcr = vec3(
    texture(luma, at / lumaExtent).r,
    texture(cb, at / chromaExtent).r,
    texture(cr, at / chromaExtent).r);
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
  let layout = "";
  let colors = "";

  // A texture for each plane, at its size. WebGL reports a refused allocation as
  // an error flag, not an exception, and a draw from a texture it did not make is
  // black: so it is checked here.
  const allocate = (planes: DecodedPlanes) => {
    const [luma, chroma] = planes.planes;
    const halved = chroma.width !== luma.width || chroma.rows !== luma.rows;
    planes.planes.forEach((plane, unit) => {
      gl.deleteTexture(textures[unit]);
      textures[unit] = gl.createTexture();
      gl.activeTexture(gl.TEXTURE0 + unit);
      gl.bindTexture(gl.TEXTURE_2D, textures[unit]);
      // A pixel reads its own texel, but for halved chroma, which is spread
      // between the pixels it covers.
      const filter = unit > 0 && halved ? gl.LINEAR : gl.NEAREST;
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
      gl.texStorage2D(gl.TEXTURE_2D, 1, gl.R8, plane.width, plane.rows);
    });
    const error = gl.getError();
    if (error !== gl.NO_ERROR) {
      throw new Error(
        `the GPU refused a ${planes.width}x${planes.height} picture (WebGL error 0x${error.toString(16)})`,
      );
    }
    gl.uniform2f(uniform("lumaExtent"), luma.width, luma.rows);
    gl.uniform2f(
      uniform("chromaExtent"),
      chroma.width * (chroma.width === luma.width ? 1 : 2),
      chroma.rows * (chroma.rows === luma.rows ? 1 : 2),
    );
  };

  return {
    draw(planes, w, h) {
      surface.usable();
      const wanted = planes.planes
        .map((plane) => `${plane.width}x${plane.rows}`)
        .join();
      if (wanted !== layout) {
        // Not remembered until made: a refused allocation is tried again.
        layout = "";
        allocate(planes);
        layout = wanted;
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
      if (`${w}x${h}` !== size) {
        surface.resize(w, h);
        gl.uniform1f(uniform("top"), h);
        size = `${w}x${h}`;
      }
      const memory = new Uint8Array(planes.memory);
      planes.planes.forEach((plane, unit) => {
        gl.activeTexture(gl.TEXTURE0 + unit);
        gl.pixelStorei(gl.UNPACK_ROW_LENGTH, plane.stride);
        gl.texSubImage2D(
          gl.TEXTURE_2D,
          0,
          0,
          0,
          plane.width,
          plane.rows,
          gl.RED,
          gl.UNSIGNED_BYTE,
          memory,
          plane.offset,
        );
      });
      // Whole, every time: the canvas's drawing buffer is not kept from one frame
      // the browser shows to the next.
      surface.draw();
    },
    close() {
      for (const texture of textures) {
        gl.deleteTexture(texture);
      }
      surface.close();
    },
  };
}
