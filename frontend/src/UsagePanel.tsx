import { useCallback, useEffect, useRef, useState } from "react";
import { gatewayConfig } from "./gatewayConfig.ts";
import {
  fetchUsage,
  formatBytes,
  type TargetUsage,
  targetLabel,
  USAGE_RANGES,
  USAGE_SOCKET_LABEL,
  USAGE_SOCKETS,
  type UsageRange,
  type UsageRecord,
  type UsageReport,
  usageByTarget,
  usageTotals,
  usageWithin,
} from "./usage.ts";

// The "Data usage" view, opened from the target picker and from the session's Info
// card, which it replaces while open; `closeLabel` names where its button returns to.
// It reads the recorded rows when it opens, when the range changes and when Refresh
// is pressed — never on a timer. See usage.ts.
//
// Usage is compared by target first: one table sums each target over all its sockets,
// and the target filter narrows the socket table and the timeframes to one of them.

/// Whether to offer the view at all: only a gateway with `[usage]` records any.
export function useUsageAvailable(): boolean {
  const [available, setAvailable] = useState(false);
  useEffect(() => {
    let cancelled = false;
    gatewayConfig().then(({ usage }) => {
      if (!cancelled) {
        setAvailable(usage);
      }
    });
    return () => {
      cancelled = true;
    };
  }, []);
  return available;
}

/// Timeframes listed below the totals, newest first. The totals cover every row.
const LISTED_TIMEFRAMES = 200;

/// The filter select's value for a target: `null` (the picker) cannot be an option
/// value, and a prefix keeps a target named "all" apart from the "all" choice.
function targetKey(target: string | null): string {
  return target === null ? "picker" : `target:${target}`;
}

function timeLabel(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString();
}

function ByTargetTable({ byTarget }: { byTarget: readonly TargetUsage[] }) {
  return (
    <>
      <h2 className="usage-heading">By target</h2>
      <table className="usage-table" aria-label="By target">
        <thead>
          <tr>
            <th scope="col">Target</th>
            <th scope="col">Sent</th>
            <th scope="col">Received</th>
            <th scope="col">Total</th>
          </tr>
        </thead>
        <tbody>
          {byTarget.map((usage) => (
            <tr key={targetKey(usage.target)}>
              <th scope="row">{targetLabel(usage.target)}</th>
              <td>{formatBytes(usage.sent)}</td>
              <td>{formatBytes(usage.received)}</td>
              <td>{formatBytes(usage.sent + usage.received)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  );
}

function BySocketTable({
  records,
  label,
}: {
  records: readonly UsageRecord[];
  label: string;
}) {
  const totals = usageTotals(records);
  return (
    <>
      <h2 className="usage-heading">By socket, {label}</h2>
      <table className="usage-table" aria-label="By socket">
        <thead>
          <tr>
            <th scope="col">Socket</th>
            <th scope="col">Sent</th>
            <th scope="col">Received</th>
          </tr>
        </thead>
        <tbody>
          {USAGE_SOCKETS.map((socket) => (
            <tr key={socket}>
              <th scope="row">{USAGE_SOCKET_LABEL[socket]}</th>
              <td>{formatBytes(totals[socket].sent)}</td>
              <td>{formatBytes(totals[socket].received)}</td>
            </tr>
          ))}
          <tr className="usage-total">
            <th scope="row">Total</th>
            <td>{formatBytes(totals.all.sent)}</td>
            <td>{formatBytes(totals.all.received)}</td>
          </tr>
        </tbody>
      </table>
    </>
  );
}

function TimeframesTable({ records }: { records: readonly UsageRecord[] }) {
  const listed = records.slice(-LISTED_TIMEFRAMES).reverse();
  return (
    <>
      <h2 className="usage-heading">
        Timeframes
        {records.length > listed.length
          ? ` (newest ${listed.length} of ${records.length})`
          : ""}
      </h2>
      <div className="usage-scroll">
        <table className="usage-table" aria-label="Timeframes">
          <thead>
            <tr>
              <th scope="col">Ended</th>
              <th scope="col">Target</th>
              <th scope="col">Socket</th>
              <th scope="col">Sent</th>
              <th scope="col">Received</th>
            </tr>
          </thead>
          <tbody>
            {listed.map((record) => (
              <tr
                key={`${targetKey(record.target)}-${record.socket}-${record.start}-${record.end}`}
              >
                <td
                  title={`${timeLabel(record.start)} – ${timeLabel(record.end)}`}
                >
                  {timeLabel(record.end)}
                </td>
                <td>{targetLabel(record.target)}</td>
                <td>{USAGE_SOCKET_LABEL[record.socket]}</td>
                <td>{formatBytes(record.sentBytes)}</td>
                <td>{formatBytes(record.receivedBytes)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}

export default function UsagePanel({
  closeLabel,
  onClose,
  onUnauthorized,
}: {
  closeLabel: string;
  onClose: () => void;
  onUnauthorized: () => void;
}) {
  const [range, setRange] = useState<UsageRange>("day");
  const [report, setReport] = useState<UsageReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [readAt, setReadAt] = useState<number | null>(null);
  const [targetFilter, setTargetFilter] = useState("all");

  // Only the newest read may commit: a range change or a Refresh while one is still
  // out makes the earlier answer stale, and an unmounted panel takes none.
  const generation = useRef(0);

  const load = useCallback(async () => {
    const request = ++generation.current;
    setLoading(true);
    const now = Date.now() / 1000;
    const result = await fetchUsage(usageWithin(range));
    if (request !== generation.current) {
      return;
    }
    setLoading(false);
    if (result.kind === "unauthorized") {
      onUnauthorized();
    } else if (result.kind === "error") {
      // A failed read leaves nothing of the previous range on screen.
      setReport(null);
      setReadAt(null);
      setError(result.message);
    } else {
      setError(null);
      setReport(result.report);
      setReadAt(now);
    }
  }, [range, onUnauthorized]);

  useEffect(() => {
    void load();
    return () => {
      generation.current++;
    };
  }, [load]);

  const records = report?.records ?? [];
  const byTarget = usageByTarget(records);
  // A target the new range has no rows for falls back to every target rather than
  // leaving the filter on an option the list no longer has.
  const selected = byTarget.find(
    (usage) => targetKey(usage.target) === targetFilter,
  );
  const filtered = selected
    ? records.filter((record) => record.target === selected.target)
    : records;
  const filterLabel = selected ? targetLabel(selected.target) : "all targets";

  return (
    <>
      <h1>Data usage</h1>
      <p className="picker-hint">
        Bytes between this browser and the gateway, per target and WebSocket.
        {report &&
          ` Recorded every ${report.intervalSecs} s; each target's socket keeps its newest ${report.maxRecords} timeframes.`}
      </p>
      <div className="usage-controls">
        <select
          aria-label="Time range"
          value={range}
          onChange={(e) => {
            // The previous range's rows are not this range's, even while it loads.
            setReport(null);
            setReadAt(null);
            setRange(e.target.value as UsageRange);
          }}
        >
          {USAGE_RANGES.map((r) => (
            <option key={r.id} value={r.id}>
              {r.label}
            </option>
          ))}
        </select>
        <select
          aria-label="Target"
          value={selected ? targetFilter : "all"}
          onChange={(e) => setTargetFilter(e.target.value)}
        >
          <option value="all">All targets</option>
          {byTarget.map((usage) => (
            <option
              key={targetKey(usage.target)}
              value={targetKey(usage.target)}
            >
              {targetLabel(usage.target)}
            </option>
          ))}
        </select>
        <button
          type="button"
          className="picker-logout"
          onClick={() => void load()}
          disabled={loading}
        >
          {loading ? "Loading…" : "Refresh"}
        </button>
      </div>
      {error && <p className="picker-error">{error}</p>}
      {readAt !== null && (
        <p className="usage-read-at">Read at {timeLabel(readAt)}</p>
      )}
      {report && records.length === 0 && (
        <p className="picker-hint">No data moved in this range.</p>
      )}
      {byTarget.length > 0 && <ByTargetTable byTarget={byTarget} />}
      {filtered.length > 0 && (
        <>
          <BySocketTable records={filtered} label={filterLabel} />
          <TimeframesTable records={filtered} />
        </>
      )}
      <button type="button" className="picker-logout" onClick={onClose}>
        {closeLabel}
      </button>
    </>
  );
}
