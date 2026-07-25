import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  type BackfillProgressState,
  isTerminalBackfillEvent,
  selectBackfillTaskProgress,
} from "@/components/two-factor-backfill-selection";

export type BackfillBrowser = "chromium" | "camoufox";
export type BackfillMode = "canary" | "bulk";
export type BackfillNetworkConfig =
  | { kind: "none" }
  | { kind: "proxy"; proxyId: string }
  | { kind: "vpn"; vpnId: string };

export type BackfillEmailProvider = "gmail.123452026.xyz" | "sms.iosmq.xyz";
export type EmailProviderProvenance =
  | "registration_config"
  | "inferred_from_cdk";
export type AccountInventoryStatus =
  | "available"
  | "exported"
  | "sold"
  | "invalid"
  | "reserved";
export type RegistrationOutcomeReason =
  | "registered"
  | "free_trial_no"
  | "registration_failed"
  | "batch_summary";
export type TwoFactorBackfillExclusion = "operator_excluded" | "manual_review";

export type TwoFactorBackfillIneligibilityReason =
  | "account_not_found"
  | "missing_account_key"
  | "registration_unsuccessful"
  | "missing_email"
  | "missing_password"
  | "free_trial_no_override_required"
  | "two_factor_already_enabled"
  | "inconsistent_local_two_factor_state"
  | "missing_cdk"
  | "unknown_cdk_prefix"
  | "missing_email_provider_provenance"
  | "legacy_access_acknowledgement_required"
  | "access_locked"
  | "backfill_in_progress"
  | "backfill_completed"
  | "backfill_reconciliation_requires_manual_review"
  | "backfill_completed_outcome_missing"
  | { inventory_status_not_available: AccountInventoryStatus }
  | { invalid_outcome_not_eligible: RegistrationOutcomeReason | null }
  | { explicitly_excluded: TwoFactorBackfillExclusion };

export interface TwoFactorBackfillPreviewRequest {
  selectedAccountKeys: string[];
  allowFreeTrialNo: boolean;
  acknowledgeLegacyAccess: boolean;
  browser: BackfillBrowser;
  network: BackfillNetworkConfig;
}

export interface TwoFactorBackfillAccountPreview {
  accountKey: string;
  accountId: string;
  email: string;
  eligible: boolean;
  ineligibilityReasons: TwoFactorBackfillIneligibilityReason[];
  emailProvider: BackfillEmailProvider | null;
  emailProviderProvenance: EmailProviderProvenance | null;
  requiresProviderPersistence: boolean;
  recordRevision: number;
}

export interface TwoFactorBackfillPreview {
  accounts: TwoFactorBackfillAccountPreview[];
  bulkAvailable: boolean;
}

export type TwoFactorBackfillRecoveryState =
  | "secret_captured"
  | "remote_confirmed"
  | "manual_review";

export interface TwoFactorBackfillRecoverySummary {
  operationId: string;
  accountKey: string;
  state: TwoFactorBackfillRecoveryState;
  expectedAccountRevision: number;
  finalAccountRevision: number | null;
  journalRevision: number;
  createdAt: string;
  updatedAt: string;
}

export interface TwoFactorBackfillRecoveryResult {
  summary: TwoFactorBackfillRecoverySummary;
  recovered: boolean;
  requiresManualReview: boolean;
}

export interface TwoFactorBackfillStartRequest
  extends TwoFactorBackfillPreviewRequest {
  browser: BackfillBrowser;
  network: BackfillNetworkConfig;
  mode: BackfillMode;
}

export type BackfillStep =
  | "eligibility"
  | "providerMigration"
  | "login"
  | "emailOtp"
  | "inspectTwoFactor"
  | "captureSecret"
  | "confirmTwoFactor"
  | "verifyRemote"
  | "persistAccount"
  | "journalComplete"
  | "batchPaused"
  | "cancelled"
  | "completed"
  | "failed";

export type BackfillOutcome =
  | "enabled"
  | "failed"
  | "cancelled"
  | "reconciliationRequired"
  | "batchPaused";

export interface TwoFactorBackfillProgress {
  taskId: string;
  accountKey: string;
  accountIndex: number;
  totalAccounts: number;
  step: BackfillStep;
  outcome?: BackfillOutcome;
  errorCode?: string;
  retryable: boolean;
  timestamp: string;
}

function isTerminalEvent(progress: TwoFactorBackfillProgress): boolean {
  return isTerminalBackfillEvent(progress as BackfillProgressState);
}

function progressKey(progress: TwoFactorBackfillProgress): string {
  return progress.accountKey || `task:${progress.taskId}`;
}

export function useTwoFactorBackfillEvents(
  onTerminal?: () => Promise<void> | void,
) {
  const [progressMap, setProgressMap] = useState<
    Map<string, TwoFactorBackfillProgress>
  >(new Map());
  const [taskId, setTaskId] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const [terminal, setTerminal] = useState<TwoFactorBackfillProgress | null>(
    null,
  );
  const [error, setError] = useState<string | null>(null);

  const disposedRef = useRef(false);
  const unlistenRef = useRef<UnlistenFn | null>(null);
  const listenerPromiseRef = useRef<Promise<void> | null>(null);
  const listenerGenerationRef = useRef(0);
  const activeTaskIdRef = useRef<string | null>(null);
  const pendingProgressRef = useRef<TwoFactorBackfillProgress[]>([]);
  const acceptingEventsRef = useRef(false);
  const terminalTaskIdsRef = useRef(new Set<string>());
  const onTerminalRef = useRef(onTerminal);

  useEffect(() => {
    onTerminalRef.current = onTerminal;
  }, [onTerminal]);

  const handleProgress = useCallback((progress: TwoFactorBackfillProgress) => {
    if (!acceptingEventsRef.current) return;
    const activeTaskId = activeTaskIdRef.current;
    if (!activeTaskId) {
      pendingProgressRef.current.push(progress);
      return;
    }
    if (progress.taskId !== activeTaskId) return;

    setProgressMap((previous) => {
      const next = new Map(previous);
      next.set(progressKey(progress), progress);
      return next;
    });

    if (
      isTerminalEvent(progress) &&
      !terminalTaskIdsRef.current.has(progress.taskId)
    ) {
      terminalTaskIdsRef.current.add(progress.taskId);
      acceptingEventsRef.current = false;
      setTerminal(progress);
      setRunning(false);
      Promise.resolve(onTerminalRef.current?.()).catch(() => {});
    }
  }, []);

  const ensureListener = useCallback(async () => {
    if (!listenerPromiseRef.current) {
      const generation = ++listenerGenerationRef.current;
      listenerPromiseRef.current = listen<TwoFactorBackfillProgress>(
        "twofa-backfill-progress",
        (event) => handleProgress(event.payload),
      )
        .then((unlisten) => {
          if (
            disposedRef.current ||
            generation !== listenerGenerationRef.current
          ) {
            unlisten();
          } else {
            unlistenRef.current = unlisten;
          }
        })
        .catch((listenerError) => {
          if (generation === listenerGenerationRef.current) {
            listenerPromiseRef.current = null;
          }
          throw listenerError;
        });
    }
    await listenerPromiseRef.current;
  }, [handleProgress]);

  useEffect(() => {
    disposedRef.current = false;
    void ensureListener().catch((listenerError) => {
      setError(String(listenerError));
    });

    return () => {
      disposedRef.current = true;
      acceptingEventsRef.current = false;
      listenerGenerationRef.current += 1;
      unlistenRef.current?.();
      unlistenRef.current = null;
      listenerPromiseRef.current = null;
    };
  }, [ensureListener]);

  const preview = useCallback(
    async (
      request: TwoFactorBackfillPreviewRequest,
    ): Promise<TwoFactorBackfillPreview> => {
      setError(null);
      try {
        return await invoke<TwoFactorBackfillPreview>(
          "preview_two_factor_backfill",
          { request },
        );
      } catch (previewError) {
        const message = String(previewError);
        setError(message);
        throw previewError;
      }
    },
    [],
  );

  const listRecovery = useCallback(
    () =>
      invoke<TwoFactorBackfillRecoverySummary[]>(
        "list_two_factor_backfill_recovery",
      ),
    [],
  );

  const recoverJournal = useCallback(
    (operationId: string, accountKey: string) =>
      invoke<TwoFactorBackfillRecoveryResult>(
        "recover_two_factor_backfill_journal",
        { operationId, accountKey },
      ),
    [],
  );

  const start = useCallback(
    async (request: TwoFactorBackfillStartRequest): Promise<string> => {
      setError(null);
      setTerminal(null);
      setProgressMap(new Map());
      setTaskId(null);
      setRunning(true);
      activeTaskIdRef.current = null;
      pendingProgressRef.current = [];
      try {
        await ensureListener();
        acceptingEventsRef.current = true;
        const nextTaskId = await invoke<string>("start_auto_registration", {
          config: {
            operation: "existingAccount",
            ...request,
          },
        });
        activeTaskIdRef.current = nextTaskId;
        setTaskId(nextTaskId);
        const pendingProgress = pendingProgressRef.current;
        pendingProgressRef.current = [];
        for (const progress of selectBackfillTaskProgress(
          pendingProgress,
          nextTaskId,
        )) {
          handleProgress(progress);
        }
        return nextTaskId;
      } catch (startError) {
        acceptingEventsRef.current = false;
        activeTaskIdRef.current = null;
        setTaskId(null);
        setRunning(false);
        const message = String(startError);
        setError(message);
        throw startError;
      }
    },
    [ensureListener, handleProgress],
  );

  const cancel = useCallback(async () => {
    const activeTaskId = activeTaskIdRef.current;
    if (!activeTaskId) return;
    try {
      await invoke("cancel_two_factor_backfill", { taskId: activeTaskId });
    } catch (cancelError) {
      const message = String(cancelError);
      setError(message);
      throw cancelError;
    }
  }, []);

  const reset = useCallback(() => {
    acceptingEventsRef.current = false;
    activeTaskIdRef.current = null;
    pendingProgressRef.current = [];
    setTaskId(null);
    setProgressMap(new Map());
    setTerminal(null);
    setError(null);
  }, []);

  return {
    progressMap,
    taskId,
    running,
    starting: running && taskId === null,
    cancellable: running && taskId !== null,
    terminal,
    error,
    preview,
    listRecovery,
    recoverJournal,
    start,
    cancel,
    reset,
  };
}
