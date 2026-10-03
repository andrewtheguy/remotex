// BETA: the decode worker behind hevcWasmDecoder.ts. It loads libavcodec's
// HEVC decoder (andrewtheguy/hevc-wasm, which the gateway serves at /hevc/) once,
// runs one decoder per stream the paint worker opens, and answers every unit with
// one picture or none.
//
// A picture leaves as where its planes are in the module's memory, which is
// shared, and the paint worker uploads them to the GPU from there
// (hevcPicture.ts). The decoder holds a picture until its next unit is put in, so
// that unit waits here until the paint worker has released the picture.

import { hevcDecoderUrl } from "./gateway.ts";
import type {
  DecodedPlanes,
  HevcCommand,
  HevcEvent,
} from "./hevcWasmDecoder.ts";

/** The module hevc-wasm's src/decoder.c builds, as its glue exposes it. */
interface HevcModule {
  _hevc_create(threads: number): number;
  _hevc_input(decoder: number, size: number): number;
  _hevc_decode(decoder: number, keyframe: number): number;
  _hevc_picture(decoder: number): number;
  _hevc_destroy(decoder: number): void;
  /** Shared, as a module with threads has it. */
  wasmMemory: { readonly buffer: SharedArrayBuffer };
}

type CreateModule = (options: {
  threads: number;
  locateFile: (path: string) => string;
}) => Promise<HevcModule>;

const scope = self as unknown as {
  postMessage(message: HevcEvent): void;
  onmessage: ((ev: MessageEvent<HevcCommand>) => void) | null;
};

// The decoder's slice threads, each a worker the module starts before it resolves.
// Rows of a picture decode in parallel under wavefront parallel processing, and
// past eight a 3200×2000 picture was measured gaining nothing more.
const THREADS = Math.max(1, Math.min(navigator.hardwareConcurrency || 4, 8));

let loading: Promise<HevcModule> | null = null;

function load(): Promise<HevcModule> {
  loading ??= (async () => {
    const { default: create } = (await import(
      /* @vite-ignore */ hevcDecoderUrl("hevc.js")
    )) as { default: CreateModule };
    return create({
      threads: THREADS,
      locateFile: (path) =>
        path.endsWith(".wasm") ? hevcDecoderUrl("hevc.wasm") : path,
    });
  })();
  return loading;
}

// FFmpeg's enums (libavutil/pixfmt.h) for what the page's shader presents
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

/** The picture `_hevc_decode` just returned, as its planes. */
function picture(module: HevcModule, decoder: number): DecodedPlanes | string {
  const memory = module.wasmMemory.buffer;
  const p = new Int32Array(memory, module._hevc_picture(decoder), 16);
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

const decoders = new Map<number, number>();

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
  const decoder = module._hevc_create(THREADS);
  if (!decoder) {
    failed(id, "NotSupportedError", "the HEVC decoder did not open");
    return;
  }
  decoders.set(id, decoder);
}

async function destroy(id: number): Promise<void> {
  const decoder = decoders.get(id);
  decoders.delete(id);
  ending.delete(id);
  if (decoder) {
    (await load())._hevc_destroy(decoder);
  }
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
    module._hevc_destroy(decoder);
    failed(id, name, message);
  };
  const size = command.data.byteLength;
  const input = module._hevc_input(decoder, size);
  if (!input) {
    fail("EncodingError", "the HEVC decoder is out of memory");
    return;
  }
  new Uint8Array(module.wasmMemory.buffer, input, size).set(
    new Uint8Array(command.data),
  );
  const ret = module._hevc_decode(decoder, command.keyframe ? 1 : 0);
  if (ret < 0) {
    fail("EncodingError", `libavcodec failed to decode a unit (error ${ret})`);
    return;
  }
  if (ret === 0) {
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
