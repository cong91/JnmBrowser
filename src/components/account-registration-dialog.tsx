"use client";

import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuFolderOpen, LuRocket } from "react-icons/lu";
import { toast } from "sonner";
import { CdkInventoryTable } from "@/components/cdk-inventory-table";
import { RegisteredAccountsTable } from "@/components/registered-accounts-table";
import { RegistrationProgressCard } from "@/components/registration-progress-card";
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
  type EmailProvider,
  type NetworkMode,
  useRegistrationEvents,
} from "@/hooks/use-registration-events";
import { useVpnEvents } from "@/hooks/use-vpn-events";
import {
  cardCodesPlaceholder,
  clampAccountsPerCard,
  emailProviderHintKey,
  isEmailProvider,
} from "@/lib/email-providers";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function AccountRegistrationDialog({ open, onOpenChange }: Props) {
  const { t } = useTranslation();
  const {
    progressMap,
    accounts,
    cdkInventory,
    loading,
    startRegistration,
    cancelRegistration,
    refreshAccounts,
    refreshCdkInventory,
    deleteAccount,
    deleteCdkRecord,
    updateAccountStatus,
  } = useRegistrationEvents();
  const { vpnConfigs, isLoading: isLoadingVpns } = useVpnEvents();

  const [cdkText, setCdkText] = useState("");
  const [proxyId, setProxyId] = useState("");
  const [vpnId, setVpnId] = useState("");
  const [browserType, setBrowserType] = useState("chromium");
  const [maxRetries, setMaxRetries] = useState(3);
  const [accountsPerCdk, setAccountsPerCdk] = useState(1);
  const [concurrency, setConcurrency] = useState(1);
  const [headless, setHeadless] = useState(false);
  const [networkMode, setNetworkMode] = useState<NetworkMode>("none");
  const [rotateEveryN, setRotateEveryN] = useState(1);
  const [nordGroup, setNordGroup] = useState("Japan");
  const [nordServerName, setNordServerName] = useState("");
  const [smsEnabled, setSmsEnabled] = useState(false);
  const [smsServiceId, setSmsServiceId] = useState("");
  const [smsNetwork, setSmsNetwork] = useState("");
  const [smsCountry, setSmsCountry] = useState("vn");
  const [smsTokenOverride, setSmsTokenOverride] = useState("");
  const [hasSavedSmsToken, setHasSavedSmsToken] = useState(false);
  const [emailProvider, setEmailProvider] = useState<EmailProvider>(
    "gmail.123452026.xyz",
  );
  const [activeTab, setActiveTab] = useState("register");
  /** When set, Start clamps accountsPerCdk to this remaining budget for the selected CDK. */
  const [topUpRemaining, setTopUpRemaining] = useState<number | null>(null);

  const effectiveAccountsPerCdk = clampAccountsPerCard(
    emailProvider,
    accountsPerCdk,
  );

  const nordLocations = [
    { value: "Japan", labelKey: "registration.nordLocJapan" },
    { value: "United States", labelKey: "registration.nordLocUnitedStates" },
    { value: "Singapore", labelKey: "registration.nordLocSingapore" },
    { value: "Hong Kong", labelKey: "registration.nordLocHongKong" },
    { value: "United Kingdom", labelKey: "registration.nordLocUnitedKingdom" },
    { value: "Germany", labelKey: "registration.nordLocGermany" },
    { value: "Canada", labelKey: "registration.nordLocCanada" },
    { value: "Australia", labelKey: "registration.nordLocAustralia" },
  ] as const;

  const progressList = Array.from(progressMap.values());

  // Prefer WireGuard inventory created from Nord Access Token; CLI is backup only.
  useEffect(() => {
    if (!open) return;
    const nordVpns = vpnConfigs.filter(
      (v) => v.source === "nord" || v.name.startsWith("Nord ·"),
    );
    if (nordVpns.length === 0) return;
    if (networkMode === "none" || networkMode === "nord") {
      setNetworkMode("vpn");
    }
    if (!vpnId || !vpnConfigs.some((v) => v.id === vpnId)) {
      setVpnId(nordVpns[0].id);
    }
  }, [open, vpnConfigs, networkMode, vpnId]);

  useEffect(() => {
    if (!open) return;
    invoke<string | null>("get_sms_api_token")
      .then((token) => {
        setHasSavedSmsToken(Boolean(token?.trim()));
      })
      .catch(() => {
        setHasSavedSmsToken(false);
      });

    try {
      const raw = localStorage.getItem("jnmbrowser.autoReg.settings");
      if (!raw) return;
      const prefs = JSON.parse(raw) as {
        networkMode?: NetworkMode;
        proxyId?: string;
        vpnId?: string;
        rotateEveryN?: number;
        nordGroup?: string;
        nordServerName?: string;
        browserType?: string;
        maxRetries?: number;
        concurrency?: number;
        headless?: boolean;
        smsEnabled?: boolean;
        smsServiceId?: string;
        smsNetwork?: string;
        smsCountry?: string;
      };
      if (
        prefs.networkMode === "none" ||
        prefs.networkMode === "proxy" ||
        prefs.networkMode === "vpn" ||
        prefs.networkMode === "nord"
      ) {
        setNetworkMode(prefs.networkMode);
      }
      if (typeof prefs.proxyId === "string") setProxyId(prefs.proxyId);
      if (typeof prefs.vpnId === "string") setVpnId(prefs.vpnId);
      if (typeof prefs.rotateEveryN === "number") {
        setRotateEveryN(prefs.rotateEveryN);
      }
      if (typeof prefs.nordGroup === "string") setNordGroup(prefs.nordGroup);
      if (typeof prefs.nordServerName === "string") {
        setNordServerName(prefs.nordServerName);
      }
      if (
        prefs.browserType === "chromium" ||
        prefs.browserType === "camoufox"
      ) {
        setBrowserType(prefs.browserType);
      }
      if (typeof prefs.maxRetries === "number") setMaxRetries(prefs.maxRetries);
      if (typeof prefs.concurrency === "number") {
        setConcurrency(prefs.concurrency);
      }
      if (typeof prefs.headless === "boolean") setHeadless(prefs.headless);
      if (typeof prefs.smsEnabled === "boolean")
        setSmsEnabled(prefs.smsEnabled);
      if (typeof prefs.smsServiceId === "string") {
        setSmsServiceId(prefs.smsServiceId);
      }
      if (typeof prefs.smsNetwork === "string") setSmsNetwork(prefs.smsNetwork);
      if (typeof prefs.smsCountry === "string") setSmsCountry(prefs.smsCountry);
    } catch {
      // Ignore invalid prefs
    }
  }, [open]);

  useEffect(() => {
    if (!open) return;
    try {
      localStorage.setItem(
        "jnmbrowser.autoReg.settings",
        JSON.stringify({
          networkMode,
          proxyId,
          vpnId,
          rotateEveryN,
          nordGroup,
          nordServerName,
          browserType,
          maxRetries,
          concurrency,
          headless,
          smsEnabled,
          smsServiceId,
          smsNetwork,
          smsCountry,
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
    nordGroup,
    nordServerName,
    browserType,
    maxRetries,
    concurrency,
    headless,
    smsEnabled,
    smsServiceId,
    smsNetwork,
    smsCountry,
  ]);

  const parseCdks = (text: string): string[] =>
    text
      .split(/[\n,;]+/)
      .map((s) => s.trim())
      .filter((s) => s.length > 0);

  const remainingByCdk = (() => {
    const map = new Map<string, number>();
    for (const row of cdkInventory) {
      map.set(row.cdk.trim().toUpperCase(), row.remaining ?? 0);
    }
    return map;
  })();

  const topUpOptions = cdkInventory.filter(
    (r) => (r.remaining ?? 0) > 0 && r.status !== "running",
  );

  const cdkCount = parseCdks(cdkText).length;
  const totalAccounts = cdkCount * effectiveAccountsPerCdk;

  const resolveStartAccountsPerCdk = (cdks: string[]): number => {
    let n = effectiveAccountsPerCdk;
    if (topUpRemaining != null && cdks.length === 1) {
      n = Math.min(n, Math.max(1, topUpRemaining));
    } else if (cdks.length === 1) {
      const rem = remainingByCdk.get(cdks[0].trim().toUpperCase());
      if (typeof rem === "number" && rem > 0) {
        n = Math.min(n, rem);
      } else if (rem === 0) {
        n = 0;
      }
    }
    return n;
  };

  const handleTopUp = (cdk: string, remaining: number) => {
    const slots = Math.max(1, remaining);
    setCdkText(cdk);
    setAccountsPerCdk(clampAccountsPerCard(emailProvider, slots));
    setTopUpRemaining(remaining);
    setActiveTab("register");
    toast.message(t("registration.cdkTopUpTitle"), {
      description: t("registration.cdkTopUpHint"),
    });
  };

  const handleStart = async () => {
    const cdks = parseCdks(cdkText);
    if (cdks.length === 0) {
      toast.error(t("registration.cardCodesRequired"));
      return;
    }

    const accountsPerCdkForStart = resolveStartAccountsPerCdk(cdks);
    if (accountsPerCdkForStart < 1) {
      toast.error(t("registration.cdkTopUpDisabledFull"));
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

    await startRegistration({
      cdks,
      browserType,
      proxyId:
        networkMode === "proxy" ? proxyId.trim() || undefined : undefined,
      vpnId: networkMode === "vpn" ? vpnId.trim() || undefined : undefined,
      maxRetries,
      accountsPerCdk: accountsPerCdkForStart,
      headless,
      concurrency:
        networkMode === "nord"
          ? 1
          : networkMode === "vpn"
            ? Math.min(
                6,
                Math.max(
                  1,
                  vpnConfigs.find((v) => v.id === vpnId)?.max_sessions ?? 6,
                ),
              )
            : Math.min(8, Math.max(1, concurrency)),
      nordMaxSessions:
        networkMode === "vpn"
          ? Math.min(
              6,
              Math.max(
                1,
                vpnConfigs.find((v) => v.id === vpnId)?.max_sessions ?? 6,
              ),
            )
          : undefined,
      networkMode,
      rotateEveryN:
        networkMode === "nord" || networkMode === "vpn" ? rotateEveryN : 0,
      nordGroup:
        networkMode === "nord" || networkMode === "vpn"
          ? nordGroup.trim() || undefined
          : undefined,
      nordServerName:
        networkMode === "nord" ? nordServerName.trim() || undefined : undefined,
      emailProvider,
      smsProvider: smsEnabled ? "viotp" : undefined,
      smsServiceId: smsEnabled ? Number(smsServiceId) || undefined : undefined,
      smsNetwork: smsEnabled ? smsNetwork.trim() || undefined : undefined,
      smsCountry: smsEnabled ? smsCountry : undefined,
      smsToken: smsEnabled ? smsTokenOverride.trim() || undefined : undefined,
    });
    setActiveTab("progress");
  };

  const handleDelete = async (accountId: string) => {
    await deleteAccount(accountId);
    await refreshAccounts();
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        aria-describedby={undefined}
        className="flex h-[min(92vh,860px)] w-[min(96vw,1100px)] max-w-none flex-col overflow-hidden"
      >
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <LuRocket className="h-5 w-5 text-primary" />
            {t("registration.title")}
          </DialogTitle>
        </DialogHeader>

        <Tabs
          value={activeTab}
          onValueChange={setActiveTab}
          className="flex min-h-0 flex-1 flex-col"
        >
          <TabsList className="grid w-full grid-cols-5">
            <TabsTrigger value="register">
              {t("registration.newRegistration")}
            </TabsTrigger>
            <TabsTrigger value="progress" className="gap-1.5">
              {t("registration.progress")}
              {progressList.length > 0 && (
                <span className="rounded-full bg-primary/10 px-1.5 py-0.5 text-[10px] font-medium tabular-nums">
                  {progressList.length}
                </span>
              )}
            </TabsTrigger>
            <TabsTrigger value="stored" className="gap-1.5">
              {t("registration.storedAccounts")}
              {accounts.length > 0 && (
                <span className="rounded-full bg-muted px-1.5 py-0.5 text-[10px] font-medium tabular-nums text-muted-foreground">
                  {accounts.length}
                </span>
              )}
            </TabsTrigger>
            <TabsTrigger value="cdks" className="gap-1.5">
              {t("registration.cdkInventoryTab")}
              {cdkInventory.length > 0 && (
                <span className="rounded-full bg-muted px-1.5 py-0.5 text-[10px] font-medium tabular-nums text-muted-foreground">
                  {cdkInventory.length}
                </span>
              )}
            </TabsTrigger>
            <TabsTrigger value="settings">
              {t("registration.settingsTab")}
            </TabsTrigger>
          </TabsList>

          <TabsContent
            value="register"
            className="mt-4 flex-1 space-y-4 overflow-auto"
          >
            <div className="space-y-3 rounded-md border border-border bg-muted/20 p-3">
              <div className="space-y-2">
                <Label htmlFor="emailProvider">
                  {t("registration.emailProvider")}
                </Label>
                <Select
                  value={emailProvider}
                  onValueChange={(v) => {
                    if (!isEmailProvider(v)) return;
                    setEmailProvider(v);
                    setAccountsPerCdk(clampAccountsPerCard(v, accountsPerCdk));
                  }}
                >
                  <SelectTrigger id="emailProvider">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="gmail.123452026.xyz">
                      {t("registration.emailProviderGmail123452026")}
                    </SelectItem>
                    <SelectItem value="sms.iosmq.xyz">
                      {t("registration.emailProviderSmsIosmq")}
                    </SelectItem>
                  </SelectContent>
                </Select>
                <p className="text-xs text-muted-foreground">
                  {t(emailProviderHintKey(emailProvider))}
                </p>
              </div>

              <div className="space-y-2">
                <Label htmlFor="cdks">
                  {emailProvider === "sms.iosmq.xyz"
                    ? t("registration.mailCardLabel")
                    : t("registration.cdkLabel")}
                </Label>
                {topUpOptions.length > 0 ? (
                  <div className="space-y-1.5">
                    <Label htmlFor="cdkPick" className="text-xs font-normal">
                      {t("registration.cdkPickFromInventory")}
                    </Label>
                    <Select
                      value={(() => {
                        const codes = parseCdks(cdkText);
                        if (codes.length !== 1) return undefined;
                        const key = codes[0].trim().toUpperCase();
                        return topUpOptions.some(
                          (r) => r.cdk.trim().toUpperCase() === key,
                        )
                          ? key
                          : undefined;
                      })()}
                      onValueChange={(v) => {
                        if (!v) return;
                        const row = topUpOptions.find(
                          (r) => r.cdk.trim().toUpperCase() === v,
                        );
                        if (row) {
                          handleTopUp(row.cdk, row.remaining ?? 0);
                        }
                      }}
                    >
                      <SelectTrigger id="cdkPick">
                        <SelectValue
                          placeholder={t("registration.cdkPickPlaceholder")}
                        />
                      </SelectTrigger>
                      <SelectContent>
                        {topUpOptions.map((row) => (
                          <SelectItem
                            key={row.cdk}
                            value={row.cdk.trim().toUpperCase()}
                          >
                            {row.cdk} · {row.remaining ?? 0}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                    <p className="text-[11px] text-muted-foreground">
                      {t("registration.cdkRawEntryHint")}
                    </p>
                  </div>
                ) : null}
                <Textarea
                  id="cdks"
                  placeholder={cardCodesPlaceholder(emailProvider)}
                  value={cdkText}
                  onChange={(e) => {
                    setCdkText(e.target.value);
                    setTopUpRemaining(null);
                  }}
                  rows={4}
                  className="font-mono text-xs"
                />
                {topUpRemaining != null ? (
                  <p className="text-xs text-muted-foreground">
                    {t("registration.cdkTopUpHint")}{" "}
                    {t("registration.cdkRemainingOfMax", { max: 6 })}:{" "}
                    {topUpRemaining}
                  </p>
                ) : null}
                <p className="text-xs text-muted-foreground">
                  {emailProvider === "sms.iosmq.xyz"
                    ? t("registration.mailCardHint", {
                        count: cdkCount,
                        total: totalAccounts,
                      })
                    : t("registration.cdkHint", {
                        count: cdkCount,
                        total: totalAccounts,
                      })}
                </p>
              </div>
            </div>

            <div className="rounded-md bg-muted/50 p-3 text-xs text-muted-foreground space-y-1">
              <div className="flex items-center gap-1.5 font-medium text-foreground">
                <LuFolderOpen className="h-3.5 w-3.5" />
                {t("registration.storagePath")}
              </div>
              <code className="text-[11px] break-all">
                {t("registration.storagePathHint")}
              </code>
            </div>

            <div className="grid grid-cols-2 gap-4">
              <div className="space-y-2">
                <Label htmlFor="browserType">
                  {t("registration.browserType")}
                </Label>
                <Select value={browserType} onValueChange={setBrowserType}>
                  <SelectTrigger id="browserType">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="chromium">Chromium</SelectItem>
                    <SelectItem value="camoufox">Camoufox</SelectItem>
                  </SelectContent>
                </Select>
              </div>

              <div className="space-y-2">
                <Label htmlFor="networkMode">
                  {t("registration.networkMode")}
                </Label>
                <Select
                  value={networkMode}
                  onValueChange={(v) => setNetworkMode(v as NetworkMode)}
                >
                  <SelectTrigger id="networkMode">
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
                    <SelectItem value="nord">
                      {t("registration.networkModeNord")}
                    </SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </div>

            <p className="text-xs text-muted-foreground">
              {t("registration.settingsHint")}
            </p>

            <div className="grid grid-cols-2 gap-4 sm:grid-cols-3">
              <div className="space-y-2">
                <Label htmlFor="perCdk">
                  {t("registration.accountsPerCdk")}
                </Label>
                <Input
                  id="perCdk"
                  type="number"
                  min={1}
                  max={
                    topUpRemaining != null
                      ? Math.max(1, Math.min(6, topUpRemaining))
                      : 6
                  }
                  value={
                    topUpRemaining != null
                      ? Math.min(effectiveAccountsPerCdk, topUpRemaining)
                      : effectiveAccountsPerCdk
                  }
                  onChange={(e) => {
                    let n = Number(e.target.value);
                    if (topUpRemaining != null) {
                      n = Math.min(n, topUpRemaining);
                    }
                    setAccountsPerCdk(clampAccountsPerCard(emailProvider, n));
                  }}
                />
                {topUpRemaining != null ? (
                  <p className="text-[11px] text-muted-foreground">
                    {t("registration.cdkRemainingOfMax", { max: 6 })}:{" "}
                    {topUpRemaining}
                  </p>
                ) : null}
              </div>

              <div className="space-y-2">
                <Label htmlFor="concurrency">
                  {t("registration.concurrency")}
                </Label>
                <Input
                  id="concurrency"
                  type="number"
                  min={1}
                  max={8}
                  disabled={networkMode === "nord" || networkMode === "vpn"}
                  value={
                    networkMode === "nord"
                      ? 1
                      : networkMode === "vpn"
                        ? Math.min(
                            6,
                            Math.max(
                              1,
                              vpnConfigs.find((v) => v.id === vpnId)
                                ?.max_sessions ?? 6,
                            ),
                          )
                        : concurrency
                  }
                  onChange={(e) =>
                    setConcurrency(
                      Math.min(8, Math.max(1, Number(e.target.value) || 1)),
                    )
                  }
                />
                <p className="text-[11px] text-muted-foreground">
                  {networkMode === "nord"
                    ? t("registration.concurrencyNordHint")
                    : networkMode === "vpn"
                      ? t("registration.concurrencyVpnAutoHint")
                      : t("registration.concurrencyHint")}
                </p>
              </div>

              <div className="flex flex-col justify-end gap-2 pb-1 text-sm">
                <div className="flex h-9 items-center gap-2">
                  <input
                    id="headless"
                    type="checkbox"
                    checked={headless}
                    onChange={(e) => setHeadless(e.target.checked)}
                    className="h-4 w-4 rounded border-border"
                  />
                  <Label
                    htmlFor="headless"
                    className="cursor-pointer font-normal"
                  >
                    {t("registration.headless")}
                  </Label>
                </div>
                <div className="flex h-9 items-center gap-2">
                  <input
                    id="smsEnabled"
                    type="checkbox"
                    checked={smsEnabled}
                    onChange={(e) => setSmsEnabled(e.target.checked)}
                    className="h-4 w-4 rounded border-border"
                  />
                  <Label
                    htmlFor="smsEnabled"
                    className="cursor-pointer font-normal"
                  >
                    {t("registration.smsEnable")}
                  </Label>
                </div>
              </div>
            </div>

            <Button
              className="w-full"
              onClick={handleStart}
              disabled={loading || cdkCount === 0}
            >
              {loading
                ? t("registration.starting")
                : totalAccounts > 0
                  ? t("registration.startRegistrationWithCount", {
                      total: totalAccounts,
                    })
                  : t("registration.startRegistration")}
            </Button>
          </TabsContent>

          <TabsContent
            value="settings"
            className="mt-4 min-h-0 flex-1 space-y-4 overflow-auto pr-1"
          >
            <p className="text-sm text-muted-foreground">
              {t("registration.settingsHint")}
            </p>

            <div className="space-y-3 rounded-lg border border-border p-3">
              <h4 className="text-sm font-semibold">
                {t("registration.networkMode")}
              </h4>
              <div className="space-y-2">
                <Label htmlFor="networkModeSettings">
                  {t("registration.networkMode")}
                </Label>
                <Select
                  value={networkMode}
                  onValueChange={(v) => setNetworkMode(v as NetworkMode)}
                >
                  <SelectTrigger id="networkModeSettings">
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
                    <SelectItem value="nord">
                      {t("registration.networkModeNord")}
                    </SelectItem>
                  </SelectContent>
                </Select>
              </div>

              {networkMode === "proxy" && (
                <div className="space-y-2">
                  <Label htmlFor="proxySettings">
                    {t("registration.proxy")}
                  </Label>
                  <Input
                    id="proxySettings"
                    placeholder={t("registration.proxyPlaceholder")}
                    value={proxyId}
                    onChange={(e) => setProxyId(e.target.value)}
                  />
                </div>
              )}

              {networkMode === "vpn" && (
                <div className="space-y-3 rounded-md border border-border bg-muted/20 p-3">
                  <div className="space-y-2">
                    <Label htmlFor="vpnIdSettings">
                      {t("registration.vpn")}
                    </Label>
                    <Select
                      value={vpnId || undefined}
                      onValueChange={setVpnId}
                      disabled={isLoadingVpns || vpnConfigs.length === 0}
                    >
                      <SelectTrigger id="vpnIdSettings">
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
                    <Label htmlFor="vpnRotateEveryNSettings">
                      {t("registration.rotateEveryN")}
                    </Label>
                    <Input
                      id="vpnRotateEveryNSettings"
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

              {networkMode === "nord" && (
                <div className="space-y-3 rounded-md border border-border bg-muted/20 p-3">
                  <p className="text-xs text-muted-foreground">
                    {t("registration.nordSystemWideWarning")}
                  </p>
                  <div className="grid grid-cols-2 gap-3">
                    <div className="space-y-2">
                      <Label htmlFor="nordLocation">
                        {t("registration.nordLocation")}
                      </Label>
                      <Select
                        value={
                          nordLocations.some((l) => l.value === nordGroup)
                            ? nordGroup
                            : "custom"
                        }
                        onValueChange={(v) => {
                          if (v === "custom") {
                            if (
                              nordLocations.some((l) => l.value === nordGroup)
                            ) {
                              setNordGroup("");
                            }
                            return;
                          }
                          setNordGroup(v);
                        }}
                      >
                        <SelectTrigger id="nordLocation">
                          <SelectValue
                            placeholder={t(
                              "registration.nordLocationPlaceholder",
                            )}
                          />
                        </SelectTrigger>
                        <SelectContent>
                          {nordLocations.map((loc) => (
                            <SelectItem key={loc.value} value={loc.value}>
                              {t(loc.labelKey)}
                            </SelectItem>
                          ))}
                          <SelectItem value="custom">
                            {t("registration.nordLocCustom")}
                          </SelectItem>
                        </SelectContent>
                      </Select>
                    </div>
                    <div className="space-y-2">
                      <Label htmlFor="rotateEveryN">
                        {t("registration.rotateEveryN")}
                      </Label>
                      <Input
                        id="rotateEveryN"
                        type="number"
                        min={0}
                        max={50}
                        value={rotateEveryN}
                        onChange={(e) =>
                          setRotateEveryN(Number(e.target.value) || 0)
                        }
                      />
                    </div>
                  </div>
                  {(!nordLocations.some((l) => l.value === nordGroup) ||
                    nordGroup === "") && (
                    <div className="space-y-2">
                      <Label htmlFor="nordGroupCustom">
                        {t("registration.nordGroupCustom")}
                      </Label>
                      <Input
                        id="nordGroupCustom"
                        placeholder={t("registration.nordGroupPlaceholder")}
                        value={nordGroup}
                        onChange={(e) => setNordGroup(e.target.value)}
                      />
                    </div>
                  )}
                  <div className="space-y-2">
                    <Label htmlFor="nordServer">
                      {t("registration.nordServerName")}
                    </Label>
                    <Input
                      id="nordServer"
                      placeholder={t("registration.nordServerPlaceholder")}
                      value={nordServerName}
                      onChange={(e) => setNordServerName(e.target.value)}
                    />
                  </div>
                </div>
              )}
            </div>

            <div className="space-y-3 rounded-lg border border-border p-3">
              <div className="space-y-2">
                <Label htmlFor="retriesSettings">
                  {t("registration.maxRetries")}
                </Label>
                <Input
                  id="retriesSettings"
                  type="number"
                  min={1}
                  max={10}
                  value={maxRetries}
                  onChange={(e) => setMaxRetries(Number(e.target.value))}
                />
              </div>
            </div>

            <div className="space-y-3 rounded-md border border-border bg-muted/20 p-3">
              <div className="flex h-9 items-center gap-2 text-sm">
                <input
                  id="smsEnabledSettings"
                  type="checkbox"
                  checked={smsEnabled}
                  onChange={(e) => setSmsEnabled(e.target.checked)}
                  className="h-4 w-4 rounded border-border"
                />
                <Label
                  htmlFor="smsEnabledSettings"
                  className="cursor-pointer font-normal"
                >
                  {t("registration.smsEnable")}
                </Label>
              </div>
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
            className="mt-4 flex-1 space-y-3 overflow-auto"
          >
            {progressList.length === 0 ? (
              <div className="flex flex-col items-center justify-center rounded-xl border border-dashed bg-muted/20 px-6 py-12 text-center">
                <p className="text-sm text-muted-foreground">
                  {t("registration.noActiveTasks")}
                </p>
              </div>
            ) : (
              progressList.map((p) => (
                <RegistrationProgressCard
                  key={p.taskId}
                  progress={p}
                  onCancel={
                    p.result ? undefined : () => cancelRegistration(p.taskId)
                  }
                />
              ))
            )}
          </TabsContent>

          <TabsContent
            value="stored"
            className="mt-4 min-h-0 flex-1 overflow-auto"
          >
            <RegisteredAccountsTable
              accounts={accounts}
              onDelete={handleDelete}
              onRefresh={refreshAccounts}
              onUpdateStatus={updateAccountStatus}
            />
          </TabsContent>

          <TabsContent
            value="cdks"
            className="mt-4 min-h-0 flex-1 overflow-auto"
          >
            <CdkInventoryTable
              records={cdkInventory}
              onRefresh={refreshCdkInventory}
              onDelete={deleteCdkRecord}
              onTopUp={handleTopUp}
            />
          </TabsContent>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}
