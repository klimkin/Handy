import React, { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { commands, type CleanerError, type OpenVinoDevice } from "@/bindings";
import { Dropdown, SettingContainer } from "@/components/ui";
import { Button } from "../../ui/Button";
import { useSettings } from "../../../hooks/useSettings";
import { useModelStore } from "../../../stores/modelStore";

const DEVICE_LABEL_KEYS: Record<OpenVinoDevice, string> = {
  npu: "settings.postProcessing.api.openVino.device.options.npu",
  gpu: "settings.postProcessing.api.openVino.device.options.gpu",
  cpu: "settings.postProcessing.api.openVino.device.options.cpu",
};

const ERROR_LABEL_KEYS: Record<CleanerError, string> = {
  unknown_model: "settings.postProcessing.api.openVino.errors.unknownModel",
  model_not_downloaded:
    "settings.postProcessing.api.openVino.errors.modelNotDownloaded",
  library_missing: "settings.postProcessing.api.openVino.errors.libraryMissing",
  device_failed: "settings.postProcessing.api.openVino.errors.deviceFailed",
  generate_failed: "settings.postProcessing.api.openVino.errors.generateFailed",
  empty_output: "settings.postProcessing.api.openVino.errors.emptyOutput",
  prompt_too_long: "settings.postProcessing.api.openVino.errors.promptTooLong",
};

type Readiness =
  | { status: "checking" }
  | { status: "ready" }
  | { status: "failed"; error: CleanerError };

export const OpenVinoSettings: React.FC<{ modelId: string }> = ({
  modelId,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const device = getSetting("post_process_openvino_device") ?? "npu";
  const percentage = useModelStore(
    (s) => s.downloadProgress[modelId]?.percentage ?? 0,
  );
  const [readiness, setReadiness] = useState<Readiness>({
    status: "checking",
  });
  const [isDownloading, setIsDownloading] = useState(false);
  const latestPrepare = useRef(0);

  const prepare = useCallback(async () => {
    const request = ++latestPrepare.current;
    setReadiness({ status: "checking" });
    const result = await commands.prepareOpenvinoCleaner();
    if (request !== latestPrepare.current) return;
    setReadiness(
      result.status === "ok"
        ? { status: "ready" }
        : { status: "failed", error: result.error },
    );
  }, []);

  useEffect(() => {
    void prepare();
  }, [prepare]);

  const handleDeviceSelect = async (value: string) => {
    await updateSetting(
      "post_process_openvino_device",
      value as OpenVinoDevice,
    );
    await prepare();
  };

  const handleDownload = async () => {
    setIsDownloading(true);
    const result = await commands.downloadOpenvinoModel(modelId);
    setIsDownloading(false);
    if (result.status === "ok") await prepare();
  };

  const deviceOptions = (
    Object.keys(DEVICE_LABEL_KEYS) as OpenVinoDevice[]
  ).map((value) => ({ value, label: t(DEVICE_LABEL_KEYS[value]) }));

  const renderStatus = () => {
    if (isDownloading) {
      return (
        <>
          <span className="text-sm text-ink-3">
            {t("settings.postProcessing.api.openVino.downloading", {
              percentage: Math.round(percentage),
            })}
          </span>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => void commands.cancelDownload(modelId)}
          >
            {t("settings.postProcessing.api.openVino.cancel")}
          </Button>
        </>
      );
    }
    switch (readiness.status) {
      case "checking":
        return (
          <span className="text-sm text-ink-3">
            {t("settings.postProcessing.api.openVino.checking")}
          </span>
        );
      case "ready":
        return (
          <span className="text-sm text-ink-3">
            {t("settings.postProcessing.api.openVino.ready")}
          </span>
        );
      case "failed":
        return (
          <>
            <span className="text-sm text-ink-3">
              {t(ERROR_LABEL_KEYS[readiness.error])}
            </span>
            {readiness.error === "model_not_downloaded" ? (
              <Button variant="primary" size="sm" onClick={handleDownload}>
                {t("settings.postProcessing.api.openVino.download")}
              </Button>
            ) : (
              <Button
                variant="secondary"
                size="sm"
                onClick={() => void prepare()}
              >
                {t("settings.postProcessing.api.openVino.retry")}
              </Button>
            )}
          </>
        );
    }
  };

  return (
    <>
      <SettingContainer
        title={t("settings.postProcessing.api.openVino.device.title")}
        description={t(
          "settings.postProcessing.api.openVino.device.description",
        )}
        descriptionMode="tooltip"
        layout="horizontal"
        grouped={true}
      >
        <Dropdown
          options={deviceOptions}
          selectedValue={device}
          onSelect={(value) => void handleDeviceSelect(value)}
          disabled={isUpdating("post_process_openvino_device") || isDownloading}
        />
      </SettingContainer>
      <SettingContainer
        title={t("settings.postProcessing.api.openVino.model.title")}
        description={t(
          "settings.postProcessing.api.openVino.model.description",
        )}
        descriptionMode="tooltip"
        layout="horizontal"
        grouped={true}
      >
        <div className="flex items-center gap-2">{renderStatus()}</div>
      </SettingContainer>
    </>
  );
};
