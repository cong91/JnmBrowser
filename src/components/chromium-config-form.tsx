"use client";

import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LoadingButton } from "@/components/loading-button";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import type {
  ChromiumConfig,
  ChromiumFingerprintConfig,
  ChromiumOS,
} from "@/types";

export interface ChromiumConfigFormProps {
  config: ChromiumConfig;
  onConfigChange: (key: keyof ChromiumConfig, value: unknown) => void;
  className?: string;
  isCreating?: boolean;
  forceAdvanced?: boolean;
  readOnly?: boolean;
  crossOsUnlocked?: boolean;
  limitedMode?: boolean;
  profileVersion?: string;
  profileBrowser?: string;
}

const getCurrentOS = (): ChromiumOS => {
  if (typeof navigator === "undefined") return "linux";
  const platform = navigator.platform.toLowerCase();
  if (platform.includes("win")) return "windows";
  if (platform.includes("mac")) return "macos";
  return "linux";
};

const osLabels: Record<ChromiumOS, string> = {
  windows: "Windows",
  macos: "macOS",
  linux: "Linux",
  android: "Android",
  ios: "iOS",
};

const sectionCardClass = "space-y-3 rounded-xl border bg-card/50 p-5";
const insetCardClass = "space-y-3 rounded-xl border bg-muted/30 p-5";

export function ChromiumConfigForm({
  config,
  onConfigChange,
  className = "",
  isCreating = false,
  forceAdvanced = false,
  readOnly = false,
  profileVersion,
  profileBrowser,
}: ChromiumConfigFormProps) {
  const { t } = useTranslation();
  const [activeTab, setActiveTab] = useState(
    forceAdvanced ? "manual" : "automatic",
  );
  const [fingerprintConfig, setFingerprintConfig] =
    useState<ChromiumFingerprintConfig>({});
  const [currentOS] = useState<ChromiumOS>(getCurrentOS);
  const [isGeneratingFingerprint, setIsGeneratingFingerprint] = useState(false);

  const handleGenerateFingerprint = async () => {
    if (!profileVersion) return;
    setIsGeneratingFingerprint(true);
    try {
      const configJson = JSON.stringify(config);
      const result = await invoke<string>("generate_sample_fingerprint", {
        browser: profileBrowser ?? "chromium",
        version: profileVersion,
        configJson,
      });
      onConfigChange("fingerprint", result);
    } catch (error) {
      console.error("Failed to generate fingerprint:", error);
    } finally {
      setIsGeneratingFingerprint(false);
    }
  };

  const selectedOS = config.os || currentOS;

  useEffect(() => {
    if (isCreating && typeof window !== "undefined") {
      const screenWidth = window.screen.width;
      const screenHeight = window.screen.height;

      if (!config.screen_max_width) {
        onConfigChange("screen_max_width", screenWidth);
      }
      if (!config.screen_max_height) {
        onConfigChange("screen_max_height", screenHeight);
      }
    }
  }, [
    isCreating,
    config.screen_max_width,
    config.screen_max_height,
    onConfigChange,
  ]);

  useEffect(() => {
    if (config.fingerprint) {
      try {
        const parsed = JSON.parse(
          config.fingerprint,
        ) as ChromiumFingerprintConfig;
        setFingerprintConfig(parsed);
      } catch (error) {
        console.error("Failed to parse fingerprint config:", error);
        setFingerprintConfig({});
      }
    } else {
      setFingerprintConfig({});
    }
  }, [config.fingerprint]);

  const updateFingerprintConfig = (
    key: keyof ChromiumFingerprintConfig,
    value: unknown,
  ) => {
    const newConfig = { ...fingerprintConfig };

    if (
      value === undefined ||
      value === "" ||
      (Array.isArray(value) && value.length === 0)
    ) {
      delete newConfig[key];
    } else {
      (newConfig as Record<string, unknown>)[key] = value;
    }

    setFingerprintConfig(newConfig);

    try {
      const jsonString = JSON.stringify(newConfig);
      onConfigChange("fingerprint", jsonString);
    } catch (error) {
      console.error("Failed to serialize fingerprint config:", error);
    }
  };

  const isAutoLocationEnabled = config.geoip !== false;

  const handleAutoLocationToggle = (enabled: boolean) => {
    if (enabled) {
      onConfigChange("geoip", true);
    } else {
      onConfigChange("geoip", false);
    }
  };

  const isEditingDisabled = readOnly;

  const renderAdvancedForm = () => (
    <div className="space-y-5">
      <Alert className="mb-8">
        <AlertDescription>
          {t("fingerprint.chromiumManualDescription")}
        </AlertDescription>
      </Alert>

      {/* Operating System Selection */}
      <div className={sectionCardClass}>
        <div className="flex items-center justify-between">
          <Label>{t("fingerprint.osLabel")}</Label>
          {profileVersion && (
            <LoadingButton
              isLoading={isGeneratingFingerprint}
              onClick={handleGenerateFingerprint}
              disabled={readOnly}
              variant="outline"
              size="sm"
            >
              {isCreating
                ? t("fingerprint.generateFingerprint")
                : t("fingerprint.refreshFingerprint")}
            </LoadingButton>
          )}
        </div>
        <Select
          value={selectedOS}
          onValueChange={(value: ChromiumOS) => {
            onConfigChange("os", value);
          }}
          disabled={readOnly}
        >
          <SelectTrigger>
            <SelectValue placeholder={t("fingerprint.selectOSPlaceholder")} />
          </SelectTrigger>
          <SelectContent>
            {(
              ["windows", "macos", "linux", "android", "ios"] as ChromiumOS[]
            ).map((os) => {
              return (
                <SelectItem key={os} value={os}>
                  <span className="flex items-center gap-2">
                    {osLabels[os]}
                  </span>
                </SelectItem>
              );
            })}
          </SelectContent>
        </Select>
        {selectedOS !== currentOS && (
          <Alert className="mt-2">
            <AlertDescription>
              {t("fingerprint.crossOsWarning")}
            </AlertDescription>
          </Alert>
        )}
      </div>

      {/* Randomize Fingerprint Option */}
      <div className={insetCardClass}>
        <div className="flex items-center space-x-2">
          <Checkbox
            id="randomize-fingerprint"
            checked={config.randomize_fingerprint_on_launch ?? false}
            onCheckedChange={(checked) => {
              onConfigChange("randomize_fingerprint_on_launch", checked);
            }}
            disabled={readOnly}
          />
          <Label htmlFor="randomize-fingerprint" className="font-medium">
            {t("fingerprint.generateRandomOnLaunch")}
          </Label>
        </div>
        <p className="text-sm text-muted-foreground ml-6">
          {t("fingerprint.generateRandomDescription")}
        </p>
      </div>

      {/* Automatic Location Configuration */}
      <div className={sectionCardClass}>
        <div className="flex items-center space-x-2">
          <Checkbox
            id="auto-location-advanced"
            checked={isAutoLocationEnabled}
            onCheckedChange={handleAutoLocationToggle}
            disabled={readOnly}
          />
          <Label htmlFor="auto-location-advanced">
            {t("fingerprint.autoLocationDescription")}
          </Label>
        </div>
      </div>

      {isEditingDisabled ? (
        <Alert className="mb-6">
          <AlertDescription>
            {t("fingerprint.editingDisabledRunning")}
          </AlertDescription>
        </Alert>
      ) : (
        <Alert className="mb-6">
          <AlertDescription>{t("fingerprint.basicWarning")}</AlertDescription>
        </Alert>
      )}

      <fieldset disabled={isEditingDisabled} className="space-y-5">
        <div className={sectionCardClass}>
          <Label>{t("fingerprint.userAgentAndPlatform")}</Label>
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <div className="space-y-2 md:col-span-2">
              <Label htmlFor="user-agent">{t("fingerprint.userAgent")}</Label>
              <Input
                id="user-agent"
                value={fingerprintConfig.userAgent ?? ""}
                onChange={(e) => {
                  updateFingerprintConfig(
                    "userAgent",
                    e.target.value || undefined,
                  );
                }}
                placeholder="Mozilla/5.0..."
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="platform">{t("fingerprint.platform")}</Label>
              <Input
                id="platform"
                value={fingerprintConfig.platform ?? ""}
                onChange={(e) => {
                  updateFingerprintConfig(
                    "platform",
                    e.target.value || undefined,
                  );
                }}
                placeholder={t(
                  "config.chromium.fingerprint.platformPlaceholder",
                )}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="platform-version">
                {t("fingerprint.platformVersion")}
              </Label>
              <Input
                id="platform-version"
                value={fingerprintConfig.platformVersion ?? ""}
                onChange={(e) => {
                  updateFingerprintConfig(
                    "platformVersion",
                    e.target.value || undefined,
                  );
                }}
                placeholder="e.g., 10.0.0"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="brand">{t("fingerprint.brand")}</Label>
              <Input
                id="brand"
                value={fingerprintConfig.brand ?? ""}
                onChange={(e) => {
                  updateFingerprintConfig("brand", e.target.value || undefined);
                }}
                placeholder="e.g., Google Chrome"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="brand-version">
                {t("fingerprint.brandVersion")}
              </Label>
              <Input
                id="brand-version"
                value={fingerprintConfig.brandVersion ?? ""}
                onChange={(e) => {
                  updateFingerprintConfig(
                    "brandVersion",
                    e.target.value || undefined,
                  );
                }}
                placeholder="e.g., 142"
              />
            </div>
          </div>
        </div>

        <div className={sectionCardClass}>
          <Label>{t("fingerprint.hardwareProperties")}</Label>
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <div className="space-y-2">
              <Label htmlFor="hardware-concurrency">
                {t("fingerprint.hardwareConcurrency")}
              </Label>
              <Input
                id="hardware-concurrency"
                type="number"
                value={fingerprintConfig.hardwareConcurrency ?? ""}
                onChange={(e) => {
                  updateFingerprintConfig(
                    "hardwareConcurrency",
                    e.target.value ? parseInt(e.target.value, 10) : undefined,
                  );
                }}
                placeholder="e.g., 8"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="device-memory">
                {t("fingerprint.deviceMemory")}
              </Label>
              <Input
                id="device-memory"
                type="number"
                value={fingerprintConfig.deviceMemory ?? ""}
                onChange={(e) => {
                  updateFingerprintConfig(
                    "deviceMemory",
                    e.target.value ? parseInt(e.target.value, 10) : undefined,
                  );
                }}
                placeholder="e.g., 8"
              />
            </div>
          </div>
        </div>

        <div className={sectionCardClass}>
          <Label>{t("fingerprint.languageAndLocale")}</Label>
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <div className="space-y-2">
              <Label htmlFor="language">
                {t("fingerprint.primaryLanguage")}
              </Label>
              <Input
                id="language"
                value={fingerprintConfig.language ?? ""}
                onChange={(e) => {
                  updateFingerprintConfig(
                    "language",
                    e.target.value || undefined,
                  );
                }}
                placeholder="e.g., en-US"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="languages">{t("fingerprint.languages")}</Label>
              <Input
                id="languages"
                value={
                  Array.isArray(fingerprintConfig.languages)
                    ? JSON.stringify(fingerprintConfig.languages)
                    : ""
                }
                onChange={(e) => {
                  if (!e.target.value) {
                    updateFingerprintConfig("languages", undefined);
                    return;
                  }
                  try {
                    const parsed = JSON.parse(e.target.value);
                    if (Array.isArray(parsed)) {
                      updateFingerprintConfig("languages", parsed);
                    }
                  } catch {
                    // Ignore incomplete JSON edits.
                  }
                }}
                placeholder='["en-US", "en"]'
              />
            </div>
          </div>
        </div>

        <div className={sectionCardClass}>
          <Label>{t("fingerprint.timezoneAndGeolocation")}</Label>
          <p className="text-sm text-muted-foreground">
            {t("fingerprint.chromiumSupportedFixedSettingsDescription")}
          </p>
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <div className="space-y-2">
              <Label htmlFor="timezone">{t("fingerprint.timezoneIana")}</Label>
              <Input
                id="timezone"
                value={fingerprintConfig.timezone ?? ""}
                onChange={(e) => {
                  updateFingerprintConfig(
                    "timezone",
                    e.target.value || undefined,
                  );
                }}
                placeholder="e.g., America/New_York"
              />
            </div>
          </div>
        </div>

        <div className={sectionCardClass}>
          <Label>{t("fingerprint.webglProperties")}</Label>
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <div className="space-y-2">
              <Label htmlFor="webgl-vendor">
                {t("fingerprint.webglVendor")}
              </Label>
              <Input
                id="webgl-vendor"
                value={fingerprintConfig.webglVendor ?? ""}
                onChange={(e) => {
                  updateFingerprintConfig(
                    "webglVendor",
                    e.target.value || undefined,
                  );
                }}
                placeholder="e.g., Intel"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="webgl-renderer">
                {t("fingerprint.webglRenderer")}
              </Label>
              <Input
                id="webgl-renderer"
                value={fingerprintConfig.webglRenderer ?? ""}
                onChange={(e) => {
                  updateFingerprintConfig(
                    "webglRenderer",
                    e.target.value || undefined,
                  );
                }}
                placeholder={t(
                  "config.chromium.fingerprint.webglRendererPlaceholder",
                )}
              />
            </div>
          </div>
        </div>
      </fieldset>
    </div>
  );

  return (
    <div className={`space-y-5 ${className}`}>
      {forceAdvanced ? (
        renderAdvancedForm()
      ) : (
        <Tabs
          value={activeTab}
          onValueChange={readOnly ? undefined : setActiveTab}
          className="w-full"
        >
          <TabsList className="grid grid-cols-2 w-full">
            <TabsTrigger value="automatic" disabled={readOnly}>
              {t("fingerprint.automatic")}
            </TabsTrigger>
            <TabsTrigger value="manual" disabled={readOnly}>
              {t("fingerprint.manual")}
            </TabsTrigger>
          </TabsList>

          <TabsContent value="automatic" className="space-y-5">
            <Alert className="mt-4">
              <AlertDescription>
                {t("fingerprint.chromiumAutomaticDescription")}
              </AlertDescription>
            </Alert>

            {/* Operating System Selection */}
            <div className={sectionCardClass}>
              <Label>{t("fingerprint.osLabel")}</Label>
              <Select
                value={selectedOS}
                onValueChange={(value: ChromiumOS) => {
                  onConfigChange("os", value);
                }}
                disabled={readOnly}
              >
                <SelectTrigger>
                  <SelectValue
                    placeholder={t("fingerprint.selectOSPlaceholder")}
                  />
                </SelectTrigger>
                <SelectContent>
                  {(
                    [
                      "windows",
                      "macos",
                      "linux",
                      "android",
                      "ios",
                    ] as ChromiumOS[]
                  ).map((os) => {
                    return (
                      <SelectItem key={os} value={os}>
                        <span className="flex items-center gap-2">
                          {osLabels[os]}
                        </span>
                      </SelectItem>
                    );
                  })}
                </SelectContent>
              </Select>
              {selectedOS !== currentOS && (
                <Alert className="mt-2">
                  <AlertDescription>
                    {t("fingerprint.crossOsLimitations")}
                  </AlertDescription>
                </Alert>
              )}
            </div>

            {/* Randomize Fingerprint Option */}
            <div className={insetCardClass}>
              <div className="flex items-center space-x-2">
                <Checkbox
                  id="randomize-fingerprint-auto"
                  checked={config.randomize_fingerprint_on_launch ?? false}
                  onCheckedChange={(checked) => {
                    onConfigChange("randomize_fingerprint_on_launch", checked);
                  }}
                  disabled={readOnly}
                />
                <Label
                  htmlFor="randomize-fingerprint-auto"
                  className="font-medium"
                >
                  {t("fingerprint.generateRandomOnLaunch")}
                </Label>
              </div>
              <p className="text-sm text-muted-foreground ml-6">
                {t("fingerprint.generateRandomDescription")}
              </p>
            </div>

            {/* Automatic Location Configuration */}
            <div className={sectionCardClass}>
              <div className="flex items-center space-x-2">
                <Checkbox
                  id="auto-location"
                  checked={isAutoLocationEnabled}
                  onCheckedChange={handleAutoLocationToggle}
                  disabled={isEditingDisabled}
                />
                <Label htmlFor="auto-location">
                  {t("fingerprint.autoLocationDescription")}
                </Label>
              </div>
            </div>

            {/* Screen Resolution */}
            <div className={sectionCardClass}>
              <fieldset disabled={isEditingDisabled} className="space-y-3">
                <Label>{t("fingerprint.screenResolution")}</Label>
                <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
                  <div className="space-y-2">
                    <Label htmlFor="screen-max-width">
                      {t("fingerprint.maxWidth")}
                    </Label>
                    <Input
                      id="screen-max-width"
                      type="number"
                      value={config.screen_max_width ?? ""}
                      onChange={(e) => {
                        onConfigChange(
                          "screen_max_width",
                          e.target.value
                            ? parseInt(e.target.value, 10)
                            : undefined,
                        );
                      }}
                      placeholder="e.g., 1920"
                    />
                  </div>
                  <div className="space-y-2">
                    <Label htmlFor="screen-max-height">
                      {t("fingerprint.maxHeight")}
                    </Label>
                    <Input
                      id="screen-max-height"
                      type="number"
                      value={config.screen_max_height ?? ""}
                      onChange={(e) => {
                        onConfigChange(
                          "screen_max_height",
                          e.target.value
                            ? parseInt(e.target.value, 10)
                            : undefined,
                        );
                      }}
                      placeholder="e.g., 1080"
                    />
                  </div>
                  <div className="space-y-2">
                    <Label htmlFor="screen-min-width">
                      {t("fingerprint.minWidth")}
                    </Label>
                    <Input
                      id="screen-min-width"
                      type="number"
                      value={config.screen_min_width ?? ""}
                      onChange={(e) => {
                        onConfigChange(
                          "screen_min_width",
                          e.target.value
                            ? parseInt(e.target.value, 10)
                            : undefined,
                        );
                      }}
                      placeholder="e.g., 800"
                    />
                  </div>
                  <div className="space-y-2">
                    <Label htmlFor="screen-min-height">
                      {t("fingerprint.minHeight")}
                    </Label>
                    <Input
                      id="screen-min-height"
                      type="number"
                      value={config.screen_min_height ?? ""}
                      onChange={(e) => {
                        onConfigChange(
                          "screen_min_height",
                          e.target.value
                            ? parseInt(e.target.value, 10)
                            : undefined,
                        );
                      }}
                      placeholder="e.g., 600"
                    />
                  </div>
                </div>
              </fieldset>
            </div>
          </TabsContent>

          <TabsContent value="manual" className="space-y-5">
            {renderAdvancedForm()}
          </TabsContent>
        </Tabs>
      )}
    </div>
  );
}
