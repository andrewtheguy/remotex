// ALPHA: a picture sent for motion (`tilesStart`), drawn on the desktop's own
// canvas.
//
// The gateway composes an RDP host's graphics pipeline itself and sends what it
// composed in two forms: TILE records, a PNG each of a rectangle that is still,
// the desktop's pixels exactly; and a VP9 stream of its own for each region that
// moves, as REGION records. This module decodes both and draws them where the
// records say, in the order the painter hands them over, which is the wire's
// (framePainter.ts starts every record's decode at once and draws each in its
// turn, as it does VIDEO records).
//
// A region's stream is decoded as the desktop's stream is (videoDecoder.ts): the
// browser's own decoder where it takes the gateway's VP9, the page's WebAssembly
// decoder where it does not, with the same keyframe gate and stall backstop. One
// decoder for each stream id, built by its `regionFormat` and kept past its
// `regionEnd` for `RETIRE_MS`, because the next region to move very often reuses
// the id at the same size, and a decoder rebuilt is a hardware decode session torn
// down and set up again — slow on iOS, and the churn a hardware decoder goes quiet
// under (`createVideoStream`).
//
// What the software decoder outputs is planes on the GPU's side of the page
// (planesPicture.ts), which the desktop's 2D canvas cannot draw. A region's planes
// are drawn by a picture of the region's own size on a WebGL canvas off the page,
// and the canvas taken as a bitmap onto the desktop's. One such canvas for every
// region: its WebGL context lives as long as the canvas does, and a context for
// each stream would be one for every id the page ever saw.

import { createPlanesPicture, type PlanesPicture } from "./planesPicture.ts";
import type { RegionMsg, TileMsg } from "./protocol.ts";
import { type DecodedPicture, isSoftwarePlanes } from "./softwareDecoder.ts";
import type { PageDecoding } from "./softwareSupport.ts";
import {
  createDesktopVideo,
  type DesktopVideo,
  type VideoFormat,
  type VideoHandlers,
} from "./videoDecoder.ts";

/** How long a region's decoder outlives its `regionEnd`, for a stream that reuses its id. */
export const RETIRE_MS = 4_000;

/** The destination: the desktop's 2D context. */
type MotionContext =
  | CanvasRenderingContext2D
  | OffscreenCanvasRenderingContext2D;

/** A record of a picture sent for motion. */
export type MotionRecord = TileMsg | RegionMsg;

/** A record's decoded picture: a tile's bitmap, or a region's frame or planes. */
export type MotionImage = ImageBitmap | DecodedPicture;

/** A software decoder's planes, drawn to something the 2D canvas can draw. */
export interface PlanesImage {
  /**
   * `planes` as an image `w` by `h`, the caller's to close; null where the
   * picture has nothing to show yet. Throws where the GPU will not draw it.
   */
  render(
    planes: Parameters<PlanesPicture["place"]>[0],
    w: number,
    h: number,
  ): ImageBitmap | null;
  close(): void;
}

export interface MotionPicture {
  /** Adopt a `regionFormat`: build, or reconfigure, stream `stream`'s decoder. */
  setFormat(stream: number, format: VideoFormat): void;
  /** Adopt a `regionEnd`: the stream's decoder retires unless its id is reused first. */
  end(stream: number): void;
  /** Start one record's decode; called in wire order, and resolved to null for nothing to draw. */
  decode(record: MotionRecord): Promise<MotionImage | null>;
  /** Draw one record's picture in its turn, and close it: true where something was drawn. */
  draw(
    context: MotionContext,
    record: MotionRecord,
    image: MotionImage,
  ): boolean;
  /** Cut every region's chain: the next unit of each builds a decoder that waits for a keyframe. */
  restart(): void;
  /** Every decoder and the planes' canvas given back. */
  close(): void;
}

/** The planes' picture on a WebGL canvas of its own, off the page. */
export function createPlanesImage(): PlanesImage {
  const canvas = new OffscreenCanvas(1, 1);
  const picture = createPlanesPicture(canvas);
  return {
    render(planes, w, h) {
      picture.place(planes);
      return picture.draw(w, h) ? canvas.transferToImageBitmap() : null;
    },
    close() {
      picture.close();
    },
  };
}

/** A tile's PNG as a bitmap of its pixels exactly: no colour profile applied, nothing premultiplied. */
function decodePng(png: Uint8Array): Promise<ImageBitmap> {
  return createImageBitmap(
    new Blob([png as Uint8Array<ArrayBuffer>], { type: "image/png" }),
    { colorSpaceConversion: "none", premultiplyAlpha: "none" },
  );
}

export function createMotionPicture(options: {
  /** The region decoders' errors and keyframe asks, as the desktop's stream's are. */
  handlers: VideoHandlers;
  /**
   * Something this page cannot draw: a sentence for the page, said once. The
   * pictures after it are dropped, since nothing sent again would be drawn either.
   */
  onBroken: (reason: string) => void;
  decoding?: PageDecoding;
  /** Injectable for a test, which has no WebGL and no image decoder. */
  makePlanesImage?: () => PlanesImage;
  decodePng?: (png: Uint8Array) => Promise<ImageBitmap>;
  retireMs?: number;
}): MotionPicture {
  interface Region {
    video: DesktopVideo;
    retire: ReturnType<typeof setTimeout> | undefined;
  }
  const regions = new Map<number, Region>();
  const retireMs = options.retireMs ?? RETIRE_MS;
  const makePlanesImage = options.makePlanesImage ?? createPlanesImage;
  const decodeTile = options.decodePng ?? decodePng;
  let planes: PlanesImage | null = null;
  let broken = false;
  // Whether a unit for a stream with no format has been logged: one line, not one a unit.
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

  const fail = (why: string) => {
    if (!broken) {
      broken = true;
      planes?.close();
      planes = null;
      options.onBroken(
        `This browser could not draw the moving picture (${why}). Reload the page to start the session over.`,
      );
    }
  };

  // A tile's picture. One that does not decode leaves its rectangle as it was,
  // which is a picture the desktop no longer is: a repaint sends every tile again.
  const tile = async (record: TileMsg): Promise<ImageBitmap | null> => {
    try {
      return await decodeTile(record.png);
    } catch (error) {
      const why = error instanceof Error ? error.message : String(error);
      options.handlers.onNeedsKeyframe(
        `a ${record.w}x${record.h} tile did not decode (${why})`,
      );
      return null;
    }
  };

  const region = (record: RegionMsg): Promise<DecodedPicture | null> => {
    const held = regions.get(record.stream);
    if (!held) {
      // A unit whose stream was never announced to this page: on a reattach, from
      // before the `tilesStart` that starts the picture over.
      if (!warned) {
        warned = true;
        console.warn("motion: dropping a region unit until its format arrives");
      }
      return Promise.resolve(null);
    }
    keep(held);
    return held.video.decode(
      { w: record.w, h: record.h },
      record.data,
      record.keyframe,
    );
  };

  // A region's planes as a bitmap, null where nothing is to be drawn. Built by
  // the first, and broken for good by the first the GPU will not draw.
  const renderPlanes = (
    picture: Parameters<PlanesPicture["place"]>[0],
    record: RegionMsg,
  ): ImageBitmap | null => {
    if (broken) {
      return null;
    }
    try {
      planes ??= makePlanesImage();
      return planes.render(picture, record.w, record.h);
    } catch (error) {
      fail(error instanceof Error ? error.message : String(error));
      return null;
    }
  };

  // The region's picture at its place, cut to its rectangle: a VP9 picture of an
  // odd side is a pixel larger. A clip and not a source rectangle, which WebKit's
  // canvas does not read of a `VideoFrame` (framePainter.ts's `place`).
  const drawRegion = (
    context: MotionContext,
    record: RegionMsg,
    picture: DecodedPicture,
  ): boolean => {
    const image = isSoftwarePlanes(picture)
      ? renderPlanes(picture, record)
      : picture;
    if (!image) {
      return false;
    }
    context.save();
    try {
      context.beginPath();
      context.rect(record.x, record.y, record.w, record.h);
      context.clip();
      context.drawImage(image, record.x, record.y);
    } finally {
      context.restore();
      if (image !== picture) {
        image.close();
      }
    }
    return true;
  };

  return {
    setFormat(stream, format) {
      const held = regions.get(stream);
      if (held) {
        // The id reused: the decoder is kept where the configuration and the
        // size are the same, and replaced by its first unit where either is not.
        keep(held);
        held.video.setFormat(format);
        return;
      }
      const video = createDesktopVideo(
        options.handlers,
        undefined,
        options.decoding,
      );
      video.setFormat(format);
      regions.set(stream, { video, retire: undefined });
    },
    end(stream) {
      const held = regions.get(stream);
      if (held) {
        keep(held);
        held.retire = setTimeout(() => retire(stream), retireMs);
      }
    },
    decode(record) {
      return record.kind === "tile" ? tile(record) : region(record);
    },
    draw(context, record, image) {
      try {
        if (record.kind === "region") {
          return drawRegion(context, record, image as DecodedPicture);
        }
        context.drawImage(image as ImageBitmap, record.x, record.y);
        return true;
      } finally {
        // Whatever became of it: a picture held is decoder memory, and the
        // software decoder waits on its close.
        image.close();
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
      planes?.close();
      planes = null;
      broken = false;
      warned = false;
    },
  };
}
