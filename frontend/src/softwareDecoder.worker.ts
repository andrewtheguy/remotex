// BETA: the decode worker behind softwareDecoder.ts. It loads one software
// decoder's module once (softwareDecoderModule.ts), the one its first stream
// names, with a pool of threads, runs one decoder per stream the paint worker
// opens, and answers every unit with one picture or none.
//
// The module is wasm-bindgen's, shaped as the compositor's (egfxCompositor.ts):
// one instance here, on a shared memory, and each thread of its pool a worker
// running an instance of the same module on that memory
// (softwareDecoderPool.worker.ts), which takes a seat in the pool before the pool
// is started.
//
// A picture leaves as where its planes are in the module's memory, which is
// shared, and the paint worker uploads them to the GPU from there
// (planesPicture.ts). The decoder holds a picture until its next unit is put in,
// so that unit waits here until the paint worker has released the picture.

import {
  type DecodedPlanes,
  type DecoderCommand,
  type DecoderEvent,
  MODULE_CODEC,
  type SoftwareModule,
} from "./softwareDecoder.ts";
import {
  type DecoderGlue,
  decoderGlue,
  type ModuleDecoder,
  type PoolSeat,
  type SharedMemory,
} from "./softwareDecoderModule.ts";

interface LoadedModule {
  glue: DecoderGlue;
  /** The threads' and the paint worker's too. */
  memory: SharedMemory;
}

const scope = self as unknown as {
  postMessage(message: DecoderEvent): void;
  onmessage: ((ev: MessageEvent<DecoderCommand>) => void) | null;
};

// The pool's threads, on which a picture decodes in parallel: an HEVC picture's
// rows under wavefront parallel processing, a VP9 frame's stages. Past eight a
// 3200×2000 HEVC picture was measured gaining nothing more. On one, a decoder
// decodes on this worker and starts no pool.
const THREADS = Math.max(1, Math.min(navigator.hardwareConcurrency || 4, 8));

// The module this worker is for, which its first stream names, and its name in
// a sentence.
let loaded: SoftwareModule | null = null;
let loading: Promise<LoadedModule> | null = null;
const codec = () => (loaded ? MODULE_CODEC[loaded] : "software");

function load(decoder: SoftwareModule): Promise<LoadedModule> {
  loaded ??= decoder;
  loading ??= (async () => {
    if (decoder !== loaded) {
      throw new Error(`this worker is the ${codec()} decoder's`);
    }
    const { glue, wasm } = await decoderGlue(decoder);
    const { memory } = await glue.default(
      wasm === undefined ? undefined : { module_or_path: wasm },
    );
    if (THREADS > 1) {
      await seatThreads({ decoder, module: glue.module(), memory });
      glue.startPool(THREADS);
    }
    return { glue, memory };
  })();
  return loading;
}

/** The pool's threads started, each with its instance made, before the pool is. */
function seatThreads(seat: PoolSeat): Promise<unknown> {
  return Promise.all(
    Array.from(
      { length: THREADS },
      () =>
        new Promise<void>((resolve, reject) => {
          const thread = new Worker(
            new URL("./softwareDecoderPool.worker.ts", import.meta.url),
            { type: "module", name: `${seat.decoder}-thread` },
          );
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
              new Error(ev.message || "a thread of the decoder did not start"),
            );
          };
          thread.postMessage(seat);
        }),
    ),
  );
}

// The stream's colour description, in ITU-T H.273's codes, for what the page's
// shader presents (planesPicture.ts): Y'CbCr made with BT.709's coefficients or
// BT.601's, in sRGB's primaries or Display P3's, the Mac's, and a transfer a
// display takes as it is. A stream that states nothing is taken as WebCodecs
// takes one: BT.709.
//
// The gateway's VP9 states BT.601 for all three, VP9 having one field for them
// (`color_space`), and of the three only the matrix is a fact about it: the
// encoder's conversion (the screen-vp9 crate's `Picture`) makes its Y'CbCr from
// the desktop's own R'G'B' with BT.601's coefficients at studio swing, and
// converts neither primaries nor transfer. So SMPTE 170M's primaries are
// presented as sRGB's, which gives the display the desktop's pixels as they
// were.
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
  6: "srgb",
  12: "display-p3",
};
// BT.709's, BT.601's and sRGB's.
const TRANSFERS = [1, UNSPECIFIED, 6, 13];
const FULL_RANGE = 2;

/** The picture `decode` just completed, as its planes. */
function picture(
  module: LoadedModule,
  decoder: ModuleDecoder,
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

const decoders = new Map<number, ModuleDecoder>();

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

async function create(id: number, decoder: SoftwareModule): Promise<void> {
  let module: LoadedModule;
  try {
    module = await load(decoder);
  } catch (e) {
    // Not to be tried again here: a thread that took its seat holds the seats
    // for good, in a memory this worker's module stays on. The paint worker
    // ends this worker, and its threads with it, and the next stream has a new one.
    scope.postMessage({
      type: "broken",
      message: `the ${codec()} decoder did not load (${reason(e)})`,
    });
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
  command: Extract<DecoderCommand, { type: "decode" }>,
): Promise<void> {
  const { id } = command;
  const decoder = decoders.get(id);
  if (!decoder) {
    // Its create failed, and said so; or this worker is not the one it was
    // created in.
    failed(id, "EncodingError", `no ${codec()} decoder for this stream`);
    return;
  }
  // Loaded: the stream's `create` waited for it.
  const module = await loading;
  if (!module || ending.has(id)) {
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
    fail(
      "EncodingError",
      `the ${codec()} decoder failed on a unit (${reason(e)})`,
    );
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

function handle(command: DecoderCommand): Promise<void> {
  switch (command.type) {
    case "create":
      return create(command.id, command.module);
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
          `the ${codec()} decoder failed (${reason(e)})`,
        );
      }
    }),
  );
};
