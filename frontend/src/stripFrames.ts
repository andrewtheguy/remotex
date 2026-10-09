// BETA: when a picture a High Performance Mac sends in strips is shown, in the
// decode worker (softwareDecoder.worker.ts).
//
// The picture comes in four strips, each a unit, a frame being the strips that
// changed; the HEVC module puts the picture together as they decode. It is shown
// a frame at a time, not a strip at a time, and nothing in a strip says how many
// its frame has. So the answer to a frame's last strip so far waits for what
// follows it: another strip of the frame, and the answer is no picture; a strip
// that begins the next frame, or no strip for as long as the Mac was seen to
// take between two, and it is the frame's picture. A frame's fourth strip is
// answered at once. Every unit is answered once, in order, as the decoder's
// caller counts on.

import type { PictureStrip } from "./softwareDecoder.ts";

/** The strips of a picture. */
export const STRIPS = 4;

/**
 * How long a frame's last strip waits for another of its frame. The Mac codes a
 * frame's strips one after another, up to 7 ms apart as measured, and the
 * gateway waits as long where it decodes them (`STRIP_WAIT` in
 * src/vnc_apple_media.rs).
 */
export const STRIP_WAIT_MS = 8;

/** What the worker does for the frames, a stream being known by its id. */
export interface FrameAnswers<Picture> {
  /** Answer a unit with its picture; resolves once the picture has been read. */
  show(id: number, picture: Picture): Promise<void>;
  /** Answer a unit with no picture. */
  none(id: number): void;
  /** Whether the stream is ending, so that nothing would read a picture of it. */
  ending(id: number): boolean;
  /** Run `task` in its turn among the units queued. */
  queue(task: () => Promise<void>): void;
}

/** A frame whose last strip so far has not been answered. */
interface Frame<Picture> {
  /** How many of its strips have come. */
  strips: number;
  /** The picture as its strips so far leave it, once it has every strip since a keyframe. */
  picture: Picture | null;
  /** Ends the frame where nothing follows its last strip. */
  timer: ReturnType<typeof setTimeout>;
}

export interface StripFrames<Picture> {
  /**
   * Before a stream's next unit is decoded: answer the strip that waits, now
   * that this unit says whether its frame is over. Resolves to how many strips
   * of that frame had come.
   */
  before(id: number, strip: PictureStrip | undefined): Promise<number>;
  /**
   * After a strip is decoded, to `picture` or to none yet: whether its answer
   * waits here. `before` is what `before` resolved to for the unit.
   */
  after(
    id: number,
    strip: PictureStrip,
    picture: Picture | null,
    before: number,
  ): boolean;
  /** Drop a stream's waiting answer: the stream is over. */
  forget(id: number): void;
}

export function createStripFrames<Picture>(
  answers: FrameAnswers<Picture>,
  waitMs: number = STRIP_WAIT_MS,
): StripFrames<Picture> {
  const frames = new Map<number, Frame<Picture>>();

  const forget = (id: number) => {
    const frame = frames.get(id);
    if (frame) {
      clearTimeout(frame.timer);
      frames.delete(id);
    }
  };

  /** Answer the strip that waits: with the frame's picture where it is `over`. */
  const settle = async (id: number, over: boolean): Promise<number> => {
    const frame = frames.get(id);
    if (!frame) {
      return 0;
    }
    forget(id);
    if (answers.ending(id)) {
      return 0;
    }
    if (over && frame.picture) {
      await answers.show(id, frame.picture);
    } else {
      answers.none(id);
    }
    return frame.strips;
  };

  return {
    // A unit that is no strip ends a frame as one that begins the next does.
    before: (id, strip) => settle(id, !strip || strip.begins),
    after(id, strip, picture, before) {
      const strips = (strip.begins ? 0 : before) + 1;
      if (strips >= STRIPS) {
        return false;
      }
      const timer = setTimeout(() => {
        answers.queue(async () => {
          if (frames.get(id)?.timer === timer) {
            await settle(id, true);
          }
        });
      }, waitMs);
      frames.set(id, { strips, picture, timer });
      return true;
    },
    forget,
  };
}
