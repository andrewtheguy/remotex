// The paint worker's ordering contract, driven without a Worker or an
// OffscreenCanvas: the painter is injected, and the commands arrive exactly as
// the page posts them. What is being pinned is the part the restructure moved
// off the main thread — every command holds wire order through async decodes,
// `clear` included, because a clear that jumped the queue would run before
// frames posted ahead of it and those draws would then paint the previous
// desktop onto the next attachment's canvas.
//
// Run with `bun test src/desktopPainterWorker.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  createPainterWorker,
  type PainterEvent,
} from "./desktopPainterWorker.ts";
import type { createTilePainter, TilePainter } from "./tilePainter.ts";

// A one-record-free batch frame: kind 0x02, flags 0, count 0. Valid enough for
// the kind check, which is all the worker itself reads.
const batchFrame = () => new Uint8Array([0x02, 0, 0, 0]).buffer;
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
    painterOptions: () => {
      assert.ok(painterOptions);
      return painterOptions;
    },
  };
}

test("a batch frame reaches the painter; anything else is dropped", async () => {
  const h = harness();
  const frame = batchFrame();
  h.host.handle({ type: "frame", data: frame });
  h.host.handle({ type: "frame", data: audioFrame() });
  await settled();
  assert.deepEqual(h.calls, ["draw"]);
  assert.equal(h.drawn[0], frame);
});

test("resize and videoFormat hold their place behind a stalled draw", async () => {
  const h = harness();
  h.stall();
  h.host.handle({ type: "frame", data: batchFrame() });
  h.host.handle({ type: "resize", w: 640, h: 480, seq: 7 });
  h.host.handle({
    type: "videoFormat",
    stream: 2,
    format: { codec: "vp9", decode: "vp09.00.40.08" },
  });
  await settled();
  // The draw is still out; nothing behind it has run, and the echo the page's
  // layout state waits on has not been sent.
  assert.deepEqual(h.calls, ["draw"]);
  assert.equal(h.canvas.width, 0);
  assert.deepEqual(h.events, []);
  h.release();
  await settled();
  assert.deepEqual(h.calls, ["draw", "fill", "format:2:vp09.00.40.08"]);
  assert.equal(h.canvas.width, 640);
  assert.equal(h.canvas.height, 480);
  assert.equal(h.ctx.fillStyle, "#000");
  assert.deepEqual(h.ctx.rects, [[0, 0, 640, 480]]);
  assert.deepEqual(h.events, [{ type: "resized", seq: 7 }]);
});

test("clear holds its place too, so an earlier frame cannot outlive it", async () => {
  const h = harness();
  h.host.handle({ type: "resize", w: 640, h: 480, seq: 1 });
  await settled();
  h.stall();
  h.host.handle({ type: "frame", data: batchFrame() });
  h.host.handle({ type: "clear" });
  await settled();
  // The stalled draw is still out; the clear waits behind it. A clear that ran
  // now would let that draw paint the old desktop onto the wiped canvas.
  assert.deepEqual(h.calls, ["fill", "draw"]);
  assert.equal(h.canvas.width, 640);
  h.release();
  await settled();
  assert.deepEqual(h.calls, ["fill", "draw", "clear"]);
  assert.equal(h.canvas.width, 0);
  assert.equal(h.canvas.height, 0);
});

test("the painter's callbacks travel back as events", () => {
  const h = harness();
  const options = h.painterOptions();
  options.onCacheReset();
  options.onVideoError("no decoder");
  options.onVideoError(null);
  assert.deepEqual(h.events, [
    { type: "cacheReset" },
    { type: "videoError", reason: "no decoder" },
    { type: "videoError", reason: null },
  ]);
});
