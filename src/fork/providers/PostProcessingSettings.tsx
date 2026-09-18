import React, { useMemo } from "react";
import { useTranslation } from "react-i18next";
import type { LLMPrompt } from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { SettingsGroup } from "@/components/ui/SettingsGroup";
import { Alert } from "@/components/ui/Alert";
// Upstream's template panel, rendered as-is. Reusing it rather than porting it
// means no duplicate template state and no second set of CRUD commands.
import { PostProcessingSettingsPrompts } from "@/components/settings/PostProcessingSettingsPrompts";
import { ShortcutInput } from "@/components/settings/ShortcutInput";
import { PolicySettings } from "./PolicySettings";
import { RotationEditor } from "./RotationEditor";
import { useBinding, type BindingEdit } from "./useProviders";

export const PostProcessingSettings: React.FC = () => {
  const { t } = useTranslation("fork");
  const { settings } = useSettings();
  const { binding, save, error } = useBinding("post_process");

  const templates = useMemo<LLMPrompt[]>(
    () => settings?.post_process_prompts ?? [],
    [settings],
  );

  if (!binding) {
    return error ? <Alert variant="error">{error}</Alert> : null;
  }

  const update = (edit: BindingEdit) =>
    void save((current) =>
      typeof edit === "function" ? edit(current) : { ...current, ...edit },
    );

  // Matches the backend rule: an empty rotation means upstream's key runs.
  const rotationEmpty = (binding.entries ?? []).length === 0;
  const legacyProviderId = settings?.post_process_provider_id;
  const legacyProvider =
    legacyProviderId &&
    settings?.post_process_models?.[legacyProviderId]?.trim()
      ? settings?.post_process_providers?.find((p) => p.id === legacyProviderId)
      : undefined;

  return (
    <div className="max-w-3xl w-full mx-auto space-y-4">
      <div>
        <h1 className="text-lg font-semibold">{t("postProcess.title")}</h1>
        <p className="text-sm text-mid-gray">{t("postProcess.description")}</p>
      </div>

      {error && <Alert variant="error">{error}</Alert>}

      {/* Rendered exactly as upstream's page does: ShortcutInput supplies its
          own label and tooltip, so wrapping it in a SettingContainer stacks a
          third label on top and squeezes the key display. */}
      <SettingsGroup title={t("postProcess.shortcut")}>
        <ShortcutInput
          shortcutId="transcribe_with_post_process"
          descriptionMode="tooltip"
          grouped
        />
      </SettingsGroup>

      {/* Upstream's template panel. Its "selected prompt" is what the pool uses
          when a rotation entry names no instruction of its own. */}
      <SettingsGroup
        title={t("postProcess.templates")}
        description={t("postProcess.templatesDescription")}
      >
        <PostProcessingSettingsPrompts />
      </SettingsGroup>

      <RotationEditor
        capability="post_process"
        binding={binding}
        onChange={update}
        templates={templates}
      />

      {/* An empty rotation still post-processes, using whatever provider was
          configured before the fork. Saying so beats a user wondering why
          cleanup runs with no keys listed. */}
      {rotationEmpty && legacyProvider && (
        <p className="text-xs text-mid-gray px-1">
          {t("postProcess.usingLegacyProvider", {
            provider: legacyProvider.label,
          })}
        </p>
      )}

      <PolicySettings binding={binding} onChange={update} />
    </div>
  );
};
