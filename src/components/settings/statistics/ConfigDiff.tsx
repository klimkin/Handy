import React from "react";
import { useTranslation } from "react-i18next";
import type { StatsGroup } from "@/bindings";
import { languageName } from "@/lib/constants/languages";
import { getTranslatedModelName } from "@/lib/utils/modelTranslation";
import { useModelStore } from "@/stores/modelStore";
import { configDifferences } from "./statsView";

export function useConfigValueFormatter() {
  const { t, i18n } = useTranslation();
  const models = useModelStore((state) => state.models);

  const modelName = (id: string) => {
    const model = models.find((candidate) => candidate.id === id);
    return model ? getTranslatedModelName(model, t) : id;
  };

  return (key: string, value: string | undefined): string => {
    if (value === undefined) {
      return t("settings.statistics.details.notRecorded");
    }
    if (value === "true") return t("settings.statistics.config.on");
    if (value === "false") return t("settings.statistics.config.off");
    if (key === "model" || key === "requested_model") return modelName(value);
    if (key === "selected_language") {
      return value === "auto"
        ? t("settings.statistics.config.auto")
        : languageName(value, i18n.language);
    }
    if (key === "gpu_device" && value === "") {
      return t("settings.statistics.config.automatic");
    }
    return value;
  };
}

interface ConfigDiffProps {
  group: StatsGroup;
  current: Partial<Record<string, string>>;
}

/** The options a group ran with that are not what is set now. */
export const ConfigDiff: React.FC<ConfigDiffProps> = ({ group, current }) => {
  const { t, i18n } = useTranslation();
  const formatValue = useConfigValueFormatter();
  const differences = configDifferences(group.config, current);
  const dates = new Intl.DateTimeFormat(i18n.language, { dateStyle: "medium" });

  return (
    <div className="space-y-2 px-3 pb-3 pt-1 text-sm">
      <p className="text-xs text-ink-3">
        {t("settings.statistics.details.facts", {
          runs: group.runs,
          failures: group.failures,
          first: dates.format(group.first_run * 1000),
          last: dates.format(group.last_run * 1000),
        })}
      </p>
      {differences.length === 0 ? (
        <p>{t("settings.statistics.details.same")}</p>
      ) : (
        <table className="w-full text-start">
          <caption className="pb-1 text-start text-xs font-medium uppercase tracking-wide text-ink-3">
            {t("settings.statistics.details.title")}
          </caption>
          <thead>
            <tr className="text-xs text-ink-3">
              <th className="py-1 pe-3 text-start font-medium">
                {t("settings.statistics.details.setting")}
              </th>
              <th className="py-1 pe-3 text-start font-medium">
                {t("settings.statistics.details.then")}
              </th>
              <th className="py-1 text-start font-medium">
                {t("settings.statistics.details.now")}
              </th>
            </tr>
          </thead>
          <tbody>
            {differences.map(({ key, value, current: now }) => (
              <tr key={key} className="border-t border-line">
                <td className="py-1 pe-3">
                  {t(`settings.statistics.config.${key}`, {
                    defaultValue: key,
                  })}
                </td>
                <td className="py-1 pe-3 font-mono text-xs">
                  {formatValue(key, value)}
                </td>
                <td className="py-1 font-mono text-xs">
                  {formatValue(key, now)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
};
