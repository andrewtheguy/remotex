import {
  type BatchRecord,
  decodeBatchFrame,
  NO_SLOT,
  SLOT_COUNT,
  type TileMsg,
} from "./protocol.ts";
import {
  createVideoStreams,
  type VideoFormat,
  type VideoStreams,
} from "./videoDecoder.ts";

// The browser SPA's tile cache and batch draw loop.
//
// The cache is fixed length because the wire says how many slots there are, so
// a server cannot grow it — and the client never evicts: the server names the
// slot to overwrite, which is what keeps the two ends in step without either
// modelling the other's memory.
//
// A target that streams sends its access units through the same batches as VIDEO
// records — the whole desktop under `render_type = "video"`, a region at a time
// under `render_motion_subtype = "stream"` — so the decoders live here too rather
// than beside each caller: they belong to exactly what the slot table belongs to,
// one attachment, and `clear` is the one place that has to end both.

// What a record turns into once the cache has had its say: something to decode
// and where to put it, or nothing.
type PaintJob =
  | {
      kind: "tile";
      x: number;
      y: number;
      data: Uint8Array;
      codec: TileMsg["codec"];
      /** True when the server believes this client is keeping these bytes. */
      cached: boolean;
      /** Which slot a decoded bitmap may be kept under, or NO_SLOT. */
      slot: number;
      /** A decoded bitmap already held for this slot, acquired at resolve time. */
      decoded: DecodedTile | null;
    }
  | {
      kind: "video";
      stream: number;
      x: number;
      y: number;
      w: number;
      h: number;
      /** From the record's flags byte, so from the encoder rather than a bitstream parse. */
      keyframe: boolean;
      data: Uint8Array;
    }
  /**
   * Pixels already on this canvas, moved. Nothing to decode: the job is the whole
   * of it, and it is settled in wire order like everything else, which is what makes
   * the canvas it reads the one the records before it left behind.
   */
  | {
      kind: "copy";
      sx: number;
      sy: number;
      x: number;
      y: number;
      w: number;
      h: number;
    };

// One slot's decoded bitmap. The encoded slot table is the contract with the
// server; this is a client-side economy on top of it, so an entry can always be
// dropped and the reference re-decoded from the encoded bytes.
//
// `refs` counts batches holding the bitmap between resolve and draw. `dead`
// marks an entry dropped from the cache while still referenced — the overwrite
// and the draw can share a batch — so the close waits for the last release
// instead of pulling the bitmap out from under a pending draw.
interface DecodedTile {
  bitmap: ImageBitmap;
  bytes: number;
  refs: number;
  dead: boolean;
}

// Decoded pixels kept, across all slots, before the least recently used go.
// The encoded table is bounded by the wire (SLOT_COUNT × the 32 KB record cap);
// decoded bands are far larger, and this is the lid on that difference.
const DECODED_BUDGET_BYTES = 16 * 1024 * 1024;

// The destination's 2D context. A union rather than the element's alone because
// the painter now runs inside the paint worker, drawing through an
// `OffscreenCanvas` — the element context remains for the unit tests, which
// drive the painter directly.
export type PaintContext =
  | CanvasRenderingContext2D
  | OffscreenCanvasRenderingContext2D;

export interface TilePainter {
  /**
   * Decode one binary batch frame and paint it. Records are decoded
   * concurrently and drawn synchronously in wire order, so a later tile
   * overwrites an earlier one covering the same pixels. Malformed framing drops
   * the batch; an individual decode failure drops one tile.
   */
  draw(frame: ArrayBuffer): Promise<void>;
  /**
   * Forget every slot and drop every decoder. The next attachment's server
   * starts with an empty table, and its streams start again from a keyframe.
   */
  clear(): void;
  /**
   * Adopt a `videoFormat` for one stream: the exact string to configure
   * its decoder with. Always arrives before that stream's first access unit.
   *
   * Held here rather than passed with each unit because it is announced once and used by
   * every unit after it — and because it builds the decoder table, which a client with no
   * video decoder at all cannot have. A runtime that fails here says so through
   * `onVideoError` exactly as a failing decode does.
   */
  setVideoFormat(stream: number, format: VideoFormat): void;
}

export function createTilePainter(options: {
  /**
   * The destination, read per batch rather than captured, so a resize that
   * replaces the 2D context does not need the painter rebuilt.
   */
  context: () => PaintContext | null;
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
  /**
   * A stream's chain has been cut — its decoder went quiet and was reset, or it
   * failed and was thrown away — so that region cannot paint again until a keyframe
   * only the gateway can send. Separate from `onVideoError` because it asks for
   * something rather than saying something: it is the recovery, where the banner is
   * the report, and the two are answered in different places.
   */
  onVideoNeedsKeyframe: (reason: string) => void;
}): TilePainter {
  const tileCache: ({ data: Uint8Array; codec: TileMsg["codec"] } | null)[] =
    new Array(SLOT_COUNT).fill(null);

  // Which attachment the caches belong to. `clear()` is the attachment
  // boundary and is not queued behind draws — an eviction closes the socket
  // from under whatever batch is mid-decode — so a draw that outlives the
  // generation it started in must not paint onto, or cache into, the next one.
  let generation = 0;

  // Decoded bitmaps by slot, so a TILE_REF is a draw rather than a Blob, a
  // decode and a GPU upload. A Map because insertion order is the recency
  // order: a hit is re-inserted, and eviction walks from the front.
  const decodedCache = new Map<number, DecodedTile>();
  let decodedBytes = 0;

  const releaseDecoded = (entry: DecodedTile) => {
    entry.refs -= 1;
    if (entry.dead && entry.refs === 0) {
      entry.bitmap.close();
    }
  };

  // Out of the cache now; the bitmap itself goes when nothing is drawing it.
  const dropDecoded = (slot: number) => {
    const entry = decodedCache.get(slot);
    if (!entry) {
      return;
    }
    decodedCache.delete(slot);
    decodedBytes -= entry.bytes;
    entry.dead = true;
    if (entry.refs === 0) {
      entry.bitmap.close();
    }
  };

  const clearDecoded = () => {
    for (const slot of [...decodedCache.keys()]) {
      dropDecoded(slot);
    }
  };

  // Adopt a freshly decoded slot bitmap, evicting the least recently used past
  // the budget. Called in wire order (from the draw loop), so when one batch
  // writes a slot twice the bitmap kept is the one the encoded table kept.
  const adoptDecoded = (slot: number, bitmap: ImageBitmap) => {
    dropDecoded(slot);
    const bytes = bitmap.width * bitmap.height * 4 || 0;
    if (bytes > DECODED_BUDGET_BYTES) {
      bitmap.close();
      return;
    }
    decodedCache.set(slot, { bitmap, bytes, refs: 0, dead: false });
    decodedBytes += bytes;
    for (const held of decodedCache.keys()) {
      if (decodedBytes <= DECODED_BUDGET_BYTES) {
        break;
      }
      // A referenced entry is mid-draw; it will be releasable by the next adopt.
      if ((decodedCache.get(held)?.refs ?? 0) === 0) {
        dropDecoded(held);
      }
    }
  };

  // The decoders, for a target that streams. Built on the first access unit
  // rather than up front, because most targets send none at all.
  let video: VideoStreams | null = null;

  // Set when `createVideoStreams` refused, which means this runtime has no video
  // decoder at all. Cleared with the decoders, since a new attachment is a new chance —
  // the same page could have been reached over HTTPS the second time.
  let videoUnusable = false;

  // What is on screen about video, and whether a painted frame may take it down.
  //
  // A complaint that a frame *can* answer is one region's decoder giving up: what the
  // banner says is that this client is showing nothing, so that region coming back is
  // it ceasing to be true, and a painted frame is the only thing that can say so —
  // a decoder that failed says nothing further either way.
  //
  // A refusal cannot be answered that way and is tracked apart from it. The browser
  // will not take that configuration, and since a stream's codec string carries the
  // *level* its picture size implies (`codec_string` in src/vp9.rs), the region next
  // to it may be a level this browser is perfectly happy with. Its frames say nothing
  // whatever about the refused one, which is still showing nothing and still owes the
  // sentence explaining why.
  let videoComplained = false;
  let videoRefused = false;

  const complainAboutVideo = (reason: string, recoverable = false) => {
    if (videoRefused && recoverable) {
      // A region that failed beside one that was refused outright. The standing fact
      // is the more useful sentence and it is already up.
      return;
    }
    videoRefused = videoRefused || !recoverable;
    videoComplained = recoverable;
    options.onVideoError(reason);
  };

  const releaseVideo = () => {
    video?.close();
    video = null;
    videoUnusable = false;
    videoComplained = false;
    videoRefused = false;
    // Retracted, and not merely forgotten. This is the attachment boundary: the
    // decoders that said it are gone, the next attachment may be a different target
    // through a different origin, and the page clears its own copy on the way back to
    // the picker only — a reattach or a takeover would otherwise inherit the sentence.
    options.onVideoError(null);
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
    if (record.kind === "video" || record.kind === "copy") {
      return record;
    }
    if (record.kind === "tile") {
      const cached = record.slot !== NO_SLOT;
      if (cached) {
        tileCache[record.slot] = {
          data: new Uint8Array(record.data),
          codec: record.codec,
        };
        // The old picture is stale the moment the encoded table changes: a
        // reference later in this same batch must decode the new bytes, not
        // reuse this. The new bitmap is adopted by the draw loop.
        dropDecoded(record.slot);
      }
      return { ...record, cached, decoded: null };
    }
    const held = tileCache[record.slot];
    if (!held) {
      // The server thinks this client holds a tile it does not. Nothing else
      // will ever correct that, so say so and draw nothing here.
      askForCacheReset(batch);
      return null;
    }
    const decoded = decodedCache.get(record.slot) ?? null;
    if (decoded) {
      // Acquired now, synchronously, so nothing decoded between resolve and
      // draw can close it — and re-inserted, which is what makes the Map's
      // order a recency order.
      decoded.refs += 1;
      decodedCache.delete(record.slot);
      decodedCache.set(record.slot, decoded);
    }
    return {
      kind: "tile",
      x: record.x,
      y: record.y,
      ...held,
      cached: true,
      slot: record.slot,
      decoded,
    };
  };

  const decodeJob = async (job: PaintJob | null, batch: Batch) => {
    if (!job) {
      return null;
    }
    if (job.kind === "video") {
      return decodeAccessUnit(job);
    }
    if (job.kind === "copy") {
      // Nothing to decode, and deliberately not short-circuited past the loop
      // either: it keeps its place in the queue, so the draws before it happen
      // before it reads the canvas.
      return null;
    }
    if (job.decoded) {
      // The whole point of the decoded cache: a reference is a draw, not a
      // Blob, a decode and a GPU upload.
      return job.decoded.bitmap;
    }
    try {
      // Tiles are opaque sRGB screen pixels: skipping color-space conversion
      // and alpha premultiplication drops per-tile work the paint never uses.
      return await createImageBitmap(
        new Blob([job.data as Uint8Array<ArrayBuffer>], { type: job.codec }),
        { colorSpaceConversion: "none", premultiplyAlpha: "none" },
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

  // One access unit, routed to its own stream's decoder.
  //
  // Unlike a still tile this cannot simply be dropped when something goes wrong:
  // every later frame of that stream is expressed relative to this one, and no
  // still is coming to recover with. So a failure here is *said*, and the
  // decoders are torn down rather than left decoding from history they do not
  // have.
  // The decoder table, built on demand and shared by the format announcements and the
  // units that follow them. `null` means this runtime has no video decoder at all and
  // has already said so.
  const videoStreams = (): VideoStreams | null => {
    if (video) {
      return video;
    }
    if (videoUnusable) {
      // Said once per attachment, not once per announcement and once per unit: a
      // runtime with no decoder will fail every time it is asked, and repeating it
      // would put the same sentence on the screen twice for one cause.
      return null;
    }
    try {
      // The failed stream has already dropped itself from the table; the others
      // are chains of their own and keep decoding. Rebuilding one is not this
      // client's decision either way — a stream begins again when the gateway
      // sends a keyframe, which a repaint, a resize or a region restarting does.
      video = createVideoStreams({
        onError: complainAboutVideo,
        onNeedsKeyframe: (reason) => options.onVideoNeedsKeyframe(reason),
      });
    } catch (e) {
      videoUnusable = true;
      complainAboutVideo(
        e instanceof Error ? e.message : "This browser cannot decode video.",
      );
      return null;
    }
    videoComplained = false;
    options.onVideoError(null);
    return video;
  };

  const decodeAccessUnit = async (job: PaintJob & { kind: "video" }) => {
    const video = videoStreams();
    if (!video) {
      return null;
    }
    return video.decode(
      job.stream,
      { w: job.w, h: job.h },
      job.data,
      job.keyframe,
    );
  };

  // One record onto the canvas, and its image to wherever it goes next: back to
  // the decoded cache for a slot tile, closed for everything else. Every image
  // is settled whether or not it was drawn — with no canvas there is nothing to
  // paint, but the decoded pictures still have to go somewhere.
  const paintJob = (
    ctx: PaintContext | null,
    job: PaintJob,
    image: ImageBitmap | VideoFrame,
  ) => {
    if (job.kind === "copy") {
      // Unreachable: a copy carries no image, so it is settled by `paintCopy`.
      image.close();
      return;
    }
    if (job.kind === "video") {
      // Cropped by the source rectangle rather than drawn whole: the encoders
      // are held to even sides and a region at the edge of an odd desktop does
      // not have them, so the decoded picture can be a pixel wider or taller
      // than the rectangle. The record carries the *true* rectangle, which is
      // where it belongs on the canvas.  It is the mirror's padding that makes
      // this a crop rather than a codec's requirement, so it holds for VP9 too.
      ctx?.drawImage(image, 0, 0, job.w, job.h, job.x, job.y, job.w, job.h);
      image.close();
      if (videoComplained) {
        // Video is painting again, so whatever was said about it has stopped being
        // true. Said here rather than on a timer or behind a dismiss button: the
        // banner is a statement about the present, and this is the moment the present
        // changed.
        videoComplained = false;
        options.onVideoError(null);
      }
      return;
    }
    ctx?.drawImage(image, job.x, job.y);
    if (job.decoded) {
      releaseDecoded(job.decoded);
    } else if (job.slot !== NO_SLOT) {
      // Adopted here, in wire order, rather than when its decode happened to
      // finish: two writes to one slot in a batch must leave the bitmap that
      // matches the encoded bytes, whichever decode was slower.
      adoptDecoded(job.slot, image as ImageBitmap);
    } else {
      image.close();
    }
  };

  // The canvas onto itself. Both context types name their own surface, and both
  // surfaces are a valid image source, so this is one `drawImage` and no
  // intermediate: the browser blits, and an overlapping copy takes the source as it
  // stood before the write, which is what the record means and what a scroll needs.
  const paintCopy = (
    ctx: PaintContext | null,
    job: PaintJob & { kind: "copy" },
  ) => {
    if (!ctx) {
      return;
    }
    ctx.drawImage(
      ctx.canvas,
      job.sx,
      job.sy,
      job.w,
      job.h,
      job.x,
      job.y,
      job.w,
      job.h,
    );
  };

  // Settle one landed decode: paint it, or — when `stale`, because `clear()`
  // ran while the decode was in flight — let its image go without touching
  // the next attachment's canvas or caches. A stale tile painted would be the
  // previous desktop showing through; either way the image has to be settled,
  // released back to the (cleared) cache if held, closed if the batch owned it.
  const settleJob = (
    ctx: PaintContext | null,
    job: PaintJob | null,
    image: ImageBitmap | VideoFrame | null,
    stale: boolean,
  ) => {
    if (stale) {
      if (job?.kind === "tile" && job.decoded) {
        releaseDecoded(job.decoded);
      } else {
        image?.close();
      }
      return;
    }
    if (job?.kind === "copy") {
      paintCopy(ctx, job);
    } else if (image && job) {
      paintJob(ctx, job, image);
    } else if (job?.kind === "tile" && job.decoded) {
      releaseDecoded(job.decoded);
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
      // All decodes start at once; the paint takes them in wire order — a later
      // tile must overwrite an earlier one — as each lands, so one slow decode
      // holds back what follows it and nothing before it, and a decoded image
      // is released the moment it is drawn instead of the whole batch's worth
      // staying alive until the slowest.
      const decodes = jobs.map((job) => decodeJob(job, batch));
      const ctx = options.context();
      const born = generation;
      for (let i = 0; i < jobs.length; i += 1) {
        const image = await decodes[i];
        settleJob(ctx, jobs[i], image, generation !== born);
      }
      // Cleared after the pass so references may use slots filled earlier in
      // it, which the gateway does emit within a single batch. A fenced batch
      // must not wipe the next attachment's table with its own stale miss.
      if (batch.resetAsked && generation === born) {
        tileCache.fill(null);
        clearDecoded();
      }
    },
    clear() {
      generation += 1;
      tileCache.fill(null);
      clearDecoded();
      releaseVideo();
    },
    setVideoFormat(stream, format) {
      videoStreams()?.setFormat(stream, format);
    },
  };
}
