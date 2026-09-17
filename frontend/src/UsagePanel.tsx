import { useCallback, useEffect, useRef, useState } from "react";
import { gatewayConfig } from "./gatewayConfig.ts";
import {
  appendLive,
  clockNow,
  customUsageRange,
  DEFAULT_USAGE_RANGE,
  fetchUsage,
  fetchUsageLive,
  formatRate,
  liveSeries,
  liveTotals,
  type RateSeries,
  rateScale,
  recordedSeries,
  spanLabel,
  targetLabel,
  USAGE_PRESETS,
  USAGE_SOCKET_LABEL,
  USAGE_SOCKETS,
  USAGE_UNITS,
  type UsageLive,
  type UsageRange,
  type UsageReport,
  type UsageSeries,
  type UsageSocket,
  type UsageSource,
  type UsageUnit,
  usageRangeIsLive,
  usageRangeKey,
  usageRangeLabel,
  usageTargets,
  usageWithin,
} from "./usage.ts";
import {
  type ChartInk,
  drawChart,
  pointedIndex,
  tipLeft,
} from "./usageChart.ts";

// The "Data usage" view, opened from the target picker and from the session's Info
// card, which it replaces while open; `closeLabel` names where its button returns to.
// See usage.ts.
//
// A network meter over one range: each direction's rate right now, large, over a graph
// of the range behind it, one per direction since sent and received differ by orders
// of magnitude and would flatten each other on one scale. The rate right now — what
// the gateway's last one-second sample found moving — is polled every second while the
// view is open and kept for the last five minutes. One poll is out at a time, so a
// slow answer is never overtaken by a later one.
//
// The range decides what the graph is drawn from. One no longer than the seconds kept
// is drawn from them, second by second; its right edge is the gateway's clock as the
// page reckons it, so a poll that fails or is skipped leaves a gap rather than the last
// sample standing at "now". A longer one is drawn from the recorded timeframes, a point
// the average over one or over several, read when the range is chosen and again as
// each timeframe closes. Pause stops both reads, and so the graph, until Resume.
//
// The target and socket filters narrow the tiles and the graphs alike. Every rate is
// in bits per second; the bytes behind them stay in the model and the API.

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

/// The soonest the recorded rows are read again, whatever the timeframe's length, and
/// how soon a failed read is retried.
const RECORDED_PERIOD_MIN_SECS = 10;

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

/// When a recorded point begins, to the minute: the day too once the range leaves
/// today's.
function pointLabel(unixSecs: number, withDay: boolean): string {
  return new Date(unixSecs * 1000).toLocaleString(
    [],
    withDay
      ? { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" }
      : { hour: "numeric", minute: "2-digit" },
  );
}

/// One direction's rate right now, large, and its busiest second in the range.
function RateTile({
  name,
  direction,
  now,
  busiest,
  rangeLabel,
}: {
  name: string;
  direction: "sent" | "received";
  now: number | null;
  busiest: number;
  rangeLabel: string;
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
        peak {formatRate(busiest)}, {rangeLabel}
      </span>
    </div>
  );
}

/// What the tooltip calls a point that ends `ago` seconds before the graph does: how
/// long ago a sampled second was, when a recorded step began.
function pointName(
  ago: number,
  stepSecs: number,
  end: number | null,
  spanSecs: number,
): string {
  if (stepSecs === 1) {
    return ago === 0 ? "now" : `${ago} s ago`;
  }
  return end === null
    ? ""
    : pointLabel(end - ago - stepSecs, spanSecs > 86_400);
}

/// The points of one direction on a canvas that fills its box (see usageChart.ts),
/// redrawn as the points, the scale or the box change. Pointing at one names it.
function RateChart({
  name,
  series,
  stepSecs,
  end,
  rangeLabel,
  ink,
  small,
}: {
  name: string;
  series: RateSeries;
  stepSecs: number;
  end: number | null;
  rangeLabel: string;
  ink: ChartInk;
  small?: boolean;
}) {
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const [pointed, setPointed] = useState<number | null>(null);
  const top = rateScale(series.peak);
  const { points } = series;
  const sampled = stepSecs === 1;
  const spanSecs = points.length * stepSecs;

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
  const pointedValue = pointed === null ? null : points[pointed];
  const pointedAt =
    pointed === null
      ? ""
      : pointName(
          (points.length - 1 - pointed) * stepSecs,
          stepSecs,
          end,
          spanSecs,
        );

  return (
    <>
      <div className="usage-chart-head">
        <strong>{name}</strong>
        <span>
          {!sampled && end !== null && `${spanLabel(stepSecs)} averages · `}
          scale: 0 – {formatRate(top)}
        </span>
      </div>
      <div
        ref={box}
        className={small ? "usage-chart usage-chart-small" : "usage-chart"}
        role="img"
        aria-label={`${name}, ${rangeLabel}, peak ${formatRate(series.busiest)}`}
        onPointerMove={(e) => {
          const left = e.currentTarget.getBoundingClientRect().left;
          setPointed(
            pointedIndex(
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
            style={{
              left: `${tipLeft(pointed, points.length, width, sampled ? 60 : 100)}px`,
            }}
          >
            {pointedAt} ·{" "}
            {pointedValue === null ? "not read" : formatRate(pointedValue)}
          </div>
        )}
      </div>
      <div className="usage-axis">
        <span>{spanSecs > 0 && `${spanLabel(spanSecs)} ago`}</span>
        <span>now</span>
      </div>
    </>
  );
}

/// The meter: each direction's rate now over its graph of the range.
function Meter({
  rates,
  series,
  rangeLabel,
}: {
  rates: { sent: number; received: number } | null;
  series: UsageSeries;
  rangeLabel: string;
}) {
  return (
    <section className="usage-meter" aria-label="Rate right now">
      <div className="usage-tiles">
        <RateTile
          name="Sent"
          direction="sent"
          now={rates === null ? null : rates.sent}
          busiest={series.sent.busiest}
          rangeLabel={rangeLabel}
        />
        <RateTile
          name="Received"
          direction="received"
          now={rates === null ? null : rates.received}
          busiest={series.received.busiest}
          rangeLabel={rangeLabel}
        />
      </div>
      <RateChart
        name="Sent"
        series={series.sent}
        stepSecs={series.stepSecs}
        end={series.end}
        rangeLabel={rangeLabel}
        ink={SENT_INK}
      />
      <RateChart
        name="Received"
        series={series.received}
        stepSecs={series.stepSecs}
        end={series.end}
        rangeLabel={rangeLabel}
        ink={RECEIVED_INK}
        small
      />
    </section>
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
  // The gateway's second a sampled graph ends at, moved on by every poll's outcome.
  const [now, setNow] = useState<number | null>(null);
  const anchor = useRef<{ at: number; wall: number } | null>(null);
  const [paused, setPaused] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [targetFilter, setTargetFilter] = useState("all");
  const [socketFilter, setSocketFilter] = useState<UsageSocket | "all">("all");

  const sampled = usageRangeIsLive(range);
  // The range whose rows are on screen, so that pausing reads nothing more.
  const shown = useRef<string | null>(null);

  // The recorded rows of a range longer than the seconds kept, read when it is chosen
  // and again as each timeframe closes, one read out at a time. A failed read leaves
  // nothing of the range on screen and is tried again.
  useEffect(() => {
    if (sampled) {
      return;
    }
    const key = usageRangeKey(range);
    if (paused && shown.current === key) {
      return;
    }
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const read = async () => {
      const result = await fetchUsage(usageWithin(range));
      if (cancelled) {
        return;
      }
      let again = RECORDED_PERIOD_MIN_SECS;
      if (result.kind === "unauthorized") {
        onUnauthorized();
        return;
      }
      if (result.kind === "error") {
        shown.current = null;
        setReport(null);
        setError(result.message);
      } else {
        shown.current = key;
        setError(null);
        setReport(result.report);
        again = Math.max(again, result.report.intervalSecs);
      }
      if (!paused) {
        timer = setTimeout(() => void read(), again * 1000);
      }
    };
    void read();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [range, sampled, paused, onUnauthorized]);

  // The rate right now, once a second while the view is open and not paused, one
  // read out at a time: a tick while one is still out is skipped, so an answer never
  // lands after a later one. A read that fails shows nothing rather than a stale
  // number, and the clock moves on without a sample, which the graph draws as a gap;
  // the next second tries again.
  useEffect(() => {
    if (paused) {
      return;
    }
    let cancelled = false;
    let reading = false;
    const read = async () => {
      if (reading) {
        return;
      }
      reading = true;
      const result = await fetchUsageLive();
      reading = false;
      if (cancelled) {
        return;
      }
      if (result.kind === "unauthorized") {
        onUnauthorized();
      } else if (result.kind === "ok") {
        anchor.current = { at: result.live.at, wall: Date.now() };
        setNow(result.live.at);
        setLive(result.live);
        setHistory((kept) => appendLive(kept, result.live));
      } else {
        setNow(clockNow(anchor.current, Date.now()));
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
    shown.current = null;
    setReport(null);
    setError(null);
    setRange(next);
  }, []);

  // The targets to choose from: those the seconds kept or the range's rows saw moving.
  // A filter on a target neither knows falls back to every target rather than sitting
  // on an option the select no longer has.
  const targets = new Map<string, string | null>();
  for (const target of usageTargets(
    history,
    report ? [...report.records, ...report.open] : [],
  )) {
    targets.set(targetKey(target), target);
  }
  const selected = targets.has(targetFilter) ? targetFilter : "all";
  const keep = (source: UsageSource) =>
    (selected === "all" || targetKey(source.target) === selected) &&
    (socketFilter === "all" || source.socket === socketFilter);

  const within = usageWithin(range);
  let series: UsageSeries;
  if (sampled && within !== null) {
    series = liveSeries(history, within, keep, now);
  } else if (report !== null) {
    series = recordedSeries(report, within, keep);
  } else {
    // Still loading, or the read failed: a graph of nothing read, as wide as asked.
    const unread = { points: [null, null], peak: 0, busiest: 0 };
    series = {
      sent: unread,
      received: unread,
      stepSecs: (within ?? 0) / 2,
      end: null,
    };
  }
  const label = usageRangeLabel(range);
  const rangeLabel = label[0].toLowerCase() + label.slice(1);

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
        <RangeControls range={range} onChange={changeRange} />
        <button
          type="button"
          className="picker-logout"
          aria-pressed={paused}
          onClick={() => setPaused((p) => !p)}
        >
          {paused ? "Resume" : "Pause"}
        </button>
      </div>
      {error && <p className="picker-error">{error}</p>}
      <Meter
        rates={live === null ? null : liveTotals(live.rates.filter(keep))}
        series={series}
        rangeLabel={rangeLabel}
      />
      <button type="button" className="picker-logout" onClick={onClose}>
        {closeLabel}
      </button>
    </>
  );
}
