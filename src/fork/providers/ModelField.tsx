import React, { useEffect, useId, useState, useRef } from "react";
import { useTranslation } from "react-i18next";
import { RefreshCcw } from "lucide-react";
import { commands, type Capability } from "@/bindings";
import { Input } from "@/components/ui/Input";

interface ModelFieldProps {
  credentialId: string;
  capability: Capability;
  value: string;
  onChange: (model: string) => void;
  /** Apple Intelligence stores a token limit here, not a model id. */
  isTokenLimit?: boolean;
  /** Fetch as soon as the credential is known instead of waiting for focus. */
  autoLoad?: boolean;
  fullWidth?: boolean;
}

/**
 * Free text backed by a fetched list, rather than a plain dropdown: endpoints
 * that cannot list models still have to be usable, and a key can reach models
 * the list does not mention.
 */
export const ModelField: React.FC<ModelFieldProps> = ({
  credentialId,
  capability,
  value,
  onChange,
  isTokenLimit = false,
  autoLoad = false,
  fullWidth = false,
}) => {
  const { t } = useTranslation("fork");
  const listId = useId();
  const [models, setModels] = useState<string[]>([]);
  const [loading, setLoading] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [draft, setDraft] = useState(value ?? "");
  const width = fullWidth ? "w-full" : "w-56";

  // A different credential can arrive without a remount, and its models would
  // otherwise still be the previous key's.
  useEffect(() => {
    generation.current += 1;
    setModels([]);
    setLoaded(false);
    setLoadError(null);
    if (autoLoad && !isTokenLimit && credentialId) void load();
  }, [credentialId]);

  useEffect(() => {
    setDraft(value ?? "");
  }, [value]);

  // Bumped whenever the credential changes, so a reply that arrives after the
  // switch is dropped instead of offering the previous key's models under the
  // new one.
  const generation = useRef(0);

  const load = async () => {
    const mine = ++generation.current;
    setLoading(true);
    setLoadError(null);
    const result = await commands.testCloudCredential(credentialId);
    if (mine !== generation.current) return;
    setLoading(false);
    if (result.status === "ok") {
      setModels(result.data);
      setLoaded(true);
    } else {
      // Left unloaded so refocusing retries.
      setLoadError(result.error);
    }
  };

  const commit = () => {
    if (draft !== value) onChange(draft);
  };

  if (isTokenLimit) {
    return (
      <label className={`flex flex-col gap-1 ${fullWidth ? "w-full" : ""}`}>
        <span className="text-xs text-mid-gray">{t("binding.tokenLimit")}</span>
        <Input
          className={width}
          type="number"
          min={0}
          value={draft}
          placeholder={t("binding.tokenLimitPlaceholder")}
          onChange={(e) => setDraft(e.currentTarget.value)}
          onBlur={commit}
        />
      </label>
    );
  }

  return (
    <label className="flex flex-col gap-1">
      <span className="text-xs text-mid-gray">{t("binding.model")}</span>
      <div className="flex items-center gap-1">
        <Input
          className={width}
          list={listId}
          value={draft}
          placeholder={
            capability === "stt"
              ? t("binding.modelPlaceholderStt")
              : t("binding.modelPlaceholderChat")
          }
          onFocus={() => {
            if (!loaded && !loading) void load();
          }}
          onChange={(e) => setDraft(e.currentTarget.value)}
          onBlur={commit}
        />
        <button
          type="button"
          className="p-1 text-mid-gray hover:text-text"
          aria-label={t("binding.refreshModels")}
          onClick={() => void load()}
        >
          <RefreshCcw size={14} className={loading ? "animate-spin" : ""} />
        </button>
      </div>
      {loadError && (
        <span className="text-xs text-red-500 max-w-56">
          {t("binding.modelsUnavailable", { error: loadError })}
        </span>
      )}
      <datalist id={listId}>
        {models.map((model) => (
          <option key={model} value={model} />
        ))}
      </datalist>
    </label>
  );
};
