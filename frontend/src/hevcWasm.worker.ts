// BETA: the decode worker behind hevcWasmDecoder.ts. It loads the HEVC
// decoder's module (andrewtheguy/hevc-wasm, which the gateway serves at /hevc/)
// once, with a pool of threads, runs one decoder per stream the paint worker
// opens, and answers every unit with one picture or none.
//
// The module is wasm-bindgen's, shaped as the compositor's (egfxCompositor.ts):
// one instance here, on a shared memory, and each thread of its pool a worker
// running an instance of the same module on that memory (hevcPool.worker.ts),
// which takes a seat in the pool before the pool is started.
//
// A picture leaves as where its planes are in the module's memory, which is
// shared, and the paint worker uploads them to the GPU from there
// (hevcPicture.ts). The decoder holds a picture until its next unit is put in, so
// that unit waits here until the paint worker has released the picture.

import { hevcDecoderUrl } from "./gateway.ts";
import type { PoolSeat, SharedMemory } from "./hevcPool.worker.ts";
import type {
  DecodedPlanes,
  HevcCommand,
  HevcEvent,
} from "./hevcWasmDecoder.ts";

/** What hevc-wasm's rust/hevc-web exports, as wasm-bindgen's glue presents it. */
interface HevcGlue {
  default(options: {
    module_or_path: string;
  }): Promise<{ memory: SharedMemory }>;
  /** The compiled module, for a thread to make its instance of. */
  module(): WebAssembly.Module;
  /** The pool, of `threads` workers each in `runPoolThread` already. */
  startPool(threads: number): void;
  /** A decoder decoding on `threads` of the pool, or on the caller for one. */
  Decoder: new (
    threads: number,
  ) => HevcDecoder;
}

/** One stream's decoder in the module. */
interface HevcDecoder {
  /**
   * Room for a unit of `size` bytes in the memory, where it is written before
   * `decode`. The previous picture is released here.
   */
  input(size: number): number;
  /**
   * Decode the unit written: true when it completed a picture. Throws for a unit
   * that does not decode.
   */
  decode(): boolean;
  /** Where the picture's sixteen numbers are in the memory. */
  picture(): number;
  free(): void;
}

interface HevcModule {
  glue: HevcGlue;
  /** The threads' and the paint worker's too. */
  memory: SharedMemory;
}

const scope = self as unknown as {
  postMessage(message: HevcEvent): void;
  onmessage: ((ev: MessageEvent<HevcCommand>) => void) | null;
};

// The pool's threads, on which a picture's rows decode in parallel under
// wavefront parallel processing; past eight a 3200×2000 picture was measured
// gaining nothing more. On one, the decoder decodes on this worker and starts
// no pool.
const THREADS = Math.max(1, Math.min(navigator.hardwareConcurrency || 4, 8));

let loading: Promise<HevcModule> | null = null;

function load(): Promise<HevcModule> {
  loading ??= (async () => {
    const glue = (await import(
      /* @vite-ignore */ hevcDecoderUrl("hevc.js")
    )) as HevcGlue;
    const { memory } = await glue.default({
      module_or_path: hevcDecoderUrl("hevc.wasm"),
    });
    if (THREADS > 1) {
      await seatThreads({ module: glue.module(), memory });
      glue.startPool(THREADS);
    }
    return { glue, memory };
  })();
  // A load that failed is not kept: the next stream tries again.
  loading.catch(() => {
    loading = null;
  });
  return loading;
}

/**
 * The pool's threads started, each with its instance made, before the pool is.
 * When one does not start, none is left running.
 */
async function seatThreads(seat: PoolSeat): Promise<void> {
  const threads: Worker[] = [];
  try {
    await Promise.all(
      Array.from(
        { length: THREADS },
        () =>
          new Promise<void>((resolve, reject) => {
            const thread = new Worker(
              new URL("./hevcPool.worker.ts", import.meta.url),
              { type: "module", name: "hevc-thread" },
            );
            threads.push(thread);
            thread.onmessage = ({ data }: MessageEvent<string | null>) => {
              if (data === null) {
                resolve();
              } else {
                reject(new Error(data));
              }
            };
            thread.onerror = (ev) => {
              ev.preventDefault();
              reject(
                new Error(
                  ev.message || "a thread of the decoder did not start",
                ),
              );
            };
            thread.postMessage(seat);
          }),
      ),
    );
  } catch (e) {
    for (const thread of threads) {
      thread.terminate();
    }
    throw e;
  }
}

// The stream's colour description, in H.265's codes, for what the page's shader presents
// (hevcPicture.ts): Y'CbCr made with BT.709's coefficients or BT.601's, in sRGB's
// primaries or Display P3's, the Mac's, and a transfer a display takes as it is.
// A stream that states nothing is taken as WebCodecs takes one: BT.709.
const UNSPECIFIED = 2;
const MATRIX: Record<number, DecodedPlanes["matrix"]> = {
  1: "bt709",
  [UNSPECIFIED]: "bt709",
  5: "smpte170m",
  6: "smpte170m",
};
const PRIMARIES: Record<number, PredefinedColorSpace> = {
  1: "srgb",
  [UNSPECIFIED]: "srgb",
  12: "display-p3",
};
// BT.709's, BT.601's and sRGB's.
const TRANSFERS = [1, UNSPECIFIED, 6, 13];
const FULL_RANGE = 2;

/** The picture `decode` just completed, as its planes. */
function picture(
  module: HevcModule,
  decoder: HevcDecoder,
): DecodedPlanes | string {
  // Read afresh: a memory that grew is a new buffer.
  const memory = module.memory.buffer;
  const p = new Int32Array(memory, decoder.picture(), 16);
  const [w, h, layout, range, matrix, primaries, transfer] = p;
  if (layout < 0 || layout > 2) {
    return "the stream's pictures are not 8-bit Y'CbCr, which the page presents";
  }
  const made = MATRIX[matrix];
  const colorSpace = PRIMARIES[primaries];
  if (!made || !colorSpace || !TRANSFERS.includes(transfer)) {
    return `the stream's colors are not ones the page presents (matrix ${matrix}, primaries ${primaries}, transfer ${transfer})`;
  }
  // 0 is 4:2:0, 1 is 4:2:2, 2 is 4:4:4.
  const chromaW = layout === 2 ? w : (w + 1) >> 1;
  const chromaH = layout === 0 ? (h + 1) >> 1 : h;
  return {
    memory,
    width: w,
    height: h,
    planes: [0, 1, 2].map((i) => ({
      offset: p[7 + i],
      stride: p[10 + i],
      width: i === 0 ? w : chromaW,
      rows: i === 0 ? h : chromaH,
    })),
    fullRange: range === FULL_RANGE,
    matrix: made,
    colorSpace,
  };
}

const decoders = new Map<number, HevcDecoder>();

// The pictures the paint worker is reading, by stream: what ends each wait.
const held = new Map<number, () => void>();

function release(id: number) {
  held.get(id)?.();
  held.delete(id);
}

// Streams whose end has been asked for and not yet reached: a unit still queued
// for one is not decoded, since nothing would release its picture.
const ending = new Set<number>();

const reason = (e: unknown) => (e instanceof Error ? e.message : String(e));

function failed(id: number, name: string, message: string) {
  scope.postMessage({ type: "failed", id, name, message });
}

async function create(id: number): Promise<void> {
  let module: HevcModule;
  try {
    module = await load();
  } catch (e) {
    failed(
      id,
      "NotSupportedError",
      `the HEVC decoder did not load (${reason(e)})`,
    );
    return;
  }
  decoders.set(id, new module.glue.Decoder(THREADS));
}

async function destroy(id: number): Promise<void> {
  const decoder = decoders.get(id);
  decoders.delete(id);
  ending.delete(id);
  decoder?.free();
}

async function decode(
  command: Extract<HevcCommand, { type: "decode" }>,
): Promise<void> {
  const { id } = command;
  const decoder = decoders.get(id);
  if (!decoder) {
    // Its create failed, and said so; or this worker is not the one it was
    // created in.
    failed(id, "EncodingError", "no HEVC decoder for this stream");
    return;
  }
  const module = await load();
  if (ending.has(id)) {
    return;
  }
  const fail = (name: string, message: string) => {
    decoders.delete(id);
    decoder.free();
    failed(id, name, message);
  };
  const size = command.data.byteLength;
  const input = decoder.input(size);
  new Uint8Array(module.memory.buffer, input, size).set(
    new Uint8Array(command.data),
  );
  let completed: boolean;
  try {
    completed = decoder.decode();
  } catch (e) {
    fail("EncodingError", `the HEVC decoder failed on a unit (${reason(e)})`);
    return;
  }
  if (!completed) {
    scope.postMessage({ type: "decoded", id, picture: null });
    return;
  }
  const planes = picture(module, decoder);
  if (typeof planes === "string") {
    fail("NotSupportedError", planes);
    return;
  }
  // The decoder frees the picture with its next unit, and with its end: neither
  // is started until the paint worker has read it.
  const released = new Promise<void>((resolve) => held.set(id, resolve));
  scope.postMessage({ type: "decoded", id, picture: planes });
  await released;
}

function handle(command: HevcCommand): Promise<void> {
  switch (command.type) {
    case "create":
      return create(command.id);
    case "destroy":
      return destroy(command.id);
    case "decode":
      return decode(command);
    case "release":
      return Promise.resolve();
  }
}

// One command at a time, in order: a decode that arrives while the module loads
// waits behind its create.
let queue: Promise<void> = Promise.resolve();
scope.onmessage = (ev) => {
  const command = ev.data;
  // Not queued: a release is what the queue is waiting for, and a stream that
  // ends has closed its picture with it.
  if (command.type === "release") {
    release(command.id);
    return;
  }
  if (command.type === "destroy") {
    ending.add(command.id);
    release(command.id);
  }
  queue = queue.then(() =>
    handle(command).catch((e) => {
      // Every decode is answered, a thrown one included.
      if (command.type === "decode") {
        failed(
          command.id,
          "EncodingError",
          `the HEVC decoder failed (${reason(e)})`,
        );
      }
    }),
  );
};
