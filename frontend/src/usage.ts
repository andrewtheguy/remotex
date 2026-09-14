// The browser sockets' recorded data usage, read from `GET /api/usage` when the
// "Data usage" view asks for it.
//
// Nothing here polls: the gateway writes a row per socket per timeframe, and the page
// reads them only on demand. The shapes mirror `crate::usage::Record` and the handler's
// response in src/server.rs.

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

/** What one socket moved in one timeframe. Times are Unix seconds. */
export interface UsageRecord {
  socket: UsageSocket;
  start: number;
  end: number;
  sentBytes: number;
  receivedBytes: number;
}

export interface UsageReport {
  intervalSecs: number;
  maxRecords: number;
  /** Oldest first. */
  records: UsageRecord[];
}

export type UsageRange = "hour" | "day" | "week" | "all";

export const USAGE_RANGES: readonly {
  id: UsageRange;
  label: string;
  seconds: number | null;
}[] = [
  { id: "hour", label: "Last hour", seconds: 3600 },
  { id: "day", label: "Last 24 hours", seconds: 86_400 },
  { id: "week", label: "Last 7 days", seconds: 604_800 },
  { id: "all", label: "Everything kept", seconds: null },
];

/** The `since` a range asks the gateway for, from `nowSecs` (Unix seconds). */
export function usageSince(range: UsageRange, nowSecs: number): number {
  const seconds = USAGE_RANGES.find((r) => r.id === range)?.seconds ?? null;
  return seconds === null ? 0 : Math.max(0, Math.floor(nowSecs) - seconds);
}

export type UsageResult =
  | { kind: "ok"; report: UsageReport }
  | { kind: "unauthorized" }
  | { kind: "error"; message: string };

export async function fetchUsage(since: number): Promise<UsageResult> {
  try {
    const res = await gatewayFetch(`/api/usage?since=${since}`);
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

const UNITS = ["B", "KB", "MB", "GB", "TB"];

/** Bytes in binary units (1 KB = 1024 B), one decimal below 10 of a unit. */
export function formatBytes(bytes: number): string {
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  if (unit === 0) {
    return `${value} B`;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${UNITS[unit]}`;
}

export interface UsageTotals {
  sent: number;
  received: number;
}

/** Each socket's sums over `records`, plus the sum of every socket as `all`. */
export function usageTotals(
  records: readonly UsageRecord[],
): Record<UsageSocket | "all", UsageTotals> {
  const totals = {
    session: { sent: 0, received: 0 },
    audio: { sent: 0, received: 0 },
    camera: { sent: 0, received: 0 },
    mic: { sent: 0, received: 0 },
    all: { sent: 0, received: 0 },
  };
  for (const record of records) {
    for (const key of [record.socket, "all"] as const) {
      totals[key].sent += record.sentBytes;
      totals[key].received += record.receivedBytes;
    }
  }
  return totals;
}
