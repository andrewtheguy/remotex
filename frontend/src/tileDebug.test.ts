// The tiles' debug overlay: which tiles of the shown canvas it recomposes from
// the clean picture, and what it lays over them. Pixels are browser QA; what is
// checked here is the bookkeeping that decides where they go.
//
// Run with `bun test src/tileDebug.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import { createTileDebug, MARK_MS, type OverlayContext } from "./tileDebug.ts";

function harness(w: number, h: number) {
  let clock = 0;
  const timers: (() => void)[] = [];
  // The tiles copied from the clean picture, by their top left, and the marks
  // laid over a whole tile, with their strength.
  let copied: string[] = [];
  let marked: string[] = [];
  const shown: { fillStyle: string } & Omit<OverlayContext, "fillStyle"> = {
    fillStyle: "",
    fillRect(x: number, y: number, rw: number, rh: number) {
      if (shown.fillStyle.startsWith("rgba(255")) {
        const alpha = /, ([\d.]+)\)$/.exec(shown.fillStyle)?.[1];
        marked.push(`${x},${y} ${rw}x${rh} ${alpha}`);
      }
    },
    drawImage(
      _image: unknown,
      sx: number,
      sy: number,
      sw: number,
      sh: number,
      dx: number,
      dy: number,
    ) {
      assert.deepEqual([sx, sy], [dx, dy], "a tile is copied to where it is");
      copied.push(`${sx},${sy} ${sw}x${sh}`);
    },
  };
  const cleans: number[][] = [];
  const debug = createTileDebug({
    shown,
    w,
    h,
    makeClean: (cw, ch) => {
      cleans.push([cw, ch]);
      return {
        canvas: { width: cw, height: ch } as unknown as OffscreenCanvas,
        context: shown,
      };
    },
    now: () => clock,
    later: (run) => {
      timers.push(run);
    },
  });
  return {
    debug,
    cleans,
    take: () => {
      const out = { copied, marked };
      copied = [];
      marked = [];
      return out;
    },
    advance: (ms: number) => {
      clock += ms;
      const due = timers.splice(0);
      for (const run of due) {
        run();
      }
    },
  };
}

test("the first present draws every tile, clipped at the desktop's edges", () => {
  const h = harness(100, 70);
  h.debug.present();
  assert.deepEqual(h.take().copied, [
    "0,0 64x64",
    "64,0 36x64",
    "0,64 64x6",
    "64,64 36x6",
  ]);
  h.debug.present();
  assert.deepEqual(h.take().copied, [], "nothing changed, nothing is redrawn");
});

test("a painted tile is marked, fades, and is drawn clean once the mark is gone", () => {
  const h = harness(128, 64);
  h.debug.present();
  h.take();
  h.debug.painted([{ x: 64, y: 0, w: 64, h: 64 }]);
  h.debug.present();
  assert.deepEqual(h.take(), {
    copied: ["64,0 64x64"],
    marked: ["64,0 64x64 0.450"],
  });
  h.advance(MARK_MS / 2);
  assert.deepEqual(h.take(), {
    copied: ["64,0 64x64"],
    marked: ["64,0 64x64 0.225"],
  });
  h.advance(MARK_MS);
  assert.deepEqual(
    h.take(),
    { copied: ["64,0 64x64"], marked: [] },
    "the last pass draws the tile without its mark",
  );
  h.advance(MARK_MS);
  assert.deepEqual(h.take(), { copied: [], marked: [] }, "and the fade stops");
});

test("a rectangle across cells marks every cell it touches", () => {
  const h = harness(128, 128);
  h.debug.present();
  h.take();
  h.debug.painted([{ x: 60, y: 60, w: 8, h: 8 }]);
  h.debug.present();
  assert.deepEqual(h.take().copied, [
    "0,0 64x64",
    "64,0 64x64",
    "0,64 64x64",
    "64,64 64x64",
  ]);
});

test("a resize starts the clean picture over, every tile drawn again", () => {
  const h = harness(64, 64);
  h.debug.present();
  h.take();
  h.debug.resize(128, 64);
  assert.deepEqual(h.cleans, [
    [64, 64],
    [128, 64],
  ]);
  assert.deepEqual(h.take().copied, ["0,0 64x64", "64,0 64x64"]);
});
