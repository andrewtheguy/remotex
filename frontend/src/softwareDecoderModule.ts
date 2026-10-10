// The software decoders' modules (softwareDecoder.ts), as a decode worker
// and each thread of its pool load one. Both are wasm-bindgen's, with one
// interface: hevc-wasm's, which vp9-wasm's was written to.
//
// Where a module comes from is the difference. The HEVC decoder's glue and
// module are a release the gateway serves at `/hevc/` where it has the archive,
// imported from there at run time. The VP9 decoder's are in the bundle
// (frontend/wasm/vp9), and its glue finds its module beside itself.
import * as vp9 from "../wasm/vp9/pkg/vp9.js";
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
   * `decode`. The previous picture is released here.
   */
  input(size: number): number;
  /**
   * Decode the unit written: true when it completed a picture that is shown.
   * Throws for a unit that does not decode.
   */
  decode(): boolean;
  /**
   * The HEVC module's alone: decode the unit written as strip `strip`, from 0
   * at the top, of a picture of `rows` rows sent in four. True when the picture
   * has every strip since its keyframe, and `picture` then describes it whole.
   */
  decodeStrip?(strip: number, rows: number): boolean;
  /** Where the picture's sixteen numbers are in the memory. */
  picture(): number;
  free(): void;
}

/** What a module exports, as wasm-bindgen's glue presents it. */
export interface DecoderGlue {
  /**
   * Make this worker's instance: of the module at a URL, or of a compiled one on
   * the memory another instance has. Given no module, the glue fetches its own.
   */
  default(options?: {
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

/** A module's glue, and the URL its module is fetched from where the glue has none. */
export async function decoderGlue(
  decoder: SoftwareModule,
): Promise<{ glue: DecoderGlue; wasm?: string }> {
  if (decoder === "vp9") {
    return { glue: vp9 as unknown as DecoderGlue };
  }
  return {
    glue: (await import(
      /* @vite-ignore */ hevcDecoderUrl("hevc.js")
    )) as DecoderGlue,
    wasm: hevcDecoderUrl("hevc.wasm"),
  };
}
