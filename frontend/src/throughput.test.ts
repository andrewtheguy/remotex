import assert from "node:assert/strict";
import { test } from "node:test";

import {
  appendLive,
  clockNow,
  customThroughputRange,
  customThroughputWindow,
  formatRate,
  isThroughputWindow,
  LIVE_HISTORY_SECS,
  type LiveRate,
  liveSeries,
  liveTotals,
  localInputValue,
  MAX_GRAPH_POINTS,
  parseLocalInput,
  rateScale,
  recordedSeries,
  spanLabel,
  THROUGHPUT_PRESETS,
  type ThroughputLive,
  type ThroughputRecord,
  type ThroughputReport,
  targetLabel,
  throughputBounds,
  throughputQuery,
  throughputRangeIsLive,
  throughputRangeKey,
  throughputRangeLabel,
  throughputTargets,
} from "./throughput.ts";

/// A range that reaches `within` seconds back from the read, or everything kept.
const back = (within: number | null) => ({ within, end: null });

/// The seconds a range covers.
const within = (range: Parameters<typeof throughputBounds>[0]) =>
  throughputBounds(range).within;

/// A record whose bytes all moved in one second, so its peaks are its bytes.
const record = (
  target: string | null,
  socket: ThroughputRecord["socket"],
  sentBytes: number,
  receivedBytes: number,
  start = 0,
  end = start + 60,
): ThroughputRecord => ({
  target,
  socket,
  start,
  end,
  sentBytes,
  receivedBytes,
  peakSentPerSec: sentBytes,
  peakReceivedPerSec: receivedBytes,
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

const rate = (
  target: string | null,
  socket: LiveRate["socket"],
  sentPerSec: number,
  receivedPerSec: number,
): LiveRate => ({ target, socket, sentPerSec, receivedPerSec });

const sample = (at: number, ...rates: LiveRate[]): ThroughputLive => ({
  at,
  rates,
});

test("the history keeps one sample per second, newest last, as long as the longest sampled range", () => {
  let history = appendLive([], sample(100));
  history = appendLive(history, sample(101));
  assert.deepEqual(
    history.map((s) => s.at),
    [100, 101],
  );
  const again = appendLive(history, sample(101));
  assert.equal(again, history, "the poll came round before the next sample");
  assert.equal(appendLive(history, sample(99)), history, "an earlier second");
  for (let at = 102; at < 100 + LIVE_HISTORY_SECS + 5; at++) {
    history = appendLive(history, sample(at));
  }
  assert.equal(history.length, LIVE_HISTORY_SECS);
  assert.equal(history[0].at, 105, "the oldest fall off");
});

/// A series whose busiest second is its highest point, as a sampled one's is.
const series = (points: (number | null)[], busiest: number) => ({
  points,
  busiest,
});

test("a series is the window's seconds up to now, summed over what is kept, with gaps for seconds not read", () => {
  const history = [
    sample(10, rate("mac", "session", 1000, 10)),
    sample(11, rate("mac", "session", 2000, 20), rate("win", "audio", 300, 0)),
    // 12 was not read.
    sample(13, rate("win", "session", 50, 5)),
  ];
  const all = liveSeries(history, 5, () => true, 13);
  assert.deepEqual(all.sent, series([null, 1000, 2300, null, 50], 2300));
  assert.deepEqual(all.received, series([null, 10, 20, null, 5], 20));
  assert.equal(all.stepSecs, 1);
  assert.equal(all.end, 13);
  const mac = liveSeries(history, 3, (r) => r.target === "mac", 13);
  assert.deepEqual(mac.sent, series([2000, null, 0], 2000));
  assert.deepEqual(mac.received, series([20, null, 0], 20));
  const audio = liveSeries(history, 2, (r) => r.socket === "audio", 13);
  assert.deepEqual(audio.sent, series([null, 0], 0));
  // Reads have failed since the newest sample: it slides left and gaps follow it.
  const stale = liveSeries(history, 4, () => true, 15);
  assert.deepEqual(stale.sent, series([null, 50, null, null], 50));
  // Nothing read yet, or a window that ends before the samples: gaps throughout.
  assert.deepEqual(
    liveSeries(history, 3, () => true, null).sent,
    series([null, null, null], 0),
  );
  assert.deepEqual(
    liveSeries(history, 2, () => true, 9).sent,
    series([null, null], 0),
  );
});

test("the clock is the last sample's second plus the whole seconds since it was read", () => {
  assert.equal(clockNow(null, 5000), null);
  assert.equal(clockNow({ at: 100, wall: 5000 }, 5000), 100);
  assert.equal(clockNow({ at: 100, wall: 5000 }, 5400), 100);
  assert.equal(clockNow({ at: 100, wall: 5000 }, 5600), 101);
  assert.equal(clockNow({ at: 100, wall: 5000 }, 8100), 103);
  assert.equal(
    clockNow({ at: 100, wall: 5000 }, 4000),
    100,
    "never behind the sample",
  );
});

test("a scale tops out at a round number of bits per second above the peak", () => {
  assert.equal(formatRate(rateScale(0)), "1.0 kbps");
  assert.equal(
    formatRate(rateScale(100)),
    "1.0 kbps",
    "800 bps is under the floor",
  );
  assert.equal(formatRate(rateScale(125)), "2.0 kbps", "1 kbps needs headroom");
  assert.equal(formatRate(rateScale(100_000)), "1.0 Mbps");
  assert.equal(formatRate(rateScale(110_000)), "1.0 Mbps", "968 kbps fits");
  assert.equal(formatRate(rateScale(120_000)), "2.0 Mbps");
  assert.equal(formatRate(rateScale(280_000)), "2.5 Mbps");
  assert.equal(formatRate(rateScale(500_000)), "5.0 Mbps");
  assert.equal(formatRate(rateScale(1_000_000)), "10 Mbps");
  for (const peak of [0, 125, 999, 123_456, 9.9e6]) {
    assert.ok(rateScale(peak) >= peak * 1.1, `${peak} has a tenth of headroom`);
  }
});

const report = (
  now: number,
  records: ThroughputRecord[],
  open: ThroughputRecord[] = [],
): ThroughputReport => ({
  now,
  intervalSecs: 60,
  maxRecords: 1440,
  records,
  open,
});

test("a recorded range is a point per timeframe, the average over it, zero where nothing moved", () => {
  const read = report(
    600,
    [
      record("mac", "session", 6000, 60, 300),
      record("mac", "audio", 1200, 0, 300),
      record("win", "session", 600, 0, 420),
    ],
    [{ ...record("mac", "session", 300, 30, 540, 600), peakSentPerSec: 250 }],
  );
  const all = recordedSeries(read, back(360), () => true);
  assert.equal(all.stepSecs, 60);
  assert.equal(all.end, 600);
  assert.deepEqual(all.sent.points, [0, 120, 0, 10, 0, 5]);
  assert.deepEqual(all.received.points, [0, 1, 0, 0, 0, 0.5]);
  assert.equal(
    all.sent.busiest,
    6000,
    "one socket's busiest second, well above the averages the steps hold",
  );
  const audio = recordedSeries(read, back(360), (s) => s.socket === "audio");
  assert.deepEqual(audio.sent.points, [0, 20, 0, 0, 0, 0]);
  assert.equal(audio.sent.busiest, 1200);
  // A row that began before the range gives it only the part inside.
  const clipped = recordedSeries(
    report(600, [record("mac", "session", 6000, 0, 450, 510)]),
    back(120),
    () => true,
  );
  assert.deepEqual(clipped.sent.points, [50, 0]);
  // A range that is no whole number of steps begins at its cutoff all the same: the
  // oldest step averages over the seconds after it, and a row before it is left out.
  const ragged = recordedSeries(
    report(600, [
      record("mac", "session", 70_000, 0, 180, 240),
      record("mac", "session", 9000, 0, 240, 300),
      record("mac", "session", 6000, 0, 300, 360),
    ]),
    back(330),
    () => true,
  );
  assert.equal(ragged.spanSecs, 330);
  assert.deepEqual(ragged.sent.points, [150, 100, 0, 0, 0, 0]);
  assert.equal(ragged.sent.busiest, 9000);
});

test("a recorded range past the most points shares them between timeframes", () => {
  const span = MAX_GRAPH_POINTS * 60 * 3;
  const now = span + 1000;
  const shared = recordedSeries(
    report(now, [
      record("mac", "session", 18_000, 0, now - 180, now - 120),
      record("mac", "session", 18_000, 0, now - 60, now),
    ]),
    back(span),
    () => true,
  );
  assert.equal(shared.stepSecs, 180);
  assert.equal(shared.sent.points.length, MAX_GRAPH_POINTS);
  assert.equal(shared.sent.points.at(-1), 200);
  assert.equal(shared.sent.points.at(-2), 0);
});

test("everything kept reaches back to the oldest row, whatever the filters keep", () => {
  const read = report(1000, [
    record("win", "session", 600, 0, 400),
    record("mac", "session", 600, 0, 880),
  ]);
  const mac = recordedSeries(read, back(null), (s) => s.target === "mac");
  assert.equal(mac.sent.points.length, 10);
  assert.deepEqual(mac.sent.points.slice(-2), [10, 0]);
  const none = recordedSeries(report(1000, []), back(null), () => true);
  assert.deepEqual(none.sent, series([0, 0], 0));
});

test("the targets are those any sample or row saw, by label", () => {
  assert.deepEqual(
    throughputTargets(
      [
        sample(1, rate("win", "session", 1, 1)),
        sample(2, rate(null, "session", 1, 1), rate("mac", "audio", 1, 0)),
      ],
      [record("linux", "session", 1, 1), record("win", "audio", 1, 1)],
    ),
    ["linux", "mac", null, "win"],
  );
  assert.deepEqual(throughputTargets([], []), []);
  assert.equal(targetLabel(null), "No target (picker)");
  assert.equal(targetLabel("mac"), "mac");
});

test("a length of time is named in its largest unit", () => {
  assert.equal(spanLabel(45), "45 s");
  assert.equal(spanLabel(60), "1 min");
  assert.equal(spanLabel(300), "5 min");
  assert.equal(spanLabel(5400), "1.5 h");
  assert.equal(spanLabel(86_400 * 7), "7 d");
});

test("a range no longer than the seconds kept is drawn from them", () => {
  assert.ok(throughputRangeIsLive({ amount: 60, unit: "seconds" }));
  assert.ok(throughputRangeIsLive({ amount: 5, unit: "minutes" }));
  assert.equal(within({ amount: 5, unit: "minutes" }), LIVE_HISTORY_SECS);
  assert.ok(!throughputRangeIsLive({ amount: 15, unit: "minutes" }));
  assert.ok(!throughputRangeIsLive("all"));
});

test("a range asks for its length, and everything kept for no bound", () => {
  assert.equal(within({ amount: 60, unit: "seconds" }), 60);
  assert.equal(within({ amount: 15, unit: "minutes" }), 900);
  assert.equal(within({ amount: 1, unit: "hours" }), 3600);
  assert.equal(within({ amount: 24, unit: "hours" }), 86_400);
  assert.equal(within({ amount: 7, unit: "days" }), 604_800);
  assert.equal(within("all"), null);
});

test("the presets step up without a jump, each spelled by one key", () => {
  const seconds = THROUGHPUT_PRESETS.map((preset) => within(preset));
  assert.equal(seconds.at(-1), null, "everything kept comes last");
  const bounded = seconds.slice(0, -1) as number[];
  for (let i = 1; i < bounded.length; i++) {
    assert.ok(bounded[i] > bounded[i - 1], "ascending");
    assert.ok(bounded[i] <= bounded[i - 1] * 5, `no jump past 5x at ${i}`);
  }
  assert.equal(
    new Set(THROUGHPUT_PRESETS.map(throughputRangeKey)).size,
    THROUGHPUT_PRESETS.length,
  );
  assert.equal(throughputRangeKey({ amount: 24, unit: "hours" }), "24:hours");
  assert.equal(throughputRangeKey("all"), "all");
  assert.equal(
    throughputRangeLabel({ amount: 1, unit: "hours" }),
    "Last 1 hour",
  );
  assert.equal(
    throughputRangeLabel({ amount: 30, unit: "minutes" }),
    "Last 30 minutes",
  );
  assert.equal(throughputRangeLabel("all"), "Everything kept");
});

test("a window is a range between two times, read back from the recorded rows", () => {
  const range = { from: 1_700_000_000, to: 1_700_003_600 };
  assert.ok(isThroughputWindow(range));
  assert.ok(!isThroughputWindow("all"));
  assert.ok(!isThroughputWindow({ amount: 1, unit: "hours" }));
  assert.deepEqual(throughputBounds(range), {
    within: 3600,
    end: 1_700_003_600,
  });
  assert.ok(
    !throughputRangeIsLive({ from: 1_700_000_000, to: 1_700_000_060 }),
    "the seconds kept are this view's, not the clock's, however short the window",
  );
  assert.equal(throughputRangeKey(range), "window:1700000000:1700003600");
  const day = new Date(2026, 8, 17, 12, 0).getTime() / 1000;
  const inside = throughputRangeLabel({ from: day, to: day + 3600 });
  assert.ok(inside.includes(" – "));
  assert.ok(
    throughputRangeLabel({ from: day, to: day + 86_400 }).length >
      inside.length,
    "a window that crosses a day names the day at both ends",
  );
});

test("a read asks by length while the range ends now, and by both ends when it does not", () => {
  assert.equal(throughputQuery({ within: 900, end: null }), "?within=900");
  assert.equal(throughputQuery({ within: null, end: null }), "");
  assert.equal(
    throughputQuery(throughputBounds({ from: 1000, to: 4600 })),
    "?from=1000&to=4600",
  );
  assert.equal(throughputQuery({ within: null, end: 4600 }), "?from=0&to=4600");
});

test("a window is drawn between its own times, and never past the read", () => {
  const read = report(600, [
    record("mac", "session", 6000, 0, 120, 180),
    record("mac", "session", 600, 0, 300, 360),
  ]);
  const inside = recordedSeries(
    read,
    throughputBounds({ from: 120, to: 420 }),
    () => true,
  );
  assert.equal(inside.end, 420);
  assert.equal(inside.spanSecs, 300);
  assert.deepEqual(inside.sent.points, [100, 0, 0, 10, 0]);
  // A window reaching past the read stops there, keeping the start it names.
  const reaching = recordedSeries(
    read,
    throughputBounds({ from: 300, to: 900 }),
    () => true,
  );
  assert.equal(reaching.end, 600);
  assert.equal(reaching.spanSecs, 300);
  assert.deepEqual(reaching.sent.points, [10, 0, 0, 0, 0]);
});

test("a window is typed as two of this browser's minutes", () => {
  const noon = new Date(2026, 8, 17, 12, 34).getTime() / 1000;
  assert.equal(localInputValue(noon), "2026-09-17T12:34");
  assert.equal(parseLocalInput("2026-09-17T12:34"), noon);
  assert.equal(parseLocalInput(localInputValue(noon + 30)), noon, "the minute");
  assert.equal(parseLocalInput(""), null);
  assert.equal(parseLocalInput("2026-09-17"), null, "no time of day");
  assert.equal(parseLocalInput("2026-09-17T12"), null, "half typed");
  const hour = new Date(2026, 8, 17, 12, 0).getTime() / 1000;
  assert.deepEqual(
    customThroughputWindow("2026-09-17T12:00", "2026-09-17T13:00"),
    { from: hour, to: hour + 3600 },
  );
  assert.equal(
    customThroughputWindow("2026-09-17T13:00", "2026-09-17T12:00"),
    null,
    "it ends before it begins",
  );
  assert.equal(
    customThroughputWindow("2026-09-17T12:00", "2026-09-17T12:00"),
    null,
    "no time at all",
  );
  assert.equal(customThroughputWindow("", "2026-09-17T12:00"), null);
});

test("a custom range takes a whole number of at least one", () => {
  assert.deepEqual(customThroughputRange("90", "minutes"), {
    amount: 90,
    unit: "minutes",
  });
  assert.deepEqual(customThroughputRange(" 2 ", "days"), {
    amount: 2,
    unit: "days",
  });
  assert.equal(customThroughputRange("0", "hours"), null);
  assert.equal(customThroughputRange("", "hours"), null);
  assert.equal(customThroughputRange("1.5", "hours"), null);
  assert.equal(customThroughputRange("-3", "hours"), null);
  assert.equal(customThroughputRange("abc", "hours"), null);
});
