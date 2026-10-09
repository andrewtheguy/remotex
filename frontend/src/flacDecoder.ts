// The page's decoder for a session's lossless sound.
//
// A session started with lossless sound is sent FLAC instead of Opus: wlshare's
// own frames passed as they came, or an RDP host's PCM coded by the gateway. A
// packet is one FLAC frame, a stream of its own with no header in front of it,
// which is not what a browser's WebCodecs reads. So the page decodes it itself,
// in a WebAssembly module of its own (frontend/wasm/flac): small, with no threads
// and so no need of a shared memory, and fetched only by a session that plays
// such a stream.
import init, { Flac, type InitInput } from "../wasm/flac/pkg/remotex_flac.js";

/** What `audioFormat` said of a FLAC stream: the shape of every frame. */
export interface FlacStream {
  sampleRate: number;
  channels: number;
  /** Samples of one channel in every frame. */
  packetFrames: number;
}

export interface FlacDecoder {
  /**
   * One frame's samples, a plane for each channel, in -1 to 1. The planes are
   * the decoder's own memory and not a copy of it, good until the next frame is
   * decoded: play them or copy them before then. Throws for a frame that is not
   * one of the stream's: it costs that frame alone, and the next decodes on its
   * own.
   */
  decode(frame: Uint8Array): Float32Array<ArrayBuffer>[];
  /** Give the decoder's memory back. */
  close(): void;
}

/** Makes a stream's decoder. Throws for a stream that is not carried as FLAC. */
export type FlacFactory = (stream: FlacStream) => FlacDecoder;

let loaded: Promise<FlacFactory> | null = null;

/**
 * The module, fetched and compiled once for the page, and what makes decoders
 * out of it. `source` is the module's bytes for a runtime with nothing to fetch
 * from, which is a test.
 */
export function loadFlac(source?: InitInput): Promise<FlacFactory> {
  loaded ??= init(source === undefined ? undefined : { module_or_path: source })
    .then(({ memory }) => (stream: FlacStream) => {
      const flac = new Flac(
        stream.sampleRate,
        stream.channels,
        stream.packetFrames,
      );
      return {
        decode(frame: Uint8Array): Float32Array<ArrayBuffer>[] {
          // Where the samples are in the module's memory, which is not
          // shared. Its buffer is asked for each time: growing the memory
          // replaces it.
          const planar = flac.decode(frame);
          return Array.from(
            { length: stream.channels },
            (_, channel) =>
              new Float32Array(
                memory.buffer,
                planar + channel * stream.packetFrames * 4,
                stream.packetFrames,
              ),
          );
        },
        close() {
          flac.free();
        },
      };
    })
    .catch((error: unknown) => {
      // A load that failed is tried again by the next stream: the page may have
      // been offline for the one fetch, and nothing else would ever retry it.
      loaded = null;
      throw error;
    });
  return loaded;
}
