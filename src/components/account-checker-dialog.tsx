"use client";

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuCheck, LuCircle, LuX } from "react-icons/lu";
import { toast } from "sonner";
import {
  type AutomationProfilePolicy,
  accountCheckerProfilePolicyPayload,
  automationErrorTranslationKey,
  DEFAULT_AUTOMATION_PROFILE_POLICY,
} from "@/components/automation-profile-policy";
import { AutomationProfilePolicyFields } from "@/components/automation-profile-policy-fields";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { useAccountCheckerEvents } from "@/hooks/use-account-checker-events";
import { useVpnEvents } from "@/hooks/use-vpn-events";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function AccountCheckerDialog({ open, onOpenChange }: Props) {
  const { t } = useTranslation();
  const {
    passed,
    deactivated,
    unresolved,
    activeTaskId,
    startCheck,
    cancelCheck,
    exportPassed,
    exportDeactivated,
    deleteResult,
  } = useAccountCheckerEvents();
  const { vpnConfigs, isLoading: isLoadingVpns } = useVpnEvents();

  const [credentialsText, setCredentialsText] = useState("");
  const [vpnId, setVpnId] = useState("");
  const [profilePolicy, setProfilePolicy] = useState<AutomationProfilePolicy>({
    ...DEFAULT_AUTOMATION_PROFILE_POLICY,
  });
  const [running, setRunning] = useState(false);

  useEffect(() => {
    if (!activeTaskId) {
      setRunning(false);
    }
  }, [activeTaskId]);

  const handleStart = async () => {
    const lines = credentialsText
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l.length > 0);
    if (lines.length === 0) {
      toast.error(t("accountChecker.noCredentials"));
      return;
    }
    setRunning(true);
    try {
      await startCheck({
        credentialsText,
        ...accountCheckerProfilePolicyPayload(profilePolicy),
        vpnId: vpnId || undefined,
        browserType: "chromium",
        headless: false,
      });
    } catch (error) {
      toast.error(t(automationErrorTranslationKey(error)));
      setRunning(false);
    }
  };

  const handleCancel = async () => {
    if (activeTaskId) {
      await cancelCheck(activeTaskId);
    }
    setRunning(false);
  };

  const handleExport = async (fn: () => Promise<string>, label: string) => {
    const text = await fn();
    if (text) {
      await navigator.clipboard.writeText(text);
      toast.success(t("accountChecker.copied", { label }));
    } else {
      toast.info(t("accountChecker.nothingToExport"));
    }
  };

  const credentialCount = credentialsText
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0).length;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl max-h-[85vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{t("header.menu.accountChecker")}</DialogTitle>
        </DialogHeader>

        <Tabs defaultValue="input">
          <TabsList>
            <TabsTrigger value="input">
              {t("accountChecker.tabs.input")}
            </TabsTrigger>
            <TabsTrigger value="passed">
              {t("accountChecker.tabs.passed")}
              {passed.length > 0 && (
                <span className="ml-1.5 px-1.5 py-0.5 rounded-full bg-success/20 text-success text-xs font-medium">
                  {passed.length}
                </span>
              )}
            </TabsTrigger>
            <TabsTrigger value="deactivated">
              {t("accountChecker.tabs.deactivated")}
              {deactivated.length > 0 && (
                <span className="ml-1.5 px-1.5 py-0.5 rounded-full bg-destructive/20 text-destructive text-xs font-medium">
                  {deactivated.length}
                </span>
              )}
            </TabsTrigger>
            <TabsTrigger value="unresolved">
              {t("accountChecker.tabs.unresolved")}
              {unresolved.length > 0 && (
                <span className="ml-1.5 px-1.5 py-0.5 rounded-full bg-muted-foreground/20 text-muted-foreground text-xs font-medium">
                  {unresolved.length}
                </span>
              )}
            </TabsTrigger>
          </TabsList>

          {/* Input tab */}
          <TabsContent value="input" className="space-y-4">
            <div className="space-y-2">
              <label
                htmlFor="account-checker-credentials"
                className="text-sm font-medium"
              >
                {t("accountChecker.credentialsLabel")}
              </label>
              <Textarea
                id="account-checker-credentials"
                placeholder={t("accountChecker.credentialsPlaceholder")}
                value={credentialsText}
                onChange={(e) => {
                  setCredentialsText(e.target.value);
                }}
                rows={8}
                disabled={running}
                className="font-mono text-sm"
              />
            </div>
            <div className="text-xs text-muted-foreground">
              {t("accountChecker.inputCount", { count: credentialCount })}
            </div>
            <AutomationProfilePolicyFields
              idPrefix="account-checker"
              browserType="chromium"
              value={profilePolicy}
              onChange={setProfilePolicy}
              disabled={running}
              active={open}
            />
            <div className="space-y-2">
              <label
                htmlFor="account-checker-vpn"
                className="text-sm font-medium"
              >
                {t("registration.vpn")}
              </label>
              <Select
                value={vpnId || "__rotating-vpn__"}
                onValueChange={(value) =>
                  setVpnId(value === "__rotating-vpn__" ? "" : value)
                }
                disabled={running || isLoadingVpns}
              >
                <SelectTrigger id="account-checker-vpn">
                  <SelectValue
                    placeholder={
                      isLoadingVpns
                        ? t("registration.vpnLoading")
                        : t("registration.vpnPlaceholder")
                    }
                  />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="__rotating-vpn__">
                    {t("common.labels.default")}
                  </SelectItem>
                  {vpnConfigs.map((vpn) => (
                    <SelectItem key={vpn.id} value={vpn.id}>
                      {vpn.name} ({vpn.vpn_type})
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="flex gap-2">
              <Button
                onClick={handleStart}
                disabled={running || credentialCount === 0}
              >
                {running
                  ? t("accountChecker.running")
                  : t("accountChecker.start")}
              </Button>
              {running && (
                <Button variant="outline" onClick={handleCancel}>
                  {t("accountChecker.cancel")}
                </Button>
              )}
            </div>
          </TabsContent>

          {/* Passed tab */}
          <TabsContent value="passed" className="space-y-4">
            <ResultList
              results={passed}
              icon={<LuCheck className="text-success" />}
              emptyText={t("accountChecker.noPassed")}
              onDelete={deleteResult}
            />
            <Button
              variant="outline"
              size="sm"
              onClick={() => {
                void handleExport(
                  exportPassed,
                  t("accountChecker.tabs.passed"),
                );
              }}
              disabled={passed.length === 0}
            >
              {t("accountChecker.exportPassed")}
            </Button>
          </TabsContent>

          {/* Deactivated tab */}
          <TabsContent value="deactivated" className="space-y-4">
            <ResultList
              results={deactivated}
              icon={<LuX className="text-destructive" />}
              emptyText={t("accountChecker.noDeactivated")}
              onDelete={deleteResult}
            />
            <Button
              variant="outline"
              size="sm"
              onClick={() => {
                void handleExport(
                  exportDeactivated,
                  t("accountChecker.tabs.deactivated"),
                );
              }}
              disabled={deactivated.length === 0}
            >
              {t("accountChecker.exportDeactivated")}
            </Button>
          </TabsContent>

          {/* Unresolved tab */}
          <TabsContent value="unresolved" className="space-y-4">
            <ResultList
              results={unresolved}
              icon={<LuCircle className="text-muted-foreground" />}
              emptyText={t("accountChecker.noUnresolved")}
              onDelete={deleteResult}
            />
          </TabsContent>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}

function ResultList({
  results,
  icon,
  emptyText,
  onDelete,
}: {
  results: Array<{ email: string; outcome: string; reasonCode: string }>;
  icon: React.ReactNode;
  emptyText: string;
  onDelete: (email: string) => void;
}) {
  const { t } = useTranslation();

  if (results.length === 0) {
    return (
      <div className="flex flex-col items-center gap-2 py-8 text-muted-foreground">
        <LuCircle className="w-8 h-8" />
        <span className="text-sm">{emptyText}</span>
      </div>
    );
  }

  return (
    <div className="space-y-1">
      {results.map((r) => (
        <div
          key={r.email}
          className="flex items-center justify-between gap-2 rounded-md border px-3 py-2 text-sm"
        >
          <div className="flex items-center gap-2 min-w-0">
            {icon}
            <span className="truncate">{r.email}</span>
          </div>
          <Button
            variant="ghost"
            size="icon"
            className="h-6 w-6 shrink-0"
            onClick={() => {
              onDelete(r.email);
            }}
            aria-label={t("common.buttons.delete")}
          >
            <LuX className="h-4 w-4" />
          </Button>
        </div>
      ))}
    </div>
  );
}
