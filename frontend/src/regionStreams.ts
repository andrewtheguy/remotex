// The moving regions of wlshare's tiles with motion (`tilesStart` on a `wlshare`
// session started with them), drawn on the desktop's own canvas.
//
// wlshare sends its desktop in two forms, and the gateway passes both untouched:
// TILE records, a PNG each of a rectangle that is still, the desktop's pixels
// exactly, which framePainter.ts draws as it draws the gateway's own tiles; and a
// VP9 stream of its own for each region that moves, up to four at once, as REGION
// records. This module decodes and draws the second, where the records say and in
// the order the painter hands them over, which is the wire's: framePainter.ts
// starts every record's decode at once and draws each in its turn among the
// tiles. A tile over a region's rectangle comes only after the region's end, so
// drawing in that order is the whole of the composition.
//
// A region's stream is decoded as the desktop's stream is (videoDecoder.ts), with
// the same keyframe gate and stall backstop, and by the browser's own decoder, as
// every VP9 stream on this page is. One decoder for each stream id, built by its
// `regionFormat` and kept past its `regionEnd` for `RETIRE_MS`, because the next
// region to move very often reuses the id at the same size, and a decoder rebuilt
// is a hardware decode session torn down and set up again — slow on iOS, and the
// churn a hardware decoder goes quiet under (`createVideoStream`). A unit of an
// ended stream is not decoded on the decoder kept: no REGION follows a
// `regionEnd` until a `regionFormat` starts the id again.

import type { RegionMsg } from "./protocol.ts";
import { type DecodedPicture, isSoftwarePlanes } from "./softwareDecoder.ts";
import {
  createDesktopVideo,
  type DesktopVideo,
  type VideoFormat,
  type VideoHandlers,
} from "./videoDecoder.ts";

/** How long a region's decoder outlives its `regionEnd`, for a stream that reuses its id. */
export const RETIRE_MS = 4_000;

/** The destination: the desktop's 2D context. */
type RegionContext =
  | CanvasRenderingContext2D
  | OffscreenCanvasRenderingContext2D;

export interface RegionStreams {
  /** Adopt a `regionFormat`: build, or reconfigure, stream `stream`'s decoder. */
  setFormat(stream: number, format: VideoFormat): void;
  /** Adopt a `regionEnd`: the stream's decoder retires unless its id is started again first. */
  end(stream: number): void;
  /** Start one unit's decode; called in wire order, and resolved to null for nothing to draw. */
  decode(record: RegionMsg): Promise<DecodedPicture | null>;
  /** Draw one unit's picture in its turn, and close it: true where something was drawn. */
  draw(
    context: RegionContext,
    record: RegionMsg,
    picture: DecodedPicture,
  ): boolean;
  /** Cut every stream's chain: the next unit of each builds a decoder that waits for a keyframe. */
  restart(): void;
  /** Every decoder given back. */
  close(): void;
}

export function createRegionStreams(options: {
  /** The decoders' errors and keyframe asks, as the desktop's stream's are. */
  handlers: VideoHandlers;
  /** Injectable for a test, which waits on no four seconds. */
  retireMs?: number;
}): RegionStreams {
  interface Region {
    video: DesktopVideo;
    /** Whether its `regionEnd` came, and no `regionFormat` since. */
    ended: boolean;
    retire: ReturnType<typeof setTimeout> | undefined;
  }
  const regions = new Map<number, Region>();
  const retireMs = options.retireMs ?? RETIRE_MS;
  // Whether a unit of a stream with no decoder has been logged: one line, not one
  // a unit.
  let warned = false;

  const keep = (region: Region) => {
    clearTimeout(region.retire);
    region.retire = undefined;
  };

  const retire = (stream: number) => {
    const region = regions.get(stream);
    if (region) {
      keep(region);
      region.video.close();
      regions.delete(stream);
    }
  };

  return {
    setFormat(stream, format) {
      const held = regions.get(stream);
      if (held) {
        // The id started again: the decoder is kept where the configuration and
        // the size are the same, and replaced by its first unit where either is
        // not — a new stream's first unit is its keyframe either way.
        keep(held);
        held.ended = false;
        held.video.setFormat(format);
        return;
      }
      const video = createDesktopVideo(options.handlers);
      video.setFormat(format);
      regions.set(stream, { video, ended: false, retire: undefined });
    },
    end(stream) {
      const held = regions.get(stream);
      if (held && !held.ended) {
        held.ended = true;
        held.retire = setTimeout(() => retire(stream), retireMs);
      }
    },
    decode(record) {
      const held = regions.get(record.stream);
      if (!held || held.ended) {
        // A unit of a stream this page was never told of, or told is over: on a
        // reattach, from before the `tilesStart` that starts the picture over.
        if (!warned) {
          warned = true;
          console.warn(
            "regions: dropping a region's unit until its format arrives",
          );
        }
        return Promise.resolve(null);
      }
      return held.video.decode(
        { w: record.w, h: record.h },
        record.data,
        record.keyframe,
      );
    },
    draw(context, record, picture) {
      try {
        // Never so: what decodes VP9 is the browser's own decoder, whose picture
        // is a `VideoFrame`. A software decoder's planes are drawn on the GPU's
        // canvas over this one, which a region is not.
        if (isSoftwarePlanes(picture)) {
          return false;
        }
        // At its place, cut to its rectangle: a VP9 picture of an odd side is a
        // pixel larger. A clip and not a source rectangle, which WebKit's canvas
        // does not read of a `VideoFrame` (framePainter.ts's `place`).
        context.save();
        try {
          context.beginPath();
          context.rect(record.x, record.y, record.w, record.h);
          context.clip();
          context.drawImage(picture, record.x, record.y);
        } finally {
          context.restore();
        }
        return true;
      } finally {
        // Whatever became of it: a picture held is decoder memory.
        picture.close();
      }
    },
    restart() {
      for (const held of regions.values()) {
        held.video.restart();
      }
    },
    close() {
      for (const stream of [...regions.keys()]) {
        retire(stream);
      }
      warned = false;
    },
  };
}
