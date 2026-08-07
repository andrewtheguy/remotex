// The paint worker's ordering contract, driven without a Worker or an
// OffscreenCanvas: the painter is injected, and the commands arrive exactly as
// the page posts them. What is being pinned is the part the restructure moved
// off the main thread — every command holds wire order through async decodes —
// and the one command that must not: `clear` ends the attachment, so it runs
// even when the chain is stuck on a draw that never finishes, and the commands
// queued behind that draw must not then land on the next attachment's canvas.
//
// Run with `bun test src/desktopPainterWorker.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  createPainterWorker,
  type PainterEvent,
} from "./desktopPainterWorker.ts";
import type { createTilePainter, TilePainter } from "./tilePainter.ts";

// A one-record-free batch frame: kind 0x02, flags 0, count 0, then its u32
// sequence. Valid enough for the kind check, which is all the worker itself reads.
const batchFrame = (sequence = 1) =>
  new Uint8Array([0x02, 0, 0, 0, sequence, 0, 0, 0]).buffer;
// An audio frame's kind byte — the thing the session socket never carries, and
// the worker must drop rather than hand to the batch parser.
const audioFrame = () => new Uint8Array([0x03, 0, 0, 0]).buffer;

const settled = () => new Promise((resolve) => setTimeout(resolve, 0));

function harness() {
  const events: PainterEvent[] = [];
  const calls: string[] = [];
  const drawn: ArrayBuffer[] = [];
  let releaseDraw = () => {};
  let stallDraws = false;
  let clock = 0;
  let painterOptions: Parameters<typeof createTilePainter>[0] | null = null;

  const painter: TilePainter = {
    draw(frame) {
      calls.push("draw");
      drawn.push(frame);
      if (stallDraws) {
        return new Promise<void>((resolve) => {
          releaseDraw = resolve;
        });
      }
      return Promise.resolve();
    },
    clear() {
      calls.push("clear");
    },
    setVideoFormat(stream, format) {
      calls.push(`format:${stream}:${format.decode}`);
    },
  };

  const ctx = {
    fillStyle: "",
    rects: [] as number[][],
    fillRect(x: number, y: number, w: number, h: number) {
      calls.push("fill");
      this.rects.push([x, y, w, h]);
    },
  };
  const canvas = {
    width: 0,
    height: 0,
    getContext: () => ctx,
  };

  const host = createPainterWorker(
    (event) => events.push(event),
    (options) => {
      painterOptions = options;
      return painter;
    },
    () => clock,
  );
  host.handle({
    type: "init",
    canvas: canvas as unknown as OffscreenCanvas,
  });

  return {
    host,
    events,
    calls,
    drawn,
    ctx,
    canvas,
    stall: () => {
      stallDraws = true;
    },
    release: () => {
      stallDraws = false;
      releaseDraw();
    },
    // Stop stalling the draws to come without letting go of the one already out —
    // which is the state a clear has to work in, rather than after.
    unstall: () => {
      stallDraws = false;
    },
    advance: (milliseconds: number) => {
      clock += milliseconds;
    },
    painterOptions: () => {
      assert.ok(painterOptions);
      return painterOptions;
    },
  };
}

test("a batch frame reaches the painter; anything else is dropped", async () => {
  const h = harness();
  const frame = batchFrame();
  h.host.handle({ type: "frame", data: frame, sequence: 1, generation: 3 });
  h.host.handle({
    type: "frame",
    data: audioFrame(),
    sequence: 2,
    generation: 3,
  });
  await settled();
  assert.deepEqual(h.calls, ["draw"]);
  assert.equal(h.drawn[0], frame);
  assert.deepEqual(h.events, [
    {
      type: "painted",
      sequence: 1,
      generation: 3,
      queuedMs: 0,
      drawMs: 0,
    },
  ]);
});

test("resize and videoFormat hold their place behind a stalled draw", async () => {
  const h = harness();
  h.stall();
  h.host.handle({
    type: "frame",
    data: batchFrame(4),
    sequence: 4,
    generation: 2,
  });
  h.host.handle({ type: "resize", w: 640, h: 480, seq: 7 });
  h.host.handle({
    type: "videoFormat",
    stream: 2,
    format: { decode: "vp09.00.40.08" },
  });
  await settled();
  // The draw is still out; nothing behind it has run, and the echo the page's
  // layout state waits on has not been sent.
  assert.deepEqual(h.calls, ["draw"]);
  assert.equal(h.canvas.width, 0);
  assert.deepEqual(h.events, []);
  h.advance(12);
  h.release();
  await settled();
  assert.deepEqual(h.calls, ["draw", "fill", "format:2:vp09.00.40.08"]);
  assert.equal(h.canvas.width, 640);
  assert.equal(h.canvas.height, 480);
  assert.equal(h.ctx.fillStyle, "#000");
  assert.deepEqual(h.ctx.rects, [[0, 0, 640, 480]]);
  assert.deepEqual(h.events, [
    {
      type: "painted",
      sequence: 4,
      generation: 2,
      queuedMs: 0,
      drawMs: 12,
    },
    { type: "resized", seq: 7 },
  ]);
});

test("a later batch reports the time it waited behind earlier paint", async () => {
  const h = harness();
  h.stall();
  h.host.handle({
    type: "frame",
    data: batchFrame(1),
    sequence: 1,
    generation: 5,
  });
  await settled();
  h.advance(9);
  h.host.handle({
    type: "frame",
    data: batchFrame(2),
    sequence: 2,
    generation: 5,
  });
  h.advance(3);
  h.release();
  await settled();
  assert.deepEqual(h.events, [
    {
      type: "painted",
      sequence: 1,
      generation: 5,
      queuedMs: 0,
      drawMs: 12,
    },
    {
      type: "painted",
      sequence: 2,
      generation: 5,
      queuedMs: 3,
      drawMs: 0,
    },
  ]);
});

test("clear ends the attachment even with a draw that never finishes", async () => {
  const h = harness();
  h.host.handle({ type: "resize", w: 640, h: 480, seq: 1 });
  await settled();
  h.stall();
  h.host.handle({
    type: "frame",
    data: batchFrame(),
    sequence: 1,
    generation: 1,
  });
  await settled();
  h.host.handle({ type: "clear" });
  await settled();
  // A clear that waited its turn here would wait forever: this is the freeze it
  // exists to end, not an ordinary boundary.
  assert.deepEqual(h.calls, ["fill", "draw", "clear"]);
  assert.equal(h.canvas.width, 0);
  assert.equal(h.canvas.height, 0);
});

test("the next attachment paints without waiting for the stuck one", async () => {
  const h = harness();
  h.stall();
  h.host.handle({
    type: "frame",
    data: batchFrame(1),
    sequence: 1,
    generation: 1,
  });
  await settled();
  h.host.handle({ type: "clear" });
  // The stuck draw is *still* stuck — that is the whole point, and releasing it
  // here would test the recovery rather than the escape. The new target: the echo
  // the page's "Waiting for the remote desktop…" overlay is held up by, and then
  // its own first batch.
  h.unstall();
  h.host.handle({ type: "resize", w: 800, h: 600, seq: 2 });
  h.host.handle({
    type: "frame",
    data: batchFrame(1),
    sequence: 1,
    generation: 2,
  });
  await settled();
  assert.equal(h.canvas.width, 800);
  assert.deepEqual(
    h.events.filter((event) => event.type === "resized"),
    [{ type: "resized", seq: 2 }],
  );
  assert.deepEqual(
    h.events.filter((event) => event.type === "painted"),
    [{ type: "painted", sequence: 1, generation: 2, queuedMs: 0, drawMs: 0 }],
  );
});

test("a frame queued before the clear never reaches the next attachment", async () => {
  const h = harness();
  h.stall();
  // Two behind the wedge: the one being drawn, and one that never got its turn.
  h.host.handle({
    type: "frame",
    data: batchFrame(1),
    sequence: 1,
    generation: 1,
  });
  await settled();
  h.host.handle({
    type: "frame",
    data: batchFrame(2),
    sequence: 2,
    generation: 1,
  });
  h.host.handle({ type: "clear" });
  h.release();
  await settled();
  // The second one is for a desktop that is gone, and the clear did not wait for
  // it — releasing it must not paint it onto whatever comes next.
  assert.deepEqual(h.calls, ["draw", "clear"]);
});

test("the painter's callbacks travel back as events", () => {
  const h = harness();
  const options = h.painterOptions();
  options.onCacheReset();
  options.onVideoError("no decoder");
  options.onVideoError(null);
  options.onVideoStall("stream 2: went quiet");
  assert.deepEqual(h.events, [
    { type: "cacheReset" },
    { type: "videoError", reason: "no decoder" },
    { type: "videoError", reason: null },
    { type: "videoStall", reason: "stream 2: went quiet" },
  ]);
});
