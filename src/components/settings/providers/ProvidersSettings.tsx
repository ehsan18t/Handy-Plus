import React, { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown, ChevronRight, Plus, Trash2 } from "lucide-react";
import {
  commands,
  type Credential,
  type PostProcessProvider,
} from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { Button } from "@/components/ui/Button";
import { Dialog } from "@/components/ui/Dialog";
import { Input } from "@/components/ui/Input";
import { SettingsGroup } from "@/components/ui/SettingsGroup";
import { Alert } from "@/components/ui/Alert";
import Badge from "@/components/ui/Badge";
import { providerSupports } from "./useProviders";

interface DraftState {
  /** Empty id means creating, otherwise editing that credential. */
  id: string;
  providerId: string;
  label: string;
  secret: string;
}

export const ProvidersSettings: React.FC = () => {
  const { t } = useTranslation("fork");
  const { settings, refreshSettings } = useSettings();

  const credentials = useMemo<Credential[]>(
    () => settings?.cloud_credentials ?? [],
    [settings],
  );
  const providers = useMemo<PostProcessProvider[]>(
    () => settings?.post_process_providers ?? [],
    [settings],
  );

  const [expanded, setExpanded] = useState<string | null>(null);
  const [draft, setDraft] = useState<DraftState | null>(null);
  const [revealed, setRevealed] = useState(false);
  const [baseUrl, setBaseUrl] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [testing, setTesting] = useState<Record<string, boolean>>({});
  const [results, setResults] = useState<
    Record<string, { ok: boolean; message: string } | null>
  >({});

  const keysFor = (providerId: string) =>
    credentials.filter((c) => c.provider_id === providerId);

  /** Same rule the rotation lists use to decide what to offer. */
  const capabilityLabel = (provider: PostProcessProvider) => {
    const speech = providerSupports(provider, "stt");
    const chat = providerSupports(provider, "post_process");
    const capability =
      speech && chat
        ? t("providers.capability.both")
        : speech
          ? t("providers.capability.sttOnly")
          : t("providers.capability.chatOnly");

    return provider.requires_credential === false
      ? t("providers.capability.keyless", { capability })
      : capability;
  };

  const draftProvider = providers.find((p) => p.id === draft?.providerId);
  const needsKey = draftProvider?.requires_credential !== false;
  const canSave =
    Boolean(draft?.label.trim()) &&
    (Boolean(draft?.id) || !needsKey || Boolean(draft?.secret.trim()));

  const save = async () => {
    if (!draft) return;
    const label = draft.label.trim();

    if (draftProvider?.allow_base_url_edit) {
      const next = baseUrl.trim();
      if (next && next !== draftProvider.base_url) {
        const urlResult = await commands.changePostProcessBaseUrlSetting(
          draft.providerId,
          next,
        );
        if (urlResult.status === "error") {
          setError(urlResult.error);
          return;
        }
      }
    }

    const result = draft.id
      ? await commands.updateCloudCredential(
          draft.id,
          label,
          draft.providerId,
          draft.secret.trim() ? draft.secret : null,
        )
      : await commands.addCloudCredential(
          label,
          draft.providerId,
          draft.secret,
        );

    if (result.status === "error") {
      setError(result.error);
      return;
    }
    setDraft(null);
    setError(null);
    await refreshSettings();
  };

  const remove = async (id: string) => {
    const result = await commands.deleteCloudCredential(id);
    if (result.status === "error") {
      setError(result.error);
      return;
    }
    await refreshSettings();
  };

  // Keyed by credential so two overlapping tests do not clobber each other.
  const test = async (id: string) => {
    setTesting((current) => ({ ...current, [id]: true }));
    setResults((current) => ({ ...current, [id]: null }));
    setError(null);

    const result = await commands.testCloudCredential(id);

    setTesting((current) => ({ ...current, [id]: false }));
    setResults((current) => ({
      ...current,
      [id]:
        result.status === "error"
          ? { ok: false, message: result.error }
          : {
              ok: true,
              message: t("credentials.testPassed", {
                count: result.data.length,
              }),
            },
    }));
    await refreshSettings();
  };

  /** Opening any draft clears banners left over from an earlier action. */
  const openDraft = (next: DraftState) => {
    setDraft(next);
    setRevealed(false);
    setError(null);
    setBaseUrl(providers.find((p) => p.id === next.providerId)?.base_url ?? "");
  };

  return (
    // The section container is a centred flex column, so a page without an
    // explicit width sizes to its widest content instead of the panel, and no
    // amount of `truncate` inside it can bite. Matches upstream's pages.
    <div className="max-w-3xl w-full mx-auto space-y-4">
      <div>
        <h1 className="text-lg font-semibold">{t("providers.title")}</h1>
        <p className="text-sm text-mid-gray">{t("providers.description")}</p>
      </div>

      {error && <Alert variant="error">{error}</Alert>}

      <SettingsGroup>
        {providers.map((provider) => {
          const keys = keysFor(provider.id);
          const isOpen = expanded === provider.id;

          return (
            <div key={provider.id}>
              <button
                type="button"
                className="flex items-center gap-2 w-full p-3 text-start"
                onClick={() => setExpanded(isOpen ? null : provider.id)}
              >
                {isOpen ? (
                  <ChevronDown size={16} className="shrink-0" />
                ) : (
                  <ChevronRight size={16} className="shrink-0" />
                )}
                <span className="flex-1 text-sm font-medium">
                  {provider.label}
                </span>
                {/* Without this a user adds an Anthropic key and only finds out
                    it cannot do speech two pages later, when it silently fails
                    to appear in the list. */}
                <span className="text-xs text-mid-gray">
                  {capabilityLabel(provider)}
                </span>
                <span className="text-xs text-mid-gray">
                  {t("providers.keyCount", { count: keys.length })}
                </span>
              </button>

              {isOpen && (
                <div className="px-3 pb-3 space-y-2">
                  {keys.map((credential) => {
                    const result = results[credential.id];
                    return (
                      <div key={credential.id} className="ps-6">
                        <div className="flex items-center gap-2 flex-wrap">
                          <button
                            type="button"
                            className="flex-1 min-w-32 text-start text-sm truncate"
                            onClick={() =>
                              // The stored secret never reaches the frontend; an
                              // empty field means "leave it alone".
                              openDraft({
                                id: credential.id,
                                providerId: credential.provider_id,
                                label: credential.label,
                                secret: "",
                              })
                            }
                          >
                            {credential.label}
                          </button>

                          <Badge
                            variant={
                              credential.validity === "valid"
                                ? "success"
                                : credential.validity === "invalid"
                                  ? "danger"
                                  : "secondary"
                            }
                          >
                            {t(
                              `credentials.validity.${credential.validity ?? "untested"}`,
                            )}
                          </Badge>

                          <Button
                            variant="secondary"
                            size="sm"
                            disabled={testing[credential.id] ?? false}
                            onClick={() => void test(credential.id)}
                          >
                            {testing[credential.id]
                              ? t("credentials.testing")
                              : t("credentials.test")}
                          </Button>

                          <Button
                            variant="danger-ghost"
                            size="sm"
                            aria-label={t("credentials.delete")}
                            onClick={() => {
                              if (
                                window.confirm(t("credentials.deleteConfirm"))
                              ) {
                                void remove(credential.id);
                              }
                            }}
                          >
                            <Trash2 size={14} />
                          </Button>
                        </div>

                        {result && (
                          <p
                            className={`text-xs mt-1 ${
                              result.ok ? "text-green-600" : "text-red-500"
                            }`}
                          >
                            {result.message}
                          </p>
                        )}
                      </div>
                    );
                  })}

                  <div className="ps-6">
                    <Button
                      variant="secondary"
                      size="sm"
                      onClick={() =>
                        openDraft({
                          id: "",
                          providerId: provider.id,
                          label: "",
                          secret: "",
                        })
                      }
                    >
                      <span className="flex items-center gap-1">
                        <Plus size={14} />
                        {t("credentials.add")}
                      </span>
                    </Button>
                  </div>
                </div>
              )}
            </div>
          );
        })}
      </SettingsGroup>

      <p className="text-xs text-mid-gray px-1">{t("credentials.testNote")}</p>

      <Dialog
        open={draft !== null}
        onOpenChange={(open) => !open && setDraft(null)}
        title={
          draft?.id ? t("credentials.editTitle") : t("credentials.addTitle")
        }
        closeLabel={t("credentials.cancel")}
        footer={
          <div className="flex justify-end gap-2">
            <Button variant="ghost" size="sm" onClick={() => setDraft(null)}>
              {t("credentials.cancel")}
            </Button>
            <Button
              variant="primary"
              size="sm"
              disabled={!canSave}
              onClick={() => void save()}
            >
              {t("credentials.save")}
            </Button>
          </div>
        }
      >
        {draft && (
          <div className="space-y-3">
            <div className="text-xs text-mid-gray">
              {draftProvider?.label ?? draft.providerId}
            </div>

            <label className="block space-y-1">
              <span className="text-xs text-mid-gray">
                {t("credentials.label")}
              </span>
              <Input
                className="w-full"
                value={draft.label}
                placeholder={t("credentials.labelPlaceholder")}
                onChange={(e) =>
                  setDraft({ ...draft, label: e.currentTarget.value })
                }
              />
            </label>

            {needsKey && (
              <label className="block space-y-1">
                <span className="text-xs text-mid-gray">
                  {t("credentials.secret")}
                </span>
                <div className="flex gap-2">
                  <Input
                    className="flex-1"
                    type={revealed ? "text" : "password"}
                    value={draft.secret}
                    placeholder={
                      draft.id
                        ? t("credentials.secretUnchanged")
                        : t("credentials.secretPlaceholder")
                    }
                    onChange={(e) =>
                      setDraft({ ...draft, secret: e.currentTarget.value })
                    }
                  />
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => setRevealed(!revealed)}
                  >
                    {revealed ? t("credentials.hide") : t("credentials.reveal")}
                  </Button>
                </div>
              </label>
            )}

            {/* Belongs to the provider, not the key, so editing it here changes
                it for every credential on that provider. Said plainly rather
                than hidden, since it is the only place it can be set. */}
            {draftProvider?.allow_base_url_edit && (
              <label className="block space-y-1">
                <span className="text-xs text-mid-gray">
                  {t("credentials.baseUrl")}
                </span>
                <Input
                  className="w-full"
                  value={baseUrl}
                  placeholder="http://localhost:11434/v1"
                  onChange={(e) => setBaseUrl(e.currentTarget.value)}
                />
                <span className="text-xs text-mid-gray">
                  {t("credentials.baseUrlShared")}
                </span>
              </label>
            )}

            {!needsKey && (
              <Alert variant="info">{t("credentials.noKeyNeeded")}</Alert>
            )}
          </div>
        )}
      </Dialog>
    </div>
  );
};
