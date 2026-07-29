// The audio schedule's arithmetic, which is the whole of the latency behaviour and
// the only part of the player a test can reach: everything around it is an
// AudioContext, a WebCodecs decoder and Web Audio nodes, none of which exist here.
// What a browser adds — whether it can decode Opus at all — no unit test can answer;
// that is `cargo test --lib serve_a_test_tone -- --ignored` on each browser.
//
// Run with `bun test src/audioSchedule.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import { MAX_LEAD_S, START_LEAD_S, scheduleBuffer } from "./audioSchedule.ts";

/** One wave buffer from the tested Windows host: 32 KiB of CD-quality stereo. */
const BUFFER_S = 0.186;

test("a first buffer starts with a cushion rather than at the playhead", () => {
  // `nextAt` of 0 is "nothing has played yet", and the clock is well past it.
  const at = scheduleBuffer(0, 12.5, BUFFER_S);
  assert.equal(at.startAt, 12.5 + START_LEAD_S);
  assert.equal(at.trim, 0);
  assert.equal(at.clamped, false);
  assert.equal(at.nextAt, 12.5 + START_LEAD_S + BUFFER_S);
});

test("consecutive buffers are back to back, with no gap and no overlap", () => {
  let nextAt = 0;
  let now = 1;
  const starts: number[] = [];
  for (let i = 0; i < 5; i++) {
    const at = scheduleBuffer(nextAt, now, BUFFER_S);
    starts.push(at.startAt);
    assert.equal(at.trim, 0, "real-time delivery must never need a trim");
    nextAt = at.nextAt;
    // The host delivers one buffer per buffer's worth of wall clock.
    now += BUFFER_S;
  }
  for (let i = 1; i < starts.length; i++) {
    assert.ok(
      Math.abs(starts[i] - (starts[i - 1] + BUFFER_S)) < 1e-9,
      `buffer ${i} should start exactly where ${i - 1} ended`,
    );
  }
});

test("a gap in the audio restarts the cushion instead of scheduling in the past", () => {
  const first = scheduleBuffer(0, 1, BUFFER_S);
  // The remote went quiet for ten seconds, so the timeline is far behind the clock.
  const resumed = scheduleBuffer(first.nextAt, 11, BUFFER_S);
  assert.equal(resumed.startAt, 11 + START_LEAD_S);
  assert.ok(resumed.startAt > 11, "nothing can be scheduled before now");
  assert.equal(
    resumed.trim,
    0,
    "a gap is a gap; there is nothing to catch up on",
  );
});

test("a lead past the ceiling is trimmed off the front, not played late", () => {
  const now = 5;
  // A burst: the timeline runs 100 ms further ahead than the ceiling allows.
  const excess = 0.1;
  const at = scheduleBuffer(now + MAX_LEAD_S + excess, now, BUFFER_S);
  assert.equal(at.clamped, true);
  assert.equal(at.startAt, now + MAX_LEAD_S, "pulled back to the ceiling");
  assert.ok(
    Math.abs(at.trim - excess) < 1e-9,
    `exactly the excess is skipped, got ${at.trim}`,
  );
  // The delay is gone rather than deferred: what plays is the tail of the buffer,
  // ending where it would have ended if nothing had ever run ahead.
  assert.ok(
    Math.abs(at.nextAt - (now + MAX_LEAD_S + BUFFER_S - excess)) < 1e-9,
    "the timeline must come back under the ceiling, not merely stop growing",
  );
});

test("a lead longer than the buffer drops it whole rather than over-trimming", () => {
  const now = 2;
  const at = scheduleBuffer(now + 10, now, BUFFER_S);
  assert.equal(
    at.trim,
    BUFFER_S,
    "a trim can never exceed what the buffer holds",
  );
  assert.equal(at.startAt, now + MAX_LEAD_S);
  assert.equal(at.nextAt, now + MAX_LEAD_S, "nothing of it plays");
});

test("no schedule is ever in the past, and none is ever past the ceiling", () => {
  // Sweep the space rather than trusting the three cases above to cover it: the
  // failure that matters is a start time a browser silently refuses to honour.
  for (const lead of [-5, -0.5, 0, 0.05, 0.1, 0.3, 0.31, 1, 60]) {
    for (const duration of [0.02, 0.186, 1]) {
      const now = 7.25;
      const at = scheduleBuffer(now + lead, now, duration);
      assert.ok(at.startAt >= now, `lead ${lead} scheduled in the past`);
      assert.ok(
        at.startAt <= now + MAX_LEAD_S + 1e-9,
        `lead ${lead} scheduled past the ceiling`,
      );
      assert.ok(
        at.trim >= 0 && at.trim <= duration,
        `lead ${lead} trimmed ${at.trim}`,
      );
      assert.ok(
        at.nextAt >= at.startAt,
        `lead ${lead} moved the timeline backwards`,
      );
    }
  }
});

test("the cushion sits under the ceiling", () => {
  // Or a fresh start would be clamped the moment it was scheduled, and every stream
  // would begin with a trim.
  assert.ok(START_LEAD_S < MAX_LEAD_S);
});
