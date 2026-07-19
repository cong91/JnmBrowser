"use client";

import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuLogIn, LuRocket } from "react-icons/lu";
import { toast } from "sonner";
import { LoginAccountsTable } from "@/components/login-accounts-table";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
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
import { Textarea } from "@/components/ui/textarea";
import {
  type LoginNetworkMode,
  useLoginEvents,
} from "@/hooks/use-login-events";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function AccountLoginDialog({ open, onOpenChange }: Props) {
  const { t } = useTranslation();
  const {
    progressMap,
    accounts,
    loading,
    startLogin,
    cancelLogin,
    refreshAccounts,
    deleteAccount,
    updateAccountStatus,
    exportAccountsJson,
    pushAccountsToSub2api,
  } = useLoginEvents();

  const [credentialsText, setCredentialsText] = useState("");
  const [proxyId, setProxyId] = useState("");
  const [browserType, setBrowserType] = useState("chromium");
  const [maxRetries, setMaxRetries] = useState(3);
  const [headless, setHeadless] = useState(false);
  // Only none|proxy are implemented for login; Nord is not wired in engine.
  const [networkMode, setNetworkMode] = useState<LoginNetworkMode>("none");

  const [sub2apiUrl, setSub2apiUrl] = useState("http://localhost:3000");
  const [sub2apiKey, setSub2apiKey] = useState("");
  const [pushToSub2api, setPushToSub2api] = useState(true);

  const [smsEnabled, setSmsEnabled] = useState(true);
  const [smsServiceId, setSmsServiceId] = useState("");
  const [smsNetwork, setSmsNetwork] = useState("VINAPHONE");
  const [smsCountry, setSmsCountry] = useState("vn");
  const [smsTokenOverride, setSmsTokenOverride] = useState("");
  const [activeTab, setActiveTab] = useState("login");
  const [activeTaskId, setActiveTaskId] = useState<string | null>(null);

  // Load Sub2API settings when dialog opens
  useEffect(() => {
    if (open) {
      invoke<[string, string]>("get_sub2api_settings_cmd")
        .then(([url, key]) => {
          if (url) setSub2apiUrl(url);
          if (key) setSub2apiKey(key);
        })
        .catch(() => {
          // Ignore errors - settings might not exist yet
        });
    }
  }, [open]);

  // Save Sub2API settings when they change (debounced)
  useEffect(() => {
    if (!open) return;

    const timeoutId = setTimeout(() => {
      if (sub2apiUrl && sub2apiKey) {
        invoke("set_sub2api_settings_cmd", {
          url: sub2apiUrl,
          apiKey: sub2apiKey,
        }).catch((e) => {
          console.error("Failed to save Sub2API settings:", e);
        });
      }
    }, 1000);

    return () => clearTimeout(timeoutId);
  }, [sub2apiUrl, sub2apiKey, open]);

  const progressList = Array.from(progressMap.values());

  const parseCredentials = (
    text: string,
  ): Array<{ email: string; password: string; totpSecret: string }> => {
    return text
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line.length > 0)
      .map((line) => {
        const parts = line.split("|").map((p) => p.trim());
        return {
          email: parts[0] || "",
          password: parts[1] || "",
          totpSecret: parts[2] || "",
        };
      })
      .filter((c) => c.email && c.password);
  };

  const credentialCount = parseCredentials(credentialsText).length;

  const handleStart = async () => {
    const credentials = parseCredentials(credentialsText);
    if (credentials.length === 0) {
      toast.error(t("autoLogin.credentialsRequired"));
      return;
    }

    if (networkMode === "proxy" && !proxyId.trim()) {
      toast.error(t("registration.proxyRequired"));
      return;
    }

    if (pushToSub2api && (!sub2apiUrl.trim() || !sub2apiKey.trim())) {
      toast.error(t("autoLogin.sub2apiRequired"));
      return;
    }

    if (smsEnabled && !smsServiceId.trim()) {
      toast.error(t("registration.smsServiceIdPlaceholder"));
      return;
    }

    try {
      const taskId = await startLogin({
        credentialsText,
        credentials,
        browserType: browserType as "chromium" | "camoufox",
        maxRetries,
        headless,
        concurrency: 1,
        sub2apiUrl: pushToSub2api ? sub2apiUrl.trim() : "",
        sub2apiApiKey: pushToSub2api ? sub2apiKey.trim() : "",
        pushToSub2api,
        smsProvider: smsEnabled ? "viotp" : undefined,
        smsServiceId: smsEnabled
          ? Number(smsServiceId) || undefined
          : undefined,
        smsNetwork: smsEnabled ? smsNetwork.trim() || undefined : undefined,
        smsCountry: smsEnabled ? smsCountry : undefined,
        smsToken: smsEnabled ? smsTokenOverride.trim() || undefined : undefined,
        // Rust LoginNetworkMode expects unit enum string: "none" | "proxy" | "nord"
        networkMode,
        proxyId:
          networkMode === "proxy" ? proxyId.trim() || undefined : undefined,
      });
      setActiveTaskId(taskId);
      setActiveTab("progress");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    }
  };

  const handleCancel = async () => {
    if (!activeTaskId) return;
    try {
      await cancelLogin(activeTaskId);
      toast.success(t("common.buttons.cancel"));
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        className={
          activeTab === "stored"
            ? "flex max-h-[90vh] max-w-6xl flex-col overflow-hidden"
            : "flex max-h-[85vh] max-w-2xl flex-col overflow-hidden"
        }
      >
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <LuLogIn className="h-5 w-5 text-primary" />
            {t("autoLogin.title")}
          </DialogTitle>
        </DialogHeader>

        <Tabs value={activeTab} onValueChange={setActiveTab}>
          <TabsList className="grid w-full grid-cols-3">
            <TabsTrigger value="login">{t("autoLogin.tabLogin")}</TabsTrigger>
            <TabsTrigger value="progress">
              {t("autoLogin.tabProgress")}
              {progressList.length > 0 && ` (${progressList.length})`}
            </TabsTrigger>
            <TabsTrigger value="stored">
              {t("autoLogin.tabStored")}
              {accounts.length > 0 && ` (${accounts.length})`}
            </TabsTrigger>
          </TabsList>

          <TabsContent value="login" className="space-y-4 overflow-y-auto">
            <div className="space-y-2">
              <Label>{t("autoLogin.credentialsLabel")}</Label>
              <Textarea
                value={credentialsText}
                onChange={(e) => setCredentialsText(e.target.value)}
                placeholder={t("autoLogin.credentialsPlaceholder")}
                rows={6}
                className="font-mono text-sm"
              />
              <p className="text-xs text-muted-foreground">
                {t("autoLogin.credentialsHint", { count: credentialCount })}
              </p>
            </div>

            <div className="grid grid-cols-2 gap-4">
              <div className="space-y-2">
                <Label>{t("registration.browserType")}</Label>
                <Select value={browserType} onValueChange={setBrowserType}>
                  <SelectTrigger>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="chromium">Chromium</SelectItem>
                    <SelectItem value="camoufox">Camoufox</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div className="space-y-2">
                <Label>{t("registration.maxRetries")}</Label>
                <Input
                  type="number"
                  min={1}
                  max={10}
                  value={maxRetries}
                  onChange={(e) => setMaxRetries(Number(e.target.value))}
                />
              </div>
            </div>

            <div className="space-y-2">
              <Label>{t("registration.networkMode")}</Label>
              <Select
                value={networkMode}
                onValueChange={(v) => setNetworkMode(v as LoginNetworkMode)}
              >
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="none">
                    {t("registration.networkNone")}
                  </SelectItem>
                  <SelectItem value="proxy">
                    {t("registration.networkProxy")}
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>

            {networkMode === "proxy" && (
              <div className="space-y-2">
                <Label>{t("registration.proxyId")}</Label>
                <Input
                  value={proxyId}
                  onChange={(e) => setProxyId(e.target.value)}
                  placeholder={t("registration.proxyIdPlaceholder")}
                />
              </div>
            )}

            <div className="space-y-3 rounded-lg border border-border p-3">
              <h4 className="text-sm font-semibold">
                {t("autoLogin.sub2apiSection")}
              </h4>
              <div className="flex items-center gap-2">
                <input
                  type="checkbox"
                  id="pushToSub2api"
                  checked={pushToSub2api}
                  onChange={(e) => setPushToSub2api(e.target.checked)}
                  className="h-4 w-4"
                />
                <Label htmlFor="pushToSub2api" className="cursor-pointer">
                  {t("autoLogin.pushToSub2api")}
                </Label>
              </div>
              {pushToSub2api && (
                <div className="space-y-2">
                  <div className="space-y-2">
                    <Label>{t("autoLogin.sub2apiUrl")}</Label>
                    <Input
                      value={sub2apiUrl}
                      onChange={(e) => setSub2apiUrl(e.target.value)}
                      placeholder="http://localhost:3000"
                    />
                  </div>
                  <div className="space-y-2">
                    <Label>{t("autoLogin.sub2apiKey")}</Label>
                    <Input
                      type="password"
                      value={sub2apiKey}
                      onChange={(e) => setSub2apiKey(e.target.value)}
                      placeholder={t("autoLogin.sub2apiKeyPlaceholder")}
                    />
                  </div>
                </div>
              )}
            </div>

            <div className="space-y-3 rounded-lg border border-border p-3">
              <div className="flex items-center gap-2">
                <input
                  type="checkbox"
                  id="smsEnabled"
                  checked={smsEnabled}
                  onChange={(e) => setSmsEnabled(e.target.checked)}
                  className="h-4 w-4"
                />
                <Label htmlFor="smsEnabled" className="cursor-pointer">
                  {t("registration.smsEnabled")}
                </Label>
              </div>
              {smsEnabled && (
                <div className="space-y-2">
                  <div className="grid grid-cols-2 gap-2">
                    <div className="space-y-1">
                      <Label className="text-xs">
                        {t("registration.smsServiceId")}
                      </Label>
                      <Input
                        value={smsServiceId}
                        onChange={(e) => setSmsServiceId(e.target.value)}
                        placeholder={t("registration.smsServiceIdPlaceholder")}
                      />
                    </div>
                    <div className="space-y-1">
                      <Label className="text-xs">
                        {t("registration.smsNetwork")}
                      </Label>
                      <Input
                        value={smsNetwork}
                        onChange={(e) => setSmsNetwork(e.target.value)}
                        placeholder="VINAPHONE"
                      />
                    </div>
                  </div>
                  <div className="grid grid-cols-2 gap-2">
                    <div className="space-y-1">
                      <Label className="text-xs">
                        {t("registration.smsCountry")}
                      </Label>
                      <Select value={smsCountry} onValueChange={setSmsCountry}>
                        <SelectTrigger>
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          <SelectItem value="vn">Vietnam</SelectItem>
                          <SelectItem value="la">Laos</SelectItem>
                        </SelectContent>
                      </Select>
                    </div>
                    <div className="space-y-1">
                      <Label className="text-xs">
                        {t("registration.smsTokenOverride")}
                      </Label>
                      <Input
                        type="password"
                        value={smsTokenOverride}
                        onChange={(e) => setSmsTokenOverride(e.target.value)}
                        placeholder={t("registration.smsTokenPlaceholder")}
                      />
                    </div>
                  </div>
                </div>
              )}
            </div>

            <div className="flex items-center gap-2">
              <input
                type="checkbox"
                id="headless"
                checked={headless}
                onChange={(e) => setHeadless(e.target.checked)}
                className="h-4 w-4"
              />
              <Label htmlFor="headless" className="cursor-pointer">
                {t("registration.headless")}
              </Label>
            </div>

            <div className="flex gap-2">
              <Button
                onClick={handleStart}
                disabled={loading || credentialCount === 0}
                className="flex-1"
              >
                <LuRocket className="mr-2 h-4 w-4" />
                {t("autoLogin.startButton", { count: credentialCount })}
              </Button>
              {activeTaskId && (
                <Button
                  variant="outline"
                  onClick={handleCancel}
                  disabled={loading}
                >
                  {t("common.buttons.cancel")}
                </Button>
              )}
            </div>
          </TabsContent>

          <TabsContent value="progress" className="space-y-3 overflow-y-auto">
            {activeTaskId && (
              <div className="flex justify-end">
                <Button variant="outline" size="sm" onClick={handleCancel}>
                  {t("common.buttons.cancel")}
                </Button>
              </div>
            )}
            {progressList.length === 0 ? (
              <p className="text-center text-muted-foreground">
                {t("autoLogin.noProgress")}
              </p>
            ) : (
              progressList.map((progress) => (
                <div
                  key={`${progress.taskId}-${progress.credentialIndex}-${progress.step}-${progress.timestamp}`}
                  className="rounded-lg border border-border p-3"
                >
                  <div className="flex items-center justify-between">
                    <span className="text-sm font-medium">
                      {progress.credentialIndex + 1}/{progress.totalCredentials}
                    </span>
                    <span className="text-xs text-muted-foreground">
                      {progress.step}
                    </span>
                  </div>
                  <p className="text-sm">{progress.message}</p>
                  {progress.result && (
                    <p
                      className={`mt-1 text-xs ${
                        progress.result.success
                          ? "text-success"
                          : "text-destructive"
                      }`}
                    >
                      {progress.result.success
                        ? t("autoLogin.successMessage")
                        : progress.result.errorMessage}
                      {progress.result.pushError
                        ? ` · ${progress.result.pushError}`
                        : ""}
                    </p>
                  )}
                </div>
              ))
            )}
          </TabsContent>

          <TabsContent value="stored" className="min-h-0 overflow-y-auto">
            <LoginAccountsTable
              accounts={accounts}
              onDelete={deleteAccount}
              onRefresh={refreshAccounts}
              onUpdateStatus={updateAccountStatus}
              onExportJson={(ids, options) =>
                exportAccountsJson(ids, {
                  markExported: options?.markExported ?? false,
                })
              }
              onPush={(ids) =>
                pushAccountsToSub2api(ids, {
                  sub2apiUrl: sub2apiUrl.trim() || undefined,
                  sub2apiApiKey: sub2apiKey.trim() || undefined,
                })
              }
              pushEnabled={Boolean(sub2apiUrl.trim() && sub2apiKey.trim())}
            />
          </TabsContent>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}
