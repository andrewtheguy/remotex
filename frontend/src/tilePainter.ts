import {
  type BatchRecord,
  decodeBatchFrame,
  NO_SLOT,
  SLOT_COUNT,
  type TileMsg,
} from "./protocol.ts";
import { createVideoStream, type VideoStream } from "./videoDecoder.ts";

// The tile cache and the batch draw loop, shared by the browser SPA and the
// macOS viewer's canvas page.
//
// The cache is fixed length because the wire says how many slots there are, so
// a server cannot grow it — and the client never evicts: the server names the
// slot to overwrite, which is what keeps the two ends in step without either
// modelling the other's memory.
//
// A target on `render_type = "video"` sends its desktop through the same batches
// as one H.264 record per frame, so the video stream lives here too rather than
// beside each caller: it belongs to exactly what the slot table belongs to — one
// attachment — and `clear` is the one place that has to end both.

// The presentation timestamp one access unit advances by, in microseconds.
//
// A number rather than a measurement, and it does not have to be the truth: the
// wire carries no timestamps, nothing here schedules by them, and a frame is
// painted when it decodes. WebCodecs only requires that they increase, and a
// nominal 30 Hz keeps them recognisable in a decoder's own diagnostics.
const VIDEO_FRAME_US = 33_333;

// What a record turns into once the cache has had its say: a payload to decode
// and where to put it, or nothing.
interface PaintJob {
  x: number;
  y: number;
  data: Uint8Array;
  codec: TileMsg["codec"];
  /** True when the server believes this client is keeping these bytes. */
  cached: boolean;
}

export interface TilePainter {
  /**
   * Decode one binary batch frame and paint it. Records are decoded
   * concurrently and drawn synchronously in wire order, so a later tile
   * overwrites an earlier one covering the same pixels. Malformed framing drops
   * the batch; an individual decode failure drops one tile.
   */
  draw(frame: ArrayBuffer): Promise<void>;
  /**
   * Forget every slot and drop the video stream. The next attachment's server
   * starts with an empty table, and its stream starts again from a keyframe.
   */
  clear(): void;
}

export function createTilePainter(options: {
  /**
   * The destination, read per batch rather than captured, so a resize that
   * replaces the 2D context does not need the painter rebuilt.
   */
  context: () => CanvasRenderingContext2D | null;
  /**
   * The desktop's size in framebuffer pixels, null until the first resize names
   * one. Only the video path reads it, and only to crop: see `paintBatch`.
   */
  size: () => { w: number; h: number } | null;
  /**
   * The two ends disagree about the slot table and only the server can repair
   * it. Called at most once per batch: fifty references into a cache this
   * client lost are one disagreement, not fifty.
   */
  onCacheReset: () => void;
  /**
   * Why this client is showing nothing for a video target, or null once it is
   * showing something. Unlike a failed still tile this cannot be swallowed: a
   * video target sends nothing else, so the alternative to saying it is a
   * desktop that never paints and never explains itself.
   */
  onVideoError: (reason: string | null) => void;
}): TilePainter {
  const tileCache: ({ data: Uint8Array; codec: TileMsg["codec"] } | null)[] =
    new Array(SLOT_COUNT).fill(null);

  // The video stream, for a target on `render_type = "video"`. Built on the
  // first access unit rather than up front, because most targets send none at
  // all.
  let video: VideoStream | null = null;
  // Presentation timestamps for that stream, in microseconds. Counted rather
  // than measured — see `VIDEO_FRAME_US`.
  let videoTimestamp = 0;

  const releaseVideo = () => {
    video?.close();
    video = null;
    videoTimestamp = 0;
  };

  // Whether this batch has already asked for a reset. One per batch rather than
  // one per painter: `draw` is async, and nothing in the signature stops a
  // caller starting a second one before the first has finished. Sharing the flag
  // across two in flight would let one batch's miss swallow the other's — or
  // worse, let the first clear the cache in the middle of the second.
  interface Batch {
    resetAsked: boolean;
  }

  const askForCacheReset = (batch: Batch) => {
    if (batch.resetAsked) {
      return;
    }
    batch.resetAsked = true;
    options.onCacheReset();
  };

  // Store what the server says to store, and resolve what it says to reuse.
  //
  // The payload is copied out of the frame rather than held as a view of it: a
  // view would pin the whole batch — up to 256 KB — for the lifetime of one
  // slot.
  const resolveRecord = (
    record: BatchRecord,
    batch: Batch,
  ): PaintJob | null => {
    if (record.kind === "tile") {
      // An access unit is never remembered, whatever slot it names. A reference
      // replays a payload, and replaying one of these out of sequence
      // desynchronises every frame after it until the next keyframe — the same
      // reason the gateway never assigns one a slot (`Slots::place` in
      // src/wire.rs). This is the client declining to be told otherwise.
      const cached = record.slot !== NO_SLOT && record.codec !== "h264";
      if (cached) {
        tileCache[record.slot] = {
          data: new Uint8Array(record.data),
          codec: record.codec,
        };
      }
      return { ...record, cached };
    }
    const held = tileCache[record.slot];
    if (!held) {
      // The server thinks this client holds a tile it does not. Nothing else
      // will ever correct that, so say so and draw nothing here.
      askForCacheReset(batch);
      return null;
    }
    return { x: record.x, y: record.y, ...held, cached: true };
  };

  const decodeJob = async (job: PaintJob | null, batch: Batch) => {
    if (!job) {
      return null;
    }
    if (job.codec === "h264") {
      return decodeAccessUnit(job.data);
    }
    try {
      return await createImageBitmap(
        new Blob([job.data as Uint8Array<ArrayBuffer>], { type: job.codec }),
      );
    } catch {
      // A tile that will not decode is one dropped tile — unless the server is
      // keeping it as a slot, in which case every later reference to it would
      // fail the same way.
      if (job.cached) {
        askForCacheReset(batch);
      }
      return null;
    }
  };

  // One access unit of the video stream.
  //
  // Unlike a still tile this cannot simply be dropped when something goes wrong:
  // every later frame is expressed relative to this one, and a video target
  // sends no stills to recover with. So a failure here is *said*, and the stream
  // is torn down rather than left decoding from history it does not have.
  const decodeAccessUnit = async (data: Uint8Array) => {
    if (!video) {
      try {
        video = createVideoStream({
          onError: (reason) => {
            options.onVideoError(reason);
            releaseVideo();
          },
        });
      } catch (e) {
        options.onVideoError(
          e instanceof Error ? e.message : "This browser cannot decode video.",
        );
        return null;
      }
      options.onVideoError(null);
    }
    videoTimestamp += VIDEO_FRAME_US;
    return video.decode(data, videoTimestamp);
  };

  // Every image is closed whether or not it was drawn: with no canvas to draw
  // into there is nothing to paint, but the decoded pictures still have to go.
  const paintBatch = (
    jobs: (PaintJob | null)[],
    decoded: (ImageBitmap | VideoFrame | null)[],
  ) => {
    const ctx = options.context();
    const size = options.size();
    for (let i = 0; i < decoded.length; i += 1) {
      const image = decoded[i];
      const job = jobs[i];
      if (!image || !job) {
        continue;
      }
      if (job.codec === "h264" && size) {
        // Cropped by the source rectangle rather than drawn whole: H.264 needs
        // even sides and a desktop need not have them, so the decoded picture
        // can be a pixel wider or taller than the framebuffer. The tile header
        // carries the *desktop* size, which is what the canvas is.
        ctx?.drawImage(image, 0, 0, size.w, size.h, 0, 0, size.w, size.h);
      } else {
        ctx?.drawImage(image, job.x, job.y);
      }
      image.close();
    }
  };

  return {
    async draw(frame: ArrayBuffer) {
      const records = decodeBatchFrame(frame);
      if (!records) {
        return;
      }
      const batch: Batch = { resetAsked: false };
      const jobs = records.map((record) => resolveRecord(record, batch));
      paintBatch(
        jobs,
        await Promise.all(jobs.map((job) => decodeJob(job, batch))),
      );
      // Cleared after the pass so references may use slots filled earlier in
      // it, which the gateway does emit within a single batch.
      if (batch.resetAsked) {
        tileCache.fill(null);
      }
    },
    clear() {
      tileCache.fill(null);
      releaseVideo();
    },
  };
}
