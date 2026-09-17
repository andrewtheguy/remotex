// The browser sockets' throughput, as the "Throughput" view graphs it over a range.
//
// The gateway samples its counters once a second: the last sample is the rate right
// now, read from `GET /api/throughput/live` each second while the view is open and kept
// here as the seconds a short range draws. A range longer than the seconds kept is
// drawn from the recorded rows instead, read from `GET /api/throughput` with the timeframe
// still being counted; each row carries its busiest second. The shapes mirror
// `crate::throughput` and the handlers' responses in src/server.rs. Rates are shown in bits
// per second, the way a network meter does, and an average is derived here: bytes over
// the seconds they moved in.

import { gatewayFetch } from "./gateway.ts";

export type ThroughputSocket = "session" | "audio" | "camera" | "mic";

export const THROUGHPUT_SOCKETS: readonly ThroughputSocket[] = [
  "session",
  "audio",
  "camera",
  "mic",
];

export const THROUGHPUT_SOCKET_LABEL: Record<ThroughputSocket, string> = {
  session: "Session",
  audio: "Audio",
  camera: "Camera",
  mic: "Microphone",
};

/**
 * What one socket moved for one target in one timeframe, and its busiest second in
 * bytes per second. Times are Unix seconds; a `null` target is the picker, where the
 * browser sat with no target selected.
 */
export interface ThroughputRecord {
  target: string | null;
  socket: ThroughputSocket;
  start: number;
  end: number;
  sentBytes: number;
  receivedBytes: number;
  peakSentPerSec: number;
  peakReceivedPerSec: number;
}

/** Whose bytes a row or a rate counts: what the view's filters choose by. */
export interface ThroughputSource {
  target: string | null;
  socket: ThroughputSocket;
}

/** What one target's socket moved in one sampled second, in bytes per second. */
export interface LiveRate extends ThroughputSource {
  sentPerSec: number;
  receivedPerSec: number;
}

/** The rate right now: what the gateway's last one-second sample found moving. */
export interface ThroughputLive {
  /** When the sample was taken, in the gateway's Unix seconds. */
  at: number;
  /** Each target's socket that moved; the rest moved nothing. */
  rates: LiveRate[];
}

export interface ThroughputReport {
  /** The gateway's clock at the read, in Unix seconds, which `open` ends at. */
  now: number;
  intervalSecs: number;
  maxRecords: number;
  /** The written timeframes, oldest first. */
  records: ThroughputRecord[];
  /** The timeframe still being counted, as it stood at `now`. */
  open: ThroughputRecord[];
}

export type ThroughputUnit = "seconds" | "minutes" | "hours" | "days";

export const THROUGHPUT_UNITS: readonly ThroughputUnit[] = [
  "seconds",
  "minutes",
  "hours",
  "days",
];

const UNIT_SECONDS: Record<ThroughputUnit, number> = {
  seconds: 1,
  minutes: 60,
  hours: 3600,
  days: 86_400,
};

/** The last `amount` of `unit`. */
export interface ThroughputSpan {
  amount: number;
  unit: ThroughputUnit;
}

/**
 * A range the page names outright, in Unix seconds on the gateway's clock: from `from`
 * up to, but not into, `to`.
 */
export interface ThroughputWindow {
  from: number;
  to: number;
}

/**
 * How far back the graph reaches: a span back from now, everything kept, or a window
 * between two times.
 */
export type ThroughputRange = ThroughputSpan | "all" | ThroughputWindow;

export function isThroughputWindow(
  range: ThroughputRange,
): range is ThroughputWindow {
  return range !== "all" && "from" in range;
}

/** The ranges the select offers before "Custom"; any other range is custom. */
export const THROUGHPUT_PRESETS: readonly ThroughputRange[] = [
  { amount: 60, unit: "seconds" },
  { amount: 5, unit: "minutes" },
  { amount: 15, unit: "minutes" },
  { amount: 30, unit: "minutes" },
  { amount: 1, unit: "hours" },
  { amount: 3, unit: "hours" },
  { amount: 6, unit: "hours" },
  { amount: 12, unit: "hours" },
  { amount: 24, unit: "hours" },
  { amount: 3, unit: "days" },
  { amount: 7, unit: "days" },
  { amount: 14, unit: "days" },
  { amount: 30, unit: "days" },
  "all",
];

export const DEFAULT_THROUGHPUT_RANGE: ThroughputSpan = {
  amount: 60,
  unit: "seconds",
};

/** The select's value for a range: the same range spells the same key. */
export function throughputRangeKey(range: ThroughputRange): string {
  if (range === "all") {
    return "all";
  }
  return isThroughputWindow(range)
    ? `window:${range.from}:${range.to}`
    : `${range.amount}:${range.unit}`;
}

/** A Unix second as a local time, with the day before it where the range needs one. */
export function timeLabel(unixSecs: number, withDay: boolean): string {
  return new Date(unixSecs * 1000).toLocaleString(
    [],
    withDay
      ? { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" }
      : { hour: "numeric", minute: "2-digit" },
  );
}

export function throughputRangeLabel(range: ThroughputRange): string {
  if (range === "all") {
    return "Everything kept";
  }
  if (isThroughputWindow(range)) {
    const day = (at: number) => new Date(at * 1000).toDateString();
    const crosses = day(range.from) !== day(range.to);
    return `${timeLabel(range.from, true)} – ${timeLabel(range.to, crosses)}`;
  }
  const unit = range.amount === 1 ? range.unit.slice(0, -1) : range.unit;
  return `Last ${range.amount} ${unit}`;
}

/**
 * A custom range from the amount typed and the unit chosen, or `null` when the amount
 * is not a whole number of at least one.
 */
export function customThroughputRange(
  amount: string,
  unit: ThroughputUnit,
): ThroughputRange | null {
  if (!/^\d+$/.test(amount.trim())) {
    return null;
  }
  const whole = Number(amount);
  return whole >= 1 && Number.isSafeInteger(whole)
    ? { amount: whole, unit }
    : null;
}

/** A Unix second as a `datetime-local` input's value, in this browser's time zone. */
export function localInputValue(unixSecs: number): string {
  const at = new Date(unixSecs * 1000);
  const pad = (part: number) => String(part).padStart(2, "0");
  const day = `${at.getFullYear()}-${pad(at.getMonth() + 1)}-${pad(at.getDate())}`;
  return `${day}T${pad(at.getHours())}:${pad(at.getMinutes())}`;
}

/**
 * A `datetime-local` value as a Unix second, or `null` when it names no minute — the
 * input is empty, or half typed. The time is this browser's, as the input means it.
 */
export function parseLocalInput(value: string): number | null {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(value)) {
    return null;
  }
  const at = new Date(value).getTime();
  return Number.isNaN(at) ? null : Math.floor(at / 1000);
}

/**
 * A window from the two times typed, or `null` unless both name a minute and the second
 * comes after the first.
 */
export function customThroughputWindow(
  from: string,
  to: string,
): ThroughputRange | null {
  const begins = parseLocalInput(from);
  const ends = parseLocalInput(to);
  return begins !== null && ends !== null && ends > begins
    ? { from: begins, to: ends }
    : null;
}

/**
 * What a range comes to at a read: the seconds it covers, `null` for everything kept,
 * and the second it ends at, `null` for whenever the read lands.
 */
export interface ThroughputBounds {
  within: number | null;
  end: number | null;
}

export function throughputBounds(range: ThroughputRange): ThroughputBounds {
  if (range === "all") {
    return { within: null, end: null };
  }
  return isThroughputWindow(range)
    ? { within: range.to - range.from, end: range.to }
    : { within: range.amount * UNIT_SECONDS[range.unit], end: null };
}

/**
 * The query a read of `bounds` asks with: a length alone for a range that ends at the
 * read, so the gateway counts it back from its own clock and this browser's never moves
 * it, and both ends outright for one that ends earlier, which only a clock can name.
 */
export function throughputQuery(bounds: ThroughputBounds): string {
  const { within, end } = bounds;
  if (end === null) {
    return within === null ? "" : `?within=${within}`;
  }
  return `?from=${within === null ? 0 : end - within}&to=${end}`;
}

export type ThroughputResult =
  | { kind: "ok"; report: ThroughputReport }
  | { kind: "unauthorized" }
  | { kind: "error"; message: string };

export async function fetchThroughput(
  bounds: ThroughputBounds,
): Promise<ThroughputResult> {
  try {
    const res = await gatewayFetch(`/api/throughput${throughputQuery(bounds)}`);
    if (res.status === 401) {
      return { kind: "unauthorized" };
    }
    if (res.status === 404) {
      return {
        kind: "error",
        message: "This gateway is not recording throughput",
      };
    }
    if (!res.ok) {
      return {
        kind: "error",
        message: `Could not load throughput (HTTP ${res.status})`,
      };
    }
    return { kind: "ok", report: (await res.json()) as ThroughputReport };
  } catch {
    return { kind: "error", message: "Could not load throughput" };
  }
}

export type ThroughputLiveResult =
  | { kind: "ok"; live: ThroughputLive }
  | { kind: "unauthorized" }
  | { kind: "error" };

export async function fetchThroughputLive(): Promise<ThroughputLiveResult> {
  try {
    const res = await gatewayFetch("/api/throughput/live");
    if (res.status === 401) {
      return { kind: "unauthorized" };
    }
    if (!res.ok) {
      return { kind: "error" };
    }
    return { kind: "ok", live: (await res.json()) as ThroughputLive };
  } catch {
    return { kind: "error" };
  }
}

const BIT_UNITS = ["bps", "kbps", "Mbps", "Gbps", "Tbps"];

/**
 * A rate given in bytes per second, shown in bits per second in decimal units
 * (1 kbps = 1000 bps) the way a network meter shows it, one decimal below 10 of a unit.
 */
export function formatRate(bytesPerSecond: number): string {
  let value = bytesPerSecond * 8;
  let unit = 0;
  while (value >= 1000 && unit < BIT_UNITS.length - 1) {
    value /= 1000;
    unit += 1;
  }
  if (unit === 0) {
    return `${Math.round(value)} ${BIT_UNITS[unit]}`;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${BIT_UNITS[unit]}`;
}

/** The rate right now summed over `rates`, in bytes per second. */
export function liveTotals(rates: readonly LiveRate[]): {
  sent: number;
  received: number;
} {
  let sent = 0;
  let received = 0;
  for (const rate of rates) {
    sent += rate.sentPerSec;
    received += rate.receivedPerSec;
  }
  return { sent, received };
}

/**
 * How many seconds of samples the view keeps. A range no longer than this is drawn
 * from them, second by second; a longer one from the recorded timeframes.
 */
export const LIVE_HISTORY_SECS = 300;

/**
 * Whether a range is drawn from the sampled seconds rather than the recorded rows. A
 * window is never: the seconds kept are the last five minutes of this view's life, not
 * of any clock, so a range that names its own times is read back from the rows however
 * short it is.
 */
export function throughputRangeIsLive(range: ThroughputRange): boolean {
  if (isThroughputWindow(range)) {
    return false;
  }
  const within = throughputBounds(range).within;
  return within !== null && within <= LIVE_HISTORY_SECS;
}

/** A length of time in its largest unit, to one decimal: "45 s", "5 min", "1.5 h". */
export function spanLabel(secs: number): string {
  for (const [unit, name] of [
    [86_400, "d"],
    [3600, "h"],
    [60, "min"],
  ] as const) {
    if (secs >= unit) {
      return `${Math.round((secs / unit) * 10) / 10} ${name}`;
    }
  }
  return `${Math.round(secs)} s`;
}

/**
 * `history` with `live` as its newest sample: oldest first, one per second, and no
 * longer than the view keeps. A second already kept — the poll came round before the
 * gateway's next sample — leaves the history as it was, and so does a sample from an
 * earlier second.
 */
export function appendLive(
  history: readonly ThroughputLive[],
  live: ThroughputLive,
): readonly ThroughputLive[] {
  const last = history.at(-1);
  if (last !== undefined && live.at <= last.at) {
    return history;
  }
  const next = [...history, live];
  return next.length > LIVE_HISTORY_SECS
    ? next.slice(-LIVE_HISTORY_SECS)
    : next;
}

/**
 * One direction over a range: a rate per step, oldest first, `null` for a step nothing
 * was read in, and `busiest`, the busiest second in the range, all in bytes per second.
 * The scale fits `busiest`, so the graph is drawn under the peak it names rather than
 * under the highest step: second by second the two are one number, but over recorded
 * timeframes a step is an average and the busiest second stands above it.
 */
export interface RateSeries {
  points: (number | null)[];
  busiest: number;
}

/** Both directions over one range, and how it is laid out in time. */
export interface ThroughputSeries {
  sent: RateSeries;
  received: RateSeries;
  /** The seconds one point spans. */
  stepSecs: number;
  /** The seconds the range reaches back from `end`. */
  spanSecs: number;
  /** The gateway's second the last point ends at, or `null` before any read. */
  end: number | null;
}

function highest(points: readonly (number | null)[]): number {
  return points.reduce<number>(
    (max, p) => (p === null ? max : Math.max(max, p)),
    0,
  );
}

/**
 * Each direction over the last `windowSecs` seconds up to `now` — the gateway's second
 * the graph's right edge stands at, which a read that fails still moves on, so the
 * newest sample slides left and gaps take its place — summed over the rates `keep`
 * admits: a target, a socket, or all of them. A second nothing kept moved in is zero; a
 * second with no sample is a gap, and so is the whole window before the first read.
 */
export function liveSeries(
  history: readonly ThroughputLive[],
  windowSecs: number,
  keep: (source: ThroughputSource) => boolean,
  now: number | null,
): ThroughputSeries {
  const sent: (number | null)[] = new Array(windowSecs).fill(null);
  const received: (number | null)[] = new Array(windowSecs).fill(null);
  if (now !== null) {
    const first = now - windowSecs + 1;
    for (const sample of history) {
      const i = sample.at - first;
      if (i >= 0 && i < windowSecs) {
        const totals = liveTotals(sample.rates.filter(keep));
        sent[i] = totals.sent;
        received[i] = totals.received;
      }
    }
  }
  const series = (points: (number | null)[]): RateSeries => ({
    points,
    busiest: highest(points),
  });
  return {
    sent: series(sent),
    received: series(received),
    stepSecs: 1,
    spanSecs: windowSecs,
    end: now,
  };
}

/** The most points a recorded range is drawn with; past it, timeframes share one. */
export const MAX_GRAPH_POINTS = 600;

/**
 * Each direction of a report over `bounds` — the seconds the range covers, ending where
 * it ends or at the gateway's clock at the read — from the rows `keep` admits with the
 * open timeframe among them. A point is the average over its step: one
 * timeframe, or as many as it takes to stay within `MAX_GRAPH_POINTS`, a row's bytes
 * shared between the steps it overlaps. The range begins at its cutoff exactly: the
 * part of a row before it is left out, and the oldest step, which the cutoff may fall
 * inside, averages over the seconds it has after it. A timeframe nothing moved in has
 * no row, so a step with none is zero and a recorded range has no gaps. The busiest second is one
 * row's: one socket's, never two sockets' seconds added together.
 */
export function recordedSeries(
  report: ThroughputReport,
  bounds: ThroughputBounds,
  keep: (source: ThroughputSource) => boolean,
): ThroughputSeries {
  const rows = [...report.records, ...report.open];
  const interval = Math.max(1, report.intervalSecs);
  // Never past the read: nothing is recorded after it, so a range that reaches into the
  // future is drawn up to it and no further.
  const end = Math.min(bounds.end ?? report.now, report.now);
  let span: number;
  if (bounds.within === null) {
    span = Math.max(
      interval,
      end - rows.reduce((min, r) => Math.min(min, r.start), end),
    );
  } else if (bounds.end === null) {
    span = bounds.within;
  } else {
    span = Math.max(1, end - (bounds.end - bounds.within));
  }
  const timeframes = Math.ceil(span / interval);
  const stepSecs = interval * Math.ceil(timeframes / MAX_GRAPH_POINTS);
  const count = Math.max(2, Math.ceil(span / stepSecs));
  const first = end - count * stepSecs;
  const cutoff = end - span;
  const sent = new Array<number>(count).fill(0);
  const received = new Array<number>(count).fill(0);
  let busiestSent = 0;
  let busiestReceived = 0;
  for (const row of rows) {
    if (!keep(row) || row.end <= cutoff) {
      continue;
    }
    busiestSent = Math.max(busiestSent, row.peakSentPerSec);
    busiestReceived = Math.max(busiestReceived, row.peakReceivedPerSec);
    const length = row.end - row.start;
    // A timeframe that has only just opened spans no time yet: its bytes are its
    // step's.
    const last = count - 1;
    const from = Math.min(
      last,
      Math.max(0, Math.floor((row.start - first) / stepSecs)),
    );
    const to = Math.min(
      last,
      Math.max(from, Math.floor((row.end - 1 - first) / stepSecs)),
    );
    for (let i = from; i <= to; i++) {
      const stepStart = first + i * stepSecs;
      const stepFrom = Math.max(stepStart, cutoff);
      const stepEnd = stepStart + stepSecs;
      const overlap =
        Math.min(row.end, stepEnd) - Math.max(row.start, stepFrom);
      const share = length > 0 ? Math.max(0, overlap) / length : 1;
      sent[i] += (row.sentBytes * share) / (stepEnd - stepFrom);
      received[i] += (row.receivedBytes * share) / (stepEnd - stepFrom);
    }
  }
  const series = (points: number[], busiest: number): RateSeries => ({
    points,
    busiest: Math.max(busiest, highest(points)),
  });
  return {
    sent: series(sent, busiestSent),
    received: series(received, busiestReceived),
    stepSecs,
    spanSecs: span,
    end,
  };
}

/**
 * The top of a graph's scale for a busiest second in bytes per second: a round number
 * of bits per second — 1, 2, 2.5 or 5 of a power of ten — at least a tenth above the
 * second, and never below one kilobit per second, so nothing moved is not a scale of
 * nothing.
 */
export function rateScale(peakBytesPerSec: number): number {
  const bits = Math.max(1000, peakBytesPerSec * 8 * 1.1);
  const power = 10 ** Math.floor(Math.log10(bits));
  for (const step of [1, 2, 2.5, 5, 10]) {
    if (step * power >= bits) {
      return (step * power) / 8;
    }
  }
  return (10 * power) / 8;
}

/**
 * The gateway's second right now, as far as the page can tell: the last sample's second
 * plus the whole seconds since it was read, `anchor.wall` being this browser's clock at
 * the read. Between samples, and while reads fail, the graph moves on by this.
 */
export function clockNow(
  anchor: { at: number; wall: number } | null,
  wall: number,
): number | null {
  return anchor === null
    ? null
    : anchor.at + Math.max(0, Math.round((wall - anchor.wall) / 1000));
}

/** Every target some kept sample or some row saw moving, by label. */
export function throughputTargets(
  history: readonly ThroughputLive[],
  rows: readonly ThroughputRecord[],
): (string | null)[] {
  const targets = new Map<string, string | null>();
  for (const sample of history) {
    for (const rate of sample.rates) {
      targets.set(targetLabel(rate.target), rate.target);
    }
  }
  for (const row of rows) {
    targets.set(targetLabel(row.target), row.target);
  }
  return [...targets]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([, target]) => target);
}

export function targetLabel(target: string | null): string {
  return target ?? "No target (picker)";
}
