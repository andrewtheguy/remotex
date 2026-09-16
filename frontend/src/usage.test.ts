import assert from "node:assert/strict";
import { test } from "node:test";

import {
  customUsageRange,
  formatRate,
  liveTotals,
  targetLabel,
  USAGE_PRESETS,
  type UsageRecord,
  usageByTarget,
  usageRangeKey,
  usageRangeLabel,
  usageRate,
  usageTotals,
  usageWithin,
} from "./usage.ts";

/// A record whose bytes all moved in one second, so its peaks are its bytes.
const record = (
  target: string | null,
  socket: UsageRecord["socket"],
  sentBytes: number,
  receivedBytes: number,
  start = 0,
  end = start + 60,
): UsageRecord => ({
  target,
  socket,
  start,
  end,
  sentBytes,
  receivedBytes,
  peakSentPerSec: sentBytes,
  peakReceivedPerSec: receivedBytes,
});

test("a rate is bytes over seconds, and none over no time", () => {
  assert.equal(usageRate(6000, 60), 100);
  assert.equal(usageRate(0, 60), 0);
  assert.equal(usageRate(500, 0), null, "a timeframe that just began");
});

test("a rate in bytes per second is shown in decimal bits per second", () => {
  assert.equal(formatRate(0), "0 bps");
  assert.equal(formatRate(50), "400 bps");
  assert.equal(formatRate(124.9), "999 bps");
  assert.equal(formatRate(125), "1.0 kbps");
  assert.equal(formatRate(1000), "8.0 kbps");
  assert.equal(formatRate(12_500), "100 kbps");
  assert.equal(formatRate(2_500_000), "20 Mbps");
  assert.equal(formatRate(1.5e9), "12 Gbps");
});

test("the rate right now is summed over what moved", () => {
  assert.deepEqual(
    liveTotals([
      {
        target: "mac",
        socket: "session",
        sentPerSec: 1000,
        receivedPerSec: 10,
      },
      { target: "mac", socket: "audio", sentPerSec: 200, receivedPerSec: 0 },
    ]),
    { sent: 1200, received: 10 },
  );
  assert.deepEqual(liveTotals([]), { sent: 0, received: 0 });
});

test("a range asks for its length, and everything kept for no bound", () => {
  assert.equal(usageWithin({ amount: 15, unit: "minutes" }), 900);
  assert.equal(usageWithin({ amount: 1, unit: "hours" }), 3600);
  assert.equal(usageWithin({ amount: 24, unit: "hours" }), 86_400);
  assert.equal(usageWithin({ amount: 7, unit: "days" }), 604_800);
  assert.equal(usageWithin("all"), null);
});

test("the presets step up without a jump, each spelled by one key", () => {
  const seconds = USAGE_PRESETS.map(usageWithin);
  assert.equal(seconds.at(-1), null, "everything kept comes last");
  const bounded = seconds.slice(0, -1) as number[];
  for (let i = 1; i < bounded.length; i++) {
    assert.ok(bounded[i] > bounded[i - 1], "ascending");
    assert.ok(bounded[i] <= bounded[i - 1] * 4, `no jump past 4x at ${i}`);
  }
  assert.equal(
    new Set(USAGE_PRESETS.map(usageRangeKey)).size,
    USAGE_PRESETS.length,
  );
  assert.equal(usageRangeKey({ amount: 24, unit: "hours" }), "24:hours");
  assert.equal(usageRangeKey("all"), "all");
  assert.equal(usageRangeLabel({ amount: 1, unit: "hours" }), "Last 1 hour");
  assert.equal(
    usageRangeLabel({ amount: 30, unit: "minutes" }),
    "Last 30 minutes",
  );
  assert.equal(usageRangeLabel("all"), "Everything kept");
});

test("a custom range takes a whole number of at least one", () => {
  assert.deepEqual(customUsageRange("90", "minutes"), {
    amount: 90,
    unit: "minutes",
  });
  assert.deepEqual(customUsageRange(" 2 ", "days"), {
    amount: 2,
    unit: "days",
  });
  assert.equal(customUsageRange("0", "hours"), null);
  assert.equal(customUsageRange("", "hours"), null);
  assert.equal(customUsageRange("1.5", "hours"), null);
  assert.equal(customUsageRange("-3", "hours"), null);
  assert.equal(customUsageRange("abc", "hours"), null);
});

test("totals are summed per socket and over every socket, with the seconds they span and their busiest second", () => {
  const totals = usageTotals([
    record("mac", "session", 1000, 10),
    record("win", "session", 500, 5),
    record(null, "mic", 0, 900, 60),
    // A second socket in a timeframe already spanned adds bytes, not seconds.
    record("mac", "audio", 200, 0),
  ]);
  const t = (
    sent: number,
    received: number,
    seconds: number,
    peakSent: number,
    peakReceived: number,
  ) => ({ sent, received, seconds, peakSent, peakReceived });
  assert.deepEqual(totals.session, t(1500, 15, 60, 1000, 10));
  assert.deepEqual(totals.mic, t(0, 900, 60, 0, 900));
  assert.deepEqual(totals.audio, t(200, 0, 60, 200, 0));
  assert.deepEqual(totals.camera, t(0, 0, 0, 0, 0));
  // A peak is one socket's busiest second, never two sockets' added together.
  assert.deepEqual(totals.all, t(1700, 915, 120, 1000, 900));
});

test("targets are compared by everything they moved, busiest first", () => {
  assert.deepEqual(
    usageByTarget([
      record("mac", "session", 100, 10),
      record(null, "session", 5, 5),
      record("win", "session", 4000, 20),
      record("mac", "audio", 900, 0),
      record("mac", "session", 0, 10, 60, 90),
    ]),
    [
      {
        target: "win",
        sent: 4000,
        received: 20,
        seconds: 60,
        peakSent: 4000,
        peakReceived: 20,
      },
      {
        target: "mac",
        sent: 1000,
        received: 20,
        seconds: 90,
        peakSent: 900,
        peakReceived: 10,
      },
      {
        target: null,
        sent: 5,
        received: 5,
        seconds: 60,
        peakSent: 5,
        peakReceived: 5,
      },
    ],
  );
  assert.equal(targetLabel(null), "No target (picker)");
  assert.equal(targetLabel("mac"), "mac");
});
