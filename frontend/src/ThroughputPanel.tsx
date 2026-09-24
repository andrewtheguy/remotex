import {
  type FormEvent,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import { gatewayConfig } from "./gatewayConfig.ts";
import {
  appendLive,
  clockNow,
  customThroughputRange,
  customThroughputWindow,
  DEFAULT_THROUGHPUT_RANGE,
  fetchThroughput,
  fetchThroughputLive,
  formatRate,
  highest,
  isThroughputWindow,
  liveSeries,
  liveTotals,
  localInputValue,
  type RateSeries,
  rateScale,
  recordedSeries,
  spanLabel,
  THROUGHPUT_PRESETS,
  THROUGHPUT_SOCKET_LABEL,
  THROUGHPUT_SOCKETS,
  THROUGHPUT_UNITS,
  type ThroughputLive,
  type ThroughputRange,
  type ThroughputReport,
  type ThroughputSeries,
  type ThroughputSocket,
  type ThroughputSource,
  type ThroughputUnit,
  targetLabel,
  throughputBounds,
  throughputRangeIsLive,
  throughputRangeKey,
  throughputRangeLabel,
  throughputTargets,
  timeLabel,
} from "./throughput.ts";
import {
  type ChartInk,
  drawChart,
  pointedIndex,
  tipLeft,
} from "./throughputChart.ts";

// The "Throughput" view, opened from the target picker and from the session's Info
// card, which it replaces while open; `closeLabel` names where its button returns to.
// See throughput.ts.
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

/// Whether to offer the view at all: only a gateway with `[meter].enabled` records any.
export function useThroughputAvailable(): boolean {
  const [available, setAvailable] = useState(false);
  useEffect(() => {
    let cancelled = false;
    gatewayConfig().then(({ throughput }) => {
      if (!cancelled) {
        setAvailable(throughput);
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

/// How soon the recorded rows are read again when the read carried the seconds that
/// moved — the graph then draws a point a second, and a stale one shows — and how soon a
/// failed read is retried.
const RECORDED_PERIOD_SECS = 10;

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

/// How long until the recorded rows are read again: a range drawn a point per timeframe
/// has nothing new to say until the next one closes, while one drawn a point per second
/// moves on with every read.
function readAgainSecs(report: ThroughputReport): number {
  return report.hasSeconds
    ? RECORDED_PERIOD_SECS
    : Math.max(RECORDED_PERIOD_SECS, report.intervalSecs);
}

/// The filter select's value for a target: `null` (the picker) cannot be an option
/// value, and a prefix keeps a target named "all" apart from the "all" choice.
function targetKey(target: string | null): string {
  return target === null ? "picker" : `target:${target}`;
}

/// One direction's rate right now, large, and under it the range's average — the
/// graph's dashed line — and its busiest second, which the graph does not mark.
function RateTile({
  name,
  direction,
  now,
  series,
  rangeLabel,
}: {
  name: string;
  direction: "sent" | "received";
  now: number | null;
  series: RateSeries;
  rangeLabel: string;
}) {
  const [value, unit] = now === null ? ["—", ""] : formatRate(now).split(" ");
  return (
    <div className="throughput-tile">
      <span className="throughput-tile-label">
        <span className={`throughput-swatch throughput-${direction}`} />
        {name}
      </span>
      <span className="throughput-tile-value">
        {value}
        {unit && <small>{unit}</small>}
      </span>
      <span className="throughput-tile-sub">
        avg {formatRate(series.mean)}, peak {formatRate(series.busiest)},{" "}
        {rangeLabel}
      </span>
    </div>
  );
}

/// What the tooltip calls a point that ends `ago` seconds before the graph does: how
/// long ago a second was on a range that ends now, and otherwise when the step began.
function pointName(
  ago: number,
  stepSecs: number,
  end: number | null,
  spanSecs: number,
  relative: boolean,
): string {
  if (stepSecs === 1 && relative) {
    return ago === 0 ? "now" : `${ago} s ago`;
  }
  return end === null
    ? ""
    : timeLabel(end - Math.min(ago + stepSecs, spanSecs), spanSecs > 86_400);
}

/// The points of one direction on a canvas that fills its box (see throughputChart.ts),
/// redrawn as the points, the scale or the box change. Pointing at one names it.
function RateChart({
  name,
  series,
  stepSecs,
  spanSecs,
  end,
  rangeLabel,
  axis,
  relative,
  ink,
  small,
}: {
  name: string;
  series: RateSeries;
  stepSecs: number;
  spanSecs: number;
  end: number | null;
  rangeLabel: string;
  /// What each end of the plot stands at: how long ago, or the time itself.
  axis: readonly [string, string];
  /// Whether the range ends at the read, so a point can be named by how long ago it was.
  relative: boolean;
  ink: ChartInk;
  small?: boolean;
}) {
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const [hovered, setPointed] = useState<number | null>(null);
  const { points, mean } = series;
  // The scale fits what is drawn — the steps and the mean line, which stands above
  // every step when a step averages quiet seconds the mean leaves out. The busiest
  // second stands above both over recorded timeframes, and is the tile's to name.
  const top = rateScale(Math.max(highest(points), mean));
  // A series of another length may replace this one under a resting pointer.
  const pointed = hovered !== null && hovered < points.length ? hovered : null;
  const sampled = stepSecs === 1 && relative;

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
      drawChart(ctx, { width, height, points, top, mean }, ink);
    };
    draw();
    const observer = new ResizeObserver(draw);
    observer.observe(element);
    return () => observer.disconnect();
  }, [points, top, mean, ink]);

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
          relative,
        );

  return (
    <>
      <div className="throughput-chart-head">
        <strong>{name}</strong>
        <span>
          {stepSecs > 1 && end !== null && `${spanLabel(stepSecs)} averages · `}
          scale: 0 – {formatRate(top)}
        </span>
      </div>
      <div
        ref={box}
        className={
          small ? "throughput-chart throughput-chart-small" : "throughput-chart"
        }
        role="img"
        aria-label={`${name}, ${rangeLabel}, average ${formatRate(mean)}, peak ${formatRate(series.busiest)}`}
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
            className="throughput-tip"
            style={{
              left: `${tipLeft(pointed, points.length, width, sampled ? 60 : 100)}px`,
            }}
          >
            {pointedAt} ·{" "}
            {pointedValue === null ? "not read" : formatRate(pointedValue)}
          </div>
        )}
      </div>
      <div className="throughput-axis">
        <span>{axis[0]}</span>
        <span>{axis[1]}</span>
      </div>
    </>
  );
}

/// The meter: each direction's rate now over its graph of the range.
function Meter({
  rates,
  series,
  rangeLabel,
  axis,
  relative,
}: {
  rates: { sent: number; received: number } | null;
  series: ThroughputSeries;
  rangeLabel: string;
  axis: readonly [string, string];
  relative: boolean;
}) {
  return (
    <section className="throughput-meter" aria-label="Rate right now">
      <div className="throughput-tiles">
        <RateTile
          name="Sent"
          direction="sent"
          now={rates === null ? null : rates.sent}
          series={series.sent}
          rangeLabel={rangeLabel}
        />
        <RateTile
          name="Received"
          direction="received"
          now={rates === null ? null : rates.received}
          series={series.received}
          rangeLabel={rangeLabel}
        />
      </div>
      <RateChart
        name="Sent"
        series={series.sent}
        stepSecs={series.stepSecs}
        spanSecs={series.spanSecs}
        end={series.end}
        rangeLabel={rangeLabel}
        axis={axis}
        relative={relative}
        ink={SENT_INK}
      />
      <RateChart
        name="Received"
        series={series.received}
        stepSecs={series.stepSecs}
        spanSecs={series.spanSecs}
        end={series.end}
        rangeLabel={rangeLabel}
        axis={axis}
        relative={relative}
        ink={RECEIVED_INK}
        small
      />
    </section>
  );
}

/// How long a range the "Between…" fields open on.
const SEEDED_WINDOW_SECS = 3600;

/// The range select and, under it, the fields the two custom ranges are typed in:
/// "Custom…" is a length back from now, "Between…" a start and an end of its own. Both
/// apply on Apply (or Enter), not per keystroke, so half a number and half a date are
/// never read.
function RangeControls({
  range,
  now,
  onChange,
}: {
  range: ThroughputRange;
  /// The gateway's second, which the fields open on; before the first read, this
  /// browser's own.
  now: number | null;
  onChange: (range: ThroughputRange) => void;
}) {
  const listed = THROUGHPUT_PRESETS.some(
    (p) => throughputRangeKey(p) === throughputRangeKey(range),
  );
  const [mode, setMode] = useState<"preset" | "span" | "window">(() => {
    if (listed) {
      return "preset";
    }
    return isThroughputWindow(range) ? "window" : "span";
  });
  const seed =
    range === "all" || isThroughputWindow(range)
      ? DEFAULT_THROUGHPUT_RANGE
      : range;
  const [amount, setAmount] = useState(String(seed.amount));
  const [unit, setUnit] = useState<ThroughputUnit>(seed.unit);
  // The range on screen when the fields are opened, or the hour up to now.
  const openOn = () => {
    const clock = now ?? Math.floor(Date.now() / 1000);
    const window = isThroughputWindow(range)
      ? range
      : { from: clock - SEEDED_WINDOW_SECS, to: clock };
    return {
      from: localInputValue(window.from),
      to: localInputValue(window.to),
    };
  };
  const [times, setTimes] = useState(openOn);
  const typed =
    mode === "window"
      ? customThroughputWindow(times.from, times.to)
      : customThroughputRange(amount, unit);

  const apply = (e: FormEvent) => {
    e.preventDefault();
    if (typed !== null) {
      onChange(typed);
    }
  };
  const applyButton = (
    <button
      type="submit"
      className="picker-logout"
      disabled={
        typed === null ||
        throughputRangeKey(typed) === throughputRangeKey(range)
      }
    >
      Apply
    </button>
  );

  return (
    <>
      <select
        aria-label="Time range"
        value={mode === "preset" ? throughputRangeKey(range) : mode}
        onChange={(e) => {
          if (e.target.value === "window") {
            setTimes(openOn());
          }
          if (e.target.value === "span" || e.target.value === "window") {
            setMode(e.target.value);
            return;
          }
          setMode("preset");
          const chosen = THROUGHPUT_PRESETS.find(
            (p) => throughputRangeKey(p) === e.target.value,
          );
          if (chosen !== undefined) {
            onChange(chosen);
          }
        }}
      >
        {THROUGHPUT_PRESETS.map((choice) => (
          <option
            key={throughputRangeKey(choice)}
            value={throughputRangeKey(choice)}
          >
            {throughputRangeLabel(choice)}
          </option>
        ))}
        <option value="span">Custom…</option>
        <option value="window">Between…</option>
      </select>
      {mode === "span" && (
        <form className="throughput-custom" onSubmit={apply}>
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
            onChange={(e) => setUnit(e.target.value as ThroughputUnit)}
          >
            {THROUGHPUT_UNITS.map((u) => (
              <option key={u} value={u}>
                {u}
              </option>
            ))}
          </select>
          {applyButton}
        </form>
      )}
      {mode === "window" && (
        <form className="throughput-custom" onSubmit={apply}>
          <span>From</span>
          <input
            aria-label="Start"
            type="datetime-local"
            value={times.from}
            onChange={(e) => setTimes({ ...times, from: e.target.value })}
          />
          <span>to</span>
          <input
            aria-label="End"
            type="datetime-local"
            value={times.to}
            onChange={(e) => setTimes({ ...times, to: e.target.value })}
          />
          {applyButton}
        </form>
      )}
    </>
  );
}

/// What one range comes to at this read: the graph's series, drawn from the seconds
/// kept or from the recorded rows, how the tiles name the range, and what each end of
/// the plot stands at. Before the first answer it is a graph of nothing read, as wide
/// as the range asked for.
function meterView(
  range: ThroughputRange,
  read: {
    report: ThroughputReport | null;
    history: readonly ThroughputLive[];
    now: number | null;
  },
  keep: (source: ThroughputSource) => boolean,
): {
  series: ThroughputSeries;
  rangeLabel: string;
  axis: readonly [string, string];
  relative: boolean;
} {
  const { within, end } = throughputBounds(range);
  const unread: RateSeries = { points: [null, null], mean: 0, busiest: 0 };
  let series: ThroughputSeries;
  if (throughputRangeIsLive(range) && within !== null) {
    series = liveSeries(read.history, within, keep, read.now);
  } else if (read.report !== null) {
    series = recordedSeries(read.report, { within, end }, keep);
  } else {
    series = {
      sent: unread,
      received: unread,
      stepSecs: (within ?? 0) / 2,
      spanSecs: within ?? 0,
      end: null,
    };
  }
  const label = throughputRangeLabel(range);
  return {
    series,
    relative: !isThroughputWindow(range),
    // A window names itself; the rest read on after "avg 1.2 Mbps, peak 5.0 Mbps, ".
    rangeLabel: isThroughputWindow(range)
      ? label
      : label[0].toLowerCase() + label.slice(1),
    // A window stands at the times it names, the rest at how far back they reach.
    axis: isThroughputWindow(range)
      ? [timeLabel(range.from, true), timeLabel(series.end ?? range.to, true)]
      : [series.spanSecs > 0 ? `${spanLabel(series.spanSecs)} ago` : "", "now"],
  };
}

export default function ThroughputPanel({
  closeLabel,
  onClose,
  onUnauthorized,
}: {
  closeLabel: string;
  onClose: () => void;
  onUnauthorized: () => void;
}) {
  const [range, setRange] = useState<ThroughputRange>(DEFAULT_THROUGHPUT_RANGE);
  const [report, setReport] = useState<ThroughputReport | null>(null);
  const [live, setLive] = useState<ThroughputLive | null>(null);
  const [history, setHistory] = useState<readonly ThroughputLive[]>([]);
  // The gateway's second a sampled graph ends at, moved on by every poll's outcome.
  const [now, setNow] = useState<number | null>(null);
  const anchor = useRef<{ at: number; wall: number } | null>(null);
  const [paused, setPaused] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [targetFilter, setTargetFilter] = useState("all");
  const [socketFilter, setSocketFilter] = useState<ThroughputSocket | "all">(
    "all",
  );

  const sampled = throughputRangeIsLive(range);
  // The range whose rows are on screen, so that pausing reads nothing more.
  const shown = useRef<string | null>(null);

  // The recorded rows of a range longer than the seconds kept, read when it is chosen
  // and again as each timeframe closes, one read out at a time. A failed read leaves
  // nothing of the range on screen and is tried again.
  useEffect(() => {
    if (sampled) {
      return;
    }
    const key = throughputRangeKey(range);
    if (paused && shown.current === key) {
      return;
    }
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const read = async () => {
      const result = await fetchThroughput(throughputBounds(range));
      if (cancelled) {
        return;
      }
      let again = RECORDED_PERIOD_SECS;
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
        again = readAgainSecs(result.report);
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
      const result = await fetchThroughputLive();
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

  const changeRange = useCallback((next: ThroughputRange) => {
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
  for (const target of throughputTargets(
    history,
    report ? [...report.records, ...report.open] : [],
  )) {
    targets.set(targetKey(target), target);
  }
  const selected = targets.has(targetFilter) ? targetFilter : "all";
  const keep = (source: ThroughputSource) =>
    (selected === "all" || targetKey(source.target) === selected) &&
    (socketFilter === "all" || source.socket === socketFilter);

  const view = meterView(range, { report, history, now }, keep);

  return (
    <>
      <h1>Throughput</h1>
      <p className="picker-hint">
        The rate between this browser and the gateway, per target and WebSocket,
        in bits per second, sampled by the gateway once a second.
      </p>
      <div className="throughput-controls">
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
            setSocketFilter(e.target.value as ThroughputSocket | "all")
          }
        >
          <option value="all">All sockets</option>
          {THROUGHPUT_SOCKETS.map((socket) => (
            <option key={socket} value={socket}>
              {THROUGHPUT_SOCKET_LABEL[socket]}
            </option>
          ))}
        </select>
        <RangeControls range={range} now={now} onChange={changeRange} />
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
        series={view.series}
        rangeLabel={view.rangeLabel}
        axis={view.axis}
        relative={view.relative}
      />
      <button type="button" className="picker-logout" onClick={onClose}>
        {closeLabel}
      </button>
    </>
  );
}
