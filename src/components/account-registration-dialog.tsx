"use client";

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { LuFolderOpen, LuRocket } from "react-icons/lu";
import { toast } from "sonner";
import { RegisteredAccountsTable } from "@/components/registered-accounts-table";
import { RegistrationProgressCard } from "@/components/registration-progress-card";
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
  type NetworkMode,
  useRegistrationEvents,
} from "@/hooks/use-registration-events";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function AccountRegistrationDialog({ open, onOpenChange }: Props) {
  const { t } = useTranslation();
  const {
    progressMap,
    accounts,
    loading,
    startRegistration,
    cancelRegistration,
    refreshAccounts,
    deleteAccount,
    updateAccountStatus,
  } = useRegistrationEvents();

  const [cdkText, setCdkText] = useState("");
  const [proxyId, setProxyId] = useState("");
  const [browserType, setBrowserType] = useState("chromium");
  const [maxRetries, setMaxRetries] = useState(3);
  const [accountsPerCdk, setAccountsPerCdk] = useState(1);
  const [headless, setHeadless] = useState(false);
  const [networkMode, setNetworkMode] = useState<NetworkMode>("none");
  const [rotateEveryN, setRotateEveryN] = useState(1);
  const [nordGroup, setNordGroup] = useState("Japan");
  const [nordServerName, setNordServerName] = useState("");
  const [activeTab, setActiveTab] = useState("register");

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

  const parseCdks = (text: string): string[] =>
    text
      .split(/[\n,;]+/)
      .map((s) => s.trim())
      .filter((s) => s.length > 0);

  const cdkCount = parseCdks(cdkText).length;
  const totalAccounts = cdkCount * accountsPerCdk;

  const handleStart = async () => {
    const cdks = parseCdks(cdkText);
    if (cdks.length === 0) return;

    if (networkMode === "proxy" && !proxyId.trim()) {
      toast.error(t("registration.proxyRequired"));
      return;
    }

    await startRegistration({
      cdks,
      browserType,
      proxyId:
        networkMode === "proxy" ? proxyId.trim() || undefined : undefined,
      maxRetries,
      accountsPerCdk,
      headless,
      concurrency: 1,
      networkMode,
      rotateEveryN: networkMode === "nord" ? rotateEveryN : 0,
      nordGroup:
        networkMode === "nord" ? nordGroup.trim() || undefined : undefined,
      nordServerName:
        networkMode === "nord" ? nordServerName.trim() || undefined : undefined,
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
        className={
          activeTab === "stored"
            ? "flex max-h-[90vh] max-w-6xl flex-col overflow-hidden"
            : "flex max-h-[85vh] max-w-2xl flex-col overflow-hidden"
        }
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
          <TabsList className="grid w-full grid-cols-3">
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
          </TabsList>

          <TabsContent
            value="register"
            className="mt-4 flex-1 space-y-4 overflow-auto"
          >
            <div className="space-y-2">
              <Label htmlFor="cdks">{t("registration.cdkLabel")}</Label>
              <Textarea
                id="cdks"
                placeholder="GMAIL-K4L5-EUW5-PHBV-A6KW&#10;GMAIL-XXXX-XXXX-XXXX-XXXX"
                value={cdkText}
                onChange={(e) => setCdkText(e.target.value)}
                rows={4}
                className="font-mono text-xs"
              />
              <p className="text-xs text-muted-foreground">
                {t("registration.cdkHint", {
                  count: cdkCount,
                  total: totalAccounts,
                })}
              </p>
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
                    <SelectItem value="nord">
                      {t("registration.networkModeNord")}
                    </SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </div>

            {networkMode === "proxy" && (
              <div className="space-y-2">
                <Label htmlFor="proxy">{t("registration.proxy")}</Label>
                <Input
                  id="proxy"
                  placeholder={t("registration.proxyPlaceholder")}
                  value={proxyId}
                  onChange={(e) => setProxyId(e.target.value)}
                />
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

            <div className="grid grid-cols-3 gap-4">
              <div className="space-y-2">
                <Label htmlFor="retries">{t("registration.maxRetries")}</Label>
                <Input
                  id="retries"
                  type="number"
                  min={1}
                  max={10}
                  value={maxRetries}
                  onChange={(e) => setMaxRetries(Number(e.target.value))}
                />
              </div>

              <div className="space-y-2">
                <Label htmlFor="perCdk">
                  {t("registration.accountsPerCdk")}
                </Label>
                <Input
                  id="perCdk"
                  type="number"
                  min={1}
                  max={6}
                  value={accountsPerCdk}
                  onChange={(e) => setAccountsPerCdk(Number(e.target.value))}
                />
              </div>

              <div className="flex items-end pb-2">
                <div className="flex h-9 items-center gap-2 text-sm">
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
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}
