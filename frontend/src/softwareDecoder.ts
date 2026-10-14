// The page's software decoders, for a stream the browser's `VideoDecoder` does
// not take, or is not given:
// - `vp9`, the gateway's VP9 at 4:4:4, profile 1: andrewtheguy/vp9-wasm's
//   decoder, which is in the bundle (frontend/wasm/vp9). Which decodes the VP9 is
//   each page's to say: the browser's own decoder where it takes profile 1, and
//   this one where it does not (nativeVp9.ts), so a session's pages may differ.
// - `hevc`, a High Performance Mac's passed HEVC: andrewtheguy/hevc-wasm's
//   decoder, which the gateway serves where it has the release archive. Which
//   decodes that is each page's to say too: the browser's own decoder where it
//   takes the Mac's HEVC, and this one where it does not (appleMedia.ts).
//
// `pageModuleFor` says which, and `videoDecoder.ts` builds it.
//
// Each is written for the one stream it decodes and compiled to WebAssembly with
// SIMD128 and threads, and the two modules present one interface, so one worker
// implementation runs either (softwareDecoder.worker.ts). A module runs in a
// decode worker of its own beside the paint worker: a picture takes milliseconds
// of CPU, and the paint worker has to go on answering a `clear` while one does.
//
// A decoder here is shaped as a `VideoDecoder` — configure, decode, close, an
// output and an error callback — so `createVideoStream` (videoDecoder.ts) runs it
// exactly as it runs the browser's: the same keyframe gate, the same FIFO of
// promises, the same stall backstop. Unlike a `VideoDecoder`, it keeps the pairing
// that stream only hopes for: the decode worker answers every unit with one
// picture or with none, and a none is `noPicture`, which settles that unit's entry
// to null rather than leaving it for the next picture to resolve.
//
// What it outputs is not a `VideoFrame` but the picture's planes where the decoder
// left them (`SoftwarePlanes`): the module's memory is one its threads share, so
// the paint worker reads it too, and uploads the planes to the GPU from there
// (planesPicture.ts). A `VideoFrame` over them was a copy of the picture, and
// drawing it on the desktop's canvas had the GPU convert and copy it again: on an
// Intel UHD 630 that was well over twice the GPU's time and three times the
// workers'. Copying the planes out here instead is no way around it: a copy out
// of a shared memory took longer than the rest of presenting together. So the
// decoder holds each picture's memory until the picture is closed, and decodes
// the units after it into other memory meanwhile: the next unit decodes while the
// paint worker uploads one (`HELD` in softwareDecoder.worker.ts).
//
// A High Performance Mac's display may come in four strips, each a unit and each
// decoded to a picture of its own. The decode worker says where in the display a
// strip's planes go, and the paint worker uploads them there, into textures of the
// display's size, and draws the display at its frame's last strip
// (planesPicture.ts): the strips that did not change are on the GPU already.

/** The modules: which stream each decodes is `softwareModuleFor`'s to say. */
export type SoftwareModule = "hevc" | "vp9";

/** What a module decodes, as a sentence names it. */
export const MODULE_CODEC: Record<SoftwareModule, string> = {
  hevc: "HEVC",
  vp9: "VP9",
};

/**
 * The module that decodes a configuration string, or null for one neither does:
 * HEVC, and VP9 profile 1, which is the gateway's 4:4:4 (`codec_string` in
 * src/vp9.rs).
 */
export function softwareModuleFor(codec: string): SoftwareModule | null {
  if (codec.startsWith("hev1.") || codec.startsWith("hvc1.")) {
    return "hevc";
  }
  return codec.startsWith("vp09.01.") ? "vp9" : null;
}

/**
 * The page's module that decodes a stream announced as `decode`, or undefined
 * for the browser's own decoder: the module where the browser's own decoder
 * does not take the stream, the gateway's VP9 at profile 1 (`nativeVp9`) or a
 * Mac's HEVC (`nativeHevc`).
 */
export function pageModuleFor(
  decode: string,
  native: { nativeVp9: boolean; nativeHevc: boolean },
): SoftwareModule | undefined {
  const module = softwareModuleFor(decode);
  if (module === "vp9" && !native.nativeVp9) {
    return "vp9";
  }
  return module === "hevc" && !native.nativeHevc ? "hevc" : undefined;
}

/** What the paint worker sends the decode worker. */
export type DecoderCommand =
  /** A stream's decoder, in `module`: the one module a decode worker ever loads. */
  | { type: "create"; id: number; module: SoftwareModule }
  | { type: "decode"; id: number; data: ArrayBuffer; strip?: PictureStrip }
  /** The paint worker is done reading the picture of that serial. */
  | { type: "release"; id: number; serial: number }
  | { type: "destroy"; id: number };

/**
 * A unit that is one strip of a picture of `rows` rows and not the whole of it
 * (`VideoStrip` in protocol.ts): the decode worker says where its planes go.
 */
export interface PictureStrip {
  index: number;
  rows: number;
}

/** One plane of a picture: where its first row is in the memory, and its size. */
export interface PicturePlane {
  offset: number;
  stride: number;
  width: number;
  rows: number;
}

/**
 * A decoded picture as the decode worker describes it: 8-bit Y'CbCr planes, of
 * the whole picture or of one strip of it.
 */
export interface DecodedPlanes {
  /** The module's memory, which the planes are in until the picture is released. */
  memory: SharedArrayBuffer;
  /** The picture's size: the display's, for a strip. */
  width: number;
  height: number;
  /**
   * Luma, then the two chroma planes, as many of their rows as are of the
   * picture: the chroma may be half the luma's size either way.
   */
  planes: PicturePlane[];
  /** How the chroma is subsampled, which sizes its planes of the picture. */
  subsampling: "4:4:4" | "4:2:2" | "4:2:0";
  /** The picture's row the luma's first is: 0, but for a strip below the first. */
  row: number;
  /**
   * The strip of the picture these are, from 0 at the top, of four: a picture
   * shown once each has come since a keyframe. None for a picture whole.
   */
  strip?: number;
  /** Predicted from nothing before it: the strips placed before it are of no frame since. */
  keyframe: boolean;
  fullRange: boolean;
  /** Which coefficients made the luma: BT.709's, or BT.601's. */
  matrix: "bt709" | "smpte170m";
  /** The primaries, as the canvas color space that has them. */
  colorSpace: PredefinedColorSpace;
}

/** A decoded picture the paint worker holds: read until closed, and closed once. */
export interface SoftwarePlanes extends DecodedPlanes {
  /** Done with the planes; the decoder may go on to its next unit. */
  close(): void;
}

/** What a stream's decoder outputs: the browser's frame, or a software decoder's planes. */
export type DecodedPicture = VideoFrame | SoftwarePlanes;

export function isSoftwarePlanes(
  picture: DecodedPicture,
): picture is SoftwarePlanes {
  return "planes" in picture;
}

/**
 * What the decode worker answers: one `decoded` or `failed` per `decode`, in
 * order. A strip's answer is its frame's picture where it is the last of the
 * frame, and none otherwise.
 */
export type DecoderEvent =
  | { type: "decoded"; id: number; picture: null }
  /** `serial` is the picture's, which its release names. */
  | { type: "decoded"; id: number; picture: DecodedPlanes; serial: number }
  /**
   * The decoder is over. `name` follows WebCodecs: `NotSupportedError` for a
   * module or a picture this browser cannot run at all, `EncodingError` for a unit
   * that failed to decode.
   */
  | { type: "failed"; id: number; name: string; message: string }
  /**
   * The module did not load, and cannot in this worker again: every decoder in
   * it is over, and the next stream starts another worker.
   */
  | { type: "broken"; message: string };

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
  /**
   * `strip` is for a software decoder, whose decode worker says where in the
   * picture the strip's planes go. `VideoDecoder` has no such argument: the
   * painter places each strip it outputs by the strip's number
   * (framePainter.ts).
   */
  decode(chunk: EncodedVideoChunk, strip?: PictureStrip): void;
  close(): void;
}

interface Client {
  onEvent: (event: Exclude<DecoderEvent, { type: "broken" }>) => void;
}

// One decode worker for each module, for the paint worker's lifetime, started by
// the first stream the module decodes: loading a module compiles it and starts
// its threads, which a resize should not pay for again. Each stream is a decoder
// of its own inside it.
const workers: Record<SoftwareModule, Worker | null> = {
  hevc: null,
  vp9: null,
};
const clients: Record<SoftwareModule, Map<number, Client>> = {
  hevc: new Map(),
  vp9: new Map(),
};
let nextId = 1;

function decodeWorker(module: SoftwareModule): Worker {
  const running = workers[module];
  if (running) {
    return running;
  }
  const started = new Worker(
    new URL("./softwareDecoder.worker.ts", import.meta.url),
    { type: "module", name: `${module}-decoder` },
  );
  // The worker is over, and every decoder in it: the next stream starts another.
  const over = (message: string) => {
    if (workers[module] !== started) {
      return;
    }
    workers[module] = null;
    for (const [id, client] of clients[module]) {
      client.onEvent({
        type: "failed",
        id,
        name: "NotSupportedError",
        message,
      });
    }
    started.terminate();
  };
  started.onmessage = (ev: MessageEvent<DecoderEvent>) => {
    const event = ev.data;
    if (event.type === "broken") {
      over(event.message);
      return;
    }
    const client = clients[module].get(event.id);
    if (client) {
      client.onEvent(event);
    }
    // A stream closed while this was on its way has nothing to release: its
    // `destroy` did.
  };
  started.onerror = (ev) => {
    // A worker that failed to start takes every decoder in it down.
    ev.preventDefault();
    over(
      `the ${MODULE_CODEC[module]} decoder's worker failed (${ev.message || "no message"})`,
    );
  };
  workers[module] = started;
  return started;
}

/** A `VideoDecoder` for the stream `module` decodes, decoding in its decode worker. */
export function createSoftwareDecoder(
  module: SoftwareModule,
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
    clients[module].delete(id);
    if (configured) {
      workers[module]?.postMessage({
        type: "destroy",
        id,
      } satisfies DecoderCommand);
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
      if (softwareModuleFor(config.codec) !== module) {
        // Asynchronously, as `VideoDecoder` reports a refused configuration.
        queueMicrotask(() =>
          fail(
            "NotSupportedError",
            `not a configuration the ${MODULE_CODEC[module]} decoder takes: ${config.codec}`,
          ),
        );
        return;
      }
      state = "configured";
      clients[module].set(id, {
        onEvent: (event) => {
          if (event.type === "failed") {
            fail(event.name, event.message);
          } else if (event.picture) {
            let open = true;
            const { serial } = event;
            init.output({
              ...event.picture,
              close() {
                // Once, and only to a decoder still there: `destroy` releases too.
                if (open && state === "configured") {
                  workers[module]?.postMessage({
                    type: "release",
                    id,
                    serial,
                  } satisfies DecoderCommand);
                }
                open = false;
              },
            });
          } else {
            init.noPicture?.();
          }
        },
      });
      decodeWorker(module).postMessage({
        type: "create",
        id,
        module,
      } satisfies DecoderCommand);
    },
    decode(chunk, strip) {
      if (state !== "configured") {
        throw new DOMException(
          "decode on a decoder not configured",
          "InvalidStateError",
        );
      }
      const data = new ArrayBuffer(chunk.byteLength);
      chunk.copyTo(data);
      decodeWorker(module).postMessage(
        { type: "decode", id, data, strip } satisfies DecoderCommand,
        [data],
      );
    },
    close,
  };
}

/** Test seam: end every decode worker, so the next stream starts its module's. */
export function resetSoftwareDecodersForTests(): void {
  for (const module of ["hevc", "vp9"] as const) {
    workers[module]?.terminate();
    workers[module] = null;
    clients[module].clear();
  }
}
