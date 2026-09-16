import { useCallback, useEffect, useRef, useState } from "react";
import { gatewayConfig } from "./gatewayConfig.ts";
import {
  agoLabel,
  appendLive,
  customUsageRange,
  DEFAULT_USAGE_RANGE,
  fetchUsage,
  fetchUsageLive,
  formatRate,
  GRAPH_WINDOWS,
  graphWindowLabel,
  type LiveRate,
  liveSeries,
  liveTargets,
  liveTotals,
  type RateSeries,
  rateScale,
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
  type UsageSocket,
  type UsageTotals,
  type UsageUnit,
  usageByTarget,
  usageRangeKey,
  usageRangeLabel,
  usageRate,
  usageTotals,
  usageWithin,
} from "./usage.ts";
import {
  type ChartInk,
  drawChart,
  pointedSecond,
  tipLeft,
} from "./usageChart.ts";

// The "Data usage" view, opened from the target picker and from the session's Info
// card, which it replaces while open; `closeLabel` names where its button returns to.
// See usage.ts.
//
// Two reads. The rate right now — what the gateway's last one-second sample found
// moving — is polled every second while the view is open, kept for the last five
// minutes, and drawn the way a network meter does: the number now over a graph of
// the window behind it, one per direction since sent and received differ by orders
// of magnitude and would flatten each other on one scale. Pause stops the polling,
// and so the graph, until Resume. The recorded rows are read when the view opens,
// when the range changes and when Refresh is pressed, never on a timer.
//
// Usage is compared by target first: one table sums each target over all its sockets,
// and the target filter narrows the meter and the socket table to one of them; the
// socket filter narrows the meter alone. The rate is the metric: the tables show, for
// each direction, the average over the seconds spanned and the busiest second, in
// bits per second. The bytes behind them stay in the model and the API.

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

/// How often the rate right now is read: the gateway samples once a second.
const LIVE_PERIOD_MS = 1000;

/// The meter's ink, in the page's dark palette; the two hues were checked apart for
/// every kind of color vision against the surface, and the swatches in index.css
/// match them.
const SURFACE = { grid: "#21262d", label: "#6e7681", surface: "#161b22" };
const SENT_INK: ChartInk = {
  line: "#4493f8",
  fill: "rgba(68, 147, 248, 0.18)",
  ...SURFACE,
};
const RECEIVED_INK: ChartInk = {
  line: "#bf7f12",
  fill: "rgba(191, 127, 18, 0.2)",
  ...SURFACE,
};

/// The filter select's value for a target: `null` (the picker) cannot be an option
/// value, and a prefix keeps a target named "all" apart from the "all" choice.
function targetKey(target: string | null): string {
  return target === null ? "picker" : `target:${target}`;
}

function timeLabel(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString();
}

/// One direction's rate right now, large, and its busiest second in the window.
function RateTile({
  name,
  direction,
  now,
  peak,
  windowSecs,
}: {
  name: string;
  direction: "sent" | "received";
  now: number | null;
  peak: number;
  windowSecs: number;
}) {
  const [value, unit] = now === null ? ["—", ""] : formatRate(now).split(" ");
  return (
    <div className="usage-tile">
      <span className="usage-tile-label">
        <span className={`usage-swatch usage-${direction}`} />
        {name}
      </span>
      <span className="usage-tile-value">
        {value}
        {unit && <small>{unit}</small>}
      </span>
      <span className="usage-tile-sub">
        peak {formatRate(peak)}, {graphWindowLabel(windowSecs)}
      </span>
    </div>
  );
}

/// The seconds of one direction on a canvas that fills its box (see usageChart.ts),
/// redrawn as the seconds, the scale or the box change. Pointing at a second names
/// it.
function RateChart({
  name,
  series,
  windowSecs,
  ink,
  small,
}: {
  name: string;
  series: RateSeries;
  windowSecs: number;
  ink: ChartInk;
  small?: boolean;
}) {
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const [pointed, setPointed] = useState<number | null>(null);
  const top = rateScale(series.peak);
  const { points } = series;

  useEffect(() => {
    const element = box.current;
    const surface = canvas.current;
    if (element === null || surface === null) {
      return;
    }
    const draw = () => {
      const dpr = window.devicePixelRatio || 1;
      const width = element.clientWidth;
      const height = element.clientHeight;
      const ctx = surface.getContext("2d");
      if (width === 0 || height === 0 || ctx === null) {
        return;
      }
      if (
        surface.width !== Math.round(width * dpr) ||
        surface.height !== Math.round(height * dpr)
      ) {
        surface.width = Math.round(width * dpr);
        surface.height = Math.round(height * dpr);
      }
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      drawChart(ctx, { width, height, points, top }, ink);
    };
    draw();
    const observer = new ResizeObserver(draw);
    observer.observe(element);
    return () => observer.disconnect();
  }, [points, top, ink]);

  const width = box.current?.clientWidth ?? 0;
  const ago = pointed === null ? 0 : points.length - 1 - pointed;
  const pointedValue = pointed === null ? null : points[pointed];

  return (
    <>
      <div className="usage-chart-head">
        <strong>{name}</strong>
        <span>scale: 0 – {formatRate(top)}</span>
      </div>
      <div
        ref={box}
        className={small ? "usage-chart usage-chart-small" : "usage-chart"}
        role="img"
        aria-label={`${name}, ${graphWindowLabel(windowSecs)}, peak ${formatRate(series.peak)}`}
        onPointerMove={(e) => {
          const left = e.currentTarget.getBoundingClientRect().left;
          setPointed(
            pointedSecond(
              e.clientX - left,
              points.length,
              e.currentTarget.clientWidth,
            ),
          );
        }}
        onPointerLeave={() => setPointed(null)}
      >
        <canvas ref={canvas} />
        {pointed !== null && (
          <div
            className="usage-tip"
            style={{ left: `${tipLeft(pointed, points.length, width)}px` }}
          >
            {ago === 0 ? "now" : `${ago} s ago`} ·{" "}
            {pointedValue === null ? "not read" : formatRate(pointedValue)}
          </div>
        )}
      </div>
      <div className="usage-axis">
        <span>{agoLabel(windowSecs)}</span>
        <span>now</span>
      </div>
    </>
  );
}

/// The meter: each direction's rate now and its graph over the window, summed over
/// the rates `keep` admits.
function Meter({
  live,
  history,
  windowSecs,
  keep,
}: {
  live: UsageLive | null;
  history: readonly UsageLive[];
  windowSecs: number;
  keep: (rate: LiveRate) => boolean;
}) {
  const series = liveSeries(history, windowSecs, keep);
  const now = live === null ? null : liveTotals(live.rates.filter(keep));
  return (
    <section className="usage-meter" aria-label="Rate right now">
      <div className="usage-tiles">
        <RateTile
          name="Sent"
          direction="sent"
          now={now === null ? null : now.sent}
          peak={series.sent.peak}
          windowSecs={windowSecs}
        />
        <RateTile
          name="Received"
          direction="received"
          now={now === null ? null : now.received}
          peak={series.received.peak}
          windowSecs={windowSecs}
        />
      </div>
      <RateChart
        name="Sent"
        series={series.sent}
        windowSecs={windowSecs}
        ink={SENT_INK}
      />
      <RateChart
        name="Received"
        series={series.received}
        windowSecs={windowSecs}
        ink={RECEIVED_INK}
        small
      />
    </section>
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
  const [history, setHistory] = useState<readonly UsageLive[]>([]);
  const [paused, setPaused] = useState(false);
  const [windowSecs, setWindowSecs] = useState(GRAPH_WINDOWS[0]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [targetFilter, setTargetFilter] = useState("all");
  const [socketFilter, setSocketFilter] = useState<UsageSocket | "all">("all");

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

  // The rate right now, once a second while the view is open and not paused. A read
  // that fails shows nothing rather than a stale number, and leaves a gap in the
  // graph; the next second tries again.
  useEffect(() => {
    if (paused) {
      return;
    }
    let cancelled = false;
    const read = async () => {
      const result = await fetchUsageLive();
      if (cancelled) {
        return;
      }
      if (result.kind === "unauthorized") {
        onUnauthorized();
      } else if (result.kind === "ok") {
        setLive(result.live);
        setHistory((kept) => appendLive(kept, result.live));
      } else {
        setLive(null);
      }
    };
    void read();
    const timer = setInterval(() => void read(), LIVE_PERIOD_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [paused, onUnauthorized]);

  const changeRange = useCallback((next: UsageRange) => {
    // The previous range's rows are not this range's, even while it loads.
    setReport(null);
    setRange(next);
  }, []);

  // The open timeframe counts with the written ones.
  const records = report ? [...report.records, ...report.open] : [];
  const byTarget = usageByTarget(records);
  // The targets to choose from: the range's, busiest first, then any the graph has
  // seen moving that the range has no rows for yet. A filter on a target neither
  // knows falls back to every target rather than sitting on an option the select no
  // longer has.
  const targets = new Map<string, string | null>();
  for (const usage of byTarget) {
    targets.set(targetKey(usage.target), usage.target);
  }
  for (const target of liveTargets(history)) {
    if (!targets.has(targetKey(target))) {
      targets.set(targetKey(target), target);
    }
  }
  const selected = targets.has(targetFilter) ? targetFilter : "all";
  const filtered =
    selected === "all"
      ? records
      : records.filter((record) => targetKey(record.target) === selected);
  const keep = (rate: LiveRate) =>
    (selected === "all" || targetKey(rate.target) === selected) &&
    (socketFilter === "all" || rate.socket === socketFilter);
  const filterLabel =
    selected === "all"
      ? "all targets"
      : targetLabel(targets.get(selected) as string | null);

  return (
    <>
      <h1>Data usage</h1>
      <p className="picker-hint">
        The rate between this browser and the gateway, per target and WebSocket,
        in bits per second, sampled by the gateway once a second.
      </p>
      <div className="usage-controls">
        <select
          aria-label="Target"
          value={selected}
          onChange={(e) => setTargetFilter(e.target.value)}
        >
          <option value="all">All targets</option>
          {[...targets].map(([key, target]) => (
            <option key={key} value={key}>
              {targetLabel(target)}
            </option>
          ))}
        </select>
        <select
          aria-label="Socket"
          value={socketFilter}
          onChange={(e) =>
            setSocketFilter(e.target.value as UsageSocket | "all")
          }
        >
          <option value="all">All sockets</option>
          {USAGE_SOCKETS.map((socket) => (
            <option key={socket} value={socket}>
              {USAGE_SOCKET_LABEL[socket]}
            </option>
          ))}
        </select>
        <select
          aria-label="Graph window"
          value={windowSecs}
          onChange={(e) => setWindowSecs(Number(e.target.value))}
        >
          {GRAPH_WINDOWS.map((secs) => {
            const label = graphWindowLabel(secs);
            return (
              <option key={secs} value={secs}>
                {label[0].toUpperCase() + label.slice(1)}
              </option>
            );
          })}
        </select>
        <button
          type="button"
          className="picker-logout"
          aria-pressed={paused}
          onClick={() => setPaused((p) => !p)}
        >
          {paused ? "Resume" : "Pause"}
        </button>
      </div>
      <Meter
        live={live}
        history={history}
        windowSecs={windowSecs}
        keep={keep}
      />
      <div className="usage-controls">
        <RangeControls range={range} onChange={changeRange} />
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
          {usageRangeLabel(range)}, read at {timeLabel(report.now)}. Recorded
          every {report.intervalSecs} s; each target's socket keeps its newest{" "}
          {report.maxRecords} timeframes.
        </p>
      )}
      {report && records.length === 0 && (
        <p className="picker-hint">No data moved in this range.</p>
      )}
      {byTarget.length > 0 && <ByTargetTable byTarget={byTarget} />}
      {filtered.length > 0 && (
        <BySocketTable records={filtered} label={filterLabel} />
      )}
      <button type="button" className="picker-logout" onClick={onClose}>
        {closeLabel}
      </button>
    </>
  );
}
