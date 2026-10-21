// The H.264 decoders of a passed RDP pipeline: ALPHA, behind the picker's
// adaptive choice (rdpH264.ts).
//
// A host that draws video with H.264 gives every surface it draws that way a stream
// of its own, and each stream is a chain: an access unit means "what changed since
// the one before it". So there is one `VideoDecoder` for each surface, made on the
// surface's first unit and closed when the surface is created again or deleted, and
// every unit of a stream goes through its decoder in the order it arrived, shown or
// not.
//
// The stream is the host's as it came: Annex B, its parameter sets inline at each
// keyframe, so a decoder is configured with a codec string and no description. The
// string is made from the parameter set's own profile and level, which the
// compositor's scan reads off the unit (egfxCompositor.ts), and a keyframe that
// names another configures the decoder again.
//
// **Its colour, told.** The host's stream says it is full range and names no
// colour space; MS-RDPEGFX says full-range BT.709. The decoder is told so. A
// picture the browser then labels with that colour is one it converts to RGB as
// the compositor would, and can be drawn as the browser draws it, from the GPU a
// hardware decoder made it on (`drawnAsCoded`). Measured (tmp/h264-gpu, 2026-10-20):
// Chrome's hardware decoder on Windows takes the colour space and Safari's labels
// the stream so untold, both drawing within a step of the compositor; Chrome's
// hardware decoder on a Mac keeps its limited-range label, and its software
// decoder a BT.601 matrix, and their pictures are copied and converted here as
// before.
//
// **One picture for each unit, awaited.** The compositor composes a run's commands
// in order and takes a unit's picture where its command is, so a unit is decoded
// and its picture in hand before the run is composed. So the decoder has to give
// one picture out for one unit in, holding none back for the units after it, which
// the page finds out before a host is told it may send H.264: it puts a stream of
// its own through these decoders when it loads (rdpH264.ts). One that then gives
// none in time, or fails, ends the pipeline, since the host sends no keyframe to a
// client that asks and the units after it are a chain with a link missing.

import type { H264Unit } from "./egfxCompositor.ts";

/**
 * How long a decoder may take over one access unit's picture. A decoder that is
 * starting takes the longest; one that has dropped the unit, or is holding its
 * picture for the unit after it, never answers.
 */
const PICTURE_TIMEOUT_MS = 5000;

/**
 * The colour MS-RDPEGFX gives the host's H.264: full-range BT.709, its pixels the
 * desktop's own, so no transfer between them.
 */
const RDP_COLOUR: VideoColorSpaceInit = {
  primaries: "bt709",
  transfer: "iec61966-2-1",
  matrix: "bt709",
  fullRange: true,
};

/**
 * Whether the browser draws this picture as the compositor would paint it: labelled
 * with the stream's own colour, and shown at the size it was coded, so a texture
 * made of it holds the compositor's pixels where the picture's are.
 */
export function drawnAsCoded(
  frame: Pick<
    VideoFrame,
    "colorSpace" | "visibleRect" | "displayWidth" | "displayHeight"
  >,
): boolean {
  const { primaries, transfer, matrix, fullRange } = frame.colorSpace;
  const visible = frame.visibleRect;
  return (
    primaries === RDP_COLOUR.primaries &&
    transfer === RDP_COLOUR.transfer &&
    matrix === RDP_COLOUR.matrix &&
    fullRange === true &&
    visible !== null &&
    frame.displayWidth === visible.width &&
    frame.displayHeight === visible.height
  );
}

/**
 * How a decoder for the host's stream is configured: for one picture out for each
 * access unit in, which is what composing in command order needs. Which decoder
 * that is, the GPU's or one in software, is left to the browser, as it is for the
 * desktop's own stream (videoDecoder.ts). Null where the browser has none.
 */
export async function rdpH264Config(
  codec: string,
): Promise<VideoDecoderConfig | null> {
  const config: VideoDecoderConfig = {
    codec,
    optimizeForLatency: true,
    colorSpace: RDP_COLOUR,
  };
  const support = await VideoDecoder.isConfigSupported(config);
  return support.supported === true ? config : null;
}

export interface EgfxVideo {
  /**
   * Decode one access unit of its surface's stream. Resolves to the unit's picture,
   * which the caller closes; rejects where there is none to be had.
   */
  decode(unit: H264Unit, data: Uint8Array): Promise<VideoFrame>;
  /** The surface's stream is over: its decoder is done with. */
  drop(surface: number): void;
  /** Every decoder closed, and whatever is waiting on one rejected. */
  close(): void;
}

interface Waiting {
  resolve: (frame: VideoFrame) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

interface Stream {
  decoder: VideoDecoder;
  /** The codec string the decoder is configured with, null before a keyframe. */
  codec: string | null;
  /** The unit whose picture is awaited: one at a time, the caller's order. */
  waiting: Waiting | null;
  units: number;
}

/** The decoders, each given `patienceMs` for a unit's picture. */
export function createEgfxVideo(patienceMs = PICTURE_TIMEOUT_MS): EgfxVideo {
  const streams = new Map<number, Stream>();
  let closed = false;

  const settle = (stream: Stream): Waiting | null => {
    const waiting = stream.waiting;
    stream.waiting = null;
    if (waiting) {
      clearTimeout(waiting.timer);
    }
    return waiting;
  };

  const end = (stream: Stream, why: string) => {
    settle(stream)?.reject(new Error(why));
    if (stream.decoder.state !== "closed") {
      stream.decoder.close();
    }
  };

  const open = (surface: number): Stream => {
    const stream: Stream = {
      decoder: new VideoDecoder({
        output: (frame) => {
          const waiting = settle(stream);
          if (waiting) {
            waiting.resolve(frame);
          } else {
            // A picture nothing asked for: a second one for one unit.
            frame.close();
          }
        },
        error: (error) => {
          settle(stream)?.reject(
            new Error(`its H.264 decoder failed: ${error.message}`),
          );
        },
      }),
      codec: null,
      waiting: null,
      units: 0,
    };
    streams.set(surface, stream);
    return stream;
  };

  // The decoder configured for the stream a keyframe's parameter sets name, where
  // it is not already.
  const configure = async (stream: Stream, unit: H264Unit) => {
    if (unit.codec === null || unit.codec === stream.codec) {
      return;
    }
    const config = await rdpH264Config(unit.codec);
    if (closed || streams.get(unit.surface) !== stream) {
      throw new Error("its H.264 decoders are closed");
    }
    if (!config) {
      throw new Error(`this browser has no decoder for ${unit.codec}`);
    }
    stream.decoder.configure(config);
    stream.codec = unit.codec;
  };

  // One unit into its decoder, and the wait for its picture.
  const submit = (stream: Stream, unit: H264Unit, data: Uint8Array) =>
    new Promise<VideoFrame>((resolve, reject) => {
      const timer = setTimeout(() => {
        if (settle(stream)) {
          reject(new Error("its H.264 decoder gave no picture for a unit"));
        }
      }, patienceMs);
      stream.waiting = { resolve, reject, timer };
      try {
        stream.decoder.decode(
          new EncodedVideoChunk({
            type: unit.key ? "key" : "delta",
            // Never shown by its time: a stream's units are numbered.
            timestamp: stream.units,
            data,
          }),
        );
        stream.units += 1;
      } catch (error) {
        settle(stream);
        reject(error instanceof Error ? error : new Error(String(error)));
      }
    });

  return {
    async decode(unit, data) {
      if (closed) {
        throw new Error("its H.264 decoders are closed");
      }
      const stream = streams.get(unit.surface) ?? open(unit.surface);
      await configure(stream, unit);
      if (stream.codec === null || stream.decoder.state !== "configured") {
        throw new Error("an H.264 stream did not start at a keyframe");
      }
      return submit(stream, unit, data);
    },
    drop(surface) {
      const stream = streams.get(surface);
      if (stream) {
        streams.delete(surface);
        end(stream, "its surface was deleted");
      }
    },
    close() {
      closed = true;
      for (const stream of streams.values()) {
        end(stream, "its H.264 decoders are closed");
      }
      streams.clear();
    },
  };
}
