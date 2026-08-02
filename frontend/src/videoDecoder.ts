// A WebCodecs `VideoDecoder` shaped like the rest of the tile path.
//
// Two render dials send access units (see `VideoUnit` in src/protocol.rs).
// `render_type = "video"` sends the whole desktop as one inter-frame H.264 stream;
// `render_motion_subtype = "h264"` sends one stream per moving region, with the
// still codecs carrying everything else — so a session may have several of these
// running at once, which is what `createVideoStreams` is for. Everything else about
// that path is ordinary: the units arrive as VIDEO records in the same batches and
// are painted onto the same canvas. What is not ordinary is that each stream is a
// *chain* — every frame means "what changed since the one before it" — so unlike a
// still tile, none of them may be dropped, reordered, or decoded twice.
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

/** One session's decoders, one per `stream` id on the wire. */
export interface VideoStreams {
  /**
   * Decode one access unit for `stream`, resolving to its frame — or to null when
   * there is nothing to paint for it.
   *
   * A record whose size differs from the last one on the same id means that region
   * restarted on a different picture: the decoder is replaced rather than reused,
   * because the `avc1.PPCCLL` codec string carries no resolution and an in-band size
   * change is not a thing to bet two browsers on. The gateway sends a keyframe
   * whenever that happens, so a fresh decoder always has somewhere to start.
   *
   * The caller owns the frame and must `close()` it.
   */
  decode: (
    stream: number,
    size: { w: number; h: number },
    data: Uint8Array,
  ) => Promise<VideoFrame | null>;
  /** Drop every decoder. Everything still pending resolves to null. */
  close: () => void;
}

/**
 * Build the decoder table for one connection.
 *
 * Throws if there is no `VideoDecoder` to be had (see {@link videoUnavailable}), so
 * that a runtime which cannot decode says so once, at the first access unit, rather
 * than once per region. Decoders themselves are created on the first unit for their
 * id, because most targets send none at all and a target on the region dial may never
 * use more than one.
 */
export function createVideoStreams(handlers: VideoHandlers): VideoStreams {
  const unavailable = videoUnavailable();
  if (unavailable) {
    throw new Error(unavailable);
  }
  interface Live {
    stream: VideoStream;
    w: number;
    h: number;
    /**
     * Presentation timestamps, in microseconds, counted rather than measured — the
     * wire carries none, nothing here schedules by them, and WebCodecs only requires
     * that they increase.
     */
    timestamp: number;
  }
  const live = new Map<number, Live>();

  return {
    decode(id, size, data) {
      let held = live.get(id);
      if (held && (held.w !== size.w || held.h !== size.h)) {
        held.stream.close();
        held = undefined;
      }
      if (!held) {
        let stream: VideoStream;
        // Bound to this id, so a decoder that gives up takes its own region down and
        // no others: under `render_motion_subtype = "h264"` the rest of the desktop
        // is still arriving as still tiles and still painting, and the other regions
        // have chains of their own that this one says nothing about. Under
        // `render_type = "video"` there is only ever one, so it is the same outcome.
        const failed = (reason: string) => {
          // Only if this entry is still the live one: a region that restarted on a
          // new size has already replaced it, and dropping the newer decoder because
          // the older one errored would lose a chain that is decoding fine.
          if (live.get(id) === held) {
            live.delete(id);
          }
          handlers.onError(reason);
        };
        try {
          stream = createVideoStream({ onError: failed });
        } catch (e) {
          // Unreachable once the table exists — it refused to be built without a
          // decoder — but a throw from here would escape into the paint loop and
          // drop a whole batch of tiles that had nothing to do with video.
          handlers.onError(
            e instanceof Error
              ? e.message
              : "This browser cannot decode video.",
          );
          return Promise.resolve(null);
        }
        held = { stream, w: size.w, h: size.h, timestamp: 0 };
        live.set(id, held);
      }
      held.timestamp += VIDEO_FRAME_US;
      return held.stream.decode(data, held.timestamp);
    },
    close() {
      for (const held of live.values()) {
        held.stream.close();
      }
      live.clear();
    },
  };
}

// The presentation timestamp one access unit advances by, in microseconds.
//
// A number rather than a measurement, and it does not have to be the truth: the wire
// carries no timestamps and a frame is painted when it decodes. A nominal 30 Hz keeps
// them recognisable in a decoder's own diagnostics, and it is the interval
// `VIDEO_FRAME_INTERVAL` in src/encode.rs actually paces rounds at.
const VIDEO_FRAME_US = 33_333;

export interface VideoHandlers {
  /**
   * A decoder gave up, and the stream it was decoding is over: every frame after
   * the one it failed on is expressed against history it no longer has.
   *
   * Reported rather than worked around, because there is no fallback to switch to.
   * How much of the desktop that costs depends on the dial — under
   * `render_type = "video"` it is all of it, and under
   * `render_motion_subtype = "h264"` it is one region, with the still codecs
   * carrying everything around it — so this says what happened and lets the caller
   * decide how loudly to say it.
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
