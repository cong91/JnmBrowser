"use client";

import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuLogIn, LuRocket } from "react-icons/lu";
import { toast } from "sonner";
import { LoginAccountsTable } from "@/components/login-accounts-table";
import { SmsProviderFields } from "@/components/sms-provider-fields";
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
  type LoginResult,
  useLoginEvents,
} from "@/hooks/use-login-events";
import { useVpnEvents } from "@/hooks/use-vpn-events";

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
    updateAccountFields,
    exportAccountsJson,
    pushAccountsToSub2api,
  } = useLoginEvents();

  const { vpnConfigs, isLoading: isLoadingVpns } = useVpnEvents();

  const [credentialsText, setCredentialsText] = useState("");
  const [proxyId, setProxyId] = useState("");
  const [vpnId, setVpnId] = useState("");
  const [rotateEveryN, setRotateEveryN] = useState(1);
  const [browserType, setBrowserType] = useState("chromium");
  const [maxRetries, setMaxRetries] = useState(3);
  const [headless, setHeadless] = useState(false);
  // none | proxy | vpn (inventory WireGuard / Nord conf). Prefer VPN over Nord CLI.
  const [networkMode, setNetworkMode] = useState<LoginNetworkMode>("none");

  const [sub2apiUrl, setSub2apiUrl] = useState("http://localhost:3000");
  const [sub2apiKey, setSub2apiKey] = useState("");
  // Optional: only validate/require credentials when the user enables push.
  const [pushToSub2api, setPushToSub2api] = useState(false);

  const [smsEnabled, setSmsEnabled] = useState(true);
  const [smsServiceId, setSmsServiceId] = useState("");
  const [smsNetwork, setSmsNetwork] = useState("");
  const [smsCountry, setSmsCountry] = useState("vn");
  const [smsTokenOverride, setSmsTokenOverride] = useState("");
  const [hasSavedSmsToken, setHasSavedSmsToken] = useState(false);
  const [activeTab, setActiveTab] = useState("login");
  const [activeTaskId, setActiveTaskId] = useState<string | null>(null);

  // Load Sub2API + SMS settings when dialog opens
  useEffect(() => {
    if (!open) return;

    invoke<[string, string]>("get_sub2api_settings_cmd")
      .then(([url, key]) => {
        if (url) setSub2apiUrl(url);
        if (key) setSub2apiKey(key);
      })
      .catch(() => {
        // Ignore errors - settings might not exist yet
      });

    invoke<string | null>("get_sms_api_token")
      .then((token) => {
        const saved = Boolean(token?.trim());
        setHasSavedSmsToken(saved);
        if (saved) {
          setSmsEnabled(true);
        }
      })
      .catch(() => {
        setHasSavedSmsToken(false);
      });

    try {
      const raw = localStorage.getItem("jnmbrowser.autoLogin.smsPrefs");
      if (raw) {
        const prefs = JSON.parse(raw) as {
          serviceId?: string;
          network?: string;
          country?: string;
          enabled?: boolean;
        };
        if (typeof prefs.serviceId === "string") {
          setSmsServiceId(prefs.serviceId);
        }
        if (typeof prefs.network === "string" && prefs.network.trim()) {
          setSmsNetwork(prefs.network);
        }
        if (typeof prefs.country === "string" && prefs.country.trim()) {
          setSmsCountry(prefs.country);
        }
        if (typeof prefs.enabled === "boolean") {
          setSmsEnabled(prefs.enabled);
        }
      }
    } catch {
      // Ignore invalid local prefs
    }

    try {
      const raw = localStorage.getItem("jnmbrowser.autoLogin.settings");
      if (!raw) return;
      const prefs = JSON.parse(raw) as {
        networkMode?: LoginNetworkMode;
        proxyId?: string;
        vpnId?: string;
        rotateEveryN?: number;
        pushToSub2api?: boolean;
        browserType?: string;
        maxRetries?: number;
        headless?: boolean;
      };
      if (
        prefs.networkMode === "none" ||
        prefs.networkMode === "proxy" ||
        prefs.networkMode === "vpn"
      ) {
        setNetworkMode(prefs.networkMode);
      }
      if (typeof prefs.proxyId === "string") setProxyId(prefs.proxyId);
      if (typeof prefs.vpnId === "string") setVpnId(prefs.vpnId);
      if (typeof prefs.rotateEveryN === "number") {
        setRotateEveryN(prefs.rotateEveryN);
      }
      if (typeof prefs.pushToSub2api === "boolean") {
        setPushToSub2api(prefs.pushToSub2api);
      }
      if (
        prefs.browserType === "chromium" ||
        prefs.browserType === "camoufox"
      ) {
        setBrowserType(prefs.browserType);
      }
      if (typeof prefs.maxRetries === "number") setMaxRetries(prefs.maxRetries);
      if (typeof prefs.headless === "boolean") setHeadless(prefs.headless);
    } catch {
      // Ignore invalid local prefs
    }
  }, [open]);

  // Prefer inventory Nord/WireGuard confs when available (same as auto-reg).
  useEffect(() => {
    if (!open) return;
    const nordVpns = vpnConfigs.filter(
      (v) => v.source === "nord" || v.name.startsWith("Nord ·"),
    );
    if (nordVpns.length === 0) return;
    if (networkMode === "none") {
      setNetworkMode("vpn");
    }
    if (!vpnId || !vpnConfigs.some((v) => v.id === vpnId)) {
      setVpnId(nordVpns[0].id);
    }
  }, [open, vpnConfigs, networkMode, vpnId]);

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

  // Persist SMS form prefs so service ID / network don't look "unset"
  useEffect(() => {
    if (!open) return;
    try {
      localStorage.setItem(
        "jnmbrowser.autoLogin.smsPrefs",
        JSON.stringify({
          serviceId: smsServiceId,
          network: smsNetwork,
          country: smsCountry,
          enabled: smsEnabled,
        }),
      );
    } catch {
      // Ignore storage failures
    }
  }, [open, smsServiceId, smsNetwork, smsCountry, smsEnabled]);

  // Persist network / Sub2API toggles / browser prefs across sessions.
  useEffect(() => {
    if (!open) return;
    try {
      localStorage.setItem(
        "jnmbrowser.autoLogin.settings",
        JSON.stringify({
          networkMode,
          proxyId,
          vpnId,
          rotateEveryN,
          pushToSub2api,
          browserType,
          maxRetries,
          headless,
        }),
      );
    } catch {
      // Ignore storage failures
    }
  }, [
    open,
    networkMode,
    proxyId,
    vpnId,
    rotateEveryN,
    pushToSub2api,
    browserType,
    maxRetries,
    headless,
  ]);

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
    if (networkMode === "vpn" && !vpnId.trim()) {
      toast.error(t("registration.vpnRequired"));
      return;
    }

    if (pushToSub2api && (!sub2apiUrl.trim() || !sub2apiKey.trim())) {
      toast.error(t("autoLogin.sub2apiRequired"));
      return;
    }

    if (smsEnabled) {
      if (!hasSavedSmsToken && !smsTokenOverride.trim()) {
        toast.error(t("registration.smsTokenRequired"));
        return;
      }
      if (!smsServiceId.trim()) {
        toast.error(t("sms.serviceRequired"));
        return;
      }
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
        // Rust LoginNetworkMode: "none" | "proxy" | "vpn" | "nord"
        networkMode,
        proxyId:
          networkMode === "proxy" ? proxyId.trim() || undefined : undefined,
        vpnId: networkMode === "vpn" ? vpnId.trim() || undefined : undefined,
        rotateEveryN: networkMode === "vpn" ? rotateEveryN : 0,
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

  const credentialLine = (acc: LoginResult): string | null => {
    const email = acc.email?.trim();
    const password = acc.password?.trim();
    if (!email || !password) return null;
    const totp = acc.totpSecret?.trim();
    return totp ? `${email}|${password}|${totp}` : `${email}|${password}`;
  };

  /** Re-login selected/failed stored rows using saved password+totp. */
  const handleRetryLogin = async (rows: LoginResult[]) => {
    // Prefer password saved on the result; fall back to Login-tab paste for older
    // invalid rows created before credentials were persisted.
    const pastedByEmail = new Map(
      parseCredentials(credentialsText).map(
        (c) => [c.email.toLowerCase(), c] as const,
      ),
    );
    const lines: string[] = [];
    for (const row of rows) {
      const saved = credentialLine(row);
      if (saved) {
        lines.push(saved);
        continue;
      }
      const pasted = pastedByEmail.get(row.email.trim().toLowerCase());
      if (pasted) {
        lines.push(
          pasted.totpSecret
            ? `${pasted.email}|${pasted.password}|${pasted.totpSecret}`
            : `${pasted.email}|${pasted.password}`,
        );
      }
    }
    if (lines.length === 0) {
      toast.error(t("autoLogin.retryNoCredentials"));
      return;
    }

    if (networkMode === "proxy" && !proxyId.trim()) {
      toast.error(t("registration.proxyRequired"));
      return;
    }
    if (networkMode === "vpn" && !vpnId.trim()) {
      toast.error(t("registration.vpnRequired"));
      return;
    }
    if (pushToSub2api && (!sub2apiUrl.trim() || !sub2apiKey.trim())) {
      toast.error(t("autoLogin.sub2apiRequired"));
      return;
    }
    if (smsEnabled) {
      if (!hasSavedSmsToken && !smsTokenOverride.trim()) {
        toast.error(t("registration.smsTokenRequired"));
        return;
      }
      if (!smsServiceId.trim()) {
        toast.error(t("sms.serviceRequired"));
        return;
      }
    }

    const text = lines.join("\n");
    const credentials = parseCredentials(text);
    setCredentialsText(text);

    try {
      const taskId = await startLogin({
        credentialsText: text,
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
        networkMode,
        proxyId:
          networkMode === "proxy" ? proxyId.trim() || undefined : undefined,
        vpnId: networkMode === "vpn" ? vpnId.trim() || undefined : undefined,
        rotateEveryN: networkMode === "vpn" ? rotateEveryN : 0,
      });
      setActiveTaskId(taskId);
      setActiveTab("progress");
      toast.success(t("autoLogin.retryStarted", { count: credentials.length }));
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex h-[min(92vh,820px)] w-[min(96vw,1100px)] max-w-none flex-col overflow-hidden">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <LuLogIn className="h-5 w-5 text-primary" />
            {t("autoLogin.title")}
          </DialogTitle>
        </DialogHeader>

        <Tabs
          value={activeTab}
          onValueChange={setActiveTab}
          className="flex min-h-0 flex-1 flex-col"
        >
          <TabsList className="grid w-full shrink-0 grid-cols-4">
            <TabsTrigger value="login">{t("autoLogin.tabLogin")}</TabsTrigger>
            <TabsTrigger value="progress">
              {t("autoLogin.tabProgress")}
              {progressList.length > 0 && ` (${progressList.length})`}
            </TabsTrigger>
            <TabsTrigger value="stored">
              {t("autoLogin.tabStored")}
              {accounts.length > 0 && ` (${accounts.length})`}
            </TabsTrigger>
            <TabsTrigger value="settings">
              {t("autoLogin.tabSettings")}
            </TabsTrigger>
          </TabsList>

          <TabsContent
            value="login"
            className="mt-4 flex min-h-0 flex-1 flex-col overflow-hidden"
          >
            <div className="min-h-0 flex-1 space-y-4 overflow-y-auto pr-1">
              <div className="space-y-2">
                <Label>{t("autoLogin.credentialsLabel")}</Label>
                <Textarea
                  value={credentialsText}
                  onChange={(e) => setCredentialsText(e.target.value)}
                  placeholder={t("autoLogin.credentialsPlaceholder")}
                  rows={8}
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
                      <SelectItem value="chromium">
                        {t("browser.chromium")}
                      </SelectItem>
                      <SelectItem value="camoufox">
                        {t("browser.camoufox")}
                      </SelectItem>
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
                      {t("registration.networkModeNone")}
                    </SelectItem>
                    <SelectItem value="proxy">
                      {t("registration.networkModeProxy")}
                    </SelectItem>
                    <SelectItem value="vpn">
                      {t("registration.networkModeVpn")}
                    </SelectItem>
                  </SelectContent>
                </Select>
                <p className="text-xs text-muted-foreground">
                  {t("autoLogin.settingsHint")}
                </p>
              </div>

              <div className="flex flex-wrap items-center gap-x-6 gap-y-2 rounded-lg border border-border p-3">
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
                <div className="flex items-center gap-2">
                  <input
                    type="checkbox"
                    id="smsEnabled"
                    checked={smsEnabled}
                    onChange={(e) => setSmsEnabled(e.target.checked)}
                    className="h-4 w-4"
                  />
                  <Label htmlFor="smsEnabled" className="cursor-pointer">
                    {t("registration.smsEnable")}
                  </Label>
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
              </div>
            </div>

            <div className="mt-4 flex shrink-0 gap-2 border-t border-border pt-4">
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

          <TabsContent
            value="settings"
            className="mt-4 min-h-0 flex-1 space-y-4 overflow-y-auto pr-1"
          >
            <p className="text-sm text-muted-foreground">
              {t("autoLogin.settingsHint")}
            </p>

            <div className="space-y-3 rounded-lg border border-border p-3">
              <h4 className="text-sm font-semibold">
                {t("registration.networkMode")}
              </h4>
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
                      {t("registration.networkModeNone")}
                    </SelectItem>
                    <SelectItem value="proxy">
                      {t("registration.networkModeProxy")}
                    </SelectItem>
                    <SelectItem value="vpn">
                      {t("registration.networkModeVpn")}
                    </SelectItem>
                  </SelectContent>
                </Select>
              </div>

              {networkMode === "proxy" && (
                <div className="space-y-2">
                  <Label>{t("registration.proxy")}</Label>
                  <Input
                    value={proxyId}
                    onChange={(e) => setProxyId(e.target.value)}
                    placeholder={t("registration.proxyPlaceholder")}
                  />
                </div>
              )}

              {networkMode === "vpn" && (
                <div className="space-y-3 rounded-md border border-border bg-muted/20 p-3">
                  <div className="space-y-2">
                    <Label htmlFor="loginVpnIdSettings">
                      {t("registration.vpn")}
                    </Label>
                    <Select
                      value={vpnId || undefined}
                      onValueChange={setVpnId}
                      disabled={isLoadingVpns || vpnConfigs.length === 0}
                    >
                      <SelectTrigger id="loginVpnIdSettings">
                        <SelectValue
                          placeholder={
                            isLoadingVpns
                              ? t("registration.vpnLoading")
                              : vpnConfigs.length === 0
                                ? t("registration.vpnEmpty")
                                : t("registration.vpnPlaceholder")
                          }
                        />
                      </SelectTrigger>
                      <SelectContent>
                        {vpnConfigs.map((vpn) => (
                          <SelectItem key={vpn.id} value={vpn.id}>
                            {vpn.name} ({vpn.vpn_type})
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                    <p className="text-xs text-muted-foreground">
                      {t("registration.vpnPerProfileHint")}
                    </p>
                  </div>
                  <div className="space-y-2">
                    <Label htmlFor="loginVpnRotateEveryNSettings">
                      {t("registration.rotateEveryN")}
                    </Label>
                    <Input
                      id="loginVpnRotateEveryNSettings"
                      type="number"
                      min={0}
                      max={50}
                      value={rotateEveryN}
                      onChange={(e) =>
                        setRotateEveryN(Number(e.target.value) || 0)
                      }
                    />
                    <p className="text-xs text-muted-foreground">
                      {t("registration.vpnRotateHint")}
                    </p>
                  </div>
                </div>
              )}
            </div>

            <div className="space-y-3 rounded-lg border border-border p-3">
              <h4 className="text-sm font-semibold">
                {t("autoLogin.sub2apiSection")}
              </h4>
              <div className="flex items-center gap-2">
                <input
                  type="checkbox"
                  id="pushToSub2apiSettings"
                  checked={pushToSub2api}
                  onChange={(e) => setPushToSub2api(e.target.checked)}
                  className="h-4 w-4"
                />
                <Label
                  htmlFor="pushToSub2apiSettings"
                  className="cursor-pointer"
                >
                  {t("autoLogin.pushToSub2api")}
                </Label>
              </div>
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

            <div className="space-y-3 rounded-lg border border-border p-3">
              <div className="flex items-center gap-2">
                <input
                  type="checkbox"
                  id="smsEnabledSettings"
                  checked={smsEnabled}
                  onChange={(e) => setSmsEnabled(e.target.checked)}
                  className="h-4 w-4"
                />
                <Label htmlFor="smsEnabledSettings" className="cursor-pointer">
                  {t("registration.smsEnable")}
                </Label>
              </div>
              <p className="text-xs text-muted-foreground">
                {hasSavedSmsToken
                  ? t("registration.smsTokenConfigured")
                  : t("registration.smsTokenMissing")}
              </p>
              <SmsProviderFields
                enabled={smsEnabled}
                country={smsCountry}
                onCountryChange={setSmsCountry}
                serviceId={smsServiceId}
                onServiceIdChange={setSmsServiceId}
                network={smsNetwork}
                onNetworkChange={setSmsNetwork}
                tokenOverride={smsTokenOverride}
                onTokenOverrideChange={setSmsTokenOverride}
                hasSavedToken={hasSavedSmsToken}
              />
            </div>
          </TabsContent>

          <TabsContent
            value="progress"
            className="mt-4 min-h-0 flex-1 space-y-3 overflow-y-auto"
          >
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

          <TabsContent
            value="stored"
            className="mt-4 min-h-0 flex-1 overflow-y-auto"
          >
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
              onRetryLogin={handleRetryLogin}
              onEditAccount={async (accountId, fields) => {
                await updateAccountFields(accountId, fields);
              }}
              retrying={loading}
              pushEnabled={Boolean(sub2apiUrl.trim() && sub2apiKey.trim())}
            />
          </TabsContent>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}
