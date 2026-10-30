import assert from "node:assert/strict";
import type { BoxStats, StatsGroup } from "@/bindings";
import {
  configDifferences,
  failureRate,
  eligibleRuns,
  missingSamples,
  formatMetric,
  formatAxisTick,
  formatPercent,
  niceAxis,
  scaleX,
  sortGroups,
  axisForDomain,
  fitAxis,
  panDomain,
  zoomDomain,
} from "./statsView";

const box = (median: number): BoxStats => ({
  min: median / 2,
  q1: median * 0.75,
  median,
  q3: median * 1.25,
  max: median * 2,
  mean: median,
});

const group = (
  model: string,
  medianMs: number | null,
  runs: number,
  failures: number,
): StatsGroup => ({
  model,
  language: "en",
  variant: 1,
  config: { model },
  is_current: false,
  runs,
  failures,
  first_run: 0,
  last_run: 0,
  capture_gap_ms: null,
  output_gap_ms: medianMs === null ? null : box(medianMs),
  capture_missing: runs,
  output_missing: medianMs === null ? runs : 0,
  cancellations: 0,
  capture_unavailable_reasons: {},
  output_unavailable_reasons: {},
});

const cancelled = {
  ...group("cancelled", null, 10, 2),
  cancellations: 5,
  capture_missing: 5,
  output_missing: 5,
};
assert.equal(eligibleRuns(cancelled), 5);
assert.equal(failureRate(cancelled), 0.4);
assert.equal(failureRate({ ...cancelled, runs: 5, failures: 0 }), 0);
assert.equal(missingSamples(cancelled, "capture_gap_ms"), 5);
assert.equal(missingSamples(cancelled, "output_gap_ms"), 5);

const names = (groups: StatsGroup[]) => groups.map((g) => g.model);

const groups = [
  group("slow", 900, 10, 0),
  group("none", null, 4, 4),
  group("fast", 100, 10, 3),
];
assert.deepEqual(names(sortGroups(groups, "output_gap_ms", "speed")), [
  "fast",
  "slow",
  "none",
]);
assert.deepEqual(names(sortGroups(groups, "output_gap_ms", "failures")), [
  "slow",
  "fast",
  "none",
]);
assert.deepEqual(
  names(
    sortGroups(
      [group("a", null, 4, 4), group("b", null, 2, 2)],
      "output_gap_ms",
      "speed",
    ),
  ),
  ["a", "b"],
);

for (const max of [0.37, 1, 4.2, 870, 12_345]) {
  const axis = niceAxis(0, max);
  assert.ok(axis.max >= max, `${axis.max} holds ${max}`);
  assert.ok(axis.ticks.length >= 2 && axis.ticks.length <= 9);
  assert.equal(axis.ticks[0], 0);
  assert.equal(axis.ticks[axis.ticks.length - 1], axis.max);
}
assert.deepEqual(niceAxis(0, 870), {
  min: 0,
  max: 1000,
  ticks: [0, 200, 400, 600, 800, 1000],
});
assert.deepEqual(niceAxis(0, 0), { min: 0, max: 1, ticks: [0, 1] });

const fullRange = { min: 0, max: 10 };
assert.equal(scaleX(0, fullRange, 110, 5), 5);
assert.equal(scaleX(10, fullRange, 110, 5), 105);
assert.equal(scaleX(5, fullRange, 110, 5), 55);
assert.equal(scaleX(99, fullRange, 110, 5), 995);
assert.equal(scaleX(-3, fullRange, 110, 5), -25);

const focused = niceAxis(1160, 1640);
assert.ok(focused.min > 0 && focused.min <= 1160);
assert.ok(focused.max >= 1640 && focused.max - focused.min < 1000);
assert.equal(scaleX(focused.min, focused, 110, 5), 5);
assert.equal(scaleX(focused.max, focused, 110, 5), 105);
assert.equal(scaleX(1400, { min: 1200, max: 1600 }, 110, 5), 55);
assert.equal(scaleX(1100, { min: 1200, max: 1600 }, 110, 5), -20);
assert.equal(scaleX(1700, { min: 1200, max: 1600 }, 110, 5), 130);
assert.equal(formatAxisTick(1250, 1700), "1.25 s");
assert.equal(formatAxisTick(1260, 1700), "1.26 s");
assert.equal(formatAxisTick(0, 2000), "0 s");
assert.equal(formatAxisTick(0.25, 1), "0.25 ms");

const bounds = { min: 0, max: 2000 };
const view = { min: 1100, max: 1700 };
const zoomed = zoomDomain(view, bounds, 1, 0.5, 0.25);
const anchoredValue = 1250;
assert.equal(
  scaleX(anchoredValue, zoomed, 110, 5),
  scaleX(anchoredValue, view, 110, 5),
);
assert.equal(zoomed.max - zoomed.min, 300);
assert.deepEqual(zoomDomain(view, bounds, 1, 1000), bounds);
assert.equal(
  zoomDomain(view, bounds, 1, 1e-9).max - zoomDomain(view, bounds, 1, 1e-9).min,
  1,
);
assert.deepEqual(panDomain(zoomed, bounds, -10000), { min: 0, max: 300 });
assert.deepEqual(panDomain(zoomed, bounds, 10000), { min: 1700, max: 2000 });
assert.deepEqual(
  zoomDomain({ min: 100, max: 101 }, bounds, 10, 0.5),
  { min: 100, max: 101 },
  "a new outlier must not turn a zoom-in gesture into zoom-out",
);
const exact = axisForDomain({ min: 1193, max: 1487 });
assert.equal(exact.min, 1193);
assert.equal(exact.max, 1487);
assert.ok(exact.ticks.every((tick) => tick >= exact.min && tick <= exact.max));
const outliers = fitAxis([{ ...box(1200), min: 50, max: 9000 }]);
assert.ok(outliers.min <= 50 && outliers.max >= 9000);
assert.deepEqual(fitAxis([]), niceAxis(0, 0));

assert.deepEqual(
  configDifferences(
    { model: "a", vad_enabled: "true", gpu_device: "0", old_option: "x" },
    { model: "b", vad_enabled: "true", gpu_device: "0", new_option: "y" },
  ),
  [
    { key: "model", value: "a", current: "b" },
    { key: "new_option", value: undefined, current: "y" },
    { key: "old_option", value: "x", current: undefined },
  ],
);
assert.deepEqual(configDifferences({ model: "a" }, { model: "a" }), []);

assert.equal(formatPercent(0), "0%");
assert.equal(formatPercent(0.034), "3.4%");
assert.equal(formatPercent(0.25), "25%");
assert.equal(formatMetric(420.4), "420 ms");
assert.equal(formatMetric(1250), "1.3 s");
assert.equal(formatMetric(12_400), "12 s");

console.log("statsView: all assertions passed");
