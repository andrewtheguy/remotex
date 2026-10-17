// The software decoder's module (softwareDecoder.ts), as a decode worker and
// each thread of its pool load it: hevc-wasm's, built by wasm-bindgen. Its glue
// and module are a release the gateway serves at `/hevc/` where it has the
// archive, imported from there at run time.
import { hevcDecoderUrl } from "./gateway.ts";
import type { SoftwareModule } from "./softwareDecoder.ts";

/**
 * A module's memory, which its threads share: a `WebAssembly.Memory` whose
 * buffer is a `SharedArrayBuffer`, as the DOM's types do not say.
 */
export interface SharedMemory extends Omit<WebAssembly.Memory, "buffer"> {
  readonly buffer: SharedArrayBuffer;
}

/** What a decode worker's instance is, for a thread's to be of the same. */
export interface PoolSeat {
  decoder: SoftwareModule;
  module: WebAssembly.Module;
  memory: SharedMemory;
}

/** One stream's decoder in a module. */
export interface ModuleDecoder {
  /**
   * Room for a unit of `size` bytes in the memory, where it is written before
   * `decode`.
   */
  input(size: number): number;
  /**
   * Decode the unit written: true when it completed a picture that is shown,
   * which `picture` describes and which is held until `release`d. Throws for a
   * unit that does not decode.
   */
  decode(): boolean;
  /**
   * Where the sixteen numbers describing the picture the last unit completed
   * are in the memory: its size, layout, colour, each plane's start and
   * stride, whether it is a keyframe, and its serial.
   */
  picture(): number;
  /**
   * Where the four numbers saying where the picture the last unit completed
   * may differ from picture `since` are in the memory: the left, top, width
   * and height of a rectangle of its planes, the whole picture where it does
   * not descend from that one, and a width of 0 where nothing changed.
   * A strip's `since` is its strip's last picture.
   */
  changed(since: number): number;
  /**
   * The paint worker is done with picture `serial`, whose memory the decoder
   * may have back. The units after a picture decode into other memory while it
   * is held.
   */
  release(serial: number): void;
  free(): void;
}

/** What a module exports, as wasm-bindgen's glue presents it. */
export interface DecoderGlue {
  /**
   * Make this worker's instance: of the module at a URL, or of a compiled one on
   * the memory another instance has.
   */
  default(options: {
    module_or_path: string | WebAssembly.Module;
    memory?: SharedMemory;
  }): Promise<{ memory: SharedMemory }>;
  /** The compiled module, for a thread to make its instance of. */
  module(): WebAssembly.Module;
  /** The pool, of `threads` workers each in `runPoolThread` already. */
  startPool(threads: number): void;
  /** Take a seat in the pool: does not return. */
  runPoolThread(): void;
  /** A decoder decoding on `threads` of the pool, or on the caller for one. */
  Decoder: new (
    threads: number,
  ) => ModuleDecoder;
}

/** The module's glue, and the URL its module is fetched from. */
export async function decoderGlue(): Promise<{
  glue: DecoderGlue;
  wasm: string;
}> {
  return {
    glue: (await import(
      /* @vite-ignore */ hevcDecoderUrl("hevc.js")
    )) as DecoderGlue,
    wasm: hevcDecoderUrl("hevc.wasm"),
  };
}
