import type React from "react";
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  commands,
  type Capability,
  type CapabilityBinding,
  type CredentialCapabilityStatus,
  type PostProcessProvider,
} from "@/bindings";
import { useSettings } from "@/hooks/useSettings";

/** Read from the settings store; it already lives in `AppSettings`. */
export const useBinding = (capability: Capability) => {
  const { settings, refreshSettings } = useSettings();
  const [error, setError] = useState<string | null>(null);

  const binding = settings?.cloud_bindings?.[capability] ?? null;

  const save = useCallback(
    async (next: CapabilityBinding) => {
      const result = await commands.setCloudBinding(capability, next);
      setError(result.status === "error" ? result.error : null);
      // The backend clamps and prunes what it stores, so re-read rather than
      // trusting the copy we sent.
      await refreshSettings();
    },
    [capability, refreshSettings],
  );

  return { binding, save, error };
};

/** Per-capability health for every rotation entry. */
export const useCredentialStatus = (capability: Capability) => {
  const [status, setStatus] = useState<CredentialCapabilityStatus[]>([]);

  const refresh = useCallback(async () => {
    const result = await commands.getCloudCredentialStatus(capability);
    if (result.status === "ok") {
      setStatus(result.data);
    }
  }, [capability]);

  useEffect(() => {
    void refresh();
    // A stale "3h left" is misleading, so re-read while the panel is open.
    const timer = setInterval(() => void refresh(), 30_000);
    return () => clearInterval(timer);
  }, [refresh]);

  // Keyed by credential AND model: one key serving two models has two
  // independent quotas, so it can be paused for one and ready for the other.
  const byEntry = useMemo(() => {
    const map = new Map<string, CredentialCapabilityStatus>();
    for (const item of status) {
      map.set(entryKey(item.credential_id, item.model), item);
    }
    return map;
  }, [status]);

  return { byEntry, refresh };
};

/**
 * Identifies a rotation entry: a credential can appear once per model.
 * Length-prefixed so no id or model can collide with a different pair.
 */
export const entryKey = (credentialId: string, model: string) =>
  `${credentialId.length}:${credentialId}:${model}`;

/** Mirrors `PostProcessProvider::supports` on the backend. */
export const providerSupports = (
  provider: PostProcessProvider | undefined,
  capability: Capability,
): boolean => {
  if (!provider) return false;
  // Pre-fork stores have no capabilities field; the backend reads that as
  // post-processing only.
  if (!(provider.capabilities ?? ["post_process"]).includes(capability)) {
    return false;
  }
  if (capability === "stt") return Boolean(provider.stt_endpoint?.trim());
  return true;
};

/**
 * Text that saves on blur, not per keystroke: every save rewrites the whole
 * settings file, and a slow one landing late would rewind the field.
 */
export const useDraftField = (
  value: string,
  commit: (next: string) => void,
) => {
  const [draft, setDraft] = useState(value);

  useEffect(() => {
    setDraft(value);
  }, [value]);

  return {
    value: draft,
    onChange: (
      event: React.ChangeEvent<HTMLInputElement | HTMLTextAreaElement>,
    ) => setDraft(event.currentTarget.value),
    onBlur: () => {
      if (draft !== value) commit(draft);
    },
  };
};

/** Human-readable remaining cooldown, e.g. "2h 5m". */
export const formatRemaining = (seconds: number): string => {
  if (seconds <= 0) return "";
  const days = Math.floor(seconds / 86_400);
  const hours = Math.floor((seconds % 86_400) / 3_600);
  const minutes = Math.floor((seconds % 3_600) / 60);

  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  // Round up so a live countdown never displays "0m" while still benched.
  return `${Math.max(1, minutes)}m`;
};
