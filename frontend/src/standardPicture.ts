import init, {
  type InitInput,
  Standard,
} from "../wasm/apple-adaptive/pkg/remotex_apple_adaptive.js";

// A Mac's Adaptive picture, decoded in the page: the rectangles of a session
// started with Standard's Adaptive quality, which the gateway passes as the Mac
// sent them, its own 8x8 tiles, drawn onto a framebuffer of the module's own
// (wasm/apple-adaptive). Each rectangle is drawn on what the ones before it
// left, so one picture is fed every record of its session, in order, and the
// canvas is painted from its framebuffer where each rectangle drew. The Mac's Full
// quality never comes here: the gateway inflates it and sends its tiles.

/** One session's picture. */
export interface StandardPicture {
  /** A black framebuffer of the desktop's new size, in pixels. */
  resize(w: number, h: number): void;
  /**
   * Decode one rectangle onto the framebuffer: true when it drew, false for one
   * that draws nothing, such as the quantization tables. Throws for one that
   * does not decode, whose pixels the framebuffer keeps as far as they were drawn.
   */
  decode(x: number, y: number, w: number, h: number, body: Uint8Array): boolean;
  /**
   * The framebuffer, R, G, B, A, a view of the module's memory valid until the
   * next call: null before the first resize.
   */
  pixels(): Uint8ClampedArray | null;
  /** The framebuffer as an image to paint the canvas from, on the same terms. */
  image(): ImageData | null;
  close(): void;
}

export type StandardFactory = () => StandardPicture;

let loaded: Promise<StandardFactory> | null = null;

/**
 * Load the module, once a page: `source` is its bytes for a test, and otherwise
 * the bundle's own copy is fetched. A load that fails is tried again by the next
 * caller.
 */
export function loadStandard(source?: InitInput): Promise<StandardFactory> {
  loaded ??= init(source === undefined ? undefined : { module_or_path: source })
    .then(({ memory }) => (): StandardPicture => {
      const standard = new Standard();
      let width = 0;
      let height = 0;
      // A view goes with the memory it was made of, which a growth replaces.
      let view: { buffer: ArrayBuffer; pixels: Uint8ClampedArray } | null =
        null;
      let image: ImageData | null = null;
      const pixels = () => {
        if (width === 0 || height === 0) {
          return null;
        }
        if (view?.buffer !== memory.buffer) {
          view = {
            buffer: memory.buffer,
            pixels: new Uint8ClampedArray(
              memory.buffer,
              standard.pixels(),
              width * height * 4,
            ),
          };
          image = null;
        }
        return view.pixels;
      };
      return {
        resize(w, h) {
          standard.resize(w, h);
          width = w;
          height = h;
          view = null;
          image = null;
        },
        decode(x, y, w, h, body) {
          return standard.decode(x, y, w, h, body);
        },
        pixels,
        image() {
          const current = pixels();
          if (!current) {
            return null;
          }
          image ??= new ImageData(
            current as Uint8ClampedArray<ArrayBuffer>,
            width,
            height,
          );
          return image;
        },
        close() {
          standard.free();
        },
      };
    })
    .catch((error: unknown) => {
      loaded = null;
      throw error;
    });
  return loaded;
}
