import React from "react";
import { useTranslation } from "react-i18next";
import type { CapabilityBinding } from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { Input } from "@/components/ui/Input";
import { SettingContainer } from "@/components/ui/SettingContainer";
import { SettingsGroup } from "@/components/ui/SettingsGroup";
import { Textarea } from "@/components/ui/Textarea";
import { ToggleSwitch } from "@/components/ui/ToggleSwitch";
import { Alert } from "@/components/ui/Alert";
import { PolicySettings } from "./PolicySettings";
import { RotationEditor } from "./RotationEditor";
import { useBinding, useDraftField, type BindingEdit } from "./useProviders";

export const CloudSpeechSettings: React.FC = () => {
  const { t } = useTranslation("fork");
  const { settings } = useSettings();
  const { binding, save, error } = useBinding("stt");
  const translating = settings?.translate_to_english ?? false;

  const update = (edit: BindingEdit) =>
    void save((current) =>
      typeof edit === "function" ? edit(current) : { ...current, ...edit },
    );

  // Hooks run unconditionally, so these are declared before the early return.
  const language = useDraftField(binding?.language ?? "", (value) =>
    update({ language: value }),
  );
  const vocabulary = useDraftField(binding?.prompt ?? "", (value) =>
    update({ prompt: value }),
  );

  if (!binding) {
    return error ? <Alert variant="error">{error}</Alert> : null;
  }

  return (
    <div className="max-w-3xl w-full mx-auto space-y-4">
      <div>
        <h1 className="text-lg font-semibold">{t("cloudSpeech.title")}</h1>
        <p className="text-sm text-mid-gray">{t("cloudSpeech.description")}</p>
      </div>

      {error && <Alert variant="error">{error}</Alert>}

      <SettingsGroup>
        <ToggleSwitch
          checked={binding.enabled ?? false}
          onChange={(enabled) => update({ enabled })}
          label={t("binding.enable")}
          description={t("binding.enableSttDescription")}
          descriptionMode="inline"
          grouped
        />
      </SettingsGroup>

      {/* Everything below only matters once the toggle is on, so it is hidden
          rather than left live and inert. */}
      {binding.enabled && (
        <>
          <Alert variant="info">{t("binding.streamingConflict")}</Alert>

          <RotationEditor
            capability="stt"
            binding={binding}
            onChange={update}
          />

          <SettingsGroup title={t("binding.transcriptionTitle")}>
            <SettingContainer
              title={t("binding.language")}
              description={
                translating
                  ? t("binding.languageIgnoredWhenTranslating")
                  : t("binding.languageDescription")
              }
              descriptionMode="inline"
              grouped
            >
              <Input
                className="w-56"
                // The translation endpoint detects the source itself.
                disabled={translating}
                placeholder={t("binding.languagePlaceholder")}
                {...language}
              />
            </SettingContainer>

            <SettingContainer
              title={t("binding.sttPrompt")}
              description={t("binding.sttPromptDescription")}
              descriptionMode="inline"
              layout="stacked"
              grouped
            >
              <Textarea
                className="w-full"
                variant="compact"
                placeholder={t("binding.sttPromptPlaceholder")}
                {...vocabulary}
              />
            </SettingContainer>
          </SettingsGroup>

          <PolicySettings binding={binding} onChange={update} showFallback />
        </>
      )}
    </div>
  );
};
