import type { RegistrationResult } from "@/hooks/use-registration-events";

type AccountIdentity = Pick<RegistrationResult, "accountId" | "email">;

type PreviewReason = string | object;

export interface BackfillPreviewAccountState {
  eligible: boolean;
  ineligibilityReasons: readonly PreviewReason[];
}

export interface BackfillPreviewState {
  eligibleCount: number;
  selectionEligible: boolean;
  canaryReady: boolean;
  bulkReady: boolean;
  requiresFreeTrialOverride: boolean;
  requiresLegacyAcknowledgement: boolean;
}

export function backfillReasonCode(reason: PreviewReason): string {
  return typeof reason === "string" ? reason : (Object.keys(reason)[0] ?? "");
}

export function summarizeBackfillPreview(
  accounts: readonly BackfillPreviewAccountState[],
  bulkAvailable = false,
): BackfillPreviewState {
  const eligibleCount = accounts.filter((account) => account.eligible).length;
  const reasonCodes = accounts.flatMap((account) =>
    account.ineligibilityReasons.map(backfillReasonCode),
  );
  const selectionEligible =
    accounts.length > 0 && accounts.every((account) => account.eligible);

  return {
    eligibleCount,
    selectionEligible,
    canaryReady: selectionEligible && eligibleCount === 1,
    bulkReady: selectionEligible && bulkAvailable,
    requiresFreeTrialOverride: reasonCodes.includes(
      "free_trial_no_override_required",
    ),
    requiresLegacyAcknowledgement: reasonCodes.includes(
      "legacy_access_acknowledgement_required",
    ),
  };
}

export interface BackfillProgressState {
  step: string;
  accountIndex: number;
  totalAccounts: number;
  retryable: boolean;
}

export function isTerminalBackfillEvent(
  progress: BackfillProgressState,
): boolean {
  if (progress.step === "cancelled" || progress.step === "batchPaused") {
    return true;
  }
  if (progress.step === "failed" && progress.totalAccounts === 0) {
    return true;
  }
  if (progress.retryable) return false;
  return (
    (progress.step === "completed" || progress.step === "failed") &&
    progress.totalAccounts > 0 &&
    progress.accountIndex >= progress.totalAccounts - 1
  );
}

export function selectBackfillTaskProgress<T extends { taskId: string }>(
  progress: readonly T[],
  taskId: string,
): T[] {
  return progress.filter((item) => item.taskId === taskId);
}

export interface FilteredSelectionState {
  checked: boolean;
  indeterminate: boolean;
  selectedCount: number;
  totalCount: number;
}

export function accountKey(account: AccountIdentity): string {
  return account.accountId.trim() || account.email.trim();
}

export function isRegistrationAccountReadyForExport(
  account: Pick<
    RegistrationResult,
    "success" | "freeTrialEligible" | "twoFaEnabled" | "totpSecret" | "status"
  >,
): boolean {
  return Boolean(
    account.success &&
      account.freeTrialEligible &&
      account.twoFaEnabled &&
      account.totpSecret?.trim() &&
      account.status === "available",
  );
}

export function toggleAccountSelection(
  selectedKeys: ReadonlySet<string>,
  account: AccountIdentity,
): Set<string> {
  const key = accountKey(account);
  const next = new Set(selectedKeys);
  if (!key) return next;
  if (next.has(key)) next.delete(key);
  else next.add(key);
  return next;
}

export function setFilteredAccountSelection(
  selectedKeys: ReadonlySet<string>,
  filteredAccounts: readonly AccountIdentity[],
  shouldSelect: boolean,
): Set<string> {
  const next = new Set(selectedKeys);
  for (const account of filteredAccounts) {
    const key = accountKey(account);
    if (!key) continue;
    if (shouldSelect) next.add(key);
    else next.delete(key);
  }
  return next;
}

export function getFilteredSelectionState(
  selectedKeys: ReadonlySet<string>,
  filteredAccounts: readonly AccountIdentity[],
): FilteredSelectionState {
  let selectedCount = 0;
  let totalCount = 0;
  for (const account of filteredAccounts) {
    const key = accountKey(account);
    if (!key) continue;
    totalCount += 1;
    if (selectedKeys.has(key)) selectedCount += 1;
  }

  return {
    checked: totalCount > 0 && selectedCount === totalCount,
    indeterminate: selectedCount > 0 && selectedCount < totalCount,
    selectedCount,
    totalCount,
  };
}

export function pruneSelectedAccountKeys(
  selectedKeys: ReadonlySet<string>,
  accounts: readonly AccountIdentity[],
): Set<string> {
  const currentKeys = new Set(accounts.map(accountKey).filter(Boolean));
  return new Set([...selectedKeys].filter((key) => currentKeys.has(key)));
}

export function deriveSelectedAccounts<T extends AccountIdentity>(
  accounts: readonly T[],
  selectedKeys: ReadonlySet<string>,
): T[] {
  return accounts.filter((account) => selectedKeys.has(accountKey(account)));
}

export function deriveWorkflowTargetKeys(
  accounts: readonly AccountIdentity[],
  selectedKeys: ReadonlySet<string>,
): string[] {
  return [
    ...new Set(
      deriveSelectedAccounts(accounts, selectedKeys)
        .map(accountKey)
        .filter(Boolean),
    ),
  ];
}
