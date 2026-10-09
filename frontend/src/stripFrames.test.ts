// BETA: a picture in strips is shown a frame at a time, every strip answered once.
import assert from "node:assert/strict";
import { test } from "node:test";

import type { PictureStrip } from "./softwareDecoder.ts";
import { createStripFrames } from "./stripFrames.ts";

const WAIT_MS = 5;
const ROWS = 900;

/** One stream's strips through the frames, as the decode worker drives them. */
function stream() {
  const answers: (string | null)[] = [];
  let ending = false;
  let queue: Promise<void> = Promise.resolve();
  const frames = createStripFrames<string>(
    {
      show: async (_id, picture) => {
        answers.push(picture);
      },
      none: () => answers.push(null),
      ending: () => ending,
      queue: (task) => {
        queue = queue.then(task);
      },
    },
    WAIT_MS,
  );
  return {
    answers,
    frames,
    end() {
      ending = true;
    },
    /** A strip decoded to `picture`, or to none while the picture lacks a strip. */
    async strip(index: number, begins: boolean, picture: string | null) {
      const strip: PictureStrip = { index, begins, rows: ROWS };
      const before = await frames.before(1, strip);
      if (!frames.after(1, strip, picture, before)) {
        answers.push(picture);
      }
    },
    async waited() {
      await new Promise((resolve) => setTimeout(resolve, WAIT_MS * 4));
      await queue;
    },
  };
}

test("a frame's fourth strip is answered at once with the picture, the three before it with none", async () => {
  const s = stream();
  await s.strip(0, true, null);
  await s.strip(1, false, null);
  await s.strip(2, false, null);
  assert.deepEqual(s.answers, [null, null]);
  await s.strip(3, false, "whole");
  assert.deepEqual(s.answers, [null, null, null, "whole"]);
  await s.waited();
  assert.equal(s.answers.length, 4, "nothing waits after a whole frame");
});

test("a frame of fewer strips is shown when the next frame's first strip comes", async () => {
  const s = stream();
  await s.strip(1, true, "a1");
  await s.strip(3, false, "a2");
  assert.deepEqual(s.answers, [null]);
  await s.strip(0, true, "b1");
  assert.deepEqual(s.answers, [null, "a2"]);
  await s.waited();
  assert.deepEqual(s.answers, [null, "a2", "b1"]);
});

test("a frame nothing follows is shown once the wait for another strip runs out", async () => {
  const s = stream();
  await s.strip(2, true, "a");
  assert.deepEqual(s.answers, []);
  await s.waited();
  assert.deepEqual(s.answers, ["a"]);
  // Its frame's next strip, come late, is a frame of its own.
  await s.strip(3, false, "b");
  await s.waited();
  assert.deepEqual(s.answers, ["a", "b"]);
});

test("a frame whose picture lacks a strip since the keyframe is answered with none", async () => {
  const s = stream();
  await s.strip(0, true, null);
  await s.waited();
  assert.deepEqual(s.answers, [null]);
});

test("a unit that is no strip ends the frame before it", async () => {
  const s = stream();
  await s.strip(0, true, "a");
  assert.equal(await s.frames.before(1, undefined), 1);
  assert.deepEqual(s.answers, ["a"]);
});

test("a stream that ends has its waiting strip answered with nothing", async () => {
  const forgotten = stream();
  await forgotten.strip(0, true, "a");
  forgotten.frames.forget(1);
  await forgotten.waited();
  assert.deepEqual(forgotten.answers, []);

  const ending = stream();
  await ending.strip(0, true, "a");
  ending.end();
  await ending.waited();
  assert.deepEqual(ending.answers, [], "nothing would read its picture");
});
