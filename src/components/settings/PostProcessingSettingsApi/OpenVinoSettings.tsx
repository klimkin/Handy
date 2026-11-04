import React, { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { commands, type LoadError, type OpenVinoDevice } from "@/bindings";
import { Dropdown, SettingContainer } from "@/components/ui";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { useSettings } from "../../../hooks/useSettings";
import { useModelStore } from "../../../stores/modelStore";

const DEVICE_LABEL_KEYS: Record<OpenVinoDevice, string> = {
  npu: "settings.postProcessing.api.openVino.device.options.npu",
  gpu: "settings.postProcessing.api.openVino.device.options.gpu",
  cpu: "settings.postProcessing.api.openVino.device.options.cpu",
};

const ERROR_LABEL_KEYS: Record<LoadError, string> = {
  unknown_model: "settings.postProcessing.api.openVino.errors.unknownModel",
  model_not_downloaded:
    "settings.postProcessing.api.openVino.errors.modelNotDownloaded",
  library_missing: "settings.postProcessing.api.openVino.errors.libraryMissing",
  device_failed: "settings.postProcessing.api.openVino.errors.deviceFailed",
};

type Readiness =
  | { status: "checking" }
  | { status: "ready" }
  | { status: "failed"; error: LoadError };

export const OpenVinoSettings: React.FC<{ modelId: string }> = ({
  modelId,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const device = getSetting("post_process_openvino_device") ?? "npu";
  const progress = useModelStore((s) => s.downloadProgress[modelId]);
  const [starting, setStarting] = useState(false);
  const [readiness, setReadiness] = useState<Readiness>({
    status: "checking",
  });
  const latestPrepare = useRef(0);
  const downloading = starting || progress !== undefined;

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
    const unlisten = listen<string>("model-download-complete", (event) => {
      if (event.payload === modelId) void prepare();
    });
    return () => {
      unlisten.then((stop) => stop());
    };
  }, [prepare, modelId]);

  const handleDeviceSelect = async (value: string) => {
    await updateSetting(
      "post_process_openvino_device",
      value as OpenVinoDevice,
    );
    await prepare();
  };

  const handleDownload = async () => {
    setStarting(true);
    try {
      // Failure notices come from the model store's download-failed listener.
      await commands.downloadOpenvinoModel(modelId);
    } finally {
      setStarting(false);
    }
  };

  const handleCancel = () => {
    setStarting(false);
    void useModelStore.getState().cancelDownload(modelId);
  };

  const deviceOptions = (
    Object.keys(DEVICE_LABEL_KEYS) as OpenVinoDevice[]
  ).map((value) => ({ value, label: t(DEVICE_LABEL_KEYS[value]) }));

  const loadError =
    !downloading &&
    readiness.status === "failed" &&
    readiness.error !== "model_not_downloaded"
      ? readiness.error
      : null;

  const renderStatus = () => {
    if (downloading) {
      return (
        <>
          <span className="text-sm text-ink-3">
            {t("settings.postProcessing.api.openVino.downloading", {
              percentage: Math.round(progress?.percentage ?? 0),
            })}
          </span>
          <Button variant="secondary" size="sm" onClick={handleCancel}>
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
        return readiness.error === "model_not_downloaded" ? (
          <>
            <span className="text-sm text-ink-3">
              {t(ERROR_LABEL_KEYS.model_not_downloaded)}
            </span>
            <Button
              variant="primary"
              size="sm"
              onClick={() => void handleDownload()}
            >
              {t("settings.postProcessing.api.openVino.download")}
            </Button>
          </>
        ) : (
          <Button variant="secondary" size="sm" onClick={() => void prepare()}>
            {t("settings.postProcessing.api.openVino.retry")}
          </Button>
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
          disabled={isUpdating("post_process_openvino_device") || downloading}
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
      {loadError && (
        <Alert variant="error" contained>
          {t(ERROR_LABEL_KEYS[loadError])}
        </Alert>
      )}
    </>
  );
};
