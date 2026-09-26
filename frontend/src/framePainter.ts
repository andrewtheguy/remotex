import { type BatchRecord, decodeBatchFrame } from "./protocol.ts";
import {
  createDesktopVideo,
  type DesktopVideo,
  type VideoFormat,
} from "./videoDecoder.ts";

// The browser SPA's batch draw loop: each batch's records — access units and tiles —
// decoded in wire order and drawn onto the canvas. The decoder lives here rather than beside each caller:
// it belongs to exactly one attachment, and `clear` is the one place that ends it.

// The destination's 2D context. A union rather than the element's alone because
// the painter runs inside the paint worker, drawing through an `OffscreenCanvas` —
// the element context remains for the unit tests, which drive the painter directly.
export type PaintContext =
  | CanvasRenderingContext2D
  | OffscreenCanvasRenderingContext2D;

export interface FramePainter {
  /**
   * Decode one binary batch frame and paint it, in wire order. Malformed framing
   * drops the batch, which cuts the stream's chain or loses a tile's pixels, so it
   * also asks for a keyframe — a repaint, for a target that sends tiles.
   */
  draw(frame: ArrayBuffer): Promise<void>;
  /**
   * Drop the decoder. The next attachment's stream starts again from a keyframe.
   */
  clear(): void;
  /**
   * Adopt a `videoFormat`: the exact string to configure the decoder with. Always
   * arrives before the stream's first access unit.
   *
   * Held here rather than passed with each unit because it is announced once and used
   * by every unit after it. A runtime that fails here says so through `onVideoError`
   * exactly as a failing decode does.
   */
  setVideoFormat(format: VideoFormat): void;
}

export function createFramePainter(options: {
  /**
   * The destination, read per batch rather than captured, so a resize that
   * replaces the 2D context does not need the painter rebuilt.
   */
  context: () => PaintContext | null;
  /**
   * Why this client is showing nothing, or null once it is showing something. This
   * cannot be swallowed: the stream is all a target sends, so the alternative to
   * saying it is a desktop that never paints and never explains itself.
   */
  onVideoError: (reason: string | null) => void;
  /**
   * The stream's chain has been cut — its decoder went quiet, or it failed and was
   * thrown away — so the desktop cannot paint again until a keyframe only the gateway
   * can send. Separate from `onVideoError` because it asks for something rather than
   * saying something: it is the recovery, where the banner is the report, and the two
   * are answered in different places.
   */
  onVideoNeedsKeyframe: (reason: string) => void;
}): FramePainter {
  // Which attachment the decoder belongs to. `clear()` is the attachment boundary and
  // is not queued behind draws — an eviction closes the socket from under whatever
  // batch is mid-decode — so a draw that outlives the generation it started in must
  // not paint onto the next one.
  let generation = 0;

  // The decoder, built on the first announcement or unit.
  let video: DesktopVideo | null = null;

  // What is on screen about video, and whether a painted frame may take it down.
  //
  // A decoder giving up is answered by a painted frame: what the banner says is that
  // this client is showing nothing, so a frame is it ceasing to be true. A refusal is
  // not: the browser will not take that configuration, and nothing painted under it
  // says otherwise — the banner stays until the attachment ends or the gateway
  // announces a different configuration. That can happen: the announced VP9 level
  // follows the desktop's size, so a browser that refused a large picture may take
  // the smaller one a resize brings, and then its first painted frame is the answer.
  let videoComplained = false;
  // The configuration that was refused, or null.
  let refused: string | null = null;

  const complainAboutVideo = (
    reason: string,
    recoverable: boolean,
    decode: string,
  ) => {
    if (refused !== null && recoverable) {
      // The standing fact is the more useful sentence and it is already up.
      return;
    }
    if (!recoverable) {
      refused = decode;
    }
    videoComplained = recoverable;
    options.onVideoError(reason);
  };

  const releaseVideo = () => {
    video?.close();
    video = null;
    videoComplained = false;
    refused = null;
    // Retracted, and not merely forgotten. This is the attachment boundary: the
    // decoder that said it is gone, the next attachment may be a different target
    // through a different origin, and the page clears its own copy on the way back to
    // the picker only — a reattach or a takeover would otherwise inherit the sentence.
    options.onVideoError(null);
  };

  const desktopVideo = (): DesktopVideo => {
    if (video) {
      return video;
    }
    // Rebuilding a failed decoder is not this client's decision: the stream begins
    // again when the gateway sends a keyframe, which a repaint or a resize does.
    video = createDesktopVideo({
      onError: complainAboutVideo,
      onNeedsKeyframe: (reason) => options.onVideoNeedsKeyframe(reason),
    });
    videoComplained = false;
    options.onVideoError(null);
    return video;
  };

  // Every unit is part of one chain, so a dropped batch cuts it: the deltas after it
  // name a picture this decoder never made. Restarted rather than fed them, and a
  // keyframe asked for, exactly as a failed decoder is. A dropped tile is pixels
  // nothing will send again, so it asks the same way: the gateway answers with a
  // full update from the remote.
  const dropMalformed = () => {
    video?.restart();
    options.onVideoNeedsKeyframe("a malformed batch was dropped");
  };

  // One record's picture: a decoded frame for a unit, a decoded PNG for a tile.
  // Null when there is nothing to draw — a decoder that dropped the unit has said so
  // itself, and a tile that would not decode asks for a repaint here.
  const decode = (
    record: BatchRecord,
  ): Promise<VideoFrame | ImageBitmap | null> => {
    if (record.kind === "video") {
      return desktopVideo().decode(
        { w: record.w, h: record.h },
        record.data,
        record.keyframe,
      );
    }
    const png = new Blob([record.data as Uint8Array<ArrayBuffer>], {
      type: "image/png",
    });
    return createImageBitmap(png).catch(() => {
      options.onVideoNeedsKeyframe("a tile could not be decoded");
      return null;
    });
  };

  const paint = (record: BatchRecord, image: VideoFrame | ImageBitmap) => {
    const context = options.context();
    if (record.kind === "tile") {
      context?.drawImage(image, record.x, record.y);
      return;
    }
    const { w, h } = record;
    // Cropped by the desktop's size rather than drawn whole: the encoder is held
    // to even sides and an odd desktop does not have them, so the decoded picture
    // can be a pixel wider or taller than the desktop.
    context?.drawImage(image, 0, 0, w, h, 0, 0, w, h);
    if (videoComplained) {
      // Video is painting again, so whatever was said about it has stopped being
      // true. Said here rather than on a timer or behind a dismiss button: the
      // banner is a statement about the present, and this is the moment the
      // present changed.
      videoComplained = false;
      options.onVideoError(null);
    }
  };

  return {
    async draw(frame: ArrayBuffer) {
      const records = decodeBatchFrame(frame);
      if (!records) {
        dropMalformed();
        return;
      }
      const born = generation;
      // All decodes start at once — `decode` queues units on the decoder in wire
      // order — and each is drawn in wire order as it lands, so a picture is released
      // the moment it is drawn instead of the whole batch's worth staying alive until
      // the slowest. Drawing in order is what lets a later tile cover an earlier one.
      const decodes = records.map(decode);
      for (let i = 0; i < records.length; i += 1) {
        const image = await decodes[i];
        if (!image) {
          continue;
        }
        if (generation !== born) {
          // `clear()` ran while this decode was in flight: the previous desktop must
          // not show through on the next attachment's canvas.
          image.close();
          continue;
        }
        paint(records[i], image);
        image.close();
      }
    },
    clear() {
      generation += 1;
      releaseVideo();
    },
    setVideoFormat(format) {
      if (refused !== null && format.decode !== refused) {
        // Not the configuration that was refused, so the refusal no longer stands —
        // but the banner stays until a frame paints, as any other complaint's does.
        refused = null;
        videoComplained = true;
      }
      desktopVideo().setFormat(format);
    },
  };
}
