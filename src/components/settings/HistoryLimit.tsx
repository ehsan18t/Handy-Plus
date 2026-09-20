import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../hooks/useSettings";
import { Input } from "../ui/Input";
import { SettingContainer } from "../ui/SettingContainer";

const MIN_HISTORY_LIMIT = 0;
const MAX_HISTORY_LIMIT = 1000;

interface HistoryLimitProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const HistoryLimit: React.FC<HistoryLimitProps> = ({
  descriptionMode = "inline",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting } = useSettings();

  const historyLimit = getSetting("history_limit") ?? 5;
  // Text while editing. Saving per keystroke would persist every intermediate
  // number, and under the "preserve limit" retention period each one runs the
  // count cleanup, so typing 20 over 100 deletes 98 entries and their
  // recordings before the second digit arrives.
  const [draft, setDraft] = useState(String(historyLimit));

  useEffect(() => {
    setDraft(String(historyLimit));
  }, [historyLimit]);

  const commit = () => {
    const parsed = Number.parseInt(draft, 10);
    const next = Number.isNaN(parsed)
      ? historyLimit
      : Math.min(MAX_HISTORY_LIMIT, Math.max(MIN_HISTORY_LIMIT, parsed));
    setDraft(String(next));
    if (next !== historyLimit) {
      updateSetting("history_limit", next);
    }
  };

  return (
    <SettingContainer
      title={t("settings.debug.historyLimit.title")}
      description={t("settings.debug.historyLimit.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
      layout="horizontal"
    >
      <div className="flex items-center space-x-2">
        <Input
          type="number"
          min={MIN_HISTORY_LIMIT}
          max={MAX_HISTORY_LIMIT}
          value={draft}
          onChange={(e) => setDraft(e.currentTarget.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.currentTarget.blur();
            }
          }}
          // Never disabled while the write is in flight: disabling a focused
          // input blurs it, so the keystrokes after the first one went nowhere.
          className="w-20"
        />
        <span className="text-sm text-text">
          {t("settings.debug.historyLimit.entries")}
        </span>
      </div>
    </SettingContainer>
  );
};
