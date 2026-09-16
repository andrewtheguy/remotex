// The browser sockets' recorded data usage, read from `GET /api/usage` when the
// "Data usage" view asks for it.
//
// The gateway samples its counters once a second: the last sample is the rate right
// now, read from `GET /api/usage/live` each second while the view is open and kept
// here as the seconds the graph draws, and each written timeframe carries its busiest
// second. The recorded rows are read only on demand, with the timeframe still being
// counted. The shapes mirror `crate::usage` and the handlers' responses in
// src/server.rs. Rates are shown in bits per second, the way a network meter does, and
// an average is derived here: a row's bytes over the seconds of its timeframe.

import { gatewayFetch } from "./gateway.ts";

export type UsageSocket = "session" | "audio" | "camera" | "mic";

export const USAGE_SOCKETS: readonly UsageSocket[] = [
  "session",
  "audio",
  "camera",
  "mic",
];

export const USAGE_SOCKET_LABEL: Record<UsageSocket, string> = {
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
export interface UsageRecord {
  target: string | null;
  socket: UsageSocket;
  start: number;
  end: number;
  sentBytes: number;
  receivedBytes: number;
  peakSentPerSec: number;
  peakReceivedPerSec: number;
}

/** What one target's socket moved in one sampled second, in bytes per second. */
export interface LiveRate {
  target: string | null;
  socket: UsageSocket;
  sentPerSec: number;
  receivedPerSec: number;
}

/** The rate right now: what the gateway's last one-second sample found moving. */
export interface UsageLive {
  /** When the sample was taken, in the gateway's Unix seconds. */
  at: number;
  /** Each target's socket that moved; the rest moved nothing. */
  rates: LiveRate[];
}

export interface UsageReport {
  /** The gateway's clock at the read, in Unix seconds, which `open` ends at. */
  now: number;
  intervalSecs: number;
  maxRecords: number;
  /** The written timeframes, oldest first. */
  records: UsageRecord[];
  /** The timeframe still being counted, as it stood at `now`. */
  open: UsageRecord[];
}

export type UsageUnit = "minutes" | "hours" | "days";

export const USAGE_UNITS: readonly UsageUnit[] = ["minutes", "hours", "days"];

const UNIT_SECONDS: Record<UsageUnit, number> = {
  minutes: 60,
  hours: 3600,
  days: 86_400,
};

/** The last `amount` of `unit`. */
export interface UsageSpan {
  amount: number;
  unit: UsageUnit;
}

/** How far back the view reads: a span back from now, or everything kept. */
export type UsageRange = UsageSpan | "all";

/** The ranges the select offers before "Custom"; any other range is custom. */
export const USAGE_PRESETS: readonly UsageRange[] = [
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

export const DEFAULT_USAGE_RANGE: UsageSpan = { amount: 24, unit: "hours" };

/** The select's value for a range: the same range spells the same key. */
export function usageRangeKey(range: UsageRange): string {
  return range === "all" ? "all" : `${range.amount}:${range.unit}`;
}

export function usageRangeLabel(range: UsageRange): string {
  if (range === "all") {
    return "Everything kept";
  }
  const unit = range.amount === 1 ? range.unit.slice(0, -1) : range.unit;
  return `Last ${range.amount} ${unit}`;
}

/**
 * A custom range from the amount typed and the unit chosen, or `null` when the amount
 * is not a whole number of at least one.
 */
export function customUsageRange(
  amount: string,
  unit: UsageUnit,
): UsageRange | null {
  if (!/^\d+$/.test(amount.trim())) {
    return null;
  }
  const whole = Number(amount);
  return whole >= 1 && Number.isSafeInteger(whole)
    ? { amount: whole, unit }
    : null;
}

/**
 * The seconds a range reads back, or `null` for everything kept. The gateway counts
 * them back from its own clock, which the records were stamped with.
 */
export function usageWithin(range: UsageRange): number | null {
  return range === "all" ? null : range.amount * UNIT_SECONDS[range.unit];
}

export type UsageResult =
  | { kind: "ok"; report: UsageReport }
  | { kind: "unauthorized" }
  | { kind: "error"; message: string };

export async function fetchUsage(within: number | null): Promise<UsageResult> {
  try {
    const res = await gatewayFetch(
      within === null ? "/api/usage" : `/api/usage?within=${within}`,
    );
    if (res.status === 401) {
      return { kind: "unauthorized" };
    }
    if (res.status === 404) {
      return {
        kind: "error",
        message: "This gateway is not recording data usage",
      };
    }
    if (!res.ok) {
      return {
        kind: "error",
        message: `Could not load usage (HTTP ${res.status})`,
      };
    }
    return { kind: "ok", report: (await res.json()) as UsageReport };
  } catch {
    return { kind: "error", message: "Could not load usage" };
  }
}

export type UsageLiveResult =
  | { kind: "ok"; live: UsageLive }
  | { kind: "unauthorized" }
  | { kind: "error" };

export async function fetchUsageLive(): Promise<UsageLiveResult> {
  try {
    const res = await gatewayFetch("/api/usage/live");
    if (res.status === 401) {
      return { kind: "unauthorized" };
    }
    if (!res.ok) {
      return { kind: "error" };
    }
    return { kind: "ok", live: (await res.json()) as UsageLive };
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

/** The spans the graph draws, in seconds: the last minute and the last five. */
export const GRAPH_WINDOWS: readonly number[] = [60, 300];

/** How many seconds of samples the view keeps: enough for the longest window. */
export const LIVE_HISTORY_SECS = 300;

export function graphWindowLabel(windowSecs: number): string {
  return windowSecs % 60 === 0 && windowSecs > 60
    ? `last ${windowSecs / 60} minutes`
    : `last ${windowSecs} seconds`;
}

/** How long ago the graph's left edge is, for its axis. */
export function agoLabel(windowSecs: number): string {
  return windowSecs % 60 === 0 && windowSecs > 60
    ? `${windowSecs / 60} min ago`
    : `${windowSecs} s ago`;
}

/**
 * `history` with `live` as its newest sample: oldest first, one per second, and no
 * longer than the view keeps. A second already kept — the poll came round before the
 * gateway's next sample — leaves the history as it was, and so does a sample from an
 * earlier second.
 */
export function appendLive(
  history: readonly UsageLive[],
  live: UsageLive,
): readonly UsageLive[] {
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
 * One direction over a window: a rate per second, oldest first, `null` for a second
 * no sample was read in, and the busiest second among them in bytes per second.
 */
export interface RateSeries {
  points: (number | null)[];
  peak: number;
}

/**
 * Each direction over the last `windowSecs` seconds up to the newest sample, summed
 * over the rates `keep` admits — a target, a socket, or all of them. A second nothing
 * kept moved in is zero; a second with no sample at all is a gap. No history draws a
 * window of gaps.
 */
export function liveSeries(
  history: readonly UsageLive[],
  windowSecs: number,
  keep: (rate: LiveRate) => boolean,
): { sent: RateSeries; received: RateSeries } {
  const sent: (number | null)[] = new Array(windowSecs).fill(null);
  const received: (number | null)[] = new Array(windowSecs).fill(null);
  const last = history.at(-1);
  if (last !== undefined) {
    const first = last.at - windowSecs + 1;
    for (const sample of history) {
      const i = sample.at - first;
      if (i >= 0) {
        const totals = liveTotals(sample.rates.filter(keep));
        sent[i] = totals.sent;
        received[i] = totals.received;
      }
    }
  }
  const peak = (points: (number | null)[]) =>
    points.reduce<number>((max, p) => (p === null ? max : Math.max(max, p)), 0);
  return {
    sent: { points: sent, peak: peak(sent) },
    received: { points: received, peak: peak(received) },
  };
}

/**
 * The top of a graph's scale for a busiest second in bytes per second: a round number
 * of bits per second — 1, 2, 2.5 or 5 of a power of ten — at least a tenth above the
 * peak, and never below one kilobit per second, so nothing moved is not a scale of
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

/** Every target some kept sample saw moving, by label. */
export function liveTargets(history: readonly UsageLive[]): (string | null)[] {
  const targets = new Map<string, string | null>();
  for (const sample of history) {
    for (const rate of sample.rates) {
      targets.set(targetLabel(rate.target), rate.target);
    }
  }
  return [...targets]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([, target]) => target);
}

/** `bytes` over `seconds` as a rate, or `null` for no time at all. */
export function usageRate(bytes: number, seconds: number): number | null {
  return seconds > 0 ? bytes / seconds : null;
}

/**
 * Bytes summed over some rows, the seconds their timeframes span, and the busiest
 * second among them in bytes per second. An average rate is `sent` or `received` over
 * `seconds`. Rows of different sockets in one timeframe span it once, so a target busy
 * on four sockets for a minute spans one minute, and a timeframe nothing moved in is
 * not in the rows and so not in the seconds — the average is the rate while data
 * moved, not over the whole range. A peak is one row's: the busiest second of one
 * socket, never two sockets' seconds added together.
 */
export interface UsageTotals {
  sent: number;
  received: number;
  seconds: number;
  peakSent: number;
  peakReceived: number;
}

class TotalsBuilder {
  sent = 0;
  received = 0;
  peakSent = 0;
  peakReceived = 0;
  private readonly timeframes = new Map<number, number>();

  add(record: UsageRecord): void {
    this.sent += record.sentBytes;
    this.received += record.receivedBytes;
    this.peakSent = Math.max(this.peakSent, record.peakSentPerSec);
    this.peakReceived = Math.max(this.peakReceived, record.peakReceivedPerSec);
    this.timeframes.set(record.start, record.end - record.start);
  }

  totals(): UsageTotals {
    let seconds = 0;
    for (const length of this.timeframes.values()) {
      seconds += length;
    }
    return {
      sent: this.sent,
      received: this.received,
      seconds,
      peakSent: this.peakSent,
      peakReceived: this.peakReceived,
    };
  }
}

/** Each socket's totals over `records`, plus every socket together as `all`. */
export function usageTotals(
  records: readonly UsageRecord[],
): Record<UsageSocket | "all", UsageTotals> {
  const builders = {
    session: new TotalsBuilder(),
    audio: new TotalsBuilder(),
    camera: new TotalsBuilder(),
    mic: new TotalsBuilder(),
    all: new TotalsBuilder(),
  };
  for (const record of records) {
    builders[record.socket].add(record);
    builders.all.add(record);
  }
  return {
    session: builders.session.totals(),
    audio: builders.audio.totals(),
    camera: builders.camera.totals(),
    mic: builders.mic.totals(),
    all: builders.all.totals(),
  };
}

export function targetLabel(target: string | null): string {
  return target ?? "No target (picker)";
}

export interface TargetUsage extends UsageTotals {
  target: string | null;
}

/** Every target's totals over all its sockets, busiest first. */
export function usageByTarget(records: readonly UsageRecord[]): TargetUsage[] {
  const byTarget = new Map<string | null, TotalsBuilder>();
  for (const record of records) {
    let builder = byTarget.get(record.target);
    if (!builder) {
      builder = new TotalsBuilder();
      byTarget.set(record.target, builder);
    }
    builder.add(record);
  }
  return [...byTarget]
    .map(([target, builder]) => ({ target, ...builder.totals() }))
    .sort(
      (a, b) =>
        b.sent + b.received - (a.sent + a.received) ||
        targetLabel(a.target).localeCompare(targetLabel(b.target)),
    );
}
