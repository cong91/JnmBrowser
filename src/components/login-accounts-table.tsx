"use client";

import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  LuCopy,
  LuDownload,
  LuEye,
  LuEyeOff,
  LuPackageOpen,
  LuRefreshCw,
  LuTrash2,
  LuUpload,
} from "react-icons/lu";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import type { LoginResult, LoginResultStatus } from "@/hooks/use-login-events";
import { cn } from "@/lib/utils";

interface Props {
  accounts: LoginResult[];
  onDelete?: (accountId: string) => void | Promise<void>;
  onRefresh?: () => void | Promise<void>;
  onUpdateStatus?: (
    accountIds: string[],
    status: LoginResultStatus,
    note?: string,
  ) => Promise<void> | void;
  onExportJson?: (
    accountIds: string[],
    options?: { markExported?: boolean },
  ) => Promise<string>;
  onPush?: (accountIds: string[]) => Promise<{
    pushed: number;
    failed: number;
    errors: string[];
  }>;
  pushEnabled?: boolean;
}

function accountKey(acc: LoginResult): string {
  return acc.accountId || acc.email;
}

function isExportable(acc: LoginResult): boolean {
  return Boolean(acc.success && acc.accessToken);
}

function statusVariant(
  status: LoginResultStatus | undefined,
): "default" | "secondary" | "destructive" | "outline" {
  switch (status) {
    case "exported":
      return "outline";
    case "used":
      return "secondary";
    case "invalid":
      return "destructive";
    default:
      return "default";
  }
}

function statusLabelKey(
  status: LoginResultStatus | undefined | "all",
):
  | "autoLogin.statusAvailable"
  | "autoLogin.statusExported"
  | "autoLogin.statusUsed"
  | "autoLogin.statusInvalid"
  | "autoLogin.filterAll" {
  switch (status) {
    case "exported":
      return "autoLogin.statusExported";
    case "used":
      return "autoLogin.statusUsed";
    case "invalid":
      return "autoLogin.statusInvalid";
    case "all":
      return "autoLogin.filterAll";
    default:
      return "autoLogin.statusAvailable";
  }
}

function maskValue(value: string, revealed: boolean): string {
  if (!value) return "—";
  if (revealed) return value;
  if (value.length <= 8) return "••••";
  return `${value.slice(0, 4)}...${value.slice(-4)}`;
}

export function LoginAccountsTable({
  accounts,
  onDelete,
  onRefresh,
  onUpdateStatus,
  onExportJson,
  onPush,
  pushEnabled = false,
}: Props) {
  const { t } = useTranslation();
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [statusFilter, setStatusFilter] = useState<string>("all");
  const [markOnExport, setMarkOnExport] = useState(true);
  const [exporting, setExporting] = useState(false);
  const [pushing, setPushing] = useState(false);
  const [revealed, setRevealed] = useState<Set<string>>(new Set());

  const counts = useMemo(() => {
    const c = { available: 0, exported: 0, used: 0, invalid: 0 };
    for (const a of accounts) {
      const s = a.status ?? "available";
      if (s in c) c[s as keyof typeof c] += 1;
    }
    return c;
  }, [accounts]);

  const filtered = useMemo(() => {
    if (statusFilter === "all") return accounts;
    return accounts.filter((a) => (a.status ?? "available") === statusFilter);
  }, [accounts, statusFilter]);

  const selectedAccounts = useMemo(
    () => filtered.filter((a) => selected.has(accountKey(a))),
    [filtered, selected],
  );

  const exportTargets = useMemo(() => {
    if (selectedAccounts.length > 0) {
      return selectedAccounts.filter(isExportable);
    }
    return filtered.filter(
      (a) => isExportable(a) && (a.status ?? "available") === "available",
    );
  }, [selectedAccounts, filtered]);

  const pushTargets = exportTargets;

  const allFilteredSelected =
    filtered.length > 0 && selectedAccounts.length === filtered.length;

  const toggleSelect = (id: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const toggleSelectAll = () => {
    if (allFilteredSelected) {
      setSelected(new Set());
      return;
    }
    setSelected(new Set(filtered.map(accountKey)));
  };

  const toggleReveal = (id: string) => {
    setRevealed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const copyText = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      toast.success(t("autoLogin.copied"));
    } catch {
      toast.error(t("autoLogin.copyFailed"));
    }
  };

  const handleExport = async () => {
    if (!onExportJson) return;
    if (exportTargets.length === 0) {
      toast.error(t("autoLogin.exportNoRows"));
      return;
    }
    setExporting(true);
    try {
      const ids = exportTargets.map(accountKey).filter(Boolean);
      // Never mark before save — cancel must leave statuses unchanged.
      const json = await onExportJson(ids, { markExported: false });
      const filePath = await save({
        defaultPath: `sub2api-account-${new Date().toISOString().slice(0, 10)}.json`,
        filters: [{ name: "JSON", extensions: ["json"] }],
      });
      if (!filePath) {
        return;
      }
      await writeTextFile(filePath, json);

      if (markOnExport && onUpdateStatus) {
        await onUpdateStatus(ids, "exported");
      }

      toast.success(t("autoLogin.exportSuccess", { count: ids.length }));
      setSelected(new Set());
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    } finally {
      setExporting(false);
    }
  };

  const handlePush = async () => {
    if (!onPush) return;
    if (pushTargets.length === 0) {
      toast.error(t("autoLogin.exportNoRows"));
      return;
    }
    setPushing(true);
    try {
      const ids = pushTargets.map(accountKey).filter(Boolean);
      const res = await onPush(ids);
      if (res.failed > 0) {
        toast.error(
          t("autoLogin.pushPartial", {
            pushed: res.pushed,
            failed: res.failed,
          }),
        );
      } else {
        toast.success(t("autoLogin.pushSuccess", { count: res.pushed }));
      }
      setSelected(new Set());
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    } finally {
      setPushing(false);
    }
  };

  const handleMarkStatus = async (status: LoginResultStatus) => {
    if (!onUpdateStatus || selectedAccounts.length === 0) return;
    const ids = selectedAccounts.map(accountKey);
    await onUpdateStatus(ids, status);
    toast.success(t("autoLogin.statusUpdated", { count: ids.length }));
    setSelected(new Set());
  };

  const handleBulkDelete = async () => {
    if (!onDelete || selectedAccounts.length === 0) return;
    for (const acc of selectedAccounts) {
      await onDelete(accountKey(acc));
    }
    setSelected(new Set());
  };

  if (accounts.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center rounded-xl border border-dashed bg-muted/20 px-6 py-14 text-center">
        <div className="mb-3 flex h-12 w-12 items-center justify-center rounded-full bg-muted">
          <LuPackageOpen className="h-5 w-5 text-muted-foreground" />
        </div>
        <p className="text-sm font-medium">{t("autoLogin.tabStored")}</p>
        <p className="mt-1 max-w-sm text-sm text-muted-foreground">
          {t("autoLogin.noAccounts")}
        </p>
        {onRefresh && (
          <Button
            variant="outline"
            size="sm"
            className="mt-4"
            onClick={() => void onRefresh()}
          >
            <LuRefreshCw className="mr-1.5 h-3.5 w-3.5" />
            {t("common.buttons.refresh")}
          </Button>
        )}
      </div>
    );
  }

  return (
    <div className="space-y-3">
      <div className="flex flex-col gap-3 rounded-xl border bg-card p-3">
        <div className="flex flex-wrap items-center gap-2">
          <div className="mr-auto flex flex-wrap items-center gap-1.5">
            <button
              type="button"
              onClick={() => setStatusFilter("all")}
              className={cn(
                "rounded-full border px-2.5 py-1 text-xs transition-colors",
                statusFilter === "all"
                  ? "border-primary/30 bg-primary/10 text-foreground"
                  : "border-border bg-background text-muted-foreground hover:bg-muted/50",
              )}
            >
              {t("autoLogin.filterAll")} · {accounts.length}
            </button>
            {(
              [
                ["available", counts.available],
                ["exported", counts.exported],
                ["used", counts.used],
                ["invalid", counts.invalid],
              ] as const
            ).map(([key, count]) => (
              <button
                key={key}
                type="button"
                onClick={() => setStatusFilter(key)}
                className={cn(
                  "rounded-full border px-2.5 py-1 text-xs transition-colors",
                  statusFilter === key
                    ? "border-primary/30 bg-primary/10 text-foreground"
                    : "border-border bg-background text-muted-foreground hover:bg-muted/50",
                )}
              >
                {t(statusLabelKey(key))} · {count}
              </button>
            ))}
          </div>
          {onRefresh && (
            <Button
              variant="ghost"
              size="icon"
              className="h-8 w-8"
              onClick={() => void onRefresh()}
              aria-label={t("common.buttons.refresh")}
            >
              <LuRefreshCw className="h-3.5 w-3.5" />
            </Button>
          )}
        </div>

        <div className="flex flex-wrap items-center gap-2 border-t pt-3">
          <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <Checkbox
              checked={markOnExport}
              onCheckedChange={(v) => setMarkOnExport(Boolean(v))}
              aria-label={t("autoLogin.markExportedOnExport")}
            />
            <span>{t("autoLogin.markExportedOnExport")}</span>
          </div>

          <Button
            size="sm"
            className="ml-auto h-8"
            onClick={() => void handleExport()}
            disabled={exporting || exportTargets.length === 0 || !onExportJson}
          >
            <LuDownload className="mr-1.5 h-3.5 w-3.5" />
            {exporting
              ? t("autoLogin.exporting")
              : t("autoLogin.exportSelected", {
                  count: exportTargets.length,
                })}
          </Button>

          {pushEnabled && onPush && (
            <Button
              size="sm"
              variant="outline"
              className="h-8"
              onClick={() => void handlePush()}
              disabled={pushing || pushTargets.length === 0}
            >
              <LuUpload className="mr-1.5 h-3.5 w-3.5" />
              {pushing
                ? t("autoLogin.pushing")
                : t("autoLogin.pushSelected", { count: pushTargets.length })}
            </Button>
          )}
        </div>

        {selectedAccounts.length > 0 && (
          <div className="flex flex-wrap items-center gap-2 rounded-lg border border-primary/20 bg-primary/5 px-3 py-2">
            <span className="text-xs font-medium">
              {t("autoLogin.selectedCount", {
                count: selectedAccounts.length,
              })}
            </span>
            <div className="ml-auto flex flex-wrap gap-1.5">
              <Button
                size="sm"
                variant="ghost"
                className="h-7"
                onClick={() => void handleMarkStatus("available")}
              >
                {t("autoLogin.markAvailable")}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                className="h-7"
                onClick={() => void handleMarkStatus("exported")}
              >
                {t("autoLogin.markExported")}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                className="h-7"
                onClick={() => void handleMarkStatus("used")}
              >
                {t("autoLogin.markUsed")}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                className="h-7 text-destructive hover:text-destructive"
                onClick={() => void handleMarkStatus("invalid")}
              >
                {t("autoLogin.markInvalid")}
              </Button>
              {onDelete && (
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-7 text-destructive hover:text-destructive"
                  onClick={() => void handleBulkDelete()}
                >
                  <LuTrash2 className="mr-1 h-3.5 w-3.5" />
                  {t("common.buttons.delete")}
                </Button>
              )}
            </div>
          </div>
        )}
      </div>

      <div className="overflow-hidden rounded-xl border bg-card">
        <ScrollArea className="w-full">
          <Table>
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead className="w-10">
                  <Checkbox
                    checked={allFilteredSelected}
                    onCheckedChange={toggleSelectAll}
                    aria-label={t("autoLogin.selectAll")}
                  />
                </TableHead>
                <TableHead>{t("autoLogin.email")}</TableHead>
                <TableHead>{t("autoLogin.status")}</TableHead>
                <TableHead>{t("autoLogin.accountId")}</TableHead>
                <TableHead>{t("autoLogin.accessToken")}</TableHead>
                <TableHead>{t("autoLogin.phoneNumber")}</TableHead>
                <TableHead>{t("autoLogin.createdAt")}</TableHead>
                <TableHead className="w-12" />
              </TableRow>
            </TableHeader>
            <TableBody>
              {filtered.map((acc) => {
                const id = accountKey(acc);
                const status = acc.status ?? "available";
                const isRevealed = revealed.has(id);
                return (
                  <TableRow
                    key={id}
                    data-state={selected.has(id) ? "selected" : undefined}
                    className="group"
                  >
                    <TableCell>
                      <Checkbox
                        checked={selected.has(id)}
                        onCheckedChange={() => toggleSelect(id)}
                        aria-label={acc.email}
                      />
                    </TableCell>
                    <TableCell className="max-w-[200px]">
                      <button
                        type="button"
                        className="truncate font-mono text-xs hover:underline"
                        onClick={() => void copyText(acc.email)}
                        title={acc.email}
                      >
                        {acc.email}
                      </button>
                      {!acc.success && (
                        <p className="mt-0.5 truncate text-[11px] text-destructive">
                          {acc.errorMessage || t("autoLogin.statusFailed")}
                        </p>
                      )}
                    </TableCell>
                    <TableCell>
                      <Badge variant={statusVariant(status)}>
                        {t(statusLabelKey(status))}
                      </Badge>
                    </TableCell>
                    <TableCell className="max-w-[140px] font-mono text-xs text-muted-foreground">
                      <button
                        type="button"
                        className="truncate hover:underline"
                        onClick={() =>
                          acc.accountId && void copyText(acc.accountId)
                        }
                        title={acc.accountId}
                      >
                        {acc.accountId || "—"}
                      </button>
                    </TableCell>
                    <TableCell>
                      <div className="flex items-center gap-1 font-mono text-xs">
                        <span>
                          {maskValue(acc.accessToken ?? "", isRevealed)}
                        </span>
                        {acc.accessToken && (
                          <>
                            <Button
                              variant="ghost"
                              size="icon"
                              className="h-6 w-6"
                              onClick={() => toggleReveal(id)}
                              aria-label={
                                isRevealed
                                  ? t("autoLogin.hideToken")
                                  : t("autoLogin.showToken")
                              }
                            >
                              {isRevealed ? (
                                <LuEyeOff className="h-3 w-3" />
                              ) : (
                                <LuEye className="h-3 w-3" />
                              )}
                            </Button>
                            <Button
                              variant="ghost"
                              size="icon"
                              className="h-6 w-6"
                              onClick={() => void copyText(acc.accessToken)}
                              aria-label={t("common.buttons.copy")}
                            >
                              <LuCopy className="h-3 w-3" />
                            </Button>
                          </>
                        )}
                      </div>
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      {acc.phoneNumber || "—"}
                    </TableCell>
                    <TableCell className="whitespace-nowrap text-xs text-muted-foreground">
                      {acc.createdAt
                        ? new Date(acc.createdAt).toLocaleString()
                        : "—"}
                    </TableCell>
                    <TableCell>
                      {onDelete && (
                        <Button
                          variant="ghost"
                          size="icon"
                          className="h-7 w-7 text-destructive hover:text-destructive"
                          onClick={() => void onDelete(id)}
                          aria-label={t("common.buttons.delete")}
                        >
                          <LuTrash2 className="h-3.5 w-3.5" />
                        </Button>
                      )}
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        </ScrollArea>
      </div>
    </div>
  );
}
