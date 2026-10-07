// The WebGL canvas the page lays over the desktop's (RemoteDesktop.tsx), as the two
// pictures drawn on it hold it: an RDP host's graphics pipeline (egfxPicture.ts)
// and a software decoder's (planesPicture.ts). Both are pixels in a memory
// WebAssembly threads share, which the desktop's 2D canvas takes no image data
// out of and a WebGL texture takes an upload from as it is.
//
// The canvas outlives a picture: it is the page's, handed to the paint worker
// once, and its WebGL context is the one it will ever have. A picture is what one
// session keeps in that context, and gives back.
//
// Without WebGL 2 there is no such picture: the page says so.

// One triangle over the whole canvas, `uv` from its top left corner's (0, 1) down
// to the bottom right's (1, 0).
const VERTEX = `#version 300 es
out vec2 uv;
void main() {
  uv = vec2((gl_VertexID & 1) * 2, gl_VertexID & 2);
  gl_Position = vec4(uv * 2.0 - 1.0, 0.0, 1.0);
}`;

const LOST = "the browser took the picture's WebGL context away";

export interface PictureCanvas {
  readonly gl: WebGL2RenderingContext;
  /** The picture's program, in use. */
  readonly program: WebGLProgram;
  /** Throws once the browser has taken the GPU away (a lost context). */
  usable(): void;
  /**
   * The canvas at this size, its drawing buffer cleared. WebGL reports a refused
   * size as a smaller buffer, not an exception, and a draw into that one is not
   * the picture: so it is checked here, and thrown.
   */
  resize(w: number, h: number): void;
  /** One triangle over the whole canvas, through the program. */
  draw(): void;
  /**
   * Give the program back and leave the canvas black, its context as it was
   * found: what it shows next is another picture's.
   */
  close(): void;
}

function shader(gl: WebGL2RenderingContext, kind: number, source: string) {
  const made = gl.createShader(kind);
  if (!made) {
    throw new Error("WebGL 2 gave no shader");
  }
  gl.shaderSource(made, source);
  gl.compileShader(made);
  return made;
}

/**
 * Take `canvas` for a picture drawn by `fragment`, a fragment shader given `uv`
 * by the vertex shader above.
 */
export function openPictureCanvas(
  canvas: OffscreenCanvas,
  fragment: string,
): PictureCanvas {
  // The canvas's one context: made by the first picture, the same one after.
  const gl = canvas.getContext("webgl2", {
    alpha: false,
    antialias: false,
    depth: false,
    stencil: false,
  });
  if (!gl) {
    throw new Error("WebGL 2 is not available");
  }
  if (gl.isContextLost()) {
    throw new Error(LOST);
  }
  let lost = false;
  const listening = new AbortController();
  canvas.addEventListener(
    "webglcontextlost",
    (event) => {
      event.preventDefault();
      lost = true;
    },
    { signal: listening.signal },
  );
  const vertex = shader(gl, gl.VERTEX_SHADER, VERTEX);
  const pixels = shader(gl, gl.FRAGMENT_SHADER, fragment);
  const program = gl.createProgram();
  gl.attachShader(program, vertex);
  gl.attachShader(program, pixels);
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    throw new Error(
      `WebGL 2 refused the picture's shaders: ${gl.getProgramInfoLog(program) || gl.getShaderInfoLog(pixels)}`,
    );
  }
  gl.useProgram(program);
  const largest = gl.getParameter(gl.MAX_TEXTURE_SIZE) as number;

  return {
    gl,
    program,
    usable() {
      if (lost) {
        throw new Error(LOST);
      }
    },
    resize(w, h) {
      if (w > largest || h > largest) {
        throw new Error(
          `the GPU takes no picture over ${largest} pixels a side (this one is ${w}x${h})`,
        );
      }
      canvas.width = w;
      canvas.height = h;
      if (gl.drawingBufferWidth !== w || gl.drawingBufferHeight !== h) {
        throw new Error(
          `the GPU gave a ${gl.drawingBufferWidth}x${gl.drawingBufferHeight} canvas for a ${w}x${h} picture`,
        );
      }
      gl.viewport(0, 0, w, h);
    },
    draw() {
      gl.drawArrays(gl.TRIANGLES, 0, 3);
    },
    close() {
      listening.abort();
      if (lost) {
        return;
      }
      gl.deleteProgram(program);
      gl.deleteShader(vertex);
      gl.deleteShader(pixels);
      // What a picture may have set, as the next one expects to find it.
      gl.activeTexture(gl.TEXTURE0);
      gl.pixelStorei(gl.UNPACK_ALIGNMENT, 4);
      gl.pixelStorei(gl.UNPACK_ROW_LENGTH, 0);
      gl.pixelStorei(gl.UNPACK_SKIP_PIXELS, 0);
      gl.pixelStorei(gl.UNPACK_SKIP_ROWS, 0);
      if ("drawingBufferColorSpace" in gl) {
        gl.drawingBufferColorSpace = "srgb";
      }
      // One black pixel: a canvas sized to nothing keeps showing its last frame.
      canvas.width = 1;
      canvas.height = 1;
      gl.clearColor(0, 0, 0, 1);
      gl.clear(gl.COLOR_BUFFER_BIT);
    },
  };
}
