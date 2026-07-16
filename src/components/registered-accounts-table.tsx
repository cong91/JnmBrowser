"use client";

import { useTranslation } from "react-i18next";
import { LuCopy, LuEye, LuEyeOff, LuTrash2 } from "react-icons/lu";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import type { RegistrationResult } from "@/hooks/use-registration-events";

interface Props {
  accounts: RegistrationResult[];
  onDelete?: (accountId: string) => void;
  onRefresh?: () => void;
}

function maskValue(value: string, revealed: boolean): string {
  if (!value) return "—";
  if (revealed) return value;
  if (value.length <= 8) return "••••";
  return `${value.slice(0, 4)}...${value.slice(-4)}`;
}

export function RegisteredAccountsTable({ accounts, onDelete, onRefresh }: Props) {
  const { t } = useTranslation();
  const [revealedPasswords, setRevealedPasswords] = useState<Set<string>>(
    new Set(),
  );

  const toggleReveal = (accountId: string) => {
    setRevealedPasswords((prev) => {
      const next = new Set(prev);
      if (next.has(accountId)) {
        next.delete(accountId);
      } else {
        next.add(accountId);
      }
      return next;
    });
  };

  const copyText = (text: string) => {
    navigator.clipboard.writeText(text);
  };

  if (accounts.length === 0) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">
            {t("registration.storedAccounts")}
          </CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-sm text-muted-foreground">
            {t("registration.noAccounts")}
          </p>
          {onRefresh && (
            <Button variant="outline" size="sm" className="mt-2" onClick={onRefresh}>
              {t("common.buttons.refresh")}
            </Button>
          )}
        </CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">
          {t("registration.storedAccounts")} ({accounts.length})
        </CardTitle>
      </CardHeader>
      <CardContent>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>{t("registration.email")}</TableHead>
              <TableHead>{t("registration.password")}</TableHead>
              <TableHead>{t("registration.accountId")}</TableHead>
              <TableHead>{t("registration.createdAt")}</TableHead>
              <TableHead className="w-24">{t("common.labels.actions")}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {accounts.map((acc) => (
              <TableRow key={acc.accountId || acc.email}>
                <TableCell className="font-mono text-xs">
                  <button
                    type="button"
                    className="hover:underline cursor-pointer"
                    onClick={() => copyText(acc.email)}
                    title={acc.email}
                  >
                    {acc.email.length > 30
                      ? `${acc.email.slice(0, 15)}...${acc.email.slice(-10)}`
                      : acc.email}
                  </button>
                </TableCell>
                <TableCell className="font-mono text-xs">
                  <div className="flex items-center gap-1">
                    <span>
                      {maskValue(acc.password, revealedPasswords.has(acc.accountId))}
                    </span>
                    <button
                      type="button"
                      className="cursor-pointer"
                      onClick={() => toggleReveal(acc.accountId)}
                      title={revealedPasswords.has(acc.accountId) ? "Hide" : "Show"}
                    >
                      {revealedPasswords.has(acc.accountId) ? (
                        <LuEyeOff className="h-3 w-3" />
                      ) : (
                        <LuEye className="h-3 w-3" />
                      )}
                    </button>
                    <button
                      type="button"
                      className="cursor-pointer"
                      onClick={() => copyText(acc.password)}
                      title="Copy"
                    >
                      <LuCopy className="h-3 w-3" />
                    </button>
                  </div>
                </TableCell>
                <TableCell className="font-mono text-xs">
                  {acc.accountId
                    ? `${acc.accountId.slice(0, 8)}...`
                    : "—"}
                </TableCell>
                <TableCell className="text-xs text-muted-foreground">
                  {acc.createdAt
                    ? new Date(acc.createdAt).toLocaleDateString()
                    : "—"}
                </TableCell>
                <TableCell>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="h-7 w-7"
                    onClick={() => onDelete?.(acc.accountId)}
                    title={t("common.buttons.delete")}
                  >
                    <LuTrash2 className="h-3.5 w-3.5 text-destructive" />
                  </Button>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  );
}
