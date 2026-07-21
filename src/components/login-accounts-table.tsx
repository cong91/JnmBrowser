"use client";

import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  LuCopy,
  LuDownload,
  LuEye,
  LuEyeOff,
  LuLogIn,
  LuPackageOpen,
  LuPencil,
  LuRefreshCw,
  LuTrash2,
  LuUpload,
} from "react-icons/lu";
import { toast } from "sonner";
import {
  accountKey,
  isAccountReadyForExport,
  toggleAccountSelection,
} from "@/components/login-account-selection";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Textarea } from "@/components/ui/textarea";
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
  /** Re-run auto-login for the given stored rows (needs password on each). */
  onRetryLogin?: (accounts: LoginResult[]) => void | Promise<void>;
  /** Edit credential fields for one stored row. */
  onEditAccount?: (
    accountId: string,
    fields: {
      email?: string;
      password?: string;
      totpSecret?: string;
      note?: string;
      phoneNumber?: string;
      status?: LoginResultStatus;
    },
  ) => Promise<void> | void;
  pushEnabled?: boolean;
  retrying?: boolean;
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

function hasRetryCredential(acc: LoginResult): boolean {
  return Boolean(acc.email?.trim() && acc.password?.trim());
}

export function LoginAccountsTable({
  accounts,
  onDelete,
  onRefresh,
  onUpdateStatus,
  onExportJson,
  onPush,
  onRetryLogin,
  onEditAccount,
  pushEnabled = false,
  retrying = false,
}: Props) {
  const { t } = useTranslation();
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [statusFilter, setStatusFilter] = useState<string>("all");
  const [exporting, setExporting] = useState(false);
  const [pushing, setPushing] = useState(false);
  const [revealed, setRevealed] = useState<Set<string>>(new Set());
  const [editing, setEditing] = useState<LoginResult | null>(null);
  const [editEmail, setEditEmail] = useState("");
  const [editPassword, setEditPassword] = useState("");
  const [editTotp, setEditTotp] = useState("");
  const [editPhone, setEditPhone] = useState("");
  const [editNote, setEditNote] = useState("");
  const [editStatus, setEditStatus] = useState<LoginResultStatus>("available");
  const [savingEdit, setSavingEdit] = useState(false);
  const [showEditSecrets, setShowEditSecrets] = useState(false);

  useEffect(() => {
    if (!editing) return;
    setEditEmail(editing.email ?? "");
    setEditPassword(editing.password ?? "");
    setEditTotp(editing.totpSecret ?? "");
    setEditPhone(editing.phoneNumber ?? "");
    setEditNote(editing.note ?? "");
    setEditStatus(editing.status ?? "available");
    setShowEditSecrets(false);
  }, [editing]);

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

  const selectableAccounts = useMemo(
    () => filtered.filter(isAccountReadyForExport),
    [filtered],
  );

  const exportTargets = useMemo(() => {
    if (selectedAccounts.length > 0) {
      return selectedAccounts.filter(
        (account) => account.success && Boolean(account.accessToken),
      );
    }
    return selectableAccounts;
  }, [selectedAccounts, selectableAccounts]);

  const pushTargets = exportTargets;

  const retryTargets = useMemo(() => {
    if (selectedAccounts.length > 0) {
      return selectedAccounts.filter(hasRetryCredential);
    }
    // No selection: offer all invalid/failed rows that still have credentials.
    return filtered.filter(
      (a) =>
        hasRetryCredential(a) &&
        (!a.success || (a.status ?? "available") === "invalid"),
    );
  }, [selectedAccounts, filtered]);

  const allSelectableSelected =
    selectableAccounts.length > 0 &&
    selectableAccounts.every((a) => selected.has(accountKey(a)));

  const toggleSelect = (account: LoginResult) => {
    setSelected((prev) => toggleAccountSelection(prev, account));
  };

  const toggleSelectAll = () => {
    if (allSelectableSelected) {
      setSelected(new Set());
      return;
    }
    setSelected(new Set(selectableAccounts.map(accountKey)));
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

      // Successful file write always marks exported — no extra checkbox.
      if (onUpdateStatus) {
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
        // Full success → mark exported so they cannot be pushed/exported again.
        if (onUpdateStatus && res.pushed > 0) {
          await onUpdateStatus(ids, "exported");
        }
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

  const handleRetryLogin = async () => {
    if (!onRetryLogin) return;
    if (retryTargets.length === 0) {
      toast.error(t("autoLogin.retryNoCredentials"));
      return;
    }
    await onRetryLogin(retryTargets);
    setSelected(new Set());
  };

  const openEdit = (acc: LoginResult) => {
    setEditing(acc);
  };

  const handleSaveEdit = async () => {
    if (!onEditAccount || !editing) return;
    const email = editEmail.trim();
    if (!email) {
      toast.error(t("autoLogin.editEmailRequired"));
      return;
    }
    setSavingEdit(true);
    try {
      await onEditAccount(accountKey(editing), {
        email,
        password: editPassword,
        totpSecret: editTotp.trim(),
        phoneNumber: editPhone.trim(),
        note: editNote,
        status: editStatus,
      });
      toast.success(t("autoLogin.editSaved"));
      setEditing(null);
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    } finally {
      setSavingEdit(false);
    }
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

          {onRetryLogin && (
            <Button
              size="sm"
              variant="outline"
              className="h-8"
              onClick={() => void handleRetryLogin()}
              disabled={retrying || retryTargets.length === 0}
            >
              <LuLogIn className="mr-1.5 h-3.5 w-3.5" />
              {retrying
                ? t("autoLogin.retrying")
                : t("autoLogin.retryLogin", { count: retryTargets.length })}
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
              {onRetryLogin && (
                <Button
                  size="sm"
                  variant="default"
                  className="h-7"
                  onClick={() => void handleRetryLogin()}
                  disabled={retrying || retryTargets.length === 0}
                >
                  <LuLogIn className="mr-1 h-3.5 w-3.5" />
                  {retrying
                    ? t("autoLogin.retrying")
                    : t("autoLogin.retryLogin", {
                        count: retryTargets.length,
                      })}
                </Button>
              )}
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
                    checked={allSelectableSelected}
                    onCheckedChange={toggleSelectAll}
                    aria-label={t("autoLogin.selectReadyForExport")}
                    title={t("autoLogin.selectReadyForExport")}
                    disabled={selectableAccounts.length === 0}
                  />
                </TableHead>
                <TableHead>{t("autoLogin.email")}</TableHead>
                <TableHead>{t("autoLogin.status")}</TableHead>
                <TableHead>{t("registration.password")}</TableHead>
                <TableHead>{t("registration.totpSecret")}</TableHead>
                <TableHead>{t("autoLogin.accountId")}</TableHead>
                <TableHead>{t("autoLogin.accessToken")}</TableHead>
                <TableHead>{t("autoLogin.phoneNumber")}</TableHead>
                <TableHead>{t("autoLogin.createdAt")}</TableHead>
                <TableHead className="w-20" />
              </TableRow>
            </TableHeader>
            <TableBody>
              {filtered.map((acc) => {
                const id = accountKey(acc);
                const status = acc.status ?? "available";
                const isRevealed = revealed.has(id);
                const totpKey = `${id}:totp`;
                const passKey = `${id}:pass`;
                return (
                  <TableRow
                    key={id}
                    data-state={selected.has(id) ? "selected" : undefined}
                    className="group"
                  >
                    <TableCell>
                      <Checkbox
                        checked={selected.has(id)}
                        onCheckedChange={() => toggleSelect(acc)}
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
                      {acc.note ? (
                        <p className="mt-0.5 truncate text-[11px] text-muted-foreground">
                          {acc.note}
                        </p>
                      ) : null}
                    </TableCell>
                    <TableCell>
                      <Badge variant={statusVariant(status)}>
                        {t(statusLabelKey(status))}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      <div className="flex items-center gap-1 font-mono text-xs">
                        <span>
                          {maskValue(acc.password ?? "", revealed.has(passKey))}
                        </span>
                        {acc.password ? (
                          <>
                            <Button
                              variant="ghost"
                              size="icon"
                              className="h-6 w-6"
                              onClick={() => toggleReveal(passKey)}
                              aria-label={t("autoLogin.showToken")}
                            >
                              {revealed.has(passKey) ? (
                                <LuEyeOff className="h-3 w-3" />
                              ) : (
                                <LuEye className="h-3 w-3" />
                              )}
                            </Button>
                            <Button
                              variant="ghost"
                              size="icon"
                              className="h-6 w-6"
                              onClick={() => void copyText(acc.password ?? "")}
                              aria-label={t("common.buttons.copy")}
                            >
                              <LuCopy className="h-3 w-3" />
                            </Button>
                          </>
                        ) : (
                          <span className="text-muted-foreground">—</span>
                        )}
                      </div>
                    </TableCell>
                    <TableCell>
                      <div className="flex items-center gap-1 font-mono text-xs">
                        <span>
                          {maskValue(
                            acc.totpSecret ?? "",
                            revealed.has(totpKey),
                          )}
                        </span>
                        {acc.totpSecret ? (
                          <>
                            <Button
                              variant="ghost"
                              size="icon"
                              className="h-6 w-6"
                              onClick={() => toggleReveal(totpKey)}
                              aria-label={t("autoLogin.showToken")}
                            >
                              {revealed.has(totpKey) ? (
                                <LuEyeOff className="h-3 w-3" />
                              ) : (
                                <LuEye className="h-3 w-3" />
                              )}
                            </Button>
                            <Button
                              variant="ghost"
                              size="icon"
                              className="h-6 w-6"
                              onClick={() =>
                                void copyText(acc.totpSecret ?? "")
                              }
                              aria-label={t("common.buttons.copy")}
                            >
                              <LuCopy className="h-3 w-3" />
                            </Button>
                          </>
                        ) : (
                          <span className="text-muted-foreground">—</span>
                        )}
                      </div>
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
                      <div className="flex items-center gap-0.5">
                        {onEditAccount && (
                          <Button
                            variant="ghost"
                            size="icon"
                            className="h-7 w-7"
                            onClick={() => openEdit(acc)}
                            aria-label={t("autoLogin.editAccount")}
                          >
                            <LuPencil className="h-3.5 w-3.5" />
                          </Button>
                        )}
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
                      </div>
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        </ScrollArea>
      </div>

      <Dialog
        open={Boolean(editing)}
        onOpenChange={(open) => {
          if (!open) setEditing(null);
        }}
      >
        <DialogContent aria-describedby={undefined} className="max-w-lg">
          <DialogHeader>
            <DialogTitle>{t("autoLogin.editAccount")}</DialogTitle>
          </DialogHeader>
          <div className="space-y-3">
            <div className="space-y-2">
              <Label htmlFor="edit-email">{t("autoLogin.email")}</Label>
              <Input
                id="edit-email"
                value={editEmail}
                onChange={(e) => setEditEmail(e.target.value)}
                autoComplete="off"
              />
            </div>
            <div className="space-y-2">
              <div className="flex items-center justify-between">
                <Label htmlFor="edit-password">
                  {t("registration.password")}
                </Label>
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  className="h-7"
                  onClick={() => setShowEditSecrets((v) => !v)}
                >
                  {showEditSecrets
                    ? t("autoLogin.hideToken")
                    : t("autoLogin.showToken")}
                </Button>
              </div>
              <Input
                id="edit-password"
                type={showEditSecrets ? "text" : "password"}
                value={editPassword}
                onChange={(e) => setEditPassword(e.target.value)}
                autoComplete="off"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="edit-totp">{t("registration.totpSecret")}</Label>
              <Input
                id="edit-totp"
                type={showEditSecrets ? "text" : "password"}
                value={editTotp}
                onChange={(e) => setEditTotp(e.target.value)}
                autoComplete="off"
                className="font-mono"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="edit-phone">{t("autoLogin.phoneNumber")}</Label>
              <Input
                id="edit-phone"
                value={editPhone}
                onChange={(e) => setEditPhone(e.target.value)}
                autoComplete="off"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="edit-status">{t("autoLogin.status")}</Label>
              <Select
                value={editStatus}
                onValueChange={(v) => setEditStatus(v as LoginResultStatus)}
              >
                <SelectTrigger id="edit-status">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="available">
                    {t("autoLogin.statusAvailable")}
                  </SelectItem>
                  <SelectItem value="exported">
                    {t("autoLogin.statusExported")}
                  </SelectItem>
                  <SelectItem value="used">
                    {t("autoLogin.statusUsed")}
                  </SelectItem>
                  <SelectItem value="invalid">
                    {t("autoLogin.statusInvalid")}
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-2">
              <Label htmlFor="edit-note">{t("autoLogin.note")}</Label>
              <Textarea
                id="edit-note"
                value={editNote}
                onChange={(e) => setEditNote(e.target.value)}
                rows={3}
              />
            </div>
          </div>
          <DialogFooter className="gap-2 sm:gap-0">
            <Button
              type="button"
              variant="outline"
              onClick={() => setEditing(null)}
              disabled={savingEdit}
            >
              {t("common.buttons.cancel")}
            </Button>
            <Button
              type="button"
              onClick={() => void handleSaveEdit()}
              disabled={savingEdit || !onEditAccount}
            >
              {savingEdit ? t("autoLogin.saving") : t("common.buttons.save")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
