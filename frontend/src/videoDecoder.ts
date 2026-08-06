// A WebCodecs `VideoDecoder` shaped like the rest of the tile path.
//
// Two render dials send access units (see `VideoUnit` in src/protocol.rs).
// `render_type = "video"` sends the whole desktop as one inter-frame stream;
// `render_motion_subtype = "stream"` sends one stream per moving region, with the
// still codecs carrying everything else — so a session may have several of these
// running at once, which is what `createVideoStreams` is for. Everything else about
// that path is ordinary: the units arrive as VIDEO records in the same batches and
// are painted onto the same canvas. What is not ordinary is that each stream is a
// *chain* — every frame means "what changed since the one before it" — so unlike a
// still tile, none of them may be dropped, reordered, or decoded twice.
//
// **Nothing here parses a bitstream.** The gateway says how to decode a stream in a
// `videoFormat` control message before its first unit, and marks each unit's keyframe
// bit on the wire. That is not a convenience: VP9 has no in-band parameter sets at
// all, so there is nothing in a VP9 payload for a client to read a codec string out of.
// The side that did the encoding says how to decode it.
//
// The awkward part is the shape of the API rather than the codec. `decode()` is
// fire-and-forget and frames come back on a callback, while the paint path wants
// something awaitable it can hold in wire order. So each submitted unit gets a
// pending entry that its output resolves, and `decode` hands back the promise.
//
// Every pending entry must be settled on *every* path, including error and close.
// One that is never settled hangs `useRemoteDesktop`'s per-connection promise chain
// forever, which stops the whole session — not just the picture.

/**
 * How to decode one stream, from the gateway's `videoFormat` message.
 *
 * `decode` is the exact string to hand `VideoDecoder.configure`: `vp09.00.40.08`. It
 * is also what an error message names.
 */
export interface VideoFormat {
  decode: string;
}

/**
 * Why video cannot play here, distinguishing an insecure origin from a browser with
 * no decoder — the same two cases, in the same order, as `audioUnavailable`.
 *
 * `VideoDecoder` is secure-context only, exactly like the `AudioDecoder` remote audio
 * already uses. The difference is what it costs: no audio decoder means silence,
 * where no video decoder means a desktop that never paints at all, so this is said
 * plainly rather than logged.
 *
 * Reachable, and by an ordinary route: nothing asks this browser what it can decode
 * before a target is picked. A gateway once probed for that and refused the pick — the
 * probe was removed because it made every video session depend on a round trip that
 * could answer differently on the same browser twice, and its refusals blamed the
 * browser for whatever had actually gone wrong. So the answer arrives here instead,
 * where it is a fact rather than a prediction.
 */
export function videoUnavailable(): string | null {
  if (typeof VideoDecoder !== "undefined") {
    return null;
  }
  // `globalThis`, not `window`: this runs inside the paint worker, which has no
  // `window` — and a worker's secure-context bit is its creator document's, so
  // the answer is the same one the page would give.
  if (!globalThis.isSecureContext) {
    return "This target sends video, which needs a secure context: reach this gateway over HTTPS (or localhost).";
  }
  return "This target sends video, and this browser has no WebCodecs video decoder.";
}

/** One session's decoders, one per `stream` id on the wire. */
export interface VideoStreams {
  /**
   * Adopt the gateway's `videoFormat` for one stream.
   *
   * Always arrives before that stream's first unit, and again after a repaint — which
   * is what a browser that just attached gets, and it has seen neither the original
   * announcement nor a keyframe. A format that says the same thing as the one in force
   * changes nothing, so a re-announcement costs no decoder.
   */
  setFormat: (stream: number, format: VideoFormat) => void;
  /**
   * Decode one access unit for `stream`, resolving to its frame — or to null when
   * there is nothing to paint for it.
   *
   * A record whose size differs from the last one on the same id means that region
   * restarted on a different picture: the decoder is replaced rather than reused,
   * because the configuration string carries no resolution and an in-band
   * size change is not a thing to bet two browsers on. The gateway sends a keyframe
   * whenever that happens, so a fresh decoder always has somewhere to start.
   */
  decode: (
    stream: number,
    size: { w: number; h: number },
    data: Uint8Array,
    keyframe: boolean,
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
    format: VideoFormat;
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
  // What the gateway last announced per stream, which is not the same as what a
  // decoder is running on: the announcement arrives first and the decoder is built by
  // the unit that follows it.
  const formats = new Map<number, VideoFormat>();
  // Streams already logged as arriving before their format, so a takeover costs one
  // console line rather than one per frame until the repaint lands.
  const warned = new Set<number>();

  const dropDecoder = (id: number) => {
    const held = live.get(id);
    if (held) {
      held.stream.close();
      live.delete(id);
    }
  };

  // The decoder for one stream, built on demand and replaced when its picture changes.
  // Split out of `decode` because it is the only part with branches worth naming: the
  // caller's job is the format lookup and the timestamp, and this one's is the decoder's
  // lifetime.
  const liveStream = (
    id: number,
    size: { w: number; h: number },
    format: VideoFormat,
  ): Live | null => {
    const existing = live.get(id);
    if (existing && existing.w === size.w && existing.h === size.h) {
      return existing;
    }
    if (existing) {
      // A region that restarted on a different picture. The configuration
      // string carries no resolution, and an in-band size change is not a thing to bet
      // two browsers on, so the decoder is replaced rather than reused.
      dropDecoder(id);
    }
    let entry: Live | undefined;
    // Bound to this id, so a decoder that gives up takes its own region down and no
    // others: under `render_motion_subtype = "stream"` the rest of the desktop is still
    // arriving as still tiles and still painting, and the other regions have chains of
    // their own that this one says nothing about. Under `render_type = "video"` there is
    // only ever one, so it is the same outcome.
    const failed = (reason: string) => {
      // Only if this entry is still the live one: a region that restarted on a new size
      // has already replaced it, and dropping the newer decoder because the older one
      // errored would lose a chain that is decoding fine.
      if (live.get(id) === entry) {
        live.delete(id);
      }
      handlers.onError(reason);
    };
    let stream: VideoStream;
    try {
      stream = createVideoStream(format, { onError: failed });
    } catch (e) {
      // Unreachable once the table exists — it refused to be built without a decoder —
      // but a throw from here would escape into the paint loop and drop a whole batch of
      // tiles that had nothing to do with video.
      handlers.onError(
        e instanceof Error ? e.message : "This browser cannot decode video.",
      );
      return null;
    }
    entry = { stream, format, w: size.w, h: size.h, timestamp: 0 };
    live.set(id, entry);
    return entry;
  };

  return {
    setFormat(id, format) {
      formats.set(id, format);
      warned.delete(id);
      const held = live.get(id);
      if (held && held.format.decode !== format.decode) {
        // A stream that came back configured differently — a resize is the way this
        // happens — is a new chain, and its old decoder cannot decode it.
        dropDecoder(id);
      }
    },
    decode(id, size, data, keyframe) {
      const format = formats.get(id);
      if (!format) {
        // **Dropped, and that is correct rather than defensive.** It happens on a
        // takeover: the gateway announces a stream once, to whoever was attached, and a
        // browser that takes the session over receives whatever units were already in
        // flight before the repaint its attach triggers has taken effect. Those units
        // are undecodable here whatever this does — a decoder that has just been built
        // can only start at a keyframe, and the keyframe is in the repaint that is
        // already on its way with the format in front of it.
        //
        // So this used to report "the gateway sent video before saying how to decode
        // it", and that was wrong twice over: it named a contract violation for an
        // ordinary race, and it left a banner up over a session that had already
        // recovered.
        if (!warned.has(id)) {
          warned.add(id);
          console.warn(
            `video: dropping a unit on stream ${id} until its format arrives`,
          );
        }
        return Promise.resolve(null);
      }
      const held = liveStream(id, size, format);
      if (!held) {
        return Promise.resolve(null);
      }
      held.timestamp += VIDEO_FRAME_US;
      return held.stream.decode(data, held.timestamp, keyframe);
    },
    close() {
      for (const held of live.values()) {
        held.stream.close();
      }
      live.clear();
      formats.clear();
      warned.clear();
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
   * `render_motion_subtype = "stream"` it is one region, with the still codecs
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
  decode: (
    data: Uint8Array,
    timestamp: number,
    keyframe: boolean,
  ) => Promise<VideoFrame | null>;
  /** Drop the decoder. Everything still pending resolves to null. */
  close: () => void;
}

/** One access unit in flight, and the promise the paint path is holding. */
interface Pending {
  resolve: (frame: VideoFrame | null) => void;
}

/**
 * Build a decoder for one stream, configured from the format the gateway announced.
 *
 * Throws if there is no `VideoDecoder` to be had (see {@link videoUnavailable}); a
 * configuration string this browser refuses is *not* a throw, because WebCodecs
 * reports that asynchronously — it arrives at `onError`, naming the configuration.
 */
export function createVideoStream(
  format: VideoFormat,
  handlers: VideoHandlers,
): VideoStream {
  const unavailable = videoUnavailable();
  if (unavailable) {
    throw new Error(unavailable);
  }
  // FIFO, and that is the whole ordering argument: the encoder produces no frames
  // out of order — no alt-ref frames a decoder would reorder — so
  // the nth output belongs to the nth pending entry.
  const pending: Pending[] = [];
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
          ? `This browser cannot decode the video this target sends (${format.decode}).`
          : "This browser's video decoder failed.",
      );
    },
  });
  // Configured here rather than on the first keyframe, because the gateway has already
  // said what this stream is. `codedWidth` and `codedHeight` are deliberately left
  // out: the bitstream carries the coded size, and the record header carries the
  // *desktop* size, which is smaller by up to a pixel in each axis and is not what a
  // decoder should be told.
  decoder.configure({ codec: format.decode, optimizeForLatency: true });

  return {
    decode(data, timestamp, keyframe) {
      if (closed || decoder.state !== "configured") {
        return Promise.resolve(null);
      }
      const frame = new Promise<VideoFrame | null>((resolve) => {
        pending.push({ resolve });
      });
      try {
        decoder.decode(
          new EncodedVideoChunk({
            timestamp,
            type: keyframe ? "key" : "delta",
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
