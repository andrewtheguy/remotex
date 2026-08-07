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
// One that is never settled hangs the paint worker's one command chain forever, which
// stops the whole session — not just the picture, and not just this attachment: the
// `clear` and the `resize` a target switch posts sit in that same chain behind it, so
// the next target comes up connected and waiting for a desktop that cannot arrive.
//
// **The pairing is a decoder's courtesy, not its contract.** WebCodecs nowhere promises
// one output per `decode()`, and a decoder that quietly produces nothing for a chunk —
// a frame whose references it does not have is the ordinary way — settles nothing and
// says nothing. One such chunk is enough on its own: the worker draws one batch at a
// time and a batch carries at most one unit per stream, so there is never a later frame
// to shake the FIFO loose. Hence the backstop below, which is what makes the promise
// this file hands out a promise rather than a hope.

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
export function createVideoStreams(
  handlers: VideoHandlers,
  stallMs: number = STALL_MS,
): VideoStreams {
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
    const failed = (reason: string, recoverable: boolean) => {
      // Only if this entry is still the live one: a region that restarted on a new size
      // has already replaced it, and dropping the newer decoder because the older one
      // errored would lose a chain that is decoding fine.
      if (live.get(id) === entry) {
        live.delete(id);
      }
      handlers.onError(reason, recoverable);
      if (recoverable) {
        // Asked for, exactly as a stall is. The next unit on this id builds a fresh
        // decoder, and a fresh decoder can start at nothing but a keyframe — so
        // without this the region is not "one failed frame" but every frame after
        // it, and the banner the error just raised would go on telling the truth.
        handlers.onNeedsKeyframe(`stream ${id}: ${reason}`);
      }
    };
    let stream: VideoStream;
    try {
      stream = createVideoStream(
        format,
        {
          onError: failed,
          // Named, because the one thing worth knowing about a stall is which region
          // it was: under `render_motion_subtype = "stream"` there are several of
          // these and they stop for their own reasons. A stall is as terminal for
          // the decoder as an error (see `stalled` in `createVideoStream`), so the
          // entry goes the same way — the next unit on this id builds afresh, with
          // the same guard as `failed` for the same reason.
          onNeedsKeyframe: (reason) => {
            if (live.get(id) === entry) {
              live.delete(id);
            }
            handlers.onNeedsKeyframe(`stream ${id}: ${reason}`);
          },
        },
        stallMs,
      );
    } catch (e) {
      // Unreachable once the table exists — it refused to be built without a decoder —
      // but a throw from here would escape into the paint loop and drop a whole batch of
      // tiles that had nothing to do with video.
      handlers.onError(
        e instanceof Error ? e.message : "This browser cannot decode video.",
        // A runtime with no decoder at all. No keyframe repairs that either.
        false,
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

// How long a decoder may owe a frame before the stream is treated as stalled.
//
// Generous on purpose, because this is a liveness backstop and not a deadline: a
// decoder holds at most one access unit per stream at a time — the worker draws one
// batch at a time, and a batch carries at most one unit per stream — so this is sixty
// frames' grace at the 30 Hz `VIDEO_FRAME_INTERVAL` in src/encode.rs paces rounds at.
// A decode that has not landed by now is not slow, it is not coming.
const STALL_MS = 2_000;

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
   *
   * `recoverable` is false when the browser refused the configuration itself. That
   * is not a cut chain but a standing fact: the next decoder is refused exactly as
   * this one was, so nothing is asked for and the banner stays up, which is the one
   * case it is meant for.
   */
  onError: (reason: string, recoverable: boolean) => void;
  /**
   * This stream's chain has been cut and it cannot pick up again until a keyframe
   * arrives. Both ways of cutting it come here — a decoder that went quiet and one
   * that failed — and both throw the decoder away: the next unit on the id builds a
   * fresh one, which can start at nothing but the keyframe this asks for.
   *
   * Only the gateway can send that keyframe, so this has to reach something that can
   * ask. Left unasked it is a region that never paints again — and under
   * `render_type = "video"` that is the whole desktop, since that dial's one stream
   * is never restarted by a region coming and going.
   */
  onNeedsKeyframe: (reason: string) => void;
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
  stallMs: number = STALL_MS,
): VideoStream {
  const unavailable = videoUnavailable();
  if (unavailable) {
    throw new Error(unavailable);
  }
  // FIFO, and that is the whole ordering argument: the encoder produces no frames
  // out of order — no alt-ref frames a decoder would reorder — so
  // the nth output belongs to the nth pending entry. What it is *not* is a guarantee
  // that an nth output happens at all; see `stalled`.
  const pending: Pending[] = [];
  let closed = false;
  // Whether this decoder has no history to decode against — true from birth, until
  // its keyframe arrives. Every unit before it is expressed against pictures it does
  // not have, and they are dropped here rather than handed over to raise one error
  // each. Dropping is also simply what a delta to a fresh decoder deserves: the
  // alternative is a decoder that fails, is thrown away, is rebuilt by the next unit,
  // and fails again on that one too.
  let keyNeeded = true;
  // Armed whenever the decoder owes a frame, which is the only state a stall can be
  // seen from — a decoder that has stopped producing raises no event to notice.
  let watchdog: ReturnType<typeof setTimeout> | undefined;

  const disarm = () => {
    if (watchdog !== undefined) {
      clearTimeout(watchdog);
      watchdog = undefined;
    }
  };

  // The clock runs from the last thing that happened rather than from the oldest
  // unsettled unit: what is being asked is "has this decoder gone quiet", and an
  // output means it has not.
  const rearm = () => {
    disarm();
    if (!closed && pending.length > 0) {
      watchdog = setTimeout(stalled, stallMs);
    }
  };

  const settle = (frame: VideoFrame | null) => {
    const next = pending.shift();
    if (next) {
      next.resolve(frame);
    } else {
      // A frame nobody is waiting for is one that would leak: VideoFrame holds
      // decoder memory until it is closed.
      frame?.close();
    }
    rearm();
  };

  const drain = () => {
    while (pending.length > 0) {
      settle(null);
    }
  };

  // The decoder owes frames it is not going to produce. Everything it owes is settled
  // to null — one unpainted region for as long as it takes a keyframe to arrive, where
  // leaving them pending is the whole session, permanently — and the decoder goes with
  // them. `close()` is what makes abandoning them safe rather than merely quick: it
  // guarantees no output after it, so a frame that arrives late cannot resolve a
  // *later* unit's promise and slide every frame after it one place out of position.
  //
  // Discarded rather than reset and reconfigured, which is what this recovery used to
  // do and what was measured to manufacture a second failure. Chromium answers a
  // `configure()` on a live decoder by first flushing the old pipeline
  // (`DecoderTemplate::ProcessConfigureRequest` decodes an end-of-stream buffer), and
  // a decoder that has gone quiet is one whose pipeline has already failed off-thread
  // — a GPU-process decoder surfaces errors on the next flush, not on the chunk it
  // choked on, which is also why the watchdog and not an error callback saw the fault.
  // So the flush comes back failed, `OnFlushDone` shuts the decoder down, and the
  // session gets `EncodingError: Error during flush.`: a second error, a second
  // repaint request and a banner, every one describing the recovery rather than the
  // fault. Closing asks the wedged pipeline for nothing.
  const stalled = () => {
    watchdog = undefined;
    if (closed || pending.length === 0) {
      return;
    }
    const owed = pending.length;
    closed = true;
    drain();
    if (decoder.state !== "closed") {
      decoder.close();
    }
    handlers.onNeedsKeyframe(
      `the decoder produced nothing for ${owed} access unit(s) in ${stallMs} ms`,
    );
  };

  const decoder = new VideoDecoder({
    output: (frame) => settle(frame),
    error: (e) => {
      // Terminal: a decoder that has errored decodes nothing further, and every
      // frame after this one depends on frames it did not produce.
      closed = true;
      disarm();
      drain();
      const refused = e instanceof Error && e.name === "NotSupportedError";
      // The exception's name and message travel with the sentence. The decoder that
      // knew what went wrong is gone by the time anyone reads it, and which name it
      // was is the whole diagnosis: `EncodingError` indicts the bytes the gateway
      // sent, where a platform name indicts the decoder they were fed to.
      handlers.onError(
        refused
          ? `This browser cannot decode the video this target sends (${format.decode}).`
          : `This browser's video decoder failed (${e.name}: ${e.message}).`,
        !refused,
      );
    },
  });
  // Configured here rather than on the first keyframe, because the gateway has already
  // said what this stream is.
  decoder.configure({
    // `codedWidth` and `codedHeight` are deliberately left out: the bitstream carries
    // the coded size, and the record header carries the *desktop* size, which is
    // smaller by up to a pixel in each axis and is not what a decoder should be told.
    codec: format.decode,
    optimizeForLatency: true,
  });

  return {
    decode(data, timestamp, keyframe) {
      if (closed || decoder.state !== "configured") {
        return Promise.resolve(null);
      }
      if (keyNeeded && !keyframe) {
        return Promise.resolve(null);
      }
      keyNeeded = false;
      const frame = new Promise<VideoFrame | null>((resolve) => {
        pending.push({ resolve });
      });
      rearm();
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
      disarm();
      drain();
      if (decoder.state !== "closed") {
        decoder.close();
      }
    },
  };
}
