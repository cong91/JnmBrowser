"use client";

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChromiumConfigForm } from "@/components/chromium-config-form";
import { SharedCamoufoxConfigForm } from "@/components/shared-camoufox-config-form";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ScrollArea } from "@/components/ui/scroll-area";
import { getBrowserDisplayName, isChromiumBrowser } from "@/lib/browser-utils";
import type {
  BrowserProfile,
  CamoufoxConfig,
  CamoufoxOS,
  ChromiumConfig,
} from "@/types";

const getCurrentOS = (): CamoufoxOS => {
  if (typeof navigator === "undefined") return "linux";
  const platform = navigator.platform.toLowerCase();
  if (platform.includes("win")) return "windows";
  if (platform.includes("mac")) return "macos";
  return "linux";
};

import { LoadingButton } from "./loading-button";
import { RippleButton } from "./ui/ripple";

interface CamoufoxConfigDialogProps {
  isOpen: boolean;
  onClose: () => void;
  profile: BrowserProfile | null;
  onSave: (profile: BrowserProfile, config: CamoufoxConfig) => Promise<void>;
  onSaveChromium?: (
    profile: BrowserProfile,
    config: ChromiumConfig,
  ) => Promise<void>;
  isRunning?: boolean;
  crossOsUnlocked?: boolean;
}

export function CamoufoxConfigDialog({
  isOpen,
  onClose,
  profile,
  onSave,
  onSaveChromium,
  isRunning = false,
  crossOsUnlocked = false,
}: CamoufoxConfigDialogProps) {
  const { t } = useTranslation();
  // Use union type to support both Camoufox and Chromium configs
  const [config, setConfig] = useState<CamoufoxConfig | ChromiumConfig>(() => ({
    geoip: true,
    os: getCurrentOS(),
  }));
  const [isSaving, setIsSaving] = useState(false);

  const isAntiDetectBrowser =
    profile?.browser === "camoufox" ||
    isChromiumBrowser(profile?.browser ?? "");

  // Initialize config when profile changes
  useEffect(() => {
    if (profile && isAntiDetectBrowser) {
      const profileConfig = isChromiumBrowser(profile.browser)
        ? profile.chromium_config
        : profile.camoufox_config;
      setConfig(
        profileConfig || {
          geoip: true,
          os: getCurrentOS(),
        },
      );
    }
  }, [profile, isAntiDetectBrowser]);

  const updateConfig = (
    key: keyof CamoufoxConfig | keyof ChromiumConfig,
    value: unknown,
  ) => {
    setConfig((prev) => ({ ...prev, [key]: value }));
  };

  const handleSave = async () => {
    if (!profile) return;

    // Validate fingerprint JSON if it exists
    if (config.fingerprint) {
      try {
        JSON.parse(config.fingerprint);
      } catch (_error) {
        const { toast } = await import("sonner");
        toast.error(t("camoufoxDialog.invalidFingerprint"), {
          description: t("camoufoxDialog.invalidFingerprintDescription"),
        });
        return;
      }
    }

    setIsSaving(true);
    try {
      if (isChromiumBrowser(profile.browser) && onSaveChromium) {
        await onSaveChromium(profile, config as ChromiumConfig);
      } else {
        await onSave(profile, config as CamoufoxConfig);
      }
      onClose();
    } catch (error) {
      console.error("Failed to save config:", error);
      const { toast } = await import("sonner");
      toast.error(t("camoufoxDialog.saveFailed"), {
        description:
          error instanceof Error
            ? error.message
            : t("camoufoxDialog.unknownError"),
      });
    } finally {
      setIsSaving(false);
    }
  };

  const handleClose = () => {
    // Reset config to original when closing without saving
    if (profile && isAntiDetectBrowser) {
      const profileConfig = isChromiumBrowser(profile.browser)
        ? profile.chromium_config
        : profile.camoufox_config;
      setConfig(
        profileConfig || {
          geoip: true,
          os: getCurrentOS(),
        },
      );
    }
    onClose();
  };

  if (!profile || !isAntiDetectBrowser) {
    return null;
  }

  const browserName = getBrowserDisplayName(profile.browser);

  return (
    <Dialog open={isOpen} onOpenChange={handleClose}>
      <DialogContent className="max-w-3xl max-h-[90vh] flex flex-col">
        <DialogHeader className="shrink-0">
          <DialogTitle>
            {isRunning
              ? t("camoufoxDialog.titleView", {
                  name: profile.name,
                  browser: browserName,
                })
              : t("camoufoxDialog.titleConfigure", {
                  name: profile.name,
                  browser: browserName,
                })}
          </DialogTitle>
        </DialogHeader>

        <ScrollArea className="flex-1 h-[300px]">
          <div className="py-4">
            {isChromiumBrowser(profile.browser) ? (
              <ChromiumConfigForm
                config={config as ChromiumConfig}
                onConfigChange={updateConfig}
                forceAdvanced={true}
                readOnly={isRunning}
                crossOsUnlocked={crossOsUnlocked}
                limitedMode={false}
                profileVersion={profile.version}
                profileBrowser="chromium"
              />
            ) : (
              <SharedCamoufoxConfigForm
                config={config as CamoufoxConfig}
                onConfigChange={updateConfig}
                forceAdvanced={true}
                readOnly={isRunning}
                browserType="camoufox"
                crossOsUnlocked={crossOsUnlocked}
                limitedMode={false}
                profileVersion={profile.version}
                profileBrowser="camoufox"
              />
            )}
          </div>
        </ScrollArea>

        <DialogFooter className="shrink-0 pt-4 border-t">
          <RippleButton variant="outline" onClick={handleClose}>
            {isRunning ? t("common.buttons.close") : t("common.buttons.cancel")}
          </RippleButton>
          {!isRunning && (
            <LoadingButton
              isLoading={isSaving}
              onClick={handleSave}
              disabled={isSaving}
            >
              {t("common.buttons.save")}
            </LoadingButton>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
