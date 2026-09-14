import assert from "node:assert/strict";
import { test } from "node:test";

import {
  formatBytes,
  type UsageRecord,
  usageSince,
  usageTotals,
} from "./usage.ts";

test("bytes are shown in binary units, with a decimal only when small", () => {
  assert.equal(formatBytes(0), "0 B");
  assert.equal(formatBytes(1023), "1023 B");
  assert.equal(formatBytes(1024), "1.0 KB");
  assert.equal(formatBytes(1536), "1.5 KB");
  assert.equal(formatBytes(43_335), "42 KB");
  assert.equal(formatBytes(5 * 1024 * 1024), "5.0 MB");
  assert.equal(formatBytes(3 * 1024 ** 3), "3.0 GB");
});

test("a range reads back from now, and everything kept reads from zero", () => {
  assert.equal(usageSince("hour", 10_000.7), 6400);
  assert.equal(usageSince("day", 100_000), 13_600);
  assert.equal(usageSince("week", 1000), 0, "never before the epoch");
  assert.equal(usageSince("all", 100_000), 0);
});

test("totals are summed per socket and over every socket", () => {
  const record = (
    socket: UsageRecord["socket"],
    sentBytes: number,
    receivedBytes: number,
  ): UsageRecord => ({ socket, start: 0, end: 60, sentBytes, receivedBytes });
  const totals = usageTotals([
    record("session", 1000, 10),
    record("session", 500, 5),
    record("mic", 0, 900),
  ]);
  assert.deepEqual(totals.session, { sent: 1500, received: 15 });
  assert.deepEqual(totals.mic, { sent: 0, received: 900 });
  assert.deepEqual(totals.audio, { sent: 0, received: 0 });
  assert.deepEqual(totals.all, { sent: 1500, received: 915 });
});
