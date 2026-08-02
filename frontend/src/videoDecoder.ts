// A WebCodecs `VideoDecoder` shaped like the rest of the tile path.
//
// A target on `render_type = "video"` sends the desktop as one inter-frame H.264
// stream (see `Tile::FORMAT_H264` in src/protocol.rs). Everything else about that
// path is ordinary: the access units arrive as TILE records in the same batches and
// are painted onto the same canvas. What is not ordinary is that they are a *chain*
// — each frame means "what changed since the one before it" — so unlike a still
// tile, none of them may be dropped, reordered, or decoded twice.
//
// The awkward part is the shape of the API rather than the codec. `decode()` is
// fire-and-forget and frames come back on a callback, while the paint path wants
// something awaitable it can hold in wire order. So each submitted unit gets a
// pending entry that its output resolves, and `decode` hands back the promise.
//
// Every pending entry must be settled on *every* path, including error and close.
// One that is never settled hangs `useRemoteDesktop`'s per-connection promise chain
// forever, which stops the whole session — not just the picture.

import { readAccessUnit } from "./h264.ts";

/**
 * Why video cannot play here, distinguishing an insecure origin from a browser with
 * no decoder — the same two cases, in the same order, as `audioUnavailable`.
 *
 * `VideoDecoder` is secure-context only, exactly like the `AudioDecoder` remote audio
 * already uses. The difference is what it costs: no audio decoder means silence,
 * where no video decoder means a desktop that never paints at all, so this is said
 * plainly rather than logged.
 */
export function videoUnavailable(): string | null {
  if (typeof VideoDecoder !== "undefined") {
    return null;
  }
  if (!window.isSecureContext) {
    return "This target sends video, which needs a secure context: reach this gateway over HTTPS (or localhost).";
  }
  return "This target sends video, and this browser has no WebCodecs video decoder.";
}

export interface VideoHandlers {
  /**
   * The decoder gave up. There is no fallback to switch to — a video target sends
   * nothing but access units — so this is reported rather than worked around.
   */
  onError: (reason: string) => void;
}

export interface VideoStream {
  /**
   * Decode one access unit, resolving to its frame — or to null when there is
   * nothing to paint for it.
   *
   * The caller owns the frame and must `close()` it.
   */
  decode: (data: Uint8Array, timestamp: number) => Promise<VideoFrame | null>;
  /** Drop the decoder. Everything still pending resolves to null. */
  close: () => void;
}

/** One access unit in flight, and the promise the paint path is holding. */
interface Pending {
  resolve: (frame: VideoFrame | null) => void;
}

/**
 * Build a decoder for one connection's video stream.
 *
 * Throws if there is no `VideoDecoder` to be had (see {@link videoUnavailable}); an
 * *unsupported profile* is not a throw, because WebCodecs reports that
 * asynchronously — it arrives at `onError`.
 */
export function createVideoStream(handlers: VideoHandlers): VideoStream {
  const unavailable = videoUnavailable();
  if (unavailable) {
    throw new Error(unavailable);
  }
  // FIFO, and that is the whole ordering argument: H.264 with no B-frames — which
  // is what the gateway's encoder produces — emits frames in the order it was given
  // them, so the nth output belongs to the nth pending entry.
  const pending: Pending[] = [];
  let codec: string | null = null;
  let closed = false;

  const settle = (frame: VideoFrame | null) => {
    const next = pending.shift();
    if (next) {
      next.resolve(frame);
    } else {
      // A frame nobody is waiting for is one that would leak: VideoFrame holds
      // decoder memory until it is closed.
      frame?.close();
    }
  };

  const drain = () => {
    while (pending.length > 0) {
      settle(null);
    }
  };

  const decoder = new VideoDecoder({
    output: (frame) => settle(frame),
    error: (e) => {
      // Terminal: a decoder that has errored decodes nothing further, and every
      // frame after this one depends on frames it did not produce.
      closed = true;
      drain();
      handlers.onError(
        e instanceof Error && e.name === "NotSupportedError"
          ? "This browser cannot decode the H.264 video this target sends."
          : "This browser's video decoder failed.",
      );
    },
  });

  return {
    decode(data, timestamp) {
      if (closed) {
        return Promise.resolve(null);
      }
      const unit = readAccessUnit(data);
      if (!unit) {
        return Promise.resolve(null);
      }
      if (unit.codec && unit.codec !== codec) {
        // Configure on the first keyframe, and again whenever the stream restarts
        // with different parameters — a resize does exactly that. `codedWidth` and
        // `codedHeight` are deliberately left out: the bitstream carries the coded
        // size, and the tile header carries the *desktop* size, which is smaller by
        // up to a pixel in each axis and is not what a decoder should be told.
        codec = unit.codec;
        decoder.configure({ codec, optimizeForLatency: true });
      }
      if (decoder.state !== "configured") {
        // No keyframe yet. Nothing can be decoded until one arrives, and one will:
        // every repaint, resize and reattach makes the gateway send one.
        return Promise.resolve(null);
      }
      const frame = new Promise<VideoFrame | null>((resolve) => {
        pending.push({ resolve });
      });
      try {
        decoder.decode(
          new EncodedVideoChunk({
            timestamp,
            type: unit.key ? "key" : "delta",
            data: data as Uint8Array<ArrayBuffer>,
          }),
        );
      } catch {
        // A chunk the decoder refused outright produces no output, so the entry it
        // just pushed has to be settled here or it never will be.
        settle(null);
      }
      return frame;
    },
    close() {
      closed = true;
      drain();
      if (decoder.state !== "closed") {
        decoder.close();
      }
    },
  };
}
