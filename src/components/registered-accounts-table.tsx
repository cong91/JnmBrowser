"use client";

import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  LuCheck,
  LuCopy,
  LuDownload,
  LuEye,
  LuEyeOff,
  LuPackageOpen,
  LuRefreshCw,
  LuSettings2,
  LuTrash2,
} from "react-icons/lu";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
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
import type {
  AccountInventoryStatus,
  RegistrationResult,
} from "@/hooks/use-registration-events";
import { cn } from "@/lib/utils";

interface Props {
  accounts: RegistrationResult[];
  onDelete?: (accountId: string) => void;
  onRefresh?: () => void;
  onUpdateStatus?: (
    accountIds: string[],
    status: AccountInventoryStatus,
    note?: string,
  ) => Promise<void> | void;
}

type ExportField =
  | "email"
  | "password"
  | "totpSecret"
  | "accessToken"
  | "accountId"
  | "deviceId"
  | "planType"
  | "freeTrialEligible"
  | "twoFaEnabled"
  | "status"
  | "cdk"
  | "baseEmail"
  | "createdAt"
  | "note";

type ExportPreset = "seller" | "full" | "token";

const PRESET_FIELDS: Record<ExportPreset, ExportField[]> = {
  seller: ["email", "password", "totpSecret"],
  full: [
    "email",
    "password",
    "totpSecret",
    "accessToken",
    "accountId",
    "planType",
    "freeTrialEligible",
    "twoFaEnabled",
    "status",
    "createdAt",
  ],
  token: ["email", "accessToken", "accountId", "deviceId"],
};

const ALL_EXPORT_FIELDS: ExportField[] = [
  "email",
  "password",
  "totpSecret",
  "accessToken",
  "accountId",
  "deviceId",
  "planType",
  "freeTrialEligible",
  "twoFaEnabled",
  "status",
  "cdk",
  "baseEmail",
  "createdAt",
  "note",
];

function maskValue(value: string, revealed: boolean): string {
  if (!value) return "—";
  if (revealed) return value;
  if (value.length <= 8) return "••••";
  return `${value.slice(0, 4)}...${value.slice(-4)}`;
}

function fieldValue(acc: RegistrationResult, field: ExportField): string {
  switch (field) {
    case "email":
      return acc.email ?? "";
    case "password":
      return acc.password ?? "";
    case "totpSecret":
      return acc.totpSecret ?? "";
    case "accessToken":
      return acc.accessToken ?? "";
    case "accountId":
      return acc.accountId ?? "";
    case "deviceId":
      return acc.deviceId ?? "";
    case "planType":
      return acc.planType ?? "";
    case "freeTrialEligible":
      return acc.freeTrialEligible ? "true" : "false";
    case "twoFaEnabled":
      return acc.twoFaEnabled ? "true" : "false";
    case "status":
      return acc.status ?? "available";
    case "cdk":
      return acc.cdk ?? "";
    case "baseEmail":
      return acc.baseEmail ?? "";
    case "createdAt":
      return acc.createdAt ?? "";
    case "note":
      return acc.note ?? "";
  }
}

function buildExportText(
  rows: RegistrationResult[],
  fields: ExportField[],
  format: "csv" | "txt" | "json",
  delimiter: string,
): string {
  if (format === "json") {
    return JSON.stringify(
      rows.map((acc) => {
        const obj: Record<string, string> = {};
        for (const f of fields) obj[f] = fieldValue(acc, f);
        return obj;
      }),
      null,
      2,
    );
  }

  if (format === "txt") {
    return rows
      .map((acc) => fields.map((f) => fieldValue(acc, f)).join(delimiter))
      .join("\n");
  }

  const escape = (v: string) => {
    if (/[",\n]/.test(v)) return `"${v.replace(/"/g, '""')}"`;
    return v;
  };
  const header = fields.join(",");
  const body = rows
    .map((acc) => fields.map((f) => escape(fieldValue(acc, f))).join(","))
    .join("\n");
  return `${header}\n${body}`;
}

function statusVariant(
  status: AccountInventoryStatus | undefined,
): "default" | "secondary" | "destructive" | "outline" {
  switch (status) {
    case "sold":
      return "secondary";
    case "exported":
      return "outline";
    case "invalid":
      return "destructive";
    case "reserved":
      return "outline";
    default:
      return "default";
  }
}

function statusLabelKey(
  status: AccountInventoryStatus | undefined,
):
  | "registration.statusAvailable"
  | "registration.statusExported"
  | "registration.statusSold"
  | "registration.statusInvalid"
  | "registration.statusReserved" {
  switch (status) {
    case "exported":
      return "registration.statusExported";
    case "sold":
      return "registration.statusSold";
    case "invalid":
      return "registration.statusInvalid";
    case "reserved":
      return "registration.statusReserved";
    default:
      return "registration.statusAvailable";
  }
}

function accountKey(acc: RegistrationResult): string {
  return acc.accountId || acc.email;
}

export function RegisteredAccountsTable({
  accounts,
  onDelete,
  onRefresh,
  onUpdateStatus,
}: Props) {
  const { t } = useTranslation();
  const [revealed, setRevealed] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [statusFilter, setStatusFilter] = useState<string>("all");
  const [exportFields, setExportFields] = useState<ExportField[]>(
    PRESET_FIELDS.seller,
  );
  const [exportPreset, setExportPreset] = useState<ExportPreset>("seller");
  const [exportFormat, setExportFormat] = useState<"csv" | "txt" | "json">(
    "txt",
  );
  const [delimiter, setDelimiter] = useState("|");
  const [markUsedOnExport, setMarkUsedOnExport] = useState(true);
  const [exporting, setExporting] = useState(false);

  const counts = useMemo(() => {
    const c = {
      available: 0,
      exported: 0,
      sold: 0,
      reserved: 0,
      invalid: 0,
    };
    for (const a of accounts) {
      const s = a.status ?? "available";
      c[s] += 1;
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

  const exportCount =
    selectedAccounts.length > 0
      ? selectedAccounts.length
      : filtered.filter((a) => (a.status ?? "available") === "available")
          .length;

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
      toast.success(t("registration.copied"));
    } catch {
      toast.error(t("registration.copyFailed"));
    }
  };

  const toggleSelect = (id: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const allFilteredSelected =
    filtered.length > 0 && selectedAccounts.length === filtered.length;

  const toggleSelectAll = () => {
    if (allFilteredSelected) {
      setSelected(new Set());
      return;
    }
    setSelected(new Set(filtered.map(accountKey)));
  };

  const applyPreset = (preset: ExportPreset) => {
    setExportPreset(preset);
    setExportFields(PRESET_FIELDS[preset]);
  };

  const toggleExportField = (field: ExportField) => {
    setExportFields((prev) => {
      const next = prev.includes(field)
        ? prev.filter((f) => f !== field)
        : [...prev, field];
      if (next.length === 0) return prev;
      setExportPreset(
        (Object.keys(PRESET_FIELDS) as ExportPreset[]).find(
          (p) =>
            PRESET_FIELDS[p].length === next.length &&
            PRESET_FIELDS[p].every((f) => next.includes(f)),
        ) ?? ("seller" as ExportPreset),
      );
      // keep custom selection without forcing wrong preset label
      return next;
    });
  };

  const handleExport = async () => {
    const rows =
      selectedAccounts.length > 0
        ? selectedAccounts
        : filtered.filter((a) => (a.status ?? "available") === "available");
    if (rows.length === 0) {
      toast.error(t("registration.exportNoRows"));
      return;
    }
    setExporting(true);
    try {
      const content = buildExportText(
        rows,
        exportFields,
        exportFormat,
        delimiter || "|",
      );
      const ext =
        exportFormat === "json"
          ? "json"
          : exportFormat === "csv"
            ? "csv"
            : "txt";
      const filePath = await save({
        defaultPath: `chatgpt-accounts-${new Date().toISOString().slice(0, 10)}.${ext}`,
        filters: [
          {
            name: exportFormat.toUpperCase(),
            extensions: [ext],
          },
        ],
      });
      if (!filePath) {
        setExporting(false);
        return;
      }
      await writeTextFile(filePath, content);

      if (markUsedOnExport && onUpdateStatus) {
        const ids = rows.map(accountKey).filter(Boolean);
        await onUpdateStatus(ids, "exported");
      }

      toast.success(t("registration.exportSuccess", { count: rows.length }));
      setSelected(new Set());
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    } finally {
      setExporting(false);
    }
  };

  const handleMarkStatus = async (status: AccountInventoryStatus) => {
    if (!onUpdateStatus || selectedAccounts.length === 0) return;
    const ids = selectedAccounts.map(accountKey);
    await onUpdateStatus(ids, status);
    toast.success(t("registration.statusUpdated", { count: ids.length }));
    setSelected(new Set());
  };

  if (accounts.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center rounded-xl border border-dashed bg-muted/20 px-6 py-14 text-center">
        <div className="mb-3 flex h-12 w-12 items-center justify-center rounded-full bg-muted">
          <LuPackageOpen className="h-5 w-5 text-muted-foreground" />
        </div>
        <p className="text-sm font-medium">
          {t("registration.storedAccounts")}
        </p>
        <p className="mt-1 max-w-sm text-sm text-muted-foreground">
          {t("registration.noAccounts")}
        </p>
        {onRefresh && (
          <Button
            variant="outline"
            size="sm"
            className="mt-4"
            onClick={onRefresh}
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
      {/* Summary + controls */}
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
              {t("registration.filterAll")} · {accounts.length}
            </button>
            {(
              [
                ["available", counts.available],
                ["exported", counts.exported],
                ["sold", counts.sold],
                ["reserved", counts.reserved],
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
              onClick={onRefresh}
              aria-label={t("common.buttons.refresh")}
            >
              <LuRefreshCw className="h-3.5 w-3.5" />
            </Button>
          )}
        </div>

        <div className="flex flex-wrap items-center gap-2 border-t pt-3">
          <div className="flex items-center gap-1 rounded-lg border bg-background p-0.5">
            {(
              [
                ["seller", t("registration.presetSeller")],
                ["token", t("registration.presetToken")],
                ["full", t("registration.presetFull")],
              ] as const
            ).map(([key, label]) => (
              <button
                key={key}
                type="button"
                onClick={() => applyPreset(key)}
                className={cn(
                  "rounded-md px-2.5 py-1.5 text-xs transition-colors",
                  exportPreset === key
                    ? "bg-primary text-primary-foreground"
                    : "text-muted-foreground hover:bg-muted hover:text-foreground",
                )}
              >
                {label}
              </button>
            ))}
          </div>

          <Select
            value={exportFormat}
            onValueChange={(v) => setExportFormat(v as "csv" | "txt" | "json")}
          >
            <SelectTrigger className="h-8 w-[96px]">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="txt">TXT</SelectItem>
              <SelectItem value="csv">CSV</SelectItem>
              <SelectItem value="json">JSON</SelectItem>
            </SelectContent>
          </Select>

          {exportFormat === "txt" && (
            <Input
              className="h-8 w-16 font-mono text-xs"
              value={delimiter}
              onChange={(e) => setDelimiter(e.target.value || "|")}
              aria-label={t("registration.delimiter")}
            />
          )}

          <Popover>
            <PopoverTrigger asChild>
              <Button variant="outline" size="sm" className="h-8">
                <LuSettings2 className="mr-1.5 h-3.5 w-3.5" />
                {t("registration.fields")} ({exportFields.length})
              </Button>
            </PopoverTrigger>
            <PopoverContent align="start" className="w-64 p-3">
              <div className="mb-2 text-xs font-medium text-muted-foreground">
                {t("registration.exportTitle")}
              </div>
              <div className="grid grid-cols-1 gap-1.5">
                {ALL_EXPORT_FIELDS.map((field) => {
                  const checked = exportFields.includes(field);
                  return (
                    <button
                      key={field}
                      type="button"
                      onClick={() => toggleExportField(field)}
                      className={cn(
                        "flex items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs transition-colors",
                        checked
                          ? "bg-primary/10 text-foreground"
                          : "hover:bg-muted text-muted-foreground",
                      )}
                    >
                      <span
                        className={cn(
                          "flex h-4 w-4 items-center justify-center rounded border",
                          checked
                            ? "border-primary bg-primary text-primary-foreground"
                            : "border-border",
                        )}
                      >
                        {checked && <LuCheck className="h-3 w-3" />}
                      </span>
                      {t(`registration.field.${field}`)}
                    </button>
                  );
                })}
              </div>
            </PopoverContent>
          </Popover>

          <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <Checkbox
              checked={markUsedOnExport}
              onCheckedChange={(v) => setMarkUsedOnExport(Boolean(v))}
              aria-label={t("registration.markExportedOnExport")}
            />
            <span>{t("registration.markExportedOnExport")}</span>
          </div>

          <Button
            size="sm"
            className="ml-auto h-8"
            onClick={handleExport}
            disabled={exporting || exportCount === 0}
          >
            <LuDownload className="mr-1.5 h-3.5 w-3.5" />
            {exporting
              ? t("registration.exporting")
              : t("registration.exportSelected", { count: exportCount })}
          </Button>
        </div>

        {selectedAccounts.length > 0 && (
          <div className="flex flex-wrap items-center gap-2 rounded-lg border border-primary/20 bg-primary/5 px-3 py-2">
            <span className="text-xs font-medium">
              {t("registration.selectedCount", {
                count: selectedAccounts.length,
              })}
            </span>
            <div className="ml-auto flex flex-wrap gap-1.5">
              <Button
                size="sm"
                variant="ghost"
                className="h-7"
                onClick={() => handleMarkStatus("available")}
              >
                {t("registration.markAvailable")}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                className="h-7"
                onClick={() => handleMarkStatus("sold")}
              >
                {t("registration.markSold")}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                className="h-7"
                onClick={() => handleMarkStatus("reserved")}
              >
                {t("registration.markReserved")}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                className="h-7 text-destructive hover:text-destructive"
                onClick={() => handleMarkStatus("invalid")}
              >
                {t("registration.markInvalid")}
              </Button>
            </div>
          </div>
        )}
      </div>

      {/* Table */}
      <div className="overflow-hidden rounded-xl border bg-card">
        <ScrollArea className="w-full">
          <Table>
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead className="w-10">
                  <Checkbox
                    checked={allFilteredSelected}
                    onCheckedChange={toggleSelectAll}
                    aria-label={t("registration.selectAll")}
                  />
                </TableHead>
                <TableHead>{t("registration.email")}</TableHead>
                <TableHead>{t("registration.password")}</TableHead>
                <TableHead>{t("registration.status")}</TableHead>
                <TableHead>{t("registration.twoFa")}</TableHead>
                <TableHead>{t("registration.freeTrial")}</TableHead>
                <TableHead>{t("registration.totpSecret")}</TableHead>
                <TableHead>{t("registration.createdAt")}</TableHead>
                <TableHead className="w-12" />
              </TableRow>
            </TableHeader>
            <TableBody>
              {filtered.map((acc) => {
                const id = accountKey(acc);
                const status = acc.status ?? "available";
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
                    <TableCell className="max-w-[180px]">
                      <button
                        type="button"
                        className="truncate font-mono text-xs hover:underline"
                        onClick={() => copyText(acc.email)}
                        title={acc.email}
                      >
                        {acc.email}
                      </button>
                    </TableCell>
                    <TableCell>
                      <div className="flex items-center gap-1 font-mono text-xs">
                        <span>
                          {maskValue(acc.password, revealed.has(acc.accountId))}
                        </span>
                        <button
                          type="button"
                          className="rounded p-1 opacity-60 transition-opacity hover:bg-muted hover:opacity-100"
                          onClick={() => toggleReveal(acc.accountId)}
                          aria-label="toggle password"
                        >
                          {revealed.has(acc.accountId) ? (
                            <LuEyeOff className="h-3 w-3" />
                          ) : (
                            <LuEye className="h-3 w-3" />
                          )}
                        </button>
                        <button
                          type="button"
                          className="rounded p-1 opacity-60 transition-opacity hover:bg-muted hover:opacity-100"
                          onClick={() => copyText(acc.password)}
                          aria-label="copy password"
                        >
                          <LuCopy className="h-3 w-3" />
                        </button>
                      </div>
                    </TableCell>
                    <TableCell>
                      <Badge
                        variant={statusVariant(status)}
                        className="font-normal"
                      >
                        {t(statusLabelKey(status))}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-xs">
                      <span
                        className={cn(
                          "inline-flex rounded-full px-2 py-0.5",
                          acc.twoFaEnabled
                            ? "bg-success/10 text-success"
                            : "bg-muted text-muted-foreground",
                        )}
                      >
                        {acc.twoFaEnabled
                          ? t("registration.twoFaOn")
                          : t("registration.twoFaOff")}
                      </span>
                    </TableCell>
                    <TableCell className="text-xs">
                      <span
                        className={cn(
                          "inline-flex rounded-full px-2 py-0.5",
                          acc.freeTrialEligible
                            ? "bg-success/10 text-success"
                            : "bg-muted text-muted-foreground",
                        )}
                      >
                        {acc.freeTrialEligible
                          ? t("registration.freeTrialYes")
                          : t("registration.freeTrialNo")}
                      </span>
                    </TableCell>
                    <TableCell>
                      {acc.totpSecret ? (
                        <div className="flex items-center gap-1 font-mono text-xs">
                          <span>
                            {maskValue(
                              acc.totpSecret,
                              revealed.has(`${acc.accountId}:totp`),
                            )}
                          </span>
                          <button
                            type="button"
                            className="rounded p-1 opacity-60 transition-opacity hover:bg-muted hover:opacity-100"
                            onClick={() =>
                              toggleReveal(`${acc.accountId}:totp`)
                            }
                            aria-label="toggle totp"
                          >
                            {revealed.has(`${acc.accountId}:totp`) ? (
                              <LuEyeOff className="h-3 w-3" />
                            ) : (
                              <LuEye className="h-3 w-3" />
                            )}
                          </button>
                          <button
                            type="button"
                            className="rounded p-1 opacity-60 transition-opacity hover:bg-muted hover:opacity-100"
                            onClick={() => copyText(acc.totpSecret ?? "")}
                            aria-label="copy totp"
                          >
                            <LuCopy className="h-3 w-3" />
                          </button>
                        </div>
                      ) : (
                        <span className="text-xs text-muted-foreground">—</span>
                      )}
                    </TableCell>
                    <TableCell className="whitespace-nowrap text-xs text-muted-foreground">
                      {acc.createdAt
                        ? new Date(acc.createdAt).toLocaleDateString()
                        : "—"}
                    </TableCell>
                    <TableCell>
                      <Button
                        variant="ghost"
                        size="icon"
                        className="h-8 w-8 opacity-0 transition-opacity group-hover:opacity-100"
                        onClick={() => onDelete?.(acc.accountId)}
                        aria-label={t("common.buttons.delete")}
                      >
                        <LuTrash2 className="h-3.5 w-3.5 text-destructive" />
                      </Button>
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
