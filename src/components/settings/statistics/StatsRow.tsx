import React from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown } from "lucide-react";
import type { StatsGroup } from "@/bindings";
import { languageName } from "@/lib/constants/languages";
import { BoxPlot } from "./BoxPlot";
import { ConfigDiff, useConfigValueFormatter } from "./ConfigDiff";
import {
  METRICS,
  boxOf,
  failureRate,
  eligibleRuns,
  missingSamples,
  formatMetric,
  formatPercent,
  type Metric,
  type PlotAxis,
} from "./statsView";

export const ROW_GRID =
  "grid grid-cols-[11rem_minmax(0,1fr)_4.5rem_4rem_1.75rem] items-center gap-3";

interface StatsRowProps {
  group: StatsGroup;
  current: Partial<Record<string, string>>;
  metric: Metric;
  axis: PlotAxis;
  chartWidth: number;
  selected: boolean;
  /** Another row is the one being looked at. */
  dimmed: boolean;
  expanded: boolean;
  onSelect: () => void;
  onToggleExpanded: () => void;
}

export const StatsRow: React.FC<StatsRowProps> = ({
  group,
  current,
  metric,
  axis,
  chartWidth,
  selected,
  dimmed,
  expanded,
  onSelect,
  onToggleExpanded,
}) => {
  const { t, i18n } = useTranslation();
  const formatValue = useConfigValueFormatter();
  const box = boxOf(group, metric);

  const language = group.language
    ? languageName(group.language, i18n.language)
    : t("settings.statistics.language.unknown");
  const details = [
    language,
    ...(group.variant > 1
      ? [t("settings.statistics.variant", { n: group.variant })]
      : []),
  ].join(" · ");

  const missing = missingSamples(group, metric);
  const samples = eligibleRuns(group) - missing;
  const sampleSummary = t("settings.statistics.samples", {
    samples,
    missing,
    cancellations: group.cancellations,
  });

  const figures = box
    ? t("settings.statistics.boxSummary", {
        min: formatMetric(box.min),
        q1: formatMetric(box.q1),
        median: formatMetric(box.median),
        q3: formatMetric(box.q3),
        max: formatMetric(box.max),
        mean: formatMetric(box.mean),
      })
    : t("settings.statistics.noTiming");
  const metricLabel = t(
    METRICS.find((item) => item.value === metric)?.label ??
      "settings.statistics.metric.label",
  );
  const clipped = box !== null && (box.min < axis.min || box.max > axis.max);
  const summary = clipped
    ? `${figures}. ${t("settings.statistics.scale.clipped")}`
    : figures;
  const failed = failureRate(group);
  const failureTitle = t("settings.statistics.failureTitle", {
    failures: group.failures,
    runs: eligibleRuns(group),
  });

  return (
    <div
      role="listitem"
      className={`rounded-lg border transition-colors ${
        selected
          ? "border-plum bg-plum/10"
          : "border-transparent hover:bg-logo-primary/10"
      } ${dimmed ? "opacity-50 hover:opacity-100" : ""}`}
    >
      <div
        onClick={onSelect}
        onDoubleClick={onToggleExpanded}
        className={`${ROW_GRID} px-2 py-1`}
      >
        <button
          type="button"
          aria-pressed={selected}
          className="min-w-0 cursor-pointer text-start focus-visible:outline focus-visible:outline-1 focus-visible:outline-logo-primary"
        >
          <span className="flex items-center gap-1.5">
            <span className="truncate text-sm font-medium">
              {formatValue("model", group.model)}
            </span>
            {group.is_current && (
              <span
                className="shrink-0 rounded-full bg-moss-wash px-1.5 text-[10px] font-medium text-moss"
                title={t("settings.statistics.currentHint")}
              >
                {t("settings.statistics.current")}
              </span>
            )}
          </span>
          <span className="block truncate text-xs text-ink-3">{details}</span>
        </button>
        <div className="min-w-0">
          <BoxPlot
            box={box}
            axis={axis}
            width={chartWidth}
            highlighted={selected}
            summary={`${metricLabel}. ${summary}. ${sampleSummary}`}
          />
        </div>
        <div className="text-end text-sm tabular-nums" title={summary}>
          {box ? formatMetric(box.median) : "–"}
        </div>
        <div
          className={`text-end text-sm tabular-nums ${group.failures > 0 ? "text-error" : ""}`}
          title={failureTitle}
        >
          {formatPercent(failed)}
          <span className="block text-xs text-ink-3">
            {group.failures}/{eligibleRuns(group)}
          </span>
        </div>
        <button
          type="button"
          aria-expanded={expanded}
          aria-label={t(
            expanded
              ? "settings.statistics.details.hide"
              : "settings.statistics.details.show",
          )}
          title={t("settings.statistics.details.hint")}
          onClick={(event) => {
            event.stopPropagation();
            onToggleExpanded();
          }}
          onDoubleClick={(event) => event.stopPropagation()}
          className="flex size-7 cursor-pointer items-center justify-center rounded-md hover:bg-line"
        >
          <ChevronDown
            size={16}
            className={`transition-transform ${expanded ? "rotate-180" : ""}`}
          />
        </button>
      </div>
      <p className="px-2 pb-2 text-xs text-ink-3">{sampleSummary}</p>
      {expanded && <ConfigDiff group={group} current={current} />}
    </div>
  );
};
