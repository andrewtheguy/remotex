// BETA: a software decoder for a High Performance Mac's passed HEVC, for a
// browser whose `VideoDecoder` does not take it (appleMedia.ts decides).
//
// andrewtheguy/hevc-wasm's decoder, written for the Mac's stream and compiled to
// WebAssembly with SIMD128 and threads, running in a worker of its own beside
// the paint worker: a picture takes milliseconds of CPU, and the paint worker
// has to go on answering a `clear` while one does.
//
// It is shaped as a `VideoDecoder` — configure, decode, close, an output and an
// error callback — so `createVideoStream` (videoDecoder.ts) runs it exactly as it
// runs the browser's: the same keyframe gate, the same FIFO of promises, the same
// stall backstop. Unlike a `VideoDecoder`, it keeps the pairing that stream only
// hopes for: the decode worker answers every unit with one picture or with none,
// and a none is `noPicture`, which settles that unit's entry to null rather than
// leaving it for the next picture to resolve.
//
// What it outputs is not a `VideoFrame` but the picture's planes where the decoder
// left them (`HevcPlanes`): the module's memory is one its threads share, so the
// paint worker reads it too, and uploads the planes to the GPU from there
// (hevcPicture.ts). A `VideoFrame` over them was a copy of the picture, and
// drawing it on the desktop's canvas had the GPU convert and copy it again: on an
// Intel UHD 630 that was well over twice the GPU's time and three times the
// workers'. Copying the planes out here instead is no way around it: a copy out
// of a shared memory took longer than the rest of presenting together. The price
// is that the decoder waits: it reuses a picture's memory from its next unit on,
// so it starts that unit only once the picture is closed.

/** What the paint worker sends the decode worker. */
export type HevcCommand =
  | { type: "create"; id: number }
  | { type: "decode"; id: number; data: ArrayBuffer }
  /** The paint worker is done reading the picture last answered with. */
  | { type: "release"; id: number }
  | { type: "destroy"; id: number };

/** One plane of a picture: where its first row is in the memory, and its size. */
export interface HevcPlane {
  offset: number;
  stride: number;
  width: number;
  rows: number;
}

/** A decoded picture as the decode worker describes it: 8-bit Y'CbCr planes. */
export interface DecodedPlanes {
  /** The module's memory, which the planes are in until the picture is released. */
  memory: SharedArrayBuffer;
  width: number;
  height: number;
  /** Luma, then the two chroma planes, which may be half its size either way. */
  planes: HevcPlane[];
  fullRange: boolean;
  /** Which coefficients made the luma: BT.709's, or BT.601's. */
  matrix: "bt709" | "smpte170m";
  /** The primaries, as the canvas color space that has them. */
  colorSpace: PredefinedColorSpace;
}

/** A decoded picture the paint worker holds: read until closed, and closed once. */
export interface HevcPlanes extends DecodedPlanes {
  /** Done with the planes; the decoder may go on to its next unit. */
  close(): void;
}

/** What a stream's decoder outputs: the browser's frame, or this decoder's planes. */
export type DecodedPicture = VideoFrame | HevcPlanes;

export function isHevcPlanes(picture: DecodedPicture): picture is HevcPlanes {
  return "planes" in picture;
}

/** What the decode worker answers: one `decoded` or `failed` per `decode`. */
export type HevcEvent =
  | { type: "decoded"; id: number; picture: DecodedPlanes | null }
  /**
   * The decoder is over. `name` follows WebCodecs: `NotSupportedError` for a
   * module or a picture this browser cannot run at all, `EncodingError` for a unit
   * that failed to decode.
   */
  | { type: "failed"; id: number; name: string; message: string };

/** What `createVideoStream` builds a decoder with. */
export interface VideoDecoderLikeInit {
  output: (picture: DecodedPicture) => void;
  error: (error: DOMException) => void;
  /**
   * A unit decoded to no picture. Only a decoder that answers every unit calls it;
   * `VideoDecoder` never does.
   */
  noPicture?: () => void;
}

/** The part of `VideoDecoder` that `createVideoStream` uses. */
export interface VideoDecoderLike {
  readonly state: CodecState;
  configure(config: VideoDecoderConfig): void;
  decode(chunk: EncodedVideoChunk): void;
  close(): void;
}

interface Client {
  onEvent: (event: HevcEvent) => void;
}

// One decode worker for the paint worker's lifetime, started by the first HEVC
// stream: loading the module compiles it and starts its threads, which a resize
// should not pay for again. Each stream is a decoder of its own inside it.
let worker: Worker | null = null;
const clients = new Map<number, Client>();
let nextId = 1;

function decodeWorker(): Worker {
  if (worker) {
    return worker;
  }
  const started = new Worker(new URL("./hevcWasm.worker.ts", import.meta.url), {
    type: "module",
    name: "hevc-decoder",
  });
  started.onmessage = (ev: MessageEvent<HevcEvent>) => {
    const event = ev.data;
    const client = clients.get(event.id);
    if (client) {
      client.onEvent(event);
    }
    // A stream closed while this was on its way has nothing to release: its
    // `destroy` did.
  };
  started.onerror = (ev) => {
    // A worker that failed to start takes every decoder in it down.
    ev.preventDefault();
    worker = null;
    for (const [id, client] of clients) {
      client.onEvent({
        type: "failed",
        id,
        name: "NotSupportedError",
        message: `the HEVC decoder's worker failed (${ev.message || "no message"})`,
      });
    }
    started.terminate();
  };
  worker = started;
  return started;
}

/** Whether a configuration string names HEVC. */
export function isHevc(codec: string): boolean {
  return codec.startsWith("hev1.") || codec.startsWith("hvc1.");
}

/** A `VideoDecoder` for HEVC, decoding in the decode worker. */
export function createWasmHevcDecoder(
  init: VideoDecoderLikeInit,
): VideoDecoderLike {
  const id = nextId++;
  let state: CodecState = "unconfigured";

  const fail = (name: string, message: string) => {
    if (state === "closed") {
      return;
    }
    close();
    const error = new Error(message);
    error.name = name;
    init.error(error as DOMException);
  };

  const close = () => {
    if (state === "closed") {
      return;
    }
    const configured = state === "configured";
    state = "closed";
    clients.delete(id);
    if (configured) {
      worker?.postMessage({ type: "destroy", id } satisfies HevcCommand);
    }
  };

  return {
    get state() {
      return state;
    },
    configure(config) {
      if (state === "closed") {
        throw new DOMException(
          "configure on a closed decoder",
          "InvalidStateError",
        );
      }
      if (!isHevc(config.codec)) {
        // Asynchronously, as `VideoDecoder` reports a refused configuration.
        queueMicrotask(() =>
          fail(
            "NotSupportedError",
            `not an HEVC configuration: ${config.codec}`,
          ),
        );
        return;
      }
      state = "configured";
      clients.set(id, {
        onEvent: (event) => {
          if (event.type === "failed") {
            fail(event.name, event.message);
          } else if (event.picture) {
            let open = true;
            init.output({
              ...event.picture,
              close() {
                // Once, and only to a decoder still there: `destroy` releases too.
                if (open && state === "configured") {
                  worker?.postMessage({
                    type: "release",
                    id,
                  } satisfies HevcCommand);
                }
                open = false;
              },
            });
          } else {
            init.noPicture?.();
          }
        },
      });
      decodeWorker().postMessage({ type: "create", id } satisfies HevcCommand);
    },
    decode(chunk) {
      if (state !== "configured") {
        throw new DOMException(
          "decode on a decoder not configured",
          "InvalidStateError",
        );
      }
      const data = new ArrayBuffer(chunk.byteLength);
      chunk.copyTo(data);
      decodeWorker().postMessage(
        { type: "decode", id, data } satisfies HevcCommand,
        [data],
      );
    },
    close,
  };
}
