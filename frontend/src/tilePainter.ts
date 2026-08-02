import {
  type BatchRecord,
  decodeBatchFrame,
  NO_SLOT,
  SLOT_COUNT,
  type TileMsg,
} from "./protocol.ts";

// The tile cache and the batch draw loop, shared by the browser SPA and the
// macOS viewer's canvas page.
//
// The cache is fixed length because the wire says how many slots there are, so
// a server cannot grow it — and the client never evicts: the server names the
// slot to overwrite, which is what keeps the two ends in step without either
// modelling the other's memory.

// What a record turns into once the cache has had its say: a payload to decode
// and where to put it, or nothing.
interface PaintJob {
  x: number;
  y: number;
  data: Uint8Array;
  mime: TileMsg["mime"];
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
  /** Forget every slot. The next attachment's server starts with an empty table. */
  clear(): void;
}

export function createTilePainter(options: {
  /**
   * The destination, read per batch rather than captured, so a resize that
   * replaces the 2D context does not need the painter rebuilt.
   */
  context: () => CanvasRenderingContext2D | null;
  /**
   * The two ends disagree about the slot table and only the server can repair
   * it. Called at most once per batch: fifty references into a cache this
   * client lost are one disagreement, not fifty.
   */
  onCacheReset: () => void;
}): TilePainter {
  const tileCache: ({ data: Uint8Array; mime: TileMsg["mime"] } | null)[] =
    new Array(SLOT_COUNT).fill(null);

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
      if (record.slot !== NO_SLOT) {
        tileCache[record.slot] = {
          data: new Uint8Array(record.data),
          mime: record.mime,
        };
      }
      return { ...record, cached: record.slot !== NO_SLOT };
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
    try {
      return await createImageBitmap(
        new Blob([job.data as Uint8Array<ArrayBuffer>], { type: job.mime }),
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

  // Every bitmap is closed whether or not it was drawn: with no canvas to draw
  // into there is nothing to paint, but the decoded images still have to go.
  const paintBatch = (
    jobs: (PaintJob | null)[],
    bitmaps: (ImageBitmap | null)[],
  ) => {
    const ctx = options.context();
    for (let i = 0; i < bitmaps.length; i += 1) {
      const bitmap = bitmaps[i];
      const job = jobs[i];
      if (!bitmap || !job) {
        continue;
      }
      ctx?.drawImage(bitmap, job.x, job.y);
      bitmap.close();
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
    },
  };
}
