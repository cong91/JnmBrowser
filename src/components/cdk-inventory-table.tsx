"use client";

import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuRefreshCw, LuTrash2 } from "react-icons/lu";
import { Button } from "@/components/ui/button";
import type { CdkInventoryRecord } from "@/hooks/use-registration-events";

interface Props {
  records: CdkInventoryRecord[];
  onRefresh: () => Promise<void> | void;
  onDelete: (cdk: string) => Promise<void> | void;
}

export function CdkInventoryTable({ records, onRefresh, onDelete }: Props) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [expanded, setExpanded] = useState<string | null>(null);

  const totals = useMemo(() => {
    return records.reduce(
      (acc, r) => {
        acc.target += r.targetAccounts || 0;
        acc.attempted += r.attempted || 0;
        acc.yes += r.freeTrialYes || 0;
        acc.no += r.freeTrialNo || 0;
        acc.failed += r.failed || 0;
        return acc;
      },
      { target: 0, attempted: 0, yes: 0, no: 0, failed: 0 },
    );
  }, [records]);

  const handleRefresh = async () => {
    setBusy(true);
    try {
      await onRefresh();
    } finally {
      setBusy(false);
    }
  };

  const handleDelete = async (cdk: string) => {
    setBusy(true);
    try {
      await onDelete(cdk);
      await onRefresh();
    } finally {
      setBusy(false);
    }
  };

  if (records.length === 0) {
    return (
      <div className="space-y-3">
        <div className="flex items-center justify-between">
          <p className="text-sm text-muted-foreground">
            {t("registration.cdkInventoryEmpty")}
          </p>
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => void handleRefresh()}
            disabled={busy}
          >
            <LuRefreshCw className="mr-1.5 h-3.5 w-3.5" />
            {t("common.buttons.refresh")}
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-xs text-muted-foreground">
          {t("registration.cdkInventorySummary", {
            count: records.length,
            yes: totals.yes,
            no: totals.no,
            failed: totals.failed,
          })}
        </p>
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={() => void handleRefresh()}
          disabled={busy}
        >
          <LuRefreshCw className="mr-1.5 h-3.5 w-3.5" />
          {t("common.buttons.refresh")}
        </Button>
      </div>

      <div className="min-h-0 flex-1 overflow-auto rounded-md border">
        <table className="w-full text-left text-xs">
          <thead className="sticky top-0 bg-muted/80 backdrop-blur">
            <tr className="border-b text-muted-foreground">
              <th className="px-2 py-2 font-medium">
                {t("registration.field.cdk")}
              </th>
              <th className="px-2 py-2 font-medium">
                {t("registration.cdkBaseEmail")}
              </th>
              <th className="px-2 py-2 font-medium tabular-nums">
                {t("registration.cdkTarget")}
              </th>
              <th className="px-2 py-2 font-medium tabular-nums">
                {t("registration.cdkFreeTrialYes")}
              </th>
              <th className="px-2 py-2 font-medium tabular-nums">
                {t("registration.cdkFreeTrialNo")}
              </th>
              <th className="px-2 py-2 font-medium tabular-nums">
                {t("registration.cdkFailed")}
              </th>
              <th className="px-2 py-2 font-medium">
                {t("registration.status")}
              </th>
              <th className="px-2 py-2 font-medium" />
            </tr>
          </thead>
          <tbody>
            {records.map((row) => {
              const isOpen = expanded === row.cdk;
              return (
                <tbody key={row.cdk} className="contents">
                  <tr
                    className="cursor-pointer border-b hover:bg-muted/40"
                    onClick={() =>
                      setExpanded((prev) => (prev === row.cdk ? null : row.cdk))
                    }
                  >
                    <td className="px-2 py-2 font-mono text-[11px]">
                      {row.cdk}
                    </td>
                    <td className="px-2 py-2 font-mono text-[11px]">
                      {row.baseEmail || "—"}
                    </td>
                    <td className="px-2 py-2 tabular-nums">
                      {row.attempted}/{row.targetAccounts}
                    </td>
                    <td className="px-2 py-2 tabular-nums text-success">
                      {row.freeTrialYes}
                    </td>
                    <td className="px-2 py-2 tabular-nums text-warning">
                      {row.freeTrialNo}
                    </td>
                    <td className="px-2 py-2 tabular-nums text-destructive">
                      {row.failed}
                    </td>
                    <td className="px-2 py-2 capitalize">{row.status}</td>
                    <td className="px-2 py-2 text-right">
                      <Button
                        type="button"
                        variant="ghost"
                        size="sm"
                        className="h-7 w-7 p-0"
                        disabled={busy}
                        onClick={(e) => {
                          e.stopPropagation();
                          void handleDelete(row.cdk);
                        }}
                      >
                        <LuTrash2 className="h-3.5 w-3.5 text-destructive" />
                      </Button>
                    </td>
                  </tr>
                  {isOpen && (
                    <tr className="border-b bg-muted/20">
                      <td colSpan={8} className="px-3 py-2">
                        {row.lastError ? (
                          <p className="mb-2 text-[11px] text-destructive">
                            {row.lastError}
                          </p>
                        ) : null}
                        {row.accounts?.length ? (
                          <div className="space-y-1">
                            {row.accounts.map((acc, idx) => (
                              <div
                                key={`${acc.email}-${idx}`}
                                className="flex flex-wrap items-center gap-2 font-mono text-[11px]"
                              >
                                <span>{acc.email || "—"}</span>
                                <span
                                  className={
                                    acc.freeTrialEligible
                                      ? "text-success"
                                      : acc.success
                                        ? "text-warning"
                                        : "text-destructive"
                                  }
                                >
                                  {acc.freeTrialEligible
                                    ? t("registration.freeTrialYes")
                                    : acc.email
                                      ? t("registration.freeTrialNo")
                                      : t("registration.cdkFailed")}
                                </span>
                                {acc.planType ? (
                                  <span className="text-muted-foreground">
                                    {acc.planType}
                                  </span>
                                ) : null}
                                {acc.errorMessage ? (
                                  <span className="text-muted-foreground">
                                    {acc.errorMessage}
                                  </span>
                                ) : null}
                              </div>
                            ))}
                          </div>
                        ) : (
                          <p className="text-[11px] text-muted-foreground">
                            {t("registration.cdkNoAccountsYet")}
                          </p>
                        )}
                      </td>
                    </tr>
                  )}
                </tbody>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}
