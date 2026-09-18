import React, { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ArrowDown, ArrowUp, GripVertical, Plus, Trash2 } from "lucide-react";
import {
  commands,
  type Capability,
  type CapabilityBinding,
  type Credential,
  type LLMPrompt,
  type PostProcessProvider,
  type RotationEntry,
} from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { Button } from "@/components/ui/Button";
import { SettingsGroup } from "@/components/ui/SettingsGroup";
import Badge from "@/components/ui/Badge";
import { RotationEntryDialog } from "./RotationEntryDialog";
import {
  entryKey,
  formatRemaining,
  providerSupports,
  useProviderInfo,
  useCredentialStatus,
  type BindingEdit,
} from "./useProviders";

interface RotationEditorProps {
  capability: Capability;
  binding: CapabilityBinding;
  onChange: (edit: BindingEdit) => void;
  /** Post-processing only: the instruction templates entries can name. */
  templates?: LLMPrompt[];
}

/** -1 means the dialog is adding rather than editing. */
const ADDING = -1;

export const RotationEditor: React.FC<RotationEditorProps> = ({
  capability,
  binding,
  onChange,
  templates,
}) => {
  const { t } = useTranslation("fork");
  const { settings, refreshSettings } = useSettings();
  const { byEntry, refresh } = useCredentialStatus(capability);

  const [editing, setEditing] = useState<number | null>(null);
  const [dragging, setDragging] = useState<number | null>(null);

  const credentials = useMemo<Credential[]>(
    () => settings?.cloud_credentials ?? [],
    [settings],
  );
  const providers = useMemo<PostProcessProvider[]>(
    () => settings?.post_process_providers ?? [],
    [settings],
  );

  /** Mirrors the backend eligibility rule so no key is offered that the pool
   * would silently skip. */
  const providerInfo = useProviderInfo();
  const eligible = useMemo(
    () =>
      credentials.filter((credential) =>
        providerSupports(providerInfo.get(credential.provider_id), capability),
      ),
    [credentials, providerInfo, capability],
  );

  const entries = binding.entries ?? [];

  // Every mutation is a function of the rotation as it stands when the edit is
  // applied, never of the array this render closed over. Two clicks inside one
  // round-trip used to make the second undo the first.
  const setEntries = (next: (current: RotationEntry[]) => RotationEntry[]) =>
    onChange((current: CapabilityBinding) => ({
      ...current,
      entries: next(current.entries ?? []),
    }));

  const moveTo = (from: number, to: number) =>
    setEntries((current) => {
      if (to < 0 || to >= current.length || from === to) return current;
      const next = [...current];
      const [moved] = next.splice(from, 1);
      next.splice(to, 0, moved);
      return next;
    });

  const removeEntry = (index: number) =>
    setEntries((current) => current.filter((_, i) => i !== index));

  const saveEntry = (entry: RotationEntry) => {
    setEntries((current) =>
      editing === ADDING
        ? [...current, entry]
        : current.map((existing, i) => (i === editing ? entry : existing)),
    );
    setEditing(null);
  };

  // Scoped to this row's model: the same key on another model may be paused for
  // a quota that really is spent.
  const clearCooldown = async (credentialId: string, model: string) => {
    await commands.clearCloudCooldown(capability, credentialId, model);
    await refresh();
    // The command also lifts the key-level Invalid mark, which lives in settings.
    await refreshSettings();
  };

  const credentialFor = (credentialId: string) =>
    credentials.find((c) => c.id === credentialId);

  const providerFor = (credentialId: string) =>
    providers.find((p) => p.id === credentialFor(credentialId)?.provider_id);

  const templateName = (promptId: string | null | undefined) =>
    templates?.find((template) => template.id === promptId)?.name ?? null;

  return (
    <SettingsGroup
      title={t("binding.keys")}
      description={t("binding.keysDescription")}
    >
      {entries.length === 0 && (
        <div className="p-4 text-sm text-mid-gray">
          {/* "None of your keys are the right kind" is nonsense to someone who
              has no keys at all, which is the common first run. */}
          {credentials.length === 0
            ? t("binding.noKeysAtAll")
            : eligible.length === 0
              ? t("binding.noEligibleKeys")
              : t("binding.noEntries")}
        </div>
      )}

      {entries.map((entry, index) => {
        const model = entry.model?.trim() ?? "";
        const status = byEntry.get(
          entryKey(entry.credential_id, entry.model ?? ""),
        );
        const cooling = (status?.cooldown_remaining_secs ?? 0) > 0;
        const invalid = status?.validity === "invalid";
        const provider = providerFor(entry.credential_id);

        return (
          <div
            // Keyed by the entry, not the position: rows reorder in place, and
            // an index key hands mounted state to whichever entry lands there.
            key={entryKey(entry.credential_id, entry.model ?? "")}
            draggable
            onDragStart={() => setDragging(index)}
            onDragEnd={() => setDragging(null)}
            onDragOver={(e) => e.preventDefault()}
            onDrop={(e) => {
              e.preventDefault();
              if (dragging !== null) moveTo(dragging, index);
              setDragging(null);
            }}
            className={`group flex min-w-0 items-center gap-2 px-3 py-2 hover:bg-mid-gray/10 ${
              dragging === index ? "opacity-40" : ""
            }`}
          >
            <GripVertical
              size={14}
              className="shrink-0 text-mid-gray cursor-grab"
            />

            <button
              type="button"
              className="flex flex-1 min-w-0 flex-col text-start"
              onClick={() => setEditing(index)}
            >
              <span className="text-sm font-medium truncate">
                {credentialFor(entry.credential_id)?.label ??
                  entry.credential_id}
              </span>
              <span className="text-xs text-mid-gray truncate">
                {[provider?.label, model, templateName(entry.prompt_id)]
                  .filter(Boolean)
                  .join(" · ")}
              </span>
            </button>

            {(cooling || invalid) && (
              <Button
                variant="secondary"
                size="sm"
                onClick={() =>
                  void clearCooldown(entry.credential_id, entry.model ?? "")
                }
              >
                {t("status.clearCooldown")}
              </Button>
            )}

            <Badge
              className="shrink-0"
              variant={
                invalid || !model
                  ? "danger"
                  : cooling || status?.recent_strikes
                    ? "secondary"
                    : "success"
              }
            >
              {!model
                ? t("status.needsModel")
                : invalid
                  ? t("status.invalid")
                  : cooling
                    ? t("status.coolingDown", {
                        time: formatRemaining(
                          status?.cooldown_remaining_secs ?? 0,
                        ),
                      })
                    : status?.recent_strikes
                      ? t("status.strikes", { count: status.recent_strikes })
                      : t("status.available")}
            </Badge>

            {/* Muted until the row is hovered or holds focus, so a list of keys
                does not read as a wall of icons. */}
            <div className="flex shrink-0 items-center opacity-40 transition-opacity group-hover:opacity-100 focus-within:opacity-100">
              <Button
                variant="ghost"
                size="sm"
                aria-label={t("credentials.moveUp")}
                disabled={index === 0}
                onClick={() => moveTo(index, index - 1)}
              >
                <ArrowUp size={14} />
              </Button>
              <Button
                variant="ghost"
                size="sm"
                aria-label={t("credentials.moveDown")}
                disabled={index === entries.length - 1}
                onClick={() => moveTo(index, index + 1)}
              >
                <ArrowDown size={14} />
              </Button>
              <Button
                variant="danger-ghost"
                size="sm"
                aria-label={t("binding.removeEntry")}
                onClick={() => removeEntry(index)}
              >
                <Trash2 size={14} />
              </Button>
            </div>
          </div>
        );
      })}

      {eligible.length > 0 && (
        <div className="p-3">
          <Button
            variant="secondary"
            size="sm"
            onClick={() => setEditing(ADDING)}
          >
            <span className="flex items-center gap-1">
              <Plus size={14} />
              {t("binding.addEntry")}
            </span>
          </Button>
        </div>
      )}

      <RotationEntryDialog
        open={editing !== null}
        capability={capability}
        entry={editing !== null && editing !== ADDING ? entries[editing] : null}
        credentials={eligible}
        providers={providers}
        templates={templates}
        taken={entries
          .filter((_, i) => i !== editing)
          .map((e) => `${e.credential_id}:${e.model?.trim() ?? ""}`)}
        onClose={() => setEditing(null)}
        onSave={saveEntry}
      />
    </SettingsGroup>
  );
};
