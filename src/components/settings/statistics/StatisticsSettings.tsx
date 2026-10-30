import React, { useCallback, useEffect, useId, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Minus, Plus, RefreshCw } from "lucide-react";
import { commands, events, type StatsSummary } from "@/bindings";
import { useElementWidth } from "@/hooks/useElementWidth";
import { languageName } from "@/lib/constants/languages";
import { Button } from "../../ui/Button";
import { Dropdown } from "../../ui/Dropdown";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { BoxPlotAxis } from "./BoxPlot";
import { ROW_GRID, StatsRow } from "./StatsRow";
import {
  boxOf,
  METRICS,
  sortGroups,
  type Metric,
  type SortBy,
} from "./statsView";
import { usePlotViewport } from "./usePlotViewport";

const ALL_LANGUAGES = ":all";
const UNKNOWN_LANGUAGE = ":unknown";

const rowKey = (group: StatsSummary["groups"][number]) =>
  `${group.model}|${group.language ?? ""}|${group.variant}`;

type CopyStatus =
  | { kind: "idle" }
  | { kind: "copied" }
  | { kind: "error"; error: string };

interface SegmentedProps<T extends string> {
  label: string;
  value: T;
  options: { value: T; label: string }[];
  onChange: (value: T) => void;
  disabled?: boolean;
}

function Segmented<T extends string>({
  label,
  value,
  options,
  onChange,
  disabled,
}: SegmentedProps<T>) {
  return (
    <div role="group" aria-label={label} className="flex items-center gap-1">
      {options.map((option) => (
        <Button
          key={option.value}
          size="sm"
          variant={option.value === value ? "primary-soft" : "ghost"}
          aria-pressed={option.value === value}
          disabled={disabled}
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </Button>
      ))}
    </div>
  );
}

export const StatisticsSettings: React.FC = () => {
  const { t, i18n } = useTranslation();
  const [summary, setSummary] = useState<StatsSummary | null>(null);
  const [copyStatus, setCopyStatus] = useState<CopyStatus>({ kind: "idle" });
  const [error, setError] = useState<string | null>(null);
  const [metric, setMetric] = useState<Metric>("output_gap_ms");
  const tabsId = useId();
  const [sortBy, setSortBy] = useState<SortBy>("speed");
  const [language, setLanguage] = useState(ALL_LANGUAGES);
  const [selected, setSelected] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [confirmingClear, setConfirmingClear] = useState(false);
  const [chartRef, chartWidth] = useElementWidth<HTMLDivElement>();

  const load = useCallback(async () => {
    try {
      const result = await commands.getStatsSummary();
      if (result.status === "ok") {
        setSummary(result.data);
        setError(null);
      } else {
        setError(result.error);
      }
    } catch (error) {
      setError(String(error));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const unlisten = events.transcriptionCompleteEvent.listen(() => {
      void load();
    });
    return () => {
      void unlisten.then((off) => off());
    };
  }, [load]);

  const clear = async () => {
    setConfirmingClear(false);
    try {
      const result = await commands.clearStats();
      if (result.status === "error") {
        setError(result.error);
        return;
      }
      setSelected(null);
      setExpanded(null);
      setLanguage(ALL_LANGUAGES);
      viewport.reset();
      await load();
    } catch (error) {
      setError(String(error));
    }
  };

  const languages = useMemo(() => {
    const codes = new Set(summary?.groups.map((group) => group.language));
    return [...codes]
      .map((code) => ({
        value: code ?? UNKNOWN_LANGUAGE,
        label: code
          ? languageName(code, i18n.language)
          : t("settings.statistics.language.unknown"),
      }))
      .sort((a, b) => a.label.localeCompare(b.label, i18n.language));
  }, [summary, i18n.language, t]);

  const visible = useMemo(() => {
    const activeLanguage = languages.some((option) => option.value === language)
      ? language
      : ALL_LANGUAGES;
    const groups = (summary?.groups ?? []).filter(
      (group) =>
        activeLanguage === ALL_LANGUAGES ||
        (group.language ?? UNKNOWN_LANGUAGE) === activeLanguage,
    );
    return sortGroups(groups, metric, sortBy);
  }, [summary, languages, language, metric, sortBy]);
  const activeSelection = visible.some((group) => rowKey(group) === selected)
    ? selected
    : null;

  const boxes = useMemo(
    () =>
      visible.flatMap((group) => {
        const box = boxOf(group, metric);
        return box ? [box] : [];
      }),
    [visible, metric],
  );
  const languageOptions = [
    { value: ALL_LANGUAGES, label: t("settings.statistics.language.all") },
    ...languages,
  ];
  const languageValue = languageOptions.some((o) => o.value === language)
    ? language
    : ALL_LANGUAGES;
  const viewport = usePlotViewport(boxes, `${metric}|${languageValue}`);
  const axis = viewport.axis;
  const metricInfo = METRICS.find((item) => item.value === metric);

  const onMetricKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const index = METRICS.findIndex((item) => item.value === metric);
    let next: number;
    switch (event.key) {
      case "ArrowRight":
        next = (index + 1) % METRICS.length;
        break;
      case "ArrowLeft":
        next = (index + METRICS.length - 1) % METRICS.length;
        break;
      case "Home":
        next = 0;
        break;
      case "End":
        next = METRICS.length - 1;
        break;
      default:
        return;
    }
    event.preventDefault();
    setMetric(METRICS[next].value);
    event.currentTarget
      .querySelectorAll<HTMLButtonElement>('[role="tab"]')
      [next].focus();
  };

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup
        title={t("settings.statistics.title")}
        description={t("settings.statistics.description")}
      >
        <div className="flex flex-wrap items-center gap-x-4 gap-y-2 p-3">
          <div
            role="tablist"
            aria-label={t("settings.statistics.metric.label")}
            onKeyDown={onMetricKeyDown}
            className="flex items-center gap-1"
          >
            {METRICS.map((option) => (
              <Button
                key={option.value}
                id={`${tabsId}-${option.value}`}
                role="tab"
                aria-selected={option.value === metric}
                aria-controls={`${tabsId}-panel`}
                tabIndex={option.value === metric ? 0 : -1}
                size="sm"
                variant={option.value === metric ? "primary-soft" : "ghost"}
                onClick={() => setMetric(option.value)}
              >
                {t(option.label)}
              </Button>
            ))}
          </div>
          <Segmented
            label={t("settings.statistics.sort.label")}
            value={sortBy}
            options={[
              { value: "speed", label: t("settings.statistics.sort.speed") },
              {
                value: "failures",
                label: t("settings.statistics.sort.failures"),
              },
            ]}
            onChange={setSortBy}
          />
          <Dropdown
            options={languageOptions}
            selectedValue={languageValue}
            onSelect={setLanguage}
          />
          <div className="ms-auto flex items-center gap-2">
            <Button
              size="sm"
              variant="ghost"
              aria-label={t("settings.statistics.refresh")}
              title={t("settings.statistics.refresh")}
              onClick={() => void load()}
            >
              <RefreshCw size={14} />
            </Button>
            {confirmingClear ? (
              <>
                <Button size="sm" variant="danger" onClick={() => void clear()}>
                  {t("settings.statistics.clear.confirm")}
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => setConfirmingClear(false)}
                >
                  {t("settings.statistics.clear.cancel")}
                </Button>
              </>
            ) : (
              <Button
                size="sm"
                variant="danger-ghost"
                disabled={!summary}
                onClick={() => setConfirmingClear(true)}
              >
                {t("settings.statistics.clear.button")}
              </Button>
            )}
          </div>
        </div>

        {summary && (
          <div className="space-y-2 border-t border-line p-3 text-xs text-ink-3">
            {summary.legacy_runs > 0 && (
              <p>
                {t("settings.statistics.legacyTimingHint", {
                  runs: summary.legacy_runs,
                })}
              </p>
            )}
            <div className="flex min-w-0 items-center gap-2">
              <span className="shrink-0">
                {t("settings.statistics.trace.title")}
              </span>
              <code className="min-w-0 flex-1 break-all select-text">
                {summary.trace_path}
              </code>
              <Button
                size="sm"
                variant="ghost"
                disabled={!summary.trace_path}
                aria-label={t("settings.statistics.trace.copyPath")}
                onClick={() => {
                  const copy = async () => {
                    try {
                      await navigator.clipboard.writeText(summary.trace_path);
                      setCopyStatus({ kind: "copied" });
                    } catch (error) {
                      setCopyStatus({ kind: "error", error: String(error) });
                    }
                  };
                  void copy();
                }}
              >
                {t(
                  copyStatus.kind === "copied"
                    ? "settings.statistics.trace.copied"
                    : "common.copy",
                )}
              </Button>
            </div>
            {summary.pending_exports > 0 && (
              <p role="status">
                {t("settings.statistics.trace.pending", {
                  runs: summary.pending_exports,
                })}
              </p>
            )}
            {summary.trace_error && (
              <p role="alert" className="text-error">
                {t("settings.statistics.trace.error", {
                  error: summary.trace_error,
                })}
              </p>
            )}
            {copyStatus.kind === "error" && (
              <p role="alert" className="text-error">
                {t("settings.statistics.trace.copyError", {
                  error: copyStatus.error,
                })}
              </p>
            )}
          </div>
        )}

        <div
          role="tabpanel"
          id={`${tabsId}-panel`}
          aria-labelledby={`${tabsId}-${metric}`}
        >
          {error && (
            <p className="p-3 text-sm text-error">
              {t("settings.statistics.loadError", { error })}
            </p>
          )}

          {metricInfo && (
            <p className="px-3 pt-2 text-xs text-ink-3">{t(metricInfo.hint)}</p>
          )}

          {summary && visible.length === 0 && !error && (
            <p className="p-6 text-center text-sm text-ink-3">
              {t("settings.statistics.empty")}
            </p>
          )}

          {visible.length > 0 && (
            <div
              ref={viewport.ref}
              {...viewport.pointerHandlers}
              className="space-y-1 p-2"
            >
              <div
                className="flex flex-wrap items-center justify-between gap-2 px-2 pb-1"
                title={t("settings.statistics.scale.hint")}
              >
                <span className="text-xs text-ink-3">
                  {t("settings.statistics.scale.label")}
                </span>
                <div
                  role="group"
                  aria-label={t("settings.statistics.scale.label")}
                  className="flex items-center gap-1"
                >
                  <Button
                    size="sm"
                    variant="ghost"
                    aria-label={t("settings.statistics.scale.zoomOut")}
                    title={t("settings.statistics.scale.zoomOut")}
                    disabled={!viewport.canZoomOut}
                    onClick={viewport.zoomOut}
                  >
                    <Minus size={14} />
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    aria-label={t("settings.statistics.scale.zoomIn")}
                    title={t("settings.statistics.scale.zoomIn")}
                    disabled={!viewport.canZoomIn}
                    onClick={viewport.zoomIn}
                  >
                    <Plus size={14} />
                  </Button>
                  <Button
                    size="sm"
                    variant={viewport.isFit ? "primary-soft" : "ghost"}
                    aria-pressed={viewport.isFit}
                    disabled={boxes.length === 0}
                    onClick={viewport.reset}
                  >
                    {t("settings.statistics.scale.fit")}
                  </Button>
                  <Button
                    size="sm"
                    variant={viewport.isZero ? "primary-soft" : "ghost"}
                    aria-pressed={viewport.isZero}
                    disabled={boxes.length === 0}
                    onClick={viewport.fromZero}
                  >
                    {t("settings.statistics.scale.zero")}
                  </Button>
                </div>
              </div>
              <div className={`${ROW_GRID} px-2 text-xs text-ink-3`}>
                <div />
                <div ref={chartRef}>
                  <BoxPlotAxis axis={axis} width={chartWidth} />
                </div>
                <div className="text-end">
                  {t("settings.statistics.columns.median")}
                </div>
                <div className="text-end">
                  {t("settings.statistics.columns.failed")}
                </div>
                <div />
              </div>
              <div
                role="list"
                className="space-y-0.5"
                onClick={(event) => {
                  if (event.target === event.currentTarget) setSelected(null);
                }}
                onKeyDown={(event) => {
                  if (event.key === "Escape") setSelected(null);
                }}
              >
                {visible.map((group) => {
                  const key = rowKey(group);
                  return (
                    <StatsRow
                      key={key}
                      group={group}
                      current={summary?.current ?? {}}
                      metric={metric}
                      axis={axis}
                      chartWidth={chartWidth}
                      selected={activeSelection === key}
                      dimmed={
                        activeSelection !== null && activeSelection !== key
                      }
                      expanded={expanded === key}
                      onSelect={() => setSelected(key)}
                      onToggleExpanded={() => {
                        setSelected(key);
                        setExpanded((open) => (open === key ? null : key));
                      }}
                    />
                  );
                })}
              </div>
              <p className="px-2 pt-2 text-xs text-ink-3">
                {t("settings.statistics.scale.hint")}
              </p>
              <p className="px-2 text-xs text-ink-3">
                {t("settings.statistics.samplingHint")}
              </p>
              <p className="px-2 text-xs text-ink-3">
                {t("settings.statistics.legend")}
              </p>
              <p className="px-2 text-xs text-ink-3">
                {t("settings.statistics.interactionHint")}
              </p>
            </div>
          )}
        </div>
      </SettingsGroup>
    </div>
  );
};
