import type React from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  commands,
  type Capability,
  type CapabilityBinding,
  type CredentialCapabilityStatus,
  type ProviderInfo,
} from "@/bindings";
import { useSettings } from "@/hooks/useSettings";

export type BindingEdit =
  | Partial<CapabilityBinding>
  | ((current: CapabilityBinding) => CapabilityBinding);

/**
 * Read from the settings store, write through an optimistic overlay.
 *
 * The overlay is not cosmetic. `settings` only catches up after the command
 * round-trips and the store re-reads, and every edit used to be built from the
 * value rendered before that landed. Two edits made inside that window both
 * built on the pre-edit rotation, so the second silently reverted the first:
 * delete a row and immediately delete another, and the first one comes back.
 * Adding a credential and then touching anything else lost the credential the
 * same way, which is indistinguishable from "rotation does not work".
 *
 * Every edit is therefore a function of the current value, applied to the
 * newest queued value rather than the newest rendered one.
 */
export const useBinding = (capability: Capability) => {
  const { settings, refreshSettings } = useSettings();
  const [error, setError] = useState<string | null>(null);
  const [optimistic, setOptimistic] = useState<CapabilityBinding | null>(null);
  const queued = useRef<CapabilityBinding | null>(null);
  const inFlight = useRef(0);

  const server = settings?.cloud_bindings?.[capability] ?? null;

  // Adopt server state only when nothing of ours is in flight; otherwise an
  // unrelated settings write (a dictation finishing marks a key's validity)
  // would rewind an edit that has not been acknowledged yet.
  useEffect(() => {
    if (inFlight.current === 0) {
      queued.current = null;
      setOptimistic(null);
    }
  }, [server]);

  const binding = optimistic ?? server;

  const save = useCallback(
    async (edit: (current: CapabilityBinding) => CapabilityBinding) => {
      const base = queued.current ?? server;
      if (!base) return;

      const next = edit(base);
      queued.current = next;
      setOptimistic(next);

      inFlight.current += 1;
      try {
        const result = await commands.setCloudBinding(capability, next);
        setError(result.status === "error" ? result.error : null);
      } finally {
        inFlight.current -= 1;
      }
      // The backend clamps and prunes what it stores, so re-read rather than
      // trusting the copy we sent.
      await refreshSettings();
    },
    [capability, refreshSettings, server],
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

/**
 * Fork metadata for upstream's providers, keyed by provider id.
 *
 * It used to ride along on the provider objects in the settings store. It is
 * derived from the provider id on the backend and never stored, so it is
 * fetched once and cached for the session rather than re-read per render.
 */
let providerInfoCache: Map<string, ProviderInfo> | null = null;
let providerInfoInFlight: Promise<Map<string, ProviderInfo>> | null = null;

export const useProviderInfo = (): Map<string, ProviderInfo> => {
  const [info, setInfo] = useState(() => providerInfoCache ?? new Map());

  useEffect(() => {
    if (providerInfoCache) return;
    providerInfoInFlight ??= commands
      .getCloudProviders()
      .then((list) => {
        // An empty list means the backend could not answer, not that Handy has
        // no providers: it always ships several. Caching it would be the same
        // poisoning as caching a rejection.
        if (list.length === 0) throw new Error("no providers returned");
        providerInfoCache = new Map(list.map((entry) => [entry.id, entry]));
        return providerInfoCache;
      })
      .catch((cause) => {
        // Clearing the slot is what makes this retryable. Leaving a settled
        // rejected promise in it meant `??=` never fetched again, every
        // provider reported no capabilities for the rest of the session, and
        // the rotation editor offered no eligible keys and hid its Add button.
        // Only restarting the app recovered.
        providerInfoInFlight = null;
        throw cause;
      });

    let alive = true;
    void providerInfoInFlight
      .then((loaded) => {
        if (alive) setInfo(loaded);
      })
      .catch(() => {
        // Rendered as "no capabilities" until the next mount retries.
      });
    return () => {
      alive = false;
    };
  }, []);

  return info;
};

/**
 * Mirrors `providers::supports` on the backend. The capability list is already
 * effective rather than claimed, so there is no endpoint to re-check here.
 */
export const providerSupports = (
  info: ProviderInfo | undefined,
  capability: Capability,
): boolean => Boolean(info?.capabilities.includes(capability));

/** Mirrors `providers::requires_credential`. Unknown providers need a key. */
export const providerRequiresCredential = (info: ProviderInfo | undefined) =>
  info?.requires_credential !== false;

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
