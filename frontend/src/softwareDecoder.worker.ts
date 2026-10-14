// The decode worker behind softwareDecoder.ts. It loads one software
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
// (planesPicture.ts). The decoder holds a picture until the paint worker releases
// it, and decodes the units after it into other memory meanwhile, so the next unit
// decodes while the paint worker uploads one: up to `HELD` pictures a stream are
// out at once, and a unit past that waits here for a release.
//
// A High Performance Mac's picture may come in four strips, each a unit, a frame
// being the strips that changed. Each decodes to a picture of its own, the strip,
// answered with where its rows go in the display, which the paint worker uploads
// them to; it draws the display at the frame's last strip. Nothing in a strip says
// which it is: the unit comes with that and the display's rows, and a picture that
// is not such a strip fails the stream.

import { stripRows } from "./protocol.ts";
import {
  type DecodedPlanes,
  type DecoderCommand,
  type DecoderEvent,
  MODULE_CODEC,
  type PictureStrip,
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

// 0 is 4:2:0, 1 is 4:2:2, 2 is 4:4:4.
const SUBSAMPLING = ["4:2:0", "4:2:2", "4:4:4"] as const;

// The strips a picture sent in strips comes in.
const STRIPS = 4;

/**
 * The picture `decode` just completed, as its planes and its serial: of the
 * whole picture, or for a unit that is `strip`, its rows of the display. An
 * error says what the page cannot present, or for a strip what does not fit.
 */
function picture(
  module: LoadedModule,
  decoder: ModuleDecoder,
  strip: PictureStrip | undefined,
): { planes: DecodedPlanes; serial: number } | { error: string; name: string } {
  // Read afresh: a memory that grew is a new buffer.
  const memory = module.memory.buffer;
  const p = new Int32Array(memory, decoder.picture(), 16);
  const [w, h, layout, range, matrix, primaries, transfer] = p;
  const subsampling = SUBSAMPLING[layout];
  if (!subsampling) {
    return {
      name: "NotSupportedError",
      error:
        "the stream's pictures are not 8-bit Y'CbCr, which the page presents",
    };
  }
  const made = MATRIX[matrix];
  const colorSpace = PRIMARIES[primaries];
  if (!made || !colorSpace || !TRANSFERS.includes(transfer)) {
    return {
      name: "NotSupportedError",
      error: `the stream's colors are not ones the page presents (matrix ${matrix}, primaries ${primaries}, transfer ${transfer})`,
    };
  }
  // A strip is the display's whole width and a strip's rows, which the last
  // runs past the display's end with; only the rows of the display go. A last
  // strip that starts at the display's end, holding none of it, comes only for a
  // display under 180 rows, below the 600 the gateway allows, and is refused.
  let [height, row, rows] = [h, 0, h];
  if (strip) {
    row = strip.index * stripRows(strip.rows);
    if (
      strip.index >= STRIPS ||
      h !== stripRows(strip.rows) ||
      row >= strip.rows
    ) {
      return {
        name: "EncodingError",
        error: `a ${w}x${h} picture is not strip ${strip.index} of a display of ${strip.rows} rows`,
      };
    }
    height = strip.rows;
    rows = Math.min(h, strip.rows - row);
  }
  const chromaW = layout === 2 ? w : (w + 1) >> 1;
  const chromaRows = layout === 0 ? (rows + 1) >> 1 : rows;
  return {
    serial: p[14],
    planes: {
      memory,
      width: w,
      height,
      planes: [0, 1, 2].map((i) => ({
        offset: p[7 + i],
        stride: p[10 + i],
        width: i === 0 ? w : chromaW,
        rows: i === 0 ? rows : chromaRows,
      })),
      subsampling,
      row,
      strip: strip?.index,
      keyframe: p[13] === 1,
      fullRange: range === FULL_RANGE,
      matrix: made,
      colorSpace,
    },
  };
}

// The pictures out at once for a stream, the paint worker's to release: the one
// it is uploading, and the next, decoded meanwhile. Each holds its memory in the
// module, which the units after it cannot use.
const HELD = 2;

const decoders = new Map<number, ModuleDecoder>();

// Streams whose end has been asked for and not yet reached: a unit still queued
// for one is not decoded, since nothing would release its picture.
const ending = new Set<number>();

// The serials of the pictures each stream's decoder holds for the paint worker.
const held = new Map<number, Set<number>>();

// A unit waiting for one of them to be released, by stream.
const waiting = new Map<number, () => void>();

function wake(id: number) {
  waiting.get(id)?.();
  waiting.delete(id);
}

function release(id: number, serial: number) {
  if (held.get(id)?.delete(serial)) {
    decoders.get(id)?.release(serial);
    wake(id);
  }
}

/** Until fewer than `HELD` of the stream's pictures are out, or it ends. */
async function room(id: number): Promise<void> {
  while ((held.get(id)?.size ?? 0) >= HELD && !ending.has(id)) {
    await new Promise<void>((resolve) => waiting.set(id, resolve));
  }
}

const reason = (e: unknown) => (e instanceof Error ? e.message : String(e));

function failed(id: number, name: string, message: string) {
  scope.postMessage({ type: "failed", id, name, message });
}

/** The stream's decoder and every picture it holds, given back. */
function drop(id: number) {
  const decoder = decoders.get(id);
  decoders.delete(id);
  held.delete(id);
  wake(id);
  decoder?.free();
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
  held.set(id, new Set());
}

async function destroy(id: number): Promise<void> {
  drop(id);
  ending.delete(id);
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
  await room(id);
  if (!module || ending.has(id) || decoders.get(id) !== decoder) {
    return;
  }
  const fail = (name: string, message: string) => {
    drop(id);
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
  const decoded = picture(module, decoder, command.strip);
  if ("error" in decoded) {
    fail(decoded.name, decoded.error);
    return;
  }
  held.get(id)?.add(decoded.serial);
  scope.postMessage({
    type: "decoded",
    id,
    picture: decoded.planes,
    serial: decoded.serial,
  });
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
  // Not queued: a release is what a unit waiting in the queue is waiting for,
  // and a stream that ends has closed its pictures with it.
  if (command.type === "release") {
    release(command.id, command.serial);
    return;
  }
  if (command.type === "destroy") {
    ending.add(command.id);
    wake(command.id);
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
