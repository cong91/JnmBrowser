"use client";

import {
  AlertTriangle,
  CircleStop,
  LoaderCircle,
  ShieldCheck,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuCheck, LuX } from "react-icons/lu";
import {
  type AutomationProfilePolicy,
  automationErrorTranslationKey,
  automationProfilePolicyPayload,
  DEFAULT_AUTOMATION_PROFILE_POLICY,
} from "@/components/automation-profile-policy";
import { AutomationProfilePolicyFields } from "@/components/automation-profile-policy-fields";
import {
  backfillReasonCode,
  summarizeBackfillPreview,
} from "@/components/two-factor-backfill-selection";
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
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { useProxyEvents } from "@/hooks/use-proxy-events";
import {
  type AccountInventoryStatus,
  type BackfillBrowser,
  type BackfillMode,
  type BackfillNetworkConfig,
  type BackfillOutcome,
  type BackfillStep,
  type RegistrationOutcomeReason,
  type TwoFactorBackfillExclusion,
  type TwoFactorBackfillIneligibilityReason,
  type TwoFactorBackfillPreview,
  type TwoFactorBackfillRecoverySummary,
  useTwoFactorBackfillEvents,
} from "@/hooks/use-two-factor-backfill-events";
import { useVpnEvents } from "@/hooks/use-vpn-events";
import { cn } from "@/lib/utils";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  selectedAccountKeys: string[];
  onTerminal: () => Promise<void> | void;
  onRunningChange?: (running: boolean) => void;
}

type NetworkMode = "none" | "proxy" | "vpn";

const STEP_KEYS: Record<BackfillStep, string> = {
  eligibility: "registration.twoFactorBackfill.steps.eligibility",
  providerMigration: "registration.twoFactorBackfill.steps.providerMigration",
  login: "registration.twoFactorBackfill.steps.login",
  emailOtp: "registration.twoFactorBackfill.steps.emailOtp",
  inspectTwoFactor: "registration.twoFactorBackfill.steps.inspectTwoFactor",
  captureSecret: "registration.twoFactorBackfill.steps.captureSecret",
  confirmTwoFactor: "registration.twoFactorBackfill.steps.confirmTwoFactor",
  verifyRemote: "registration.twoFactorBackfill.steps.verifyRemote",
  persistAccount: "registration.twoFactorBackfill.steps.persistAccount",
  journalComplete: "registration.twoFactorBackfill.steps.journalComplete",
  batchPaused: "registration.twoFactorBackfill.steps.batchPaused",
  cancelled: "registration.twoFactorBackfill.steps.cancelled",
  completed: "registration.twoFactorBackfill.steps.completed",
  failed: "registration.twoFactorBackfill.steps.failed",
};

const OUTCOME_KEYS: Record<BackfillOutcome, string> = {
  enabled: "registration.twoFactorBackfill.outcomes.enabled",
  failed: "registration.twoFactorBackfill.outcomes.failed",
  cancelled: "registration.twoFactorBackfill.outcomes.cancelled",
  reconciliationRequired:
    "registration.twoFactorBackfill.outcomes.reconciliationRequired",
  batchPaused: "registration.twoFactorBackfill.outcomes.batchPaused",
};

const INVENTORY_STATUS_KEYS: Record<AccountInventoryStatus, string> = {
  available: "registration.statusAvailable",
  exported: "registration.statusExported",
  sold: "registration.statusSold",
  invalid: "registration.statusInvalid",
  reserved: "registration.statusReserved",
};

const REGISTRATION_OUTCOME_REASON_KEYS: Record<
  RegistrationOutcomeReason,
  string
> = {
  registered: "registration.twoFactorBackfill.registrationOutcomes.registered",
  free_trial_no:
    "registration.twoFactorBackfill.registrationOutcomes.freeTrialNo",
  registration_failed:
    "registration.twoFactorBackfill.registrationOutcomes.registrationFailed",
  batch_summary:
    "registration.twoFactorBackfill.registrationOutcomes.batchSummary",
};

const EXCLUSION_KEYS: Record<TwoFactorBackfillExclusion, string> = {
  operator_excluded:
    "registration.twoFactorBackfill.exclusions.operatorExcluded",
  manual_review: "registration.twoFactorBackfill.exclusions.manualReview",
};

const UNIT_REASON_KEYS: Record<string, string> = {
  account_not_found: "registration.twoFactorBackfill.reasons.accountNotFound",
  missing_account_key:
    "registration.twoFactorBackfill.reasons.missingAccountKey",
  registration_unsuccessful:
    "registration.twoFactorBackfill.reasons.registrationUnsuccessful",
  missing_email: "registration.twoFactorBackfill.reasons.missingEmail",
  missing_password: "registration.twoFactorBackfill.reasons.missingPassword",
  free_trial_no_override_required:
    "registration.twoFactorBackfill.reasons.freeTrialNoOverrideRequired",
  two_factor_already_enabled:
    "registration.twoFactorBackfill.reasons.twoFactorAlreadyEnabled",
  inconsistent_local_two_factor_state:
    "registration.twoFactorBackfill.reasons.inconsistentLocalTwoFactorState",
  missing_cdk: "registration.twoFactorBackfill.reasons.missingCdk",
  unknown_cdk_prefix: "registration.twoFactorBackfill.reasons.unknownCdkPrefix",
  missing_email_provider_provenance:
    "registration.twoFactorBackfill.reasons.missingEmailProviderProvenance",
  legacy_access_acknowledgement_required:
    "registration.twoFactorBackfill.reasons.legacyAccessAcknowledgementRequired",
  access_locked: "registration.twoFactorBackfill.reasons.accessLocked",
  backfill_in_progress:
    "registration.twoFactorBackfill.reasons.backfillInProgress",
  backfill_completed:
    "registration.twoFactorBackfill.reasons.backfillCompleted",
  backfill_reconciliation_requires_manual_review:
    "registration.twoFactorBackfill.reasons.reconciliationManualReview",
  backfill_completed_outcome_missing:
    "registration.twoFactorBackfill.reasons.completedOutcomeMissing",
};

function reasonTranslation(
  reason: TwoFactorBackfillIneligibilityReason,
  translate: (key: string) => string,
): {
  key: string;
  values?: Record<string, string>;
} {
  if (typeof reason === "string") {
    return {
      key:
        UNIT_REASON_KEYS[reason] ??
        "registration.twoFactorBackfill.reasons.unknown",
    };
  }
  if ("inventory_status_not_available" in reason) {
    return {
      key: "registration.twoFactorBackfill.reasons.inventoryStatusNotAvailable",
      values: {
        status: translate(
          INVENTORY_STATUS_KEYS[reason.inventory_status_not_available],
        ),
      },
    };
  }
  if ("invalid_outcome_not_eligible" in reason) {
    const outcome = reason.invalid_outcome_not_eligible;
    return {
      key: "registration.twoFactorBackfill.reasons.invalidOutcomeNotEligible",
      values: {
        reason: outcome
          ? translate(REGISTRATION_OUTCOME_REASON_KEYS[outcome])
          : translate("registration.twoFactorBackfill.reasons.unknownOutcome"),
      },
    };
  }
  return {
    key: "registration.twoFactorBackfill.reasons.explicitlyExcluded",
    values: { reason: translate(EXCLUSION_KEYS[reason.explicitly_excluded]) },
  };
}

function Segment<T extends string>({
  value,
  options,
  onChange,
  disabled,
}: {
  value: T;
  options: Array<{ value: T; label: string; disabled?: boolean }>;
  onChange: (value: T) => void;
  disabled?: boolean;
}) {
  return (
    <div className="inline-flex min-h-9 items-center rounded-md border bg-muted/30 p-0.5">
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          className={cn(
            "min-h-8 px-3 text-xs font-medium transition-colors",
            value === option.value
              ? "rounded-sm bg-background text-foreground shadow-xs"
              : "text-muted-foreground hover:text-foreground",
            (disabled || option.disabled) && "cursor-not-allowed opacity-50",
          )}
          disabled={disabled || option.disabled}
          aria-pressed={value === option.value}
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

export function TwoFactorBackfillDialog({
  open,
  onOpenChange,
  selectedAccountKeys,
  onTerminal,
  onRunningChange,
}: Props) {
  const { t } = useTranslation();
  const { storedProxies, isLoading: isLoadingProxies } = useProxyEvents();
  const { vpnConfigs, isLoading: isLoadingVpns } = useVpnEvents();
  const {
    progressMap,
    running,
    starting,
    cancellable,
    terminal,
    error,
    preview,
    listRecovery,
    recoverJournal,
    start,
    cancel,
    reset,
  } = useTwoFactorBackfillEvents(onTerminal);

  const [previewResult, setPreviewResult] =
    useState<TwoFactorBackfillPreview | null>(null);
  const [previewLoading, setPreviewLoading] = useState(false);
  const [mode, setMode] = useState<BackfillMode>("canary");
  const [browser, setBrowser] = useState<BackfillBrowser>("chromium");
  const [profilePolicy, setProfilePolicy] = useState<AutomationProfilePolicy>({
    ...DEFAULT_AUTOMATION_PROFILE_POLICY,
  });
  const [networkMode, setNetworkMode] = useState<NetworkMode>("none");
  const [proxyId, setProxyId] = useState("");
  const [vpnId, setVpnId] = useState("");
  const [allowFreeTrialNo, setAllowFreeTrialNo] = useState(false);
  const [acknowledgeLegacyAccess, setAcknowledgeLegacyAccess] = useState(false);
  const [showFreeTrialOverride, setShowFreeTrialOverride] = useState(false);
  const [showLegacyAcknowledgement, setShowLegacyAcknowledgement] =
    useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [recoveryEntries, setRecoveryEntries] = useState<
    TwoFactorBackfillRecoverySummary[]
  >([]);
  const previewRequestIdRef = useRef(0);

  const networkConfig = useMemo<BackfillNetworkConfig>(() => {
    if (networkMode === "proxy") return { kind: "proxy", proxyId };
    if (networkMode === "vpn") return { kind: "vpn", vpnId };
    return { kind: "none" };
  }, [networkMode, proxyId, vpnId]);

  const loadRecovery = useCallback(async () => {
    try {
      setRecoveryEntries(await listRecovery());
    } catch {
      setRecoveryEntries([]);
    }
  }, [listRecovery]);

  const loadPreview = useCallback(
    async (allowFreeTrial: boolean, acknowledgeLegacy: boolean) => {
      const requestId = ++previewRequestIdRef.current;
      setPreviewLoading(true);
      try {
        const result = await preview({
          selectedAccountKeys,
          allowFreeTrialNo: allowFreeTrial,
          acknowledgeLegacyAccess: acknowledgeLegacy,
          ...automationProfilePolicyPayload(profilePolicy),
          browser,
          network: networkConfig,
        });
        if (requestId !== previewRequestIdRef.current) return;
        setPreviewResult(result);
        const previewState = summarizeBackfillPreview(result.accounts);
        if (previewState.requiresFreeTrialOverride) {
          setShowFreeTrialOverride(true);
        }
        if (previewState.requiresLegacyAcknowledgement) {
          setShowLegacyAcknowledgement(true);
        }
        if (!previewState.selectionEligible) {
          setMode("canary");
        }
      } catch {
        if (requestId !== previewRequestIdRef.current) return;
        setPreviewResult(null);
      } finally {
        if (requestId === previewRequestIdRef.current) {
          setPreviewLoading(false);
        }
      }
    },
    [browser, networkConfig, preview, profilePolicy, selectedAccountKeys],
  );

  useEffect(() => {
    if (!open) return;
    previewRequestIdRef.current += 1;
    reset();
    setPreviewResult(null);
    setMode("canary");
    setAllowFreeTrialNo(false);
    setAcknowledgeLegacyAccess(false);
    setShowFreeTrialOverride(false);
    setShowLegacyAcknowledgement(false);
    setCancelling(false);
    void loadRecovery();
  }, [loadRecovery, open, reset]);

  useEffect(() => {
    if (!open) return;
    void loadPreview(allowFreeTrialNo, acknowledgeLegacyAccess);
  }, [acknowledgeLegacyAccess, allowFreeTrialNo, loadPreview, open]);

  useEffect(() => {
    onRunningChange?.(running);
    return () => onRunningChange?.(false);
  }, [onRunningChange, running]);

  const accounts = previewResult?.accounts ?? [];
  const previewState = summarizeBackfillPreview(
    accounts,
    previewResult?.bulkAvailable ?? false,
  );
  const { eligibleCount, selectionEligible, canaryReady, bulkReady } =
    previewState;
  const routeReady =
    networkMode === "none" ||
    (networkMode === "proxy" && Boolean(proxyId)) ||
    (networkMode === "vpn" && Boolean(vpnId));
  const startReady =
    !previewLoading &&
    !running &&
    routeReady &&
    (mode === "canary" ? canaryReady : bulkReady);

  const progress = Array.from(progressMap.values());
  const emailByKey = useMemo(
    () =>
      new Map(accounts.map((account) => [account.accountKey, account.email])),
    [accounts],
  );
  const outcomes = progress
    .filter((item) => item.accountKey && item.outcome)
    .map((item) => item.outcome);
  const enabledCount = outcomes.filter(
    (outcome) => outcome === "enabled",
  ).length;
  const failedCount = outcomes.filter((outcome) => outcome === "failed").length;
  const reviewCount = outcomes.filter(
    (outcome) => outcome === "reconciliationRequired",
  ).length;
  const cancelledCount = outcomes.filter(
    (outcome) => outcome === "cancelled",
  ).length;

  const handleStart = async () => {
    if (!startReady) return;
    setCancelling(false);
    try {
      await start({
        selectedAccountKeys,
        allowFreeTrialNo,
        acknowledgeLegacyAccess,
        ...automationProfilePolicyPayload(profilePolicy),
        browser,
        network: networkConfig,
        mode,
      });
    } catch {
      // The hook exposes the backend failure in the dialog.
    }
  };

  const handleCancel = async () => {
    if (!cancellable) return;
    setCancelling(true);
    try {
      await cancel();
    } catch {
      setCancelling(false);
    }
  };

  const handleRecover = async (entry: TwoFactorBackfillRecoverySummary) => {
    try {
      await recoverJournal(entry.operationId, entry.accountKey);
      await Promise.all([loadRecovery(), onTerminal()]);
    } catch {
      // Recovery failures remain visible through the next journal refresh.
    }
  };

  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen && running) return;
    onOpenChange(nextOpen);
  };

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        aria-describedby={undefined}
        className="flex h-[min(90vh,780px)] w-[min(94vw,760px)] max-w-none flex-col overflow-hidden p-0"
      >
        <DialogHeader className="border-b px-5 pb-4 pt-5">
          <DialogTitle className="flex items-center gap-2">
            <ShieldCheck className="h-5 w-5 text-primary" />
            {t("registration.twoFactorBackfill.title")}
          </DialogTitle>
        </DialogHeader>

        <ScrollArea className="min-h-0 flex-1">
          <div className="divide-y">
            <section className="grid gap-4 px-5 py-4 sm:grid-cols-2">
              <div className="space-y-2">
                <Label>{t("registration.twoFactorBackfill.mode")}</Label>
                <Segment
                  value={mode}
                  disabled={running}
                  onChange={setMode}
                  options={[
                    {
                      value: "canary",
                      label: t("registration.twoFactorBackfill.canary"),
                    },
                    {
                      value: "bulk",
                      label: t("registration.twoFactorBackfill.bulk"),
                      disabled: !bulkReady,
                    },
                  ]}
                />
                {mode === "canary" && !canaryReady ? (
                  <p className="text-xs text-warning">
                    {t("registration.twoFactorBackfill.canaryRequiresOne")}
                  </p>
                ) : null}
                {selectionEligible && !bulkReady ? (
                  <p className="text-xs text-warning">
                    {t("registration.twoFactorBackfill.bulkRequiresCanary")}
                  </p>
                ) : null}
              </div>

              <div className="space-y-2">
                <Label>{t("registration.browserType")}</Label>
                <Segment
                  value={browser}
                  disabled={running}
                  onChange={setBrowser}
                  options={[
                    { value: "chromium", label: t("browser.chromium") },
                    { value: "camoufox", label: t("browser.camoufox") },
                  ]}
                />
              </div>

              <div className="sm:col-span-2">
                <AutomationProfilePolicyFields
                  idPrefix="twofa-backfill"
                  browserType={browser}
                  value={profilePolicy}
                  onChange={setProfilePolicy}
                  disabled={running}
                  active={open}
                />
              </div>

              <div className="space-y-2 sm:col-span-2">
                <Label>{t("registration.networkMode")}</Label>
                <Segment
                  value={networkMode}
                  disabled={running}
                  onChange={setNetworkMode}
                  options={[
                    {
                      value: "none",
                      label: t("common.labels.none"),
                    },
                    {
                      value: "proxy",
                      label: t("registration.networkModeProxy"),
                    },
                    {
                      value: "vpn",
                      label: t("registration.networkModeVpn"),
                    },
                  ]}
                />
              </div>

              {networkMode === "proxy" ? (
                <div className="space-y-2 sm:col-span-2">
                  <Label htmlFor="twofa-backfill-proxy">
                    {t("registration.twoFactorBackfill.proxy")}
                  </Label>
                  <Select
                    value={proxyId || undefined}
                    onValueChange={setProxyId}
                    disabled={running || isLoadingProxies}
                  >
                    <SelectTrigger id="twofa-backfill-proxy">
                      <SelectValue
                        placeholder={t(
                          isLoadingProxies
                            ? "registration.twoFactorBackfill.proxyLoading"
                            : "registration.twoFactorBackfill.proxyPlaceholder",
                        )}
                      />
                    </SelectTrigger>
                    <SelectContent>
                      {storedProxies
                        .filter(
                          (proxy) =>
                            !proxy.is_cloud_managed && !proxy.is_cloud_derived,
                        )
                        .map((proxy) => (
                          <SelectItem key={proxy.id} value={proxy.id}>
                            {proxy.name}
                          </SelectItem>
                        ))}
                    </SelectContent>
                  </Select>
                </div>
              ) : null}

              {networkMode === "vpn" ? (
                <div className="space-y-2 sm:col-span-2">
                  <Label htmlFor="twofa-backfill-vpn">
                    {t("registration.vpn")}
                  </Label>
                  <Select
                    value={vpnId || undefined}
                    onValueChange={setVpnId}
                    disabled={running || isLoadingVpns}
                  >
                    <SelectTrigger id="twofa-backfill-vpn">
                      <SelectValue
                        placeholder={t(
                          isLoadingVpns
                            ? "registration.vpnLoading"
                            : vpnConfigs.length === 0
                              ? "registration.vpnEmpty"
                              : "registration.vpnPlaceholder",
                        )}
                      />
                    </SelectTrigger>
                    <SelectContent>
                      {vpnConfigs.map((vpn) => (
                        <SelectItem key={vpn.id} value={vpn.id}>
                          {vpn.name} ({vpn.vpn_type})
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>
              ) : null}
            </section>

            <section className="space-y-3 px-5 py-4">
              <div className="flex items-center justify-between gap-3">
                <Label>{t("registration.twoFactorBackfill.recovery")}</Label>
                <span className="text-xs text-muted-foreground">
                  {recoveryEntries.length}
                </span>
              </div>
              {recoveryEntries.length > 0 ? (
                <div className="divide-y rounded-md border">
                  {recoveryEntries.map((entry) => (
                    <div
                      key={`${entry.operationId}:${entry.accountKey}`}
                      className="flex flex-wrap items-center gap-2 px-3 py-2.5"
                    >
                      <span className="min-w-0 flex-1 truncate text-sm font-medium">
                        {entry.accountKey}
                      </span>
                      <Badge variant="outline" className="font-normal">
                        {t(
                          `registration.twoFactorBackfill.recoveryStates.${entry.state}`,
                        )}
                      </Badge>
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={running}
                        onClick={() => void handleRecover(entry)}
                      >
                        {t("registration.twoFactorBackfill.recover")}
                      </Button>
                    </div>
                  ))}
                </div>
              ) : null}
            </section>

            <section className="space-y-3 px-5 py-4">
              <div className="flex items-center justify-between gap-3">
                <Label>{t("registration.twoFactorBackfill.preview")}</Label>
                <span className="text-xs tabular-nums text-muted-foreground">
                  {t("registration.twoFactorBackfill.eligibilityCount", {
                    eligible: eligibleCount,
                    total: accounts.length,
                  })}
                </span>
              </div>

              {previewLoading ? (
                <div className="flex min-h-20 items-center justify-center text-muted-foreground">
                  <LoaderCircle className="h-4 w-4 animate-spin" />
                  <span className="ml-2 text-sm">
                    {t("registration.twoFactorBackfill.previewing")}
                  </span>
                </div>
              ) : (
                <div className="divide-y rounded-md border">
                  {accounts.map((account) => (
                    <div key={account.accountKey} className="px-3 py-2.5">
                      <div className="flex min-w-0 items-center gap-2">
                        {account.eligible ? (
                          <LuCheck className="h-4 w-4 shrink-0 text-success" />
                        ) : (
                          <LuX className="h-4 w-4 shrink-0 text-destructive" />
                        )}
                        <span className="min-w-0 flex-1 truncate text-sm font-medium">
                          {account.email.trim() ||
                            t("registration.twoFactorBackfill.unknownAccount")}
                        </span>
                        <Badge
                          variant={account.eligible ? "default" : "destructive"}
                          className="font-normal"
                        >
                          {t(
                            account.eligible
                              ? "registration.twoFactorBackfill.eligible"
                              : "registration.twoFactorBackfill.ineligible",
                          )}
                        </Badge>
                      </div>
                      {account.ineligibilityReasons.length > 0 ? (
                        <ul className="mt-1.5 space-y-1 pl-6 text-xs text-muted-foreground">
                          {account.ineligibilityReasons.map((reason, index) => {
                            const translated = reasonTranslation(reason, t);
                            return (
                              <li
                                key={`${backfillReasonCode(reason)}-${index}`}
                              >
                                {t(translated.key, translated.values)}
                              </li>
                            );
                          })}
                        </ul>
                      ) : null}
                    </div>
                  ))}
                </div>
              )}

              {showFreeTrialOverride ? (
                <div className="flex items-start gap-2 text-sm">
                  <Checkbox
                    id="twofa-backfill-free-trial-override"
                    checked={allowFreeTrialNo}
                    disabled={running}
                    onCheckedChange={(checked) => {
                      setAllowFreeTrialNo(checked === true);
                    }}
                  />
                  <Label
                    htmlFor="twofa-backfill-free-trial-override"
                    className="font-normal"
                  >
                    {t("registration.twoFactorBackfill.allowFreeTrialNo")}
                  </Label>
                </div>
              ) : null}

              {showLegacyAcknowledgement ? (
                <div className="flex items-start gap-2 text-sm">
                  <Checkbox
                    id="twofa-backfill-legacy-acknowledgement"
                    checked={acknowledgeLegacyAccess}
                    disabled={running}
                    onCheckedChange={(checked) => {
                      setAcknowledgeLegacyAccess(checked === true);
                    }}
                  />
                  <Label
                    htmlFor="twofa-backfill-legacy-acknowledgement"
                    className="font-normal"
                  >
                    {t(
                      "registration.twoFactorBackfill.acknowledgeLegacyAccess",
                    )}
                  </Label>
                </div>
              ) : null}
            </section>

            {progress.length > 0 || error ? (
              <section className="space-y-3 px-5 py-4">
                <div className="flex items-center justify-between gap-3">
                  <Label>{t("registration.twoFactorBackfill.progress")}</Label>
                  <div className="flex flex-wrap gap-2 text-xs tabular-nums text-muted-foreground">
                    <span>
                      {t("registration.twoFactorBackfill.summaryEnabled", {
                        count: enabledCount,
                      })}
                    </span>
                    <span>
                      {t("registration.twoFactorBackfill.summaryFailed", {
                        count: failedCount,
                      })}
                    </span>
                    <span>
                      {t("registration.twoFactorBackfill.summaryReview", {
                        count: reviewCount,
                      })}
                    </span>
                    <span>
                      {t("registration.twoFactorBackfill.summaryCancelled", {
                        count: cancelledCount,
                      })}
                    </span>
                  </div>
                </div>

                {error ? (
                  <div className="flex gap-2 rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">
                    <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
                    <span>
                      {t(
                        automationErrorTranslationKey(
                          error,
                          "automationProfile.errors.operationFailed",
                        ),
                      )}
                    </span>
                  </div>
                ) : null}

                <div className="divide-y rounded-md border">
                  {progress.map((item) => (
                    <div
                      key={item.accountKey || item.taskId}
                      className="flex flex-wrap items-center gap-2 px-3 py-2.5"
                    >
                      <span className="min-w-0 flex-1 truncate text-sm font-medium">
                        {item.accountKey
                          ? emailByKey.get(item.accountKey) ||
                            t("registration.twoFactorBackfill.unknownAccount")
                          : t("registration.twoFactorBackfill.task")}
                      </span>
                      <span className="text-xs text-muted-foreground">
                        {t(STEP_KEYS[item.step])}
                      </span>
                      {item.outcome ? (
                        <Badge variant="outline" className="font-normal">
                          {t(OUTCOME_KEYS[item.outcome])}
                        </Badge>
                      ) : null}
                      {item.errorCode ? (
                        <span className="text-xs text-destructive">
                          {t("registration.twoFactorBackfill.errorCode", {
                            code: item.errorCode,
                          })}
                        </span>
                      ) : null}
                      {item.retryable ? (
                        <Badge variant="secondary" className="font-normal">
                          {t("registration.twoFactorBackfill.retryable")}
                        </Badge>
                      ) : null}
                    </div>
                  ))}
                </div>
              </section>
            ) : null}
          </div>
        </ScrollArea>

        <DialogFooter className="border-t px-5 py-4 sm:items-center">
          {terminal ? (
            <span className="mr-auto text-xs text-muted-foreground">
              {t("registration.twoFactorBackfill.terminal")}
            </span>
          ) : null}
          {running ? (
            starting ? (
              <Button disabled>
                <LoaderCircle className="mr-1.5 h-4 w-4 animate-spin" />
                {t("registration.starting")}
              </Button>
            ) : (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="destructive"
                    onClick={handleCancel}
                    disabled={!cancellable || cancelling}
                  >
                    {cancelling ? (
                      <LoaderCircle className="mr-1.5 h-4 w-4 animate-spin" />
                    ) : (
                      <CircleStop className="mr-1.5 h-4 w-4" />
                    )}
                    {t("common.buttons.cancel")}
                  </Button>
                </TooltipTrigger>
                <TooltipContent>
                  {t("registration.twoFactorBackfill.cancelTooltip")}
                </TooltipContent>
              </Tooltip>
            )
          ) : (
            <>
              <Button variant="outline" onClick={() => onOpenChange(false)}>
                {t("common.buttons.close")}
              </Button>
              <Button onClick={handleStart} disabled={!startReady}>
                <ShieldCheck className="mr-1.5 h-4 w-4" />
                {t("registration.twoFactorBackfill.start")}
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
