import assert from "node:assert/strict";
import { test } from "node:test";

import {
  formatBytes,
  targetLabel,
  type UsageRecord,
  usageByTarget,
  usageTotals,
  usageWithin,
} from "./usage.ts";

const record = (
  target: string | null,
  socket: UsageRecord["socket"],
  sentBytes: number,
  receivedBytes: number,
): UsageRecord => ({
  target,
  socket,
  start: 0,
  end: 60,
  sentBytes,
  receivedBytes,
});

test("bytes are shown in binary units, with a decimal only when small", () => {
  assert.equal(formatBytes(0), "0 B");
  assert.equal(formatBytes(1023), "1023 B");
  assert.equal(formatBytes(1024), "1.0 KB");
  assert.equal(formatBytes(1536), "1.5 KB");
  assert.equal(formatBytes(43_335), "42 KB");
  assert.equal(formatBytes(5 * 1024 * 1024), "5.0 MB");
  assert.equal(formatBytes(3 * 1024 ** 3), "3.0 GB");
});

test("a range asks for its length, and everything kept for no bound", () => {
  assert.equal(usageWithin("hour"), 3600);
  assert.equal(usageWithin("day"), 86_400);
  assert.equal(usageWithin("week"), 604_800);
  assert.equal(usageWithin("all"), null);
});

test("totals are summed per socket and over every socket", () => {
  const totals = usageTotals([
    record("mac", "session", 1000, 10),
    record("win", "session", 500, 5),
    record(null, "mic", 0, 900),
  ]);
  assert.deepEqual(totals.session, { sent: 1500, received: 15 });
  assert.deepEqual(totals.mic, { sent: 0, received: 900 });
  assert.deepEqual(totals.audio, { sent: 0, received: 0 });
  assert.deepEqual(totals.all, { sent: 1500, received: 915 });
});

test("targets are compared by everything they moved, busiest first", () => {
  assert.deepEqual(
    usageByTarget([
      record("mac", "session", 100, 10),
      record(null, "session", 5, 5),
      record("win", "session", 4000, 20),
      record("mac", "audio", 900, 0),
    ]),
    [
      { target: "win", sent: 4000, received: 20 },
      { target: "mac", sent: 1000, received: 10 },
      { target: null, sent: 5, received: 5 },
    ],
  );
  assert.equal(targetLabel(null), "No target (picker)");
  assert.equal(targetLabel("mac"), "mac");
});
