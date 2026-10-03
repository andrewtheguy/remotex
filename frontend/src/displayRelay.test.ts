// The second display's relay, both ends driven by hand: what the session page's
// end sends of its picture for the part a tab shows, when, and what the tab's
// end does with it. No channel is opened; a port here is a list of what was
// posted and a way to deliver the other end's word.
//
// Run with `bun test src/displayRelay.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  createRelaySink,
  createRelaySource,
  type RelayMessage,
  type RelayPort,
} from "./displayRelay.ts";
import type { Picture } from "./egfxCompositor.ts";

function fakePort() {
  const posted: RelayMessage[] = [];
  let handler: (message: RelayMessage) => void = () => {};
  let closed = 0;
  const port: RelayPort = {
    post: (message) => {
      posted.push(message);
    },
    onMessage: (next) => {
      handler = next;
    },
    close: () => {
      closed += 1;
    },
  };
  return {
    port,
    posted,
    deliver: (message: RelayMessage) => handler(message),
    closed: () => closed,
  };
}

/** A picture `w` by `h` whose every pixel's bytes are its x, its y, `mark`, 0. */
function picture(w: number, h: number, mark = 0): Picture {
  const pixels = new Uint8ClampedArray(w * h * 4);
  for (let y = 0; y < h; y += 1) {
    for (let x = 0; x < w; x += 1) {
      pixels.set([x, y, mark, 0], (y * w + x) * 4);
    }
  }
  return { width: w, height: h, pixels };
}

/** The last message posted, which must be an update. */
function lastPaint(posted: RelayMessage[]) {
  const last = posted[posted.length - 1];
  assert.equal(last?.kind, "paint");
  if (last?.kind !== "paint") {
    throw new Error("not an update");
  }
  return { ...last, bytes: [...new Uint8Array(last.pixels)] };
}

const PART = { x: 4, y: 0, w: 4, h: 4 };

test("a source that starts asks, and a part said before there is a picture is owed", () => {
  const { port, posted, deliver } = fakePort();
  let current: Picture | null = null;
  const source = createRelaySource(port, () => current);
  assert.deepEqual(posted, [{ kind: "composing" }]);
  // Said before there is a picture: nothing to send it from yet.
  deliver({ kind: "shown", display: 2, part: PART });
  assert.equal(posted.length, 1);
  // The run that lays the picture out sends it.
  current = picture(8, 4);
  source.painted({ painted: [], width: 8, height: 4, resized: true });
  const sent = lastPaint(posted);
  assert.deepEqual([sent.seq, sent.rects], [1, [0, 0, 4, 4]]);
});

test("the whole part is sent out of the picture, relative to the part", () => {
  const { port, posted, deliver } = fakePort();
  const current = picture(8, 4);
  const source = createRelaySource(port, () => current);
  deliver({ kind: "shown", display: 2, part: PART });
  source.painted({ painted: [], width: 8, height: 4, resized: true });
  const sent = lastPaint(posted);
  assert.deepEqual(
    [sent.seq, sent.w, sent.h, sent.rects],
    [1, 4, 4, [0, 0, 4, 4]],
  );
  // The right half of every row: x from 4, each row's y.
  const expected: number[] = [];
  for (let y = 0; y < 4; y += 1) {
    for (let x = 4; x < 8; x += 1) {
      expected.push(x, y, 0, 0);
    }
  }
  assert.deepEqual(sent.bytes, expected);
});

test("a run's rectangles are cut to the part; those outside are nothing to the tab", () => {
  const { port, posted, deliver } = fakePort();
  const current = picture(8, 4);
  const source = createRelaySource(port, () => current);
  deliver({ kind: "shown", display: 2, part: PART });
  deliver({ kind: "painted", seq: 1 });
  // One rectangle wholly in the first column, one straddling the two.
  source.painted({
    painted: [0, 0, 2, 2, 3, 1, 3, 2],
    width: 8,
    height: 4,
    resized: false,
  });
  const sent = lastPaint(posted);
  assert.deepEqual(
    sent.rects,
    [0, 1, 2, 2],
    "x 4 to 6, rows 1 and 2, from the part's origin",
  );
  assert.deepEqual(
    sent.bytes,
    [4, 1, 0, 0, 5, 1, 0, 0, 4, 2, 0, 0, 5, 2, 0, 0],
  );
  assert.equal(posted.length, 3, "composing, then two updates");
  // A run painting only the other column sends nothing.
  deliver({ kind: "painted", seq: 2 });
  source.painted({
    painted: [0, 0, 4, 4],
    width: 8,
    height: 4,
    resized: false,
  });
  assert.equal(posted.length, 3);
});

test("one update is in flight at a time; what is painted meanwhile waits, merged, and goes out of the picture as it is then", () => {
  const { port, posted, deliver } = fakePort();
  let current = picture(8, 4, 1);
  const source = createRelaySource(port, () => current);
  deliver({ kind: "shown", display: 2, part: PART });
  assert.equal(posted.length, 2, "the whole part, at once: there is a picture");
  current = picture(8, 4, 2);
  source.painted({
    painted: [4, 0, 1, 1],
    width: 8,
    height: 4,
    resized: false,
  });
  current = picture(8, 4, 3);
  source.painted({
    painted: [6, 0, 1, 1],
    width: 8,
    height: 4,
    resized: false,
  });
  assert.equal(posted.length, 2, "held while the first is in flight");
  // An acknowledgement of something else changes nothing.
  deliver({ kind: "painted", seq: 7 });
  assert.equal(posted.length, 2);
  deliver({ kind: "painted", seq: 1 });
  const sent = lastPaint(posted);
  assert.equal(sent.seq, 2);
  assert.deepEqual(sent.rects, [0, 0, 1, 1, 2, 0, 1, 1]);
  assert.deepEqual(
    sent.bytes,
    [4, 0, 3, 0, 6, 0, 3, 0],
    "the latest picture, not the one each run left",
  );
});

test("a part the picture does not reach is cut to it, and a reset owes it all again", () => {
  const { port, posted, deliver } = fakePort();
  let current = picture(8, 4);
  const source = createRelaySource(port, () => current);
  // A tab laid out for a span the picture has not been reset to yet.
  deliver({ kind: "shown", display: 2, part: { x: 4, y: 0, w: 8, h: 8 } });
  let sent = lastPaint(posted);
  assert.deepEqual([sent.w, sent.h, sent.rects], [8, 8, [0, 0, 4, 4]]);
  deliver({ kind: "painted", seq: 1 });
  // The reset that lays the span out: the whole part, out of the new picture.
  current = picture(12, 8);
  source.painted({ painted: [], width: 12, height: 8, resized: true });
  sent = lastPaint(posted);
  assert.deepEqual(sent.rects, [0, 0, 8, 8]);
  assert.equal(sent.bytes.length, 8 * 8 * 4);
});

test("many rectangles waiting become the one around them", () => {
  const { port, posted, deliver } = fakePort();
  const current = picture(100, 4);
  const source = createRelaySource(port, () => current);
  deliver({ kind: "shown", display: 2, part: { x: 0, y: 0, w: 100, h: 4 } });
  assert.equal(posted.length, 2, "the whole part, in flight");
  for (let i = 0; i < 70; i += 1) {
    const run = {
      painted: [i, 1, 1, 1],
      width: 100,
      height: 4,
      resized: false,
    };
    source.painted(run);
    // The same again adds nothing.
    source.painted(run);
  }
  deliver({ kind: "painted", seq: 1 });
  const sent = lastPaint(posted);
  assert.deepEqual(sent.rects, [0, 1, 70, 1]);
});

test("a tab that says what it shows again starts over, and a reset source owes nothing", () => {
  const { port, posted, deliver } = fakePort();
  const current = picture(8, 4);
  const source = createRelaySource(port, () => current);
  deliver({ kind: "shown", display: 2, part: PART });
  source.painted({
    painted: [4, 0, 1, 1],
    width: 8,
    height: 4,
    resized: false,
  });
  // The tab reloaded: whatever was in flight is nothing to it now.
  deliver({ kind: "shown", display: 2, part: { x: 0, y: 0, w: 4, h: 4 } });
  let sent = lastPaint(posted);
  assert.deepEqual([sent.seq, sent.rects], [2, [0, 0, 4, 4]]);
  // The pipeline ended: what was owed of its picture is forgotten, and the next
  // picture's first run says what is owed.
  source.painted({
    painted: [0, 0, 1, 1],
    width: 8,
    height: 4,
    resized: false,
  });
  source.reset();
  deliver({ kind: "painted", seq: 2 });
  assert.equal(posted.length, 3);
  source.painted({ painted: [], width: 8, height: 4, resized: true });
  sent = lastPaint(posted);
  assert.deepEqual([sent.seq, sent.rects], [3, [0, 0, 4, 4]]);
  source.close();
});

test("the tab's end paints what it is sent, says so, and says what it shows when asked", () => {
  const { port, posted, deliver, closed } = fakePort();
  const patched: [number, number, number[], number][] = [];
  let painted = 0;
  const errors: string[] = [];
  const sink = createRelaySink(
    port,
    {
      patch(w, h, rects, pixels) {
        if (w === 0) {
          throw new Error("the GPU refused the picture");
        }
        patched.push([w, h, Array.from(rects), pixels.length]);
      },
    },
    () => {
      painted += 1;
    },
    (why) => {
      errors.push(why);
    },
  );
  // Asked before it shows anything, it says nothing.
  deliver({ kind: "composing" });
  assert.deepEqual(posted, []);
  sink.show(2, PART);
  assert.deepEqual(posted, [{ kind: "shown", display: 2, part: PART }]);
  deliver({ kind: "composing" });
  assert.equal(posted.length, 2);
  deliver({
    kind: "paint",
    seq: 5,
    w: 4,
    h: 4,
    rects: [0, 0, 4, 4],
    pixels: new ArrayBuffer(64),
  });
  assert.deepEqual(patched, [[4, 4, [0, 0, 4, 4], 64]]);
  assert.deepEqual(posted[2], { kind: "painted", seq: 5 });
  assert.equal(painted, 1);
  // A picture that will not take an update ends the painting, once.
  deliver({
    kind: "paint",
    seq: 6,
    w: 0,
    h: 0,
    rects: [],
    pixels: new ArrayBuffer(0),
  });
  deliver({
    kind: "paint",
    seq: 7,
    w: 4,
    h: 4,
    rects: [],
    pixels: new ArrayBuffer(0),
  });
  assert.deepEqual(errors, ["the GPU refused the picture"]);
  assert.equal(patched.length, 1);
  assert.equal(posted.length, 3, "no acknowledgement of what was not painted");
  sink.close();
  assert.equal(closed(), 1);
});
