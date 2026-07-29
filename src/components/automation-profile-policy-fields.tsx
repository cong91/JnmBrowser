"use client";

import { useEffect, useMemo } from "react";
import { useTranslation } from "react-i18next";
import type { AutomationProfilePolicy } from "@/components/automation-profile-policy";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useProfileEvents } from "@/hooks/use-profile-events";

const GENERATED_WORKER_VALUE = "__generated-worker__";

interface Props {
  idPrefix: string;
  browserType: string;
  value: AutomationProfilePolicy;
  onChange: (value: AutomationProfilePolicy) => void;
  disabled?: boolean;
  active?: boolean;
}

export function AutomationProfilePolicyFields({
  idPrefix,
  browserType,
  value,
  onChange,
  disabled = false,
  active = true,
}: Props) {
  const { t } = useTranslation();
  const {
    profiles,
    runningProfiles,
    leasedProfiles,
    isLoading,
    loadLeasedProfiles,
  } = useProfileEvents();
  const compatibleProfiles = useMemo(
    () =>
      profiles
        .filter((profile) =>
          browserType === "chromium"
            ? profile.browser === "chromium"
            : profile.browser === browserType,
        )
        .sort((left, right) => left.name.localeCompare(right.name)),
    [browserType, profiles],
  );
  const selectedProfile = compatibleProfiles.find(
    (profile) => profile.id === value.profileId,
  );
  const ephemeralUnsupported =
    selectedProfile?.browser === "camoufox" ||
    selectedProfile?.browser === "firefox";

  useEffect(() => {
    if (!active) return;
    void loadLeasedProfiles();
    const interval = setInterval(() => {
      void loadLeasedProfiles();
    }, 3000);
    return () => clearInterval(interval);
  }, [active, loadLeasedProfiles]);

  useEffect(() => {
    if (
      !isLoading &&
      value.profileId &&
      (!selectedProfile ||
        runningProfiles.has(value.profileId) ||
        leasedProfiles.has(value.profileId))
    ) {
      onChange({ ...value, profileId: "" });
    }
  }, [
    isLoading,
    leasedProfiles,
    onChange,
    runningProfiles,
    selectedProfile,
    value,
  ]);

  useEffect(() => {
    if (ephemeralUnsupported && value.dataMode === "ephemeral") {
      onChange({ ...value, dataMode: "persistent" });
    }
  }, [ephemeralUnsupported, onChange, value]);

  return (
    <div className="space-y-3 rounded-md border border-border bg-muted/20 p-3">
      <div className="space-y-2">
        <Label htmlFor={`${idPrefix}-profile`}>
          {t("automationProfile.profileLabel")}
        </Label>
        <Select
          value={value.profileId || GENERATED_WORKER_VALUE}
          onValueChange={(profileId) =>
            onChange({
              ...value,
              profileId: profileId === GENERATED_WORKER_VALUE ? "" : profileId,
            })
          }
          disabled={disabled || isLoading}
        >
          <SelectTrigger id={`${idPrefix}-profile`}>
            <SelectValue
              placeholder={
                isLoading
                  ? t("common.buttons.loading")
                  : t("profileSelector.chooseAProfile")
              }
            />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={GENERATED_WORKER_VALUE}>
              {t("automationProfile.generatedWorker")}
            </SelectItem>
            {compatibleProfiles.map((profile) => {
              const running = runningProfiles.has(profile.id);
              const leased = leasedProfiles.has(profile.id);
              const unavailable = running || leased;
              const option = leased
                ? t("automationProfile.leasedOption", { name: profile.name })
                : running
                  ? t("automationProfile.runningOption", { name: profile.name })
                  : profile.name;
              return (
                <SelectItem
                  key={profile.id}
                  value={profile.id}
                  disabled={unavailable}
                >
                  {option}
                </SelectItem>
              );
            })}
          </SelectContent>
        </Select>
        {!isLoading && compatibleProfiles.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            {t("profileSelector.noneAvailableShort")}
          </p>
        ) : null}
      </div>

      <div className="grid gap-3 sm:grid-cols-2">
        <div className="space-y-1.5">
          <div className="flex items-center gap-2">
            <Checkbox
              id={`${idPrefix}-ephemeral`}
              checked={value.dataMode === "ephemeral"}
              disabled={disabled || ephemeralUnsupported}
              onCheckedChange={(checked) =>
                onChange({
                  ...value,
                  dataMode: checked === true ? "ephemeral" : "persistent",
                })
              }
            />
            <Label htmlFor={`${idPrefix}-ephemeral`} className="font-normal">
              {t(
                value.dataMode === "ephemeral"
                  ? "automationProfile.ephemeralData"
                  : "automationProfile.persistentData",
              )}
            </Label>
          </div>
          <p className="text-xs text-muted-foreground">
            {t(
              ephemeralUnsupported
                ? "automationProfile.ephemeralUnsupported"
                : value.dataMode === "ephemeral"
                  ? "automationProfile.ephemeralDescription"
                  : "automationProfile.persistentDescription",
            )}
          </p>
        </div>

        <div className="space-y-1.5">
          <div className="flex items-center gap-2">
            <Checkbox
              id={`${idPrefix}-random-fingerprint`}
              checked={value.fingerprintMode === "randomPerLaunch"}
              disabled={disabled}
              onCheckedChange={(checked) =>
                onChange({
                  ...value,
                  fingerprintMode:
                    checked === true ? "randomPerLaunch" : "stable",
                })
              }
            />
            <Label
              htmlFor={`${idPrefix}-random-fingerprint`}
              className="font-normal"
            >
              {t(
                value.fingerprintMode === "randomPerLaunch"
                  ? "automationProfile.randomFingerprint"
                  : "automationProfile.stableFingerprint",
              )}
            </Label>
          </div>
          <p className="text-xs text-muted-foreground">
            {t(
              value.fingerprintMode === "randomPerLaunch"
                ? "automationProfile.randomFingerprintDescription"
                : "automationProfile.stableFingerprintDescription",
            )}
          </p>
        </div>
      </div>
    </div>
  );
}
