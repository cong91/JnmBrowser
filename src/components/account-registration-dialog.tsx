"use client";

import { useTranslation } from "react-i18next";
import { LuFolderOpen, LuRocket } from "react-icons/lu";
import { useState } from "react";
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
import { useRegistrationEvents } from "@/hooks/use-registration-events";
import { RegisteredAccountsTable } from "@/components/registered-accounts-table";
import { RegistrationProgressCard } from "@/components/registration-progress-card";

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
  } = useRegistrationEvents();

  const [cdkText, setCdkText] = useState("");
  const [proxyId, setProxyId] = useState("");
  const [browserType, setBrowserType] = useState("chromium");
  const [maxRetries, setMaxRetries] = useState(3);
  const [accountsPerCdk, setAccountsPerCdk] = useState(1);
  const [headless, setHeadless] = useState(false);
  const [activeTab, setActiveTab] = useState("register");

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

    await startRegistration({
      cdks,
      browserType,
      proxyId: proxyId || undefined,
      maxRetries,
      accountsPerCdk,
      headless,
      concurrency: 1,
    });
    setActiveTab("progress");
  };

  const handleDelete = async (accountId: string) => {
    await deleteAccount(accountId);
    await refreshAccounts();
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl max-h-[85vh] overflow-hidden flex flex-col">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <LuRocket className="h-5 w-5" />
            {t("registration.title")}
          </DialogTitle>
        </DialogHeader>

        <Tabs
          value={activeTab}
          onValueChange={setActiveTab}
          className="flex-1 flex flex-col min-h-0"
        >
          <TabsList className="w-full">
            <TabsTrigger value="register" className="flex-1">
              {t("registration.newRegistration")}
            </TabsTrigger>
            <TabsTrigger value="progress" className="flex-1">
              {t("registration.progress")}
              {progressList.length > 0 && (
                <span className="ml-1.5 rounded-full bg-primary/10 px-1.5 py-0.5 text-xs">
                  {progressList.length}
                </span>
              )}
            </TabsTrigger>
            <TabsTrigger value="stored" className="flex-1">
              {t("registration.storedAccounts")}
            </TabsTrigger>
          </TabsList>

          <TabsContent value="register" className="flex-1 overflow-auto mt-4 space-y-4">
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
                {t("registration.cdkHint", { count: cdkCount, total: totalAccounts })}
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
                <Label htmlFor="proxy">{t("registration.proxy")}</Label>
                <Input
                  id="proxy"
                  placeholder={t("registration.proxyPlaceholder")}
                  value={proxyId}
                  onChange={(e) => setProxyId(e.target.value)}
                />
              </div>
            </div>

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
                <Label htmlFor="perCdk">{t("registration.accountsPerCdk")}</Label>
                <Input
                  id="perCdk"
                  type="number"
                  min={1}
                  max={6}
                  value={accountsPerCdk}
                  onChange={(e) => setAccountsPerCdk(Number(e.target.value))}
                />
              </div>

              <div className="space-y-2 flex items-end pb-2">
                <label className="flex items-center gap-2 text-sm cursor-pointer">
                  <input
                    type="checkbox"
                    checked={headless}
                    onChange={(e) => setHeadless(e.target.checked)}
                    className="rounded"
                  />
                  {t("registration.headless")}
                </label>
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
                  ? t("registration.startRegistrationWithCount", { total: totalAccounts })
                  : t("registration.startRegistration")}
            </Button>
          </TabsContent>

          <TabsContent value="progress" className="flex-1 overflow-auto mt-4 space-y-4">
            {progressList.length === 0 ? (
              <p className="text-sm text-muted-foreground text-center py-8">
                {t("registration.noActiveTasks")}
              </p>
            ) : (
              progressList.map((p) => (
                <RegistrationProgressCard
                  key={p.taskId}
                  progress={p}
                  onCancel={
                    p.result
                      ? undefined
                      : () => cancelRegistration(p.taskId)
                  }
                />
              ))
            )}
          </TabsContent>

          <TabsContent value="stored" className="flex-1 overflow-auto mt-4">
            <RegisteredAccountsTable
              accounts={accounts}
              onDelete={handleDelete}
              onRefresh={refreshAccounts}
            />
          </TabsContent>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}
