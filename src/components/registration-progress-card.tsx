"use client";

import { useTranslation } from "react-i18next";
import { LuCheck, LuLoader, LuX } from "react-icons/lu";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ScrollArea } from "@/components/ui/scroll-area";
import type { RegistrationProgress, RegistrationResult } from "@/hooks/use-registration-events";

interface Props {
  progress: RegistrationProgress;
  onCancel?: () => void;
}

function maskValue(value: string): string {
  if (!value) return "";
  if (value.length <= 8) return "••••••••";
  return `${value.slice(0, 4)}...${value.slice(-4)}`;
}

function CredentialRow({
  label,
  value,
  masked = false,
}: {
  label: string;
  value: string;
  masked?: boolean;
}) {
  const display = masked ? maskValue(value) : value;
  if (!display) return null;

  const copy = () => navigator.clipboard.writeText(value);

  return (
    <div className="flex items-center justify-between py-1 text-sm">
      <span className="text-muted-foreground">{label}</span>
      <button
        type="button"
        className="font-mono text-xs hover:underline cursor-pointer"
        onClick={copy}
        title={value}
      >
        {display}
      </button>
    </div>
  );
}

export function RegistrationProgressCard({ progress, onCancel }: Props) {
  const { t } = useTranslation();

  const isComplete = progress.result?.success;
  const isFailed = progress.result && !progress.result.success;

  return (
    <Card className="w-full">
      <CardHeader className="pb-2">
        <CardTitle className="flex items-center gap-2 text-base">
          {isComplete ? (
            <LuCheck className="h-5 w-5 text-success" />
            ) : isFailed ? (
            <LuX className="h-5 w-5 text-destructive" />
          ) : (
            <LuLoader className="h-5 w-5 animate-spin text-muted-foreground" />
          )}
          {progress.message || progress.step}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        {isComplete && progress.result && (
          <div className="space-y-1 rounded-md border p-3">
            <CredentialRow label={t("registration.email")} value={progress.result.email} />
            <CredentialRow label={t("registration.password")} value={progress.result.password} masked />
            <CredentialRow label={t("registration.accountId")} value={progress.result.accountId} />
            <CredentialRow label={t("registration.accessToken")} value={progress.result.accessToken} masked />
          </div>
        )}

        {isFailed && (
          <div className="rounded-md bg-destructive/10 p-3 text-sm text-destructive">
            {progress.result?.errorMessage || progress.message}
          </div>
        )}

        {!isComplete && !isFailed && onCancel && (
          <Button
            variant="outline"
            size="sm"
            className="w-full"
            onClick={onCancel}
          >
            {t("common.buttons.cancel")}
          </Button>
        )}

        {progress.result?.stepLogs && progress.result.stepLogs.length > 0 && (
          <ScrollArea className="h-32 rounded-md border">
            <div className="p-2 font-mono text-xs space-y-0.5">
              {progress.result.stepLogs.map((log, i) => (
                <div key={i} className="text-muted-foreground">
                  {log}
                </div>
              ))}
            </div>
          </ScrollArea>
        )}
      </CardContent>
    </Card>
  );
}
