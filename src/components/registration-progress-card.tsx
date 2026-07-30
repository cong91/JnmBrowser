"use client";

import { useTranslation } from "react-i18next";
import { LuCheck, LuLoader, LuX } from "react-icons/lu";
import { registrationProgressLiveRegion } from "@/components/registration-progress-selection";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import type { RegistrationProgress } from "@/hooks/use-registration-events";

interface Props {
  progress: RegistrationProgress;
  onCancel?: () => void;
}

function terminalLabelKey(statusCode: string): string {
  if (statusCode === "reconciliation_required") {
    return "registration.twoFactorBackfill.outcomes.reconciliationRequired";
  }
  if (statusCode === "completed") {
    return "registration.twoFactorBackfill.steps.completed";
  }
  return "registration.twoFactorBackfill.steps.failed";
}

export function RegistrationProgressCard({ progress, onCancel }: Props) {
  const { t } = useTranslation();
  const terminal = progress.terminal;
  const isComplete = terminal?.success === true;
  const isFailed = terminal?.success === false;
  const { role, ariaLive } = registrationProgressLiveRegion(progress);
  const displayMessage = terminal
    ? t(terminalLabelKey(terminal.statusCode))
    : progress.message || progress.step;

  return (
    <Card
      className="w-full overflow-hidden"
      role={role}
      aria-live={ariaLive}
      aria-atomic="true"
    >
      <CardHeader className="pb-2">
        <CardTitle className="flex items-center gap-2 text-sm font-medium">
          <span
            className={
              isComplete
                ? "flex h-7 w-7 items-center justify-center rounded-full bg-success/10"
                : isFailed
                  ? "flex h-7 w-7 items-center justify-center rounded-full bg-destructive/10"
                  : "flex h-7 w-7 items-center justify-center rounded-full bg-muted"
            }
          >
            {isComplete ? (
              <LuCheck className="h-4 w-4 text-success" />
            ) : isFailed ? (
              <LuX className="h-4 w-4 text-destructive" />
            ) : (
              <LuLoader className="h-4 w-4 animate-spin text-muted-foreground" />
            )}
          </span>
          <span className="min-w-0 flex-1 truncate">{displayMessage}</span>
        </CardTitle>
      </CardHeader>
      {!terminal && onCancel && (
        <CardContent>
          <Button
            variant="outline"
            size="sm"
            className="w-full"
            onClick={onCancel}
          >
            {t("common.buttons.cancel")}
          </Button>
        </CardContent>
      )}
    </Card>
  );
}
