"use client";

import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuEye, LuEyeOff } from "react-icons/lu";
import { LoadingButton } from "@/components/loading-button";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { showErrorToast, showSuccessToast } from "@/lib/toast-utils";
import type { SyncSettings } from "@/types";

interface SyncConfigDialogProps {
  isOpen: boolean;
  onClose: (loginOccurred?: boolean) => void;
}

export function SyncConfigDialog({ isOpen, onClose }: SyncConfigDialogProps) {
  const { t } = useTranslation();

  const [serverUrl, setServerUrl] = useState("");
  const [token, setToken] = useState("");
  const [isLoading, setIsLoading] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [isTesting, setIsTesting] = useState(false);
  const [showToken, setShowToken] = useState(false);
  const [connectionStatus, setConnectionStatus] = useState<
    "unknown" | "testing" | "connected" | "error"
  >("unknown");

  const hasConfig = Boolean(serverUrl && token);

  const testConnection = useCallback(async (url: string) => {
    setConnectionStatus("testing");
    try {
      const healthUrl = `${url.replace(/\/$/, "")}/health`;
      const response = await fetch(healthUrl);
      setConnectionStatus(response.ok ? "connected" : "error");
    } catch {
      setConnectionStatus("error");
    }
  }, []);

  const loadSettings = useCallback(async () => {
    setIsLoading(true);
    try {
      const settings = await invoke<SyncSettings>("get_sync_settings");
      setServerUrl(settings.sync_server_url ?? "");
      setToken(settings.sync_token ?? "");
      if (settings.sync_server_url && settings.sync_token) {
        void testConnection(settings.sync_server_url);
      }
    } catch (error) {
      console.error("Failed to load sync settings:", error);
    } finally {
      setIsLoading(false);
    }
  }, [testConnection]);

  useEffect(() => {
    if (isOpen) {
      setConnectionStatus("unknown");
      void loadSettings();
    }
  }, [isOpen, loadSettings]);

  const handleTestConnection = useCallback(async () => {
    if (!serverUrl) {
      showErrorToast(t("sync.config.serverUrlRequired"));
      return;
    }

    setIsTesting(true);
    setConnectionStatus("testing");
    try {
      const healthUrl = `${serverUrl.replace(/\/$/, "")}/health`;
      const response = await fetch(healthUrl);
      if (response.ok) {
        setConnectionStatus("connected");
        showSuccessToast(t("sync.config.connectionSuccess"));
      } else {
        setConnectionStatus("error");
        showErrorToast(t("sync.config.serverError"));
      }
    } catch {
      setConnectionStatus("error");
      showErrorToast(t("sync.config.connectFailed"));
    } finally {
      setIsTesting(false);
    }
  }, [serverUrl, t]);

  const handleSave = useCallback(async () => {
    setIsSaving(true);
    try {
      await invoke<SyncSettings>("save_sync_settings", {
        syncServerUrl: serverUrl || null,
        syncToken: token || null,
      });
      try {
        await invoke("restart_sync_service");
      } catch (e) {
        console.error("Failed to restart sync service:", e);
      }
      showSuccessToast(t("sync.config.settingsSaved"));
      onClose();
    } catch (error) {
      console.error("Failed to save sync settings:", error);
      showErrorToast(t("sync.config.saveFailed"));
    } finally {
      setIsSaving(false);
    }
  }, [serverUrl, token, onClose, t]);

  const handleDisconnect = useCallback(async () => {
    setIsSaving(true);
    try {
      await invoke<SyncSettings>("save_sync_settings", {
        syncServerUrl: null,
        syncToken: null,
      });
      try {
        await invoke("restart_sync_service");
      } catch (e) {
        console.error("Failed to restart sync service:", e);
      }
      setServerUrl("");
      setToken("");
      setConnectionStatus("unknown");
      showSuccessToast(t("sync.config.disconnected"));
    } catch (error) {
      console.error("Failed to disconnect:", error);
      showErrorToast(t("sync.config.disconnectFailed"));
    } finally {
      setIsSaving(false);
    }
  }, [t]);

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>{t("sync.cloud.selfHostedTabLabel")}</DialogTitle>
          <DialogDescription>{t("sync.description")}</DialogDescription>
        </DialogHeader>

        {isLoading ? (
          <div className="flex justify-center py-8">
            <div className="w-6 h-6 rounded-full border-2 border-current animate-spin border-t-transparent" />
          </div>
        ) : (
          <div className="grid gap-4 py-4">
            <div className="space-y-2">
              <Label htmlFor="sync-server-url">{t("sync.serverUrl")}</Label>
              <Input
                id="sync-server-url"
                placeholder={t("sync.serverUrlPlaceholder")}
                value={serverUrl}
                onChange={(e) => {
                  setServerUrl(e.target.value);
                }}
              />
            </div>

            <div className="space-y-2">
              <Label htmlFor="sync-token">{t("sync.token")}</Label>
              <div className="relative">
                <Input
                  id="sync-token"
                  type={showToken ? "text" : "password"}
                  placeholder={t("sync.tokenPlaceholder")}
                  value={token}
                  onChange={(e) => {
                    setToken(e.target.value);
                  }}
                  className="pr-10"
                />
                <Tooltip>
                  <TooltipTrigger asChild>
                    <button
                      type="button"
                      onClick={() => {
                        setShowToken(!showToken);
                      }}
                      className="absolute right-3 top-1/2 p-1 rounded-sm transition-colors transform -translate-y-1/2 hover:bg-accent"
                      aria-label={
                        showToken
                          ? t("common.aria.hideToken")
                          : t("common.aria.showToken")
                      }
                    >
                      {showToken ? (
                        <LuEyeOff className="w-4 h-4 text-muted-foreground hover:text-foreground" />
                      ) : (
                        <LuEye className="w-4 h-4 text-muted-foreground hover:text-foreground" />
                      )}
                    </button>
                  </TooltipTrigger>
                  <TooltipContent>
                    {showToken
                      ? t("common.aria.hideToken")
                      : t("common.aria.showToken")}
                  </TooltipContent>
                </Tooltip>
              </div>
            </div>

            {connectionStatus === "testing" && (
              <div className="flex gap-2 items-center text-sm text-muted-foreground">
                <div className="w-4 h-4 rounded-full border-2 border-current animate-spin border-t-transparent" />
                {t("sync.status.syncing")}
              </div>
            )}
            {connectionStatus === "connected" && (
              <div className="flex gap-2 items-center text-sm text-muted-foreground">
                <div className="w-2 h-2 rounded-full bg-success" />
                {t("sync.status.connected")}
              </div>
            )}
            {connectionStatus === "error" && (
              <div className="flex gap-2 items-center text-sm text-muted-foreground">
                <div className="w-2 h-2 rounded-full bg-destructive" />
                {t("sync.status.disconnected")}
              </div>
            )}
          </div>
        )}

        <DialogFooter className="flex gap-2">
          {hasConfig && (
            <Button
              variant="outline"
              onClick={() => void handleDisconnect()}
              disabled={isSaving}
            >
              {t("sync.config.disconnect")}
            </Button>
          )}
          <Button
            variant="outline"
            onClick={() => void handleTestConnection()}
            disabled={isTesting || !serverUrl}
          >
            {isTesting
              ? t("sync.config.testingConnection")
              : t("sync.config.testConnection")}
          </Button>
          <LoadingButton
            onClick={() => void handleSave()}
            isLoading={isSaving}
            disabled={!serverUrl || !token}
          >
            {t("common.buttons.save")}
          </LoadingButton>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
