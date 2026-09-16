import { useCallback, useEffect, useRef, useState } from "react";
import { gatewayConfig } from "./gatewayConfig.ts";
import {
  customUsageRange,
  DEFAULT_USAGE_RANGE,
  fetchUsage,
  fetchUsageLive,
  formatRate,
  liveTotals,
  type TargetUsage,
  targetLabel,
  USAGE_PRESETS,
  USAGE_SOCKET_LABEL,
  USAGE_SOCKETS,
  USAGE_UNITS,
  type UsageLive,
  type UsageRange,
  type UsageRecord,
  type UsageReport,
  type UsageTotals,
  type UsageUnit,
  usageByTarget,
  usageRangeKey,
  usageRangeLabel,
  usageRate,
  usageTotals,
  usageWithin,
} from "./usage.ts";

// The "Data usage" view, opened from the target picker and from the session's Info
// card, which it replaces while open; `closeLabel` names where its button returns to.
// See usage.ts.
//
// Two reads. The rate right now — what the gateway's last one-second sample found
// moving — is polled every second while the view is open and shown at the top, the
// way a network meter reads. The recorded rows are read when the view opens, when the
// range changes and when Refresh is pressed, never on a timer.
//
// Usage is compared by target first: one table sums each target over all its sockets,
// and the target filter narrows the rate right now, the socket table and the
// timeframes to one of them. The rate is the metric: every table shows, for each
// direction, the average over the seconds spanned and the busiest second, in bits per
// second. The bytes behind them stay in the model and the API. The timeframe still
// being counted is read with the written ones and told apart in the list.

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

/// How often the rate right now is read: the gateway samples once a second.
const LIVE_PERIOD_MS = 1000;

/// The filter select's value for a target: `null` (the picker) cannot be an option
/// value, and a prefix keeps a target named "all" apart from the "all" choice.
function targetKey(target: string | null): string {
  return target === null ? "picker" : `target:${target}`;
}

function timeLabel(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString();
}

/// The rate right now, over `rates` (already narrowed to the target filter), or what
/// stands in for it while there is none to show.
function LiveRate({
  live,
  rates,
}: {
  live: UsageLive | null;
  rates: readonly UsageLive["rates"][number][];
}) {
  if (live === null) {
    return <p className="usage-now">Now: —</p>;
  }
  const totals = liveTotals(rates);
  return (
    <p className="usage-now">
      Now: <strong>{formatRate(totals.sent)}</strong> sent,{" "}
      <strong>{formatRate(totals.received)}</strong> received
    </p>
  );
}

/// The two header rows every table shares: a direction over its average over the
/// seconds spanned and its busiest second. `leading` names the columns before them.
function DirectionHeaders({ leading }: { leading: readonly string[] }) {
  return (
    <thead>
      <tr>
        {leading.map((name) => (
          <th key={name} scope="col" rowSpan={2}>
            {name}
          </th>
        ))}
        <th scope="colgroup" colSpan={2} className="usage-group">
          Sent
        </th>
        <th scope="colgroup" colSpan={2} className="usage-group">
          Received
        </th>
      </tr>
      <tr>
        {(["sent", "received"] as const).map((direction) =>
          ["Average", "Peak"].map((name) => (
            <th key={`${direction}-${name}`} scope="col">
              {name}
            </th>
          )),
        )}
      </tr>
    </thead>
  );
}

/// One direction's two cells: the average of `bytes` over `seconds` (a dash for none
/// yet), and the busiest second.
function DirectionCells({
  bytes,
  seconds,
  peak,
}: {
  bytes: number;
  seconds: number;
  peak: number;
}) {
  const average = usageRate(bytes, seconds);
  return (
    <>
      <td>{average === null ? "—" : formatRate(average)}</td>
      <td>{formatRate(peak)}</td>
    </>
  );
}

/// Both directions' cells for some totals.
function TotalsCells({ totals }: { totals: UsageTotals }) {
  return (
    <>
      <DirectionCells
        bytes={totals.sent}
        seconds={totals.seconds}
        peak={totals.peakSent}
      />
      <DirectionCells
        bytes={totals.received}
        seconds={totals.seconds}
        peak={totals.peakReceived}
      />
    </>
  );
}

function ByTargetTable({ byTarget }: { byTarget: readonly TargetUsage[] }) {
  return (
    <>
      <h2 className="usage-heading">By target</h2>
      <table className="usage-table" aria-label="By target">
        <DirectionHeaders leading={["Target"]} />
        <tbody>
          {byTarget.map((usage) => (
            <tr key={targetKey(usage.target)}>
              <th scope="row">{targetLabel(usage.target)}</th>
              <TotalsCells totals={usage} />
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
        <DirectionHeaders leading={["Socket"]} />
        <tbody>
          {USAGE_SOCKETS.map((socket) => (
            <tr key={socket}>
              <th scope="row">{USAGE_SOCKET_LABEL[socket]}</th>
              <TotalsCells totals={totals[socket]} />
            </tr>
          ))}
          <tr className="usage-total">
            <th scope="row">Total</th>
            <TotalsCells totals={totals.all} />
          </tr>
        </tbody>
      </table>
    </>
  );
}

function TimeframesTable({
  records,
  open,
}: {
  records: readonly UsageRecord[];
  /// Which of `records` belong to the timeframe still being counted.
  open: ReadonlySet<UsageRecord>;
}) {
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
          <DirectionHeaders leading={["Ended", "Target", "Socket"]} />
          <tbody>
            {listed.map((record) => {
              const seconds = record.end - record.start;
              return (
                <tr
                  key={`${targetKey(record.target)}-${record.socket}-${record.start}-${record.end}`}
                  className={open.has(record) ? "usage-open" : undefined}
                >
                  <td
                    title={`${timeLabel(record.start)} – ${timeLabel(record.end)}`}
                  >
                    {open.has(record) ? "In progress" : timeLabel(record.end)}
                  </td>
                  <td>{targetLabel(record.target)}</td>
                  <td>{USAGE_SOCKET_LABEL[record.socket]}</td>
                  <DirectionCells
                    bytes={record.sentBytes}
                    seconds={seconds}
                    peak={record.peakSentPerSec}
                  />
                  <DirectionCells
                    bytes={record.receivedBytes}
                    seconds={seconds}
                    peak={record.peakReceivedPerSec}
                  />
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </>
  );
}

/// The range select and, on "Custom", the amount and unit it is typed as. A custom
/// range applies on Apply (or Enter), not per keystroke, so half a number is never
/// read.
function RangeControls({
  range,
  onChange,
}: {
  range: UsageRange;
  onChange: (range: UsageRange) => void;
}) {
  const [custom, setCustom] = useState(
    () => !USAGE_PRESETS.some((p) => usageRangeKey(p) === usageRangeKey(range)),
  );
  const seed = range === "all" ? DEFAULT_USAGE_RANGE : range;
  const [amount, setAmount] = useState(String(seed.amount));
  const [unit, setUnit] = useState<UsageUnit>(seed.unit);
  const typed = customUsageRange(amount, unit);

  return (
    <>
      <select
        aria-label="Time range"
        value={custom ? "custom" : usageRangeKey(range)}
        onChange={(e) => {
          if (e.target.value === "custom") {
            setCustom(true);
            return;
          }
          setCustom(false);
          const preset = USAGE_PRESETS.find(
            (p) => usageRangeKey(p) === e.target.value,
          );
          if (preset !== undefined) {
            onChange(preset);
          }
        }}
      >
        {USAGE_PRESETS.map((preset) => (
          <option key={usageRangeKey(preset)} value={usageRangeKey(preset)}>
            {usageRangeLabel(preset)}
          </option>
        ))}
        <option value="custom">Custom…</option>
      </select>
      {custom && (
        <form
          className="usage-custom"
          onSubmit={(e) => {
            e.preventDefault();
            if (typed !== null) {
              onChange(typed);
            }
          }}
        >
          <span>Last</span>
          <input
            aria-label="Amount"
            type="number"
            inputMode="numeric"
            min={1}
            step={1}
            value={amount}
            onChange={(e) => setAmount(e.target.value)}
          />
          <select
            aria-label="Unit"
            value={unit}
            onChange={(e) => setUnit(e.target.value as UsageUnit)}
          >
            {USAGE_UNITS.map((u) => (
              <option key={u} value={u}>
                {u}
              </option>
            ))}
          </select>
          <button
            type="submit"
            className="picker-logout"
            disabled={
              typed === null || usageRangeKey(typed) === usageRangeKey(range)
            }
          >
            Apply
          </button>
        </form>
      )}
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
  const [range, setRange] = useState<UsageRange>(DEFAULT_USAGE_RANGE);
  const [report, setReport] = useState<UsageReport | null>(null);
  const [live, setLive] = useState<UsageLive | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [targetFilter, setTargetFilter] = useState("all");

  // Only the newest read may commit: a range change or a Refresh while one is still
  // out makes the earlier answer stale, and an unmounted panel takes none.
  const generation = useRef(0);

  const load = useCallback(async () => {
    const request = ++generation.current;
    setLoading(true);
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
      setError(result.message);
    } else {
      setError(null);
      setReport(result.report);
    }
  }, [range, onUnauthorized]);

  useEffect(() => {
    void load();
    return () => {
      generation.current++;
    };
  }, [load]);

  // The rate right now, once a second while the view is open. A read that fails
  // shows nothing rather than a stale number; the next second tries again.
  useEffect(() => {
    let cancelled = false;
    const read = async () => {
      const result = await fetchUsageLive();
      if (cancelled) {
        return;
      }
      if (result.kind === "unauthorized") {
        onUnauthorized();
      } else {
        setLive(result.kind === "ok" ? result.live : null);
      }
    };
    void read();
    const timer = setInterval(() => void read(), LIVE_PERIOD_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [onUnauthorized]);

  const changeRange = useCallback((next: UsageRange) => {
    // The previous range's rows are not this range's, even while it loads.
    setReport(null);
    setRange(next);
  }, []);

  // The open timeframe counts with the written ones, and is told apart in the list.
  const open = new Set(report?.open);
  const records = report ? [...report.records, ...report.open] : [];
  const byTarget = usageByTarget(records);
  // A target the new range has no rows for falls back to every target rather than
  // leaving the filter on an option the list no longer has.
  const selected = byTarget.find(
    (usage) => targetKey(usage.target) === targetFilter,
  );
  const filtered = selected
    ? records.filter((record) => record.target === selected.target)
    : records;
  const liveRates = live?.rates ?? [];
  const filteredLive = selected
    ? liveRates.filter((rate) => rate.target === selected.target)
    : liveRates;
  const filterLabel = selected ? targetLabel(selected.target) : "all targets";

  return (
    <>
      <h1>Data usage</h1>
      <p className="picker-hint">
        The rate between this browser and the gateway, per target and WebSocket,
        in bits per second.
        {report &&
          ` Recorded every ${report.intervalSecs} s; each target's socket keeps its newest ${report.maxRecords} timeframes.`}
      </p>
      <LiveRate live={live} rates={filteredLive} />
      <div className="usage-controls">
        <RangeControls range={range} onChange={changeRange} />
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
      {report && (
        <p className="usage-read-at">
          {usageRangeLabel(range)}, read at {timeLabel(report.now)}
        </p>
      )}
      {report && records.length === 0 && (
        <p className="picker-hint">No data moved in this range.</p>
      )}
      {byTarget.length > 0 && <ByTargetTable byTarget={byTarget} />}
      {filtered.length > 0 && (
        <>
          <BySocketTable records={filtered} label={filterLabel} />
          <TimeframesTable records={filtered} open={open} />
        </>
      )}
      <button type="button" className="picker-logout" onClick={onClose}>
        {closeLabel}
      </button>
    </>
  );
}
