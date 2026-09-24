// A WebCodecs `VideoDecoder` for the desktop's one stream.
//
// Every target sends the whole desktop as one inter-frame VP9 stream (see
// `VideoUnit` in src/protocol.rs). The units arrive as VIDEO records in the batches
// and are painted onto the canvas. What is not ordinary is that the stream is a
// *chain* — every frame means "what changed since the one before it" — so none of
// them may be dropped, reordered, or decoded twice.
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
// time, so there is never a later frame to shake the FIFO loose. Hence the backstop below, which is what makes the promise
// this file hands out a promise rather than a hope.

/**
 * How to decode the stream, from the gateway's `videoFormat` message.
 *
 * `decode` is the exact string to hand `VideoDecoder.configure`: `vp09.00.40.08.01.06.06.06.00`. It
 * is also what an error message names.
 */
export interface VideoFormat {
  decode: string;
}

/** The desktop's decoder, rebuilt as the stream it decodes starts over. */
export interface DesktopVideo {
  /**
   * Adopt the gateway's `videoFormat`.
   *
   * Always arrives before the stream's first unit, and again after a repaint — which
   * is what a browser that just attached gets, and it has seen neither the original
   * announcement nor a keyframe. A format that says the same thing as the one in force
   * changes nothing, so a re-announcement costs no decoder.
   */
  setFormat: (format: VideoFormat) => void;
  /**
   * Decode one access unit, resolving to its frame — or to null when there is nothing
   * to paint for it.
   *
   * A unit whose size differs from the last one means the stream restarted on a
   * different picture: the decoder is replaced rather than reused, because the
   * configuration string carries no resolution and an in-band size change is not a
   * thing to bet two browsers on. The gateway sends a keyframe whenever that happens,
   * so a fresh decoder always has somewhere to start.
   */
  decode: (
    size: { w: number; h: number },
    data: Uint8Array,
    keyframe: boolean,
  ) => Promise<VideoFrame | null>;
  /** Drop the decoder. Everything still pending resolves to null. */
  close: () => void;
}

/**
 * Build the desktop's decoder for one connection.
 *
 * That `VideoDecoder` exists is the client's entry condition and not a question for
 * this path (preflight.ts). The decoder itself is created on the first unit.
 */
export function createDesktopVideo(
  handlers: VideoHandlers,
  stallMs: number = STALL_MS,
): DesktopVideo {
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
  let live: Live | null = null;
  // What the gateway last announced, which is not the same as what a decoder is
  // running on: the announcement arrives first and the decoder is built by the unit
  // that follows it.
  let format: VideoFormat | null = null;
  // Whether a unit arriving before its format has been logged, so a takeover costs one
  // console line rather than one per frame until the repaint lands.
  let warned = false;

  const dropDecoder = () => {
    live?.stream.close();
    live = null;
  };

  // The decoder, built on demand and replaced when its picture changes.
  const liveStream = (
    size: { w: number; h: number },
    format: VideoFormat,
  ): Live | null => {
    if (live && live.w === size.w && live.h === size.h) {
      return live;
    }
    // A stream that restarted on a different picture. The configuration string
    // carries no resolution, and an in-band size change is not a thing to bet two
    // browsers on, so the decoder is replaced rather than reused.
    dropDecoder();
    let entry: Live | undefined;
    const failed = (reason: string, recoverable: boolean) => {
      // Only if this entry is still the live one: a stream that restarted on a new
      // size has already replaced it, and dropping the newer decoder because the older
      // one errored would lose a chain that is decoding fine.
      if (live === entry) {
        live = null;
      }
      handlers.onError(reason, recoverable);
      if (recoverable) {
        // Asked for, exactly as a stall is. The next unit builds a fresh decoder,
        // and a fresh decoder can start at nothing but a keyframe — so without this
        // the desktop is not "one failed frame" but every frame after it, and the
        // banner the error just raised would go on telling the truth.
        handlers.onNeedsKeyframe(reason);
      }
    };
    let stream: VideoStream;
    try {
      stream = createVideoStream(
        format,
        {
          onError: failed,
          // A stall is as terminal for the decoder as an error (see `stalled` in
          // `createVideoStream`), so the entry goes the same way — the next unit
          // builds afresh, with the same guard as `failed` for the same reason.
          onNeedsKeyframe: (reason) => {
            if (live === entry) {
              live = null;
            }
            handlers.onNeedsKeyframe(reason);
          },
        },
        stallMs,
      );
    } catch (e) {
      // A throw from here would escape into the paint loop and drop the batch.
      handlers.onError(
        e instanceof Error ? e.message : "This browser cannot decode video.",
        // A runtime with no decoder at all. No keyframe repairs that either.
        false,
      );
      return null;
    }
    entry = { stream, format, w: size.w, h: size.h, timestamp: 0 };
    live = entry;
    return entry;
  };

  return {
    setFormat(next) {
      format = next;
      warned = false;
      if (live && live.format.decode !== next.decode) {
        // A stream that came back configured differently — a resize is the way this
        // happens — is a new chain, and its old decoder cannot decode it.
        dropDecoder();
      }
    },
    decode(size, data, keyframe) {
      if (!format) {
        // **Dropped, and that is correct rather than defensive.** It happens on a
        // takeover: the gateway announces the stream once, to whoever was attached,
        // and a browser that takes the session over receives whatever units were
        // already in flight before the repaint its attach triggers has taken effect.
        // Those units are undecodable here whatever this does — a decoder that has
        // just been built can only start at a keyframe, and the keyframe is in the
        // repaint that is already on its way with the format in front of it.
        if (!warned) {
          warned = true;
          console.warn("video: dropping a unit until its format arrives");
        }
        return Promise.resolve(null);
      }
      const held = liveStream(size, format);
      if (!held) {
        return Promise.resolve(null);
      }
      held.timestamp += VIDEO_FRAME_US;
      return held.stream.decode(data, held.timestamp, keyframe);
    },
    close() {
      dropDecoder();
      format = null;
      warned = false;
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
// Generous on purpose, because this is a liveness backstop and not a deadline: the
// worker draws one batch at a time, so this is sixty frames' grace at the 30 Hz `VIDEO_FRAME_INTERVAL` in src/encode.rs paces rounds at.
// A decode that has not landed by now is not slow, it is not coming.
const STALL_MS = 2_000;

export interface VideoHandlers {
  /**
   * A decoder gave up, and the stream it was decoding is over: every frame after
   * the one it failed on is expressed against history it no longer has.
   *
   * Reported rather than worked around, because there is no fallback to switch to:
   * the stream is the whole desktop.
   *
   * `recoverable` is false when the browser refused the configuration itself. That
   * is not a cut chain but a standing fact: the next decoder is refused exactly as
   * this one was, so nothing is asked for and the banner stays up, which is the one
   * case it is meant for.
   */
  onError: (reason: string, recoverable: boolean) => void;
  /**
   * The stream's chain has been cut and it cannot pick up again until a keyframe
   * arrives. Both ways of cutting it come here — a decoder that went quiet and one
   * that failed — and both throw the decoder away: the next unit builds a fresh one,
   * which can start at nothing but the keyframe this asks for.
   *
   * Only the gateway can send that keyframe, so this has to reach something that can
   * ask. Left unasked it is a desktop that never paints again.
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
 * Build one decoder, configured from the format the gateway announced.
 *
 * A configuration string this browser refuses is *not* a throw, because WebCodecs
 * reports that asynchronously — it arrives at `onError`, naming the configuration.
 */
export function createVideoStream(
  format: VideoFormat,
  handlers: VideoHandlers,
  stallMs: number = STALL_MS,
): VideoStream {
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
  // to null — an unpainted desktop for as long as it takes a keyframe to arrive, where
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
    // `hardwareAcceleration` is deliberately left out, so a browser decodes this
    // however it decodes VP9. (The one thing this page does state about its decoder
    // is which chroma it takes, asked once at load and answered to the gateway
    // rather than to a decoder — see videoChroma.ts.) Asking for software here is a
    // hint by specification — WebKit
    // falls back past it, Firefox disregards it — and on iOS and iPadOS it is a hint
    // with nothing behind it at all: WebKit maps `prefer-software` to
    // `HardwareAcceleration::No`, the clause routing that to a local software decoder
    // is compiled `#if PLATFORM(MAC)`, and iOS has no software VP9 decoder to route to
    // — `isVP9DecoderAvailable` there *is* `vp9HardwareDecoderAvailable`. So VP9 on
    // iOS is VideoToolbox or nothing (measured against WebKit main, 2026-08-21), and
    // a preference stated here buys one platform's decoder at most.
    //
    // What makes a hardware decoder safe on this dial is the gateway rather than a
    // hint. The decoder that goes quiet is the GPU-process one — that is the signature
    // of every stall, silence where a decode error belongs and then a failed
    // end-of-stream flush (see `stalled`), where software libvpx answers every chunk
    // on the calling thread, error and all — and what it goes quiet under is churn:
    // decode sessions built and torn down in quick succession. The desktop's one
    // stream is rebuilt only by a resize. The stall backstop above stands whatever
    // ends up decoding.
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
