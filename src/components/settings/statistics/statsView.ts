import type { BoxStats, StatsGroup } from "@/bindings";

export const METRICS = [
  {
    value: "capture_gap_ms",
    label: "settings.statistics.metric.captureGap",
    hint: "settings.statistics.metric.captureGapHint",
  },
  {
    value: "output_gap_ms",
    label: "settings.statistics.metric.outputGap",
    hint: "settings.statistics.metric.outputGapHint",
  },
] as const;
export type Metric = (typeof METRICS)[number]["value"];
export type SortBy = "speed" | "failures";

export const boxOf = (group: StatsGroup, metric: Metric): BoxStats | null =>
  group[metric];

export const eligibleRuns = (group: StatsGroup): number =>
  group.runs - group.cancellations;

export const missingSamples = (group: StatsGroup, metric: Metric): number =>
  metric === "capture_gap_ms" ? group.capture_missing : group.output_missing;

export const failureRate = (group: StatsGroup): number =>
  eligibleRuns(group) === 0 ? 0 : group.failures / eligibleRuns(group);

export function sortGroups(
  groups: StatsGroup[],
  metric: Metric,
  sortBy: SortBy,
): StatsGroup[] {
  const median = (group: StatsGroup) =>
    boxOf(group, metric)?.median ?? Number.POSITIVE_INFINITY;
  const bySpeed = (a: StatsGroup, b: StatsGroup) => {
    const left = median(a);
    const right = median(b);
    return left === right ? 0 : left - right;
  };
  const byFailures = (a: StatsGroup, b: StatsGroup) =>
    failureRate(a) - failureRate(b);
  const [first, second] =
    sortBy === "speed" ? [bySpeed, byFailures] : [byFailures, bySpeed];
  return [...groups].sort((a, b) => first(a, b) || second(a, b));
}

const NICE_STEPS = [1, 2, 2.5, 5, 10];

export interface PlotDomain {
  min: number;
  max: number;
}

export interface PlotAxis extends PlotDomain {
  ticks: number[];
}

/** Round the visible domain outward so every whisker stays on the scale. */
export function niceAxis(min: number, max: number, targetTicks = 5): PlotAxis {
  if (!(max > min) || !Number.isFinite(min) || !Number.isFinite(max)) {
    return { min: 0, max: 1, ticks: [0, 1] };
  }
  const rough = (max - min) / targetTicks;
  const magnitude = 10 ** Math.floor(Math.log10(rough));
  const step =
    (NICE_STEPS.find((s) => s * magnitude >= rough) ?? 10) * magnitude;
  const first = Math.floor(min / step);
  const last = Math.ceil(max / step);
  const ticks = Array.from({ length: last - first + 1 }, (_, i) =>
    Number(((first + i) * step).toPrecision(12)),
  );
  return { min: ticks[0], max: ticks[ticks.length - 1], ticks };
}

/** Where a value falls along a drawing `width` wide, from `pad` each side. */
export function scaleX(
  value: number,
  axis: Pick<PlotAxis, "min" | "max">,
  width: number,
  pad: number,
): number {
  const inner = Math.max(width - pad * 2, 1);
  return pad + ((value - axis.min) / (axis.max - axis.min)) * inner;
}

export function fitAxis(boxes: BoxStats[]): PlotAxis {
  if (boxes.length === 0) return niceAxis(0, 0);
  const min = Math.min(...boxes.map((box) => box.min));
  const max = Math.max(...boxes.map((box) => box.max));
  const full = niceAxis(0, max);
  const padding = Math.max((max - min) * 0.1, max * 0.02, 1);
  const focused = niceAxis(Math.max(0, min - padding), max + padding);
  return focused.max - focused.min < full.max - full.min ? focused : full;
}

export function axisForDomain(domain: PlotDomain): PlotAxis {
  return {
    ...domain,
    ticks: niceAxis(domain.min, domain.max).ticks.filter(
      (tick) => tick >= domain.min && tick <= domain.max,
    ),
  };
}

function boundedDomain(
  min: number,
  span: number,
  bounds: PlotDomain,
): PlotDomain {
  const width = Math.min(span, bounds.max - bounds.min);
  const start = Math.max(bounds.min, Math.min(min, bounds.max - width));
  return { min: start, max: start + width };
}

export function zoomDomain(
  domain: PlotDomain,
  bounds: PlotDomain,
  minSpan: number,
  factor: number,
  anchor = 0.5,
): PlotDomain {
  const fraction = Math.max(0, Math.min(1, anchor));
  const currentSpan = domain.max - domain.min;
  const span = Math.max(
    Math.min(minSpan, currentSpan),
    Math.min(bounds.max - bounds.min, currentSpan * factor),
  );
  const at = domain.min + fraction * (domain.max - domain.min);
  return boundedDomain(at - fraction * span, span, bounds);
}

export function panDomain(
  domain: PlotDomain,
  bounds: PlotDomain,
  delta: number,
): PlotDomain {
  return boundedDomain(domain.min + delta, domain.max - domain.min, bounds);
}

export function formatAxisTick(value: number, max: number): string {
  const unit = max >= 1000 ? 1000 : 1;
  return `${Number((value / unit).toPrecision(12))} ${unit === 1000 ? "s" : "ms"}`;
}

export interface ConfigDifference {
  key: string;
  /** What the group ran with; `undefined` when it did not record the key. */
  value: string | undefined;
  /** What is set now; `undefined` when this version does not record it. */
  current: string | undefined;
}

/**
 * The options that make a group's settings differ from the ones in use now,
 * in key order.
 */
export function configDifferences(
  config: Partial<Record<string, string>>,
  current: Partial<Record<string, string>>,
): ConfigDifference[] {
  const keys = [...new Set([...Object.keys(config), ...Object.keys(current)])];
  return keys
    .sort()
    .filter((key) => config[key] !== current[key])
    .map((key) => ({ key, value: config[key], current: current[key] }));
}

export const formatPercent = (fraction: number): string => {
  const percent = fraction * 100;
  return `${percent >= 10 || percent === 0 ? Math.round(percent) : percent.toFixed(1)}%`;
};

export function formatMetric(value: number): string {
  if (value >= 10_000) return `${Math.round(value / 1000)} s`;
  if (value >= 1000) return `${(value / 1000).toFixed(1)} s`;
  return `${Math.round(value)} ms`;
}
