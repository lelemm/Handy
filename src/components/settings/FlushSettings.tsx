import React from "react";
import { useTranslation } from "react-i18next";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";
import type { FlushGap, FlushPostProcessContext } from "@/bindings";

interface FlushSettingsProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const FlushSettings: React.FC<FlushSettingsProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const flushGap = (getSetting("flush_gap") || "disabled") as FlushGap;
    const postProcessEnabled = getSetting("post_process_enabled") || false;
    const flushEnabled = flushGap !== "disabled";
    const context = (getSetting("flush_post_process_context") ||
      "full_session") as FlushPostProcessContext;
    const contextDisabled = !flushEnabled || !postProcessEnabled;

    const flushOptions = [
      {
        value: "disabled",
        label: t("settings.advanced.flush.options.disabled"),
      },
      {
        value: "ms500",
        label: t("settings.advanced.flush.options.ms500"),
      },
      {
        value: "ms750",
        label: t("settings.advanced.flush.options.ms750"),
      },
      {
        value: "sec1",
        label: t("settings.advanced.flush.options.sec1"),
      },
      {
        value: "sec2",
        label: t("settings.advanced.flush.options.sec2"),
      },
      {
        value: "sec5",
        label: t("settings.advanced.flush.options.sec5"),
      },
    ];

    const contextOptions = [
      {
        value: "full_session",
        label: t("settings.advanced.flush.context.options.fullSession"),
      },
      {
        value: "current_chunk",
        label: t("settings.advanced.flush.context.options.currentChunk"),
      },
    ];

    return (
      <>
        <SettingContainer
          title={t("settings.advanced.flush.title")}
          description={t("settings.advanced.flush.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
        >
          <Dropdown
            options={flushOptions}
            selectedValue={flushGap}
            onSelect={(value) => updateSetting("flush_gap", value as FlushGap)}
            disabled={isUpdating("flush_gap")}
          />
        </SettingContainer>

        <SettingContainer
          title={t("settings.advanced.flush.context.title")}
          description={t("settings.advanced.flush.context.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
          disabled={contextDisabled}
        >
          <Dropdown
            options={contextOptions}
            selectedValue={context}
            onSelect={(value) =>
              updateSetting(
                "flush_post_process_context",
                value as FlushPostProcessContext,
              )
            }
            disabled={
              contextDisabled || isUpdating("flush_post_process_context")
            }
          />
        </SettingContainer>
      </>
    );
  },
);
