import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { CapabilityBinding } from "@/bindings";
import { Dropdown } from "@/components/ui/Dropdown";
import { Input } from "@/components/ui/Input";
import { SettingContainer } from "@/components/ui/SettingContainer";
import { SettingsGroup } from "@/components/ui/SettingsGroup";
import { ToggleSwitch } from "@/components/ui/ToggleSwitch";

const COOLDOWN_CHOICES = [1, 3, 6, 12, 24, 72, 168] as const;
/** Mirrors the backend clamp in `set_cloud_binding`. */
const MAX_STRIKE_THRESHOLD = 20;

interface PolicySettingsProps {
  binding: CapabilityBinding;
  onChange: (patch: Partial<CapabilityBinding>) => void;
  /** Speech only: falling back means running the local model. */
  showFallback?: boolean;
}

export const PolicySettings: React.FC<PolicySettingsProps> = ({
  binding,
  onChange,
  showFallback = false,
}) => {
  const { t } = useTranslation("fork");
  const [threshold, setThreshold] = useState(
    String(binding.strike_threshold ?? 3),
  );

  useEffect(() => {
    setThreshold(String(binding.strike_threshold ?? 3));
  }, [binding.strike_threshold]);

  const cooldownOptions = COOLDOWN_CHOICES.map((hours) => ({
    value: String(hours * 3600),
    label:
      hours >= 24
        ? t("binding.days", { count: hours / 24 })
        : t("binding.hours", { count: hours }),
  }));

  return (
    <SettingsGroup title={t("binding.rotationTitle")}>
      <SettingContainer
        title={t("binding.policy")}
        description={t("binding.policyDescription")}
        descriptionMode="tooltip"
        grouped
      >
        <Dropdown
          className="w-56"
          options={[
            { value: "round_robin", label: t("binding.policyRoundRobin") },
            {
              value: "least_recently_used",
              label: t("binding.policyLeastRecentlyUsed"),
            },
          ]}
          selectedValue={binding.policy ?? "round_robin"}
          onSelect={(value) =>
            onChange({ policy: value as CapabilityBinding["policy"] })
          }
        />
      </SettingContainer>

      <SettingContainer
        title={t("binding.cooldown")}
        description={t("binding.cooldownDescription")}
        descriptionMode="tooltip"
        grouped
      >
        <Dropdown
          className="w-56"
          options={cooldownOptions}
          selectedValue={String(binding.cooldown_secs ?? 21600)}
          onSelect={(value) => onChange({ cooldown_secs: Number(value) })}
        />
      </SettingContainer>

      <SettingContainer
        title={t("binding.strikeThreshold")}
        description={t("binding.strikeThresholdDescription")}
        descriptionMode="tooltip"
        grouped
      >
        <Input
          className="w-20"
          type="number"
          min={1}
          max={MAX_STRIKE_THRESHOLD}
          value={threshold}
          // Text while editing: clamping per keystroke turns a backspace into a
          // saved 1 and snaps the field back.
          onChange={(e) => setThreshold(e.currentTarget.value)}
          onBlur={() => {
            const parsed = Number.parseInt(threshold, 10);
            const next = Number.isNaN(parsed)
              ? (binding.strike_threshold ?? 3)
              : Math.min(MAX_STRIKE_THRESHOLD, Math.max(1, parsed));
            setThreshold(String(next));
            if (next !== binding.strike_threshold) {
              onChange({ strike_threshold: next });
            }
          }}
        />
      </SettingContainer>

      {showFallback && (
        <ToggleSwitch
          checked={binding.fallback_enabled ?? true}
          onChange={(fallback_enabled) => onChange({ fallback_enabled })}
          label={t("binding.fallback")}
          description={t("binding.fallbackDescription")}
          descriptionMode="tooltip"
          grouped
        />
      )}
    </SettingsGroup>
  );
};
