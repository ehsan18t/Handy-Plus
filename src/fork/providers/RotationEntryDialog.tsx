import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type {
  Capability,
  Credential,
  LLMPrompt,
  PostProcessProvider,
  RotationEntry,
} from "@/bindings";
import { Button } from "@/components/ui/Button";
import { Dialog } from "@/components/ui/Dialog";
import { Dropdown } from "@/components/ui/Dropdown";
import { Alert } from "@/components/ui/Alert";
import { ModelField } from "./ModelField";

interface RotationEntryDialogProps {
  open: boolean;
  capability: Capability;
  /** The entry being edited, or null when adding. */
  entry: RotationEntry | null;
  credentials: Credential[];
  providers: PostProcessProvider[];
  /** Post-processing only. Absent hides the instruction field. */
  templates?: LLMPrompt[];
  /** Every (credential, model) pair already in the rotation except this one. */
  taken: string[];
  onClose: () => void;
  onSave: (entry: RotationEntry) => void;
}

export const RotationEntryDialog: React.FC<RotationEntryDialogProps> = ({
  open,
  capability,
  entry,
  credentials,
  providers,
  templates,
  taken,
  onClose,
  onSave,
}) => {
  const { t } = useTranslation("fork");
  const [credentialId, setCredentialId] = useState("");
  const [model, setModel] = useState("");
  const [promptId, setPromptId] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setCredentialId(entry?.credential_id ?? credentials[0]?.id ?? "");
    setModel(entry?.model ?? "");
    setPromptId(entry?.prompt_id ?? null);
  }, [open, entry, credentials]);

  const provider = providers.find(
    (p) => p.id === credentials.find((c) => c.id === credentialId)?.provider_id,
  );
  // Apple Intelligence keeps a token limit in this field, and blank is a valid
  // "use the default" there.
  const isTokenLimit = provider?.id === "apple_intelligence";
  const needsModel = !isTokenLimit && !model.trim();
  // The backend drops a repeated pair on save, so catching it here is the
  // difference between an explanation and a row that silently never appears.
  const duplicate = taken.includes(`${credentialId}:${model.trim()}`);
  const canSave = Boolean(credentialId) && !needsModel && !duplicate;

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => !next && onClose()}
      title={entry ? t("entryDialog.editTitle") : t("entryDialog.addTitle")}
      closeLabel={t("credentials.cancel")}
      footer={
        <div className="flex justify-end gap-2">
          <Button variant="ghost" size="sm" onClick={onClose}>
            {t("credentials.cancel")}
          </Button>
          <Button
            variant="primary"
            size="sm"
            disabled={!canSave}
            onClick={() =>
              onSave({
                credential_id: credentialId,
                model: model.trim(),
                prompt_id: promptId,
              })
            }
          >
            {entry ? t("credentials.save") : t("binding.addEntry")}
          </Button>
        </div>
      }
    >
      <div className="space-y-3">
        <label className="block space-y-1">
          <span className="text-xs text-mid-gray">{t("entryDialog.key")}</span>
          <Dropdown
            className="w-full"
            options={credentials.map((credential) => ({
              value: credential.id,
              label: `${credential.label} · ${
                providers.find((p) => p.id === credential.provider_id)?.label ??
                credential.provider_id
              }`,
            }))}
            selectedValue={credentialId || null}
            placeholder={t("entryDialog.keyPlaceholder")}
            onSelect={(value) => {
              setCredentialId(value);
              // A model id is provider-specific, so it never survives the swap.
              setModel("");
            }}
          />
        </label>

        {credentialId && (
          <ModelField
            credentialId={credentialId}
            capability={capability}
            value={model}
            isTokenLimit={isTokenLimit}
            onChange={setModel}
            autoLoad
            fullWidth
          />
        )}

        {templates && (
          <label className="block space-y-1">
            <span className="text-xs text-mid-gray">
              {t("binding.instruction")}
            </span>
            <Dropdown
              className="w-full"
              options={[
                { value: "", label: t("binding.useDefaultInstruction") },
                ...templates.map((template) => ({
                  value: template.id,
                  label: template.name,
                })),
              ]}
              selectedValue={
                templates.some((template) => template.id === promptId)
                  ? (promptId ?? "")
                  : ""
              }
              onSelect={(value) => setPromptId(value || null)}
            />
          </label>
        )}

        {duplicate && (
          <Alert variant="warning">{t("entryDialog.duplicate")}</Alert>
        )}
      </div>
    </Dialog>
  );
};
