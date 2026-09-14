import { useCallback, useEffect, useState } from "react";
import {
  fetchUsage,
  formatBytes,
  USAGE_RANGES,
  USAGE_SOCKET_LABEL,
  USAGE_SOCKETS,
  type UsageRange,
  type UsageReport,
  usageSince,
  usageTotals,
} from "./usage.ts";

// The "Data usage" view, opened from the target picker. It reads the recorded rows
// when it opens, when the range changes and when Refresh is pressed — never on a
// timer. See usage.ts.

/// Timeframes listed below the totals, newest first. The totals cover every row.
const LISTED_TIMEFRAMES = 200;

function timeLabel(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString();
}

export default function UsagePanel({
  onClose,
  onUnauthorized,
}: {
  onClose: () => void;
  onUnauthorized: () => void;
}) {
  const [range, setRange] = useState<UsageRange>("day");
  const [report, setReport] = useState<UsageReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [readAt, setReadAt] = useState<number | null>(null);

  const load = useCallback(
    async (isCancelled: () => boolean) => {
      setLoading(true);
      const now = Date.now() / 1000;
      const result = await fetchUsage(usageSince(range, now));
      if (isCancelled()) {
        return;
      }
      setLoading(false);
      if (result.kind === "unauthorized") {
        onUnauthorized();
      } else if (result.kind === "error") {
        setError(result.message);
      } else {
        setError(null);
        setReport(result.report);
        setReadAt(now);
      }
    },
    [range, onUnauthorized],
  );

  useEffect(() => {
    let cancelled = false;
    void load(() => cancelled);
    return () => {
      cancelled = true;
    };
  }, [load]);

  const totals = report ? usageTotals(report.records) : null;
  const listed = report
    ? report.records.slice(-LISTED_TIMEFRAMES).reverse()
    : [];

  return (
    <>
      <h1>Data usage</h1>
      <p className="picker-hint">
        Bytes between this browser and the gateway, per WebSocket.
        {report &&
          ` Recorded every ${report.intervalSecs} s; each socket keeps its newest ${report.maxRecords} timeframes.`}
      </p>
      <div className="usage-controls">
        <select
          aria-label="Time range"
          value={range}
          onChange={(e) => setRange(e.target.value as UsageRange)}
        >
          {USAGE_RANGES.map((r) => (
            <option key={r.id} value={r.id}>
              {r.label}
            </option>
          ))}
        </select>
        <button
          type="button"
          className="picker-logout"
          onClick={() => void load(() => false)}
          disabled={loading}
        >
          {loading ? "Loading…" : "Refresh"}
        </button>
      </div>
      {error && <p className="picker-error">{error}</p>}
      {readAt !== null && (
        <p className="usage-read-at">Read at {timeLabel(readAt)}</p>
      )}
      {totals && (
        <table className="usage-table" aria-label="Totals">
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
      )}
      {report && report.records.length === 0 && (
        <p className="picker-hint">No data moved in this range.</p>
      )}
      {listed.length > 0 && (
        <>
          <h2 className="usage-heading">
            Timeframes
            {report && report.records.length > listed.length
              ? ` (newest ${listed.length} of ${report.records.length})`
              : ""}
          </h2>
          <div className="usage-scroll">
            <table className="usage-table" aria-label="Timeframes">
              <thead>
                <tr>
                  <th scope="col">Ended</th>
                  <th scope="col">Socket</th>
                  <th scope="col">Sent</th>
                  <th scope="col">Received</th>
                </tr>
              </thead>
              <tbody>
                {listed.map((record) => (
                  <tr key={`${record.socket}-${record.start}-${record.end}`}>
                    <td
                      title={`${timeLabel(record.start)} – ${timeLabel(record.end)}`}
                    >
                      {timeLabel(record.end)}
                    </td>
                    <td>{USAGE_SOCKET_LABEL[record.socket]}</td>
                    <td>{formatBytes(record.sentBytes)}</td>
                    <td>{formatBytes(record.receivedBytes)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
      <button type="button" className="picker-logout" onClick={onClose}>
        Back to targets
      </button>
    </>
  );
}
