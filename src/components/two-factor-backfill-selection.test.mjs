import assert from "node:assert/strict";
import test from "node:test";
import {
  accountKey,
  deriveSelectedAccounts,
  deriveWorkflowTargetKeys,
  getFilteredSelectionState,
  isTerminalBackfillEvent,
  pruneSelectedAccountKeys,
  selectBackfillTaskProgress,
  setFilteredAccountSelection,
  summarizeBackfillPreview,
  toggleAccountSelection,
} from "./two-factor-backfill-selection.ts";

function account(overrides = {}) {
  return {
    success: true,
    email: "one@example.com",
    password: "password",
    accountId: "account-1",
    accessToken: "token",
    deviceId: "device-1",
    errorMessage: "",
    stepLogs: [],
    createdAt: "2026-07-23T00:00:00Z",
    twoFaEnabled: false,
    cdk: "GMAIL-card",
    baseEmail: "base@example.com",
    status: "available",
    ...overrides,
  };
}

test("preview summary gates canary and bulk and surfaces override reasons", () => {
  assert.deepEqual(
    summarizeBackfillPreview([
      {
        eligible: true,
        ineligibilityReasons: [],
      },
    ]),
    {
      eligibleCount: 1,
      selectionEligible: true,
      canaryReady: true,
      bulkReady: false,
      requiresFreeTrialOverride: false,
      requiresLegacyAcknowledgement: false,
    },
  );

  assert.deepEqual(
    summarizeBackfillPreview([
      {
        eligible: false,
        ineligibilityReasons: ["free_trial_no_override_required"],
      },
      {
        eligible: true,
        ineligibilityReasons: [
          { legacy_access_acknowledgement_required: null },
        ],
      },
    ]),
    {
      eligibleCount: 1,
      selectionEligible: false,
      canaryReady: false,
      bulkReady: false,
      requiresFreeTrialOverride: true,
      requiresLegacyAcknowledgement: true,
    },
  );

  assert.equal(
    summarizeBackfillPreview(
      [
        {
          eligible: true,
          ineligibilityReasons: [],
        },
      ],
      true,
    ).bulkReady,
    true,
  );
});

test("startup progress is filtered to the authoritative returned task", () => {
  const progress = [
    { taskId: "stale-task", step: "completed" },
    { taskId: "new-task", step: "login" },
  ];

  assert.deepEqual(selectBackfillTaskProgress(progress, "new-task"), [
    progress[1],
  ]);
});

test("terminal progress classification distinguishes final and intermediate events", () => {
  assert.equal(
    isTerminalBackfillEvent({
      step: "completed",
      accountIndex: 0,
      totalAccounts: 2,
      retryable: false,
    }),
    false,
  );
  assert.equal(
    isTerminalBackfillEvent({
      step: "completed",
      accountIndex: 1,
      totalAccounts: 2,
      retryable: false,
    }),
    true,
  );
  assert.equal(
    isTerminalBackfillEvent({
      step: "failed",
      accountIndex: 0,
      totalAccounts: 1,
      retryable: true,
    }),
    false,
  );
  assert.equal(
    isTerminalBackfillEvent({
      step: "cancelled",
      accountIndex: 0,
      totalAccounts: 2,
      retryable: false,
    }),
    true,
  );
});

test("accountKey trims account ID and falls back to trimmed legacy email", () => {
  assert.equal(
    accountKey(
      account({ accountId: "  account-1  ", email: "ignored@example.com" }),
    ),
    "account-1",
  );
  assert.equal(
    accountKey(account({ accountId: "   ", email: "  legacy@example.com  " })),
    "legacy@example.com",
  );
});

test("selection persists across filters and header changes only filtered rows", () => {
  const first = account();
  const second = account({ accountId: "account-2", email: "two@example.com" });
  const hidden = account({
    accountId: "account-3",
    email: "hidden@example.com",
  });
  let selected = new Set([accountKey(hidden)]);

  selected = setFilteredAccountSelection(selected, [first, second], true);
  assert.deepEqual([...selected], ["account-3", "account-1", "account-2"]);

  selected = setFilteredAccountSelection(selected, [first, second], false);
  assert.deepEqual([...selected], ["account-3"]);
});

test("workflow targets derive from full accounts rather than filtered rows", () => {
  const visible = account();
  const hidden = account({
    accountId: "account-2",
    email: "hidden@example.com",
  });
  const duplicate = account({
    accountId: " account-2 ",
    email: "duplicate@example.com",
  });
  const accounts = [visible, hidden, duplicate];
  const selected = new Set([accountKey(hidden)]);

  assert.deepEqual(deriveSelectedAccounts(accounts, selected), [
    hidden,
    duplicate,
  ]);
  assert.deepEqual(deriveWorkflowTargetKeys(accounts, selected), ["account-2"]);
  assert.deepEqual(deriveWorkflowTargetKeys([visible], selected), []);
});

test("stale selected keys are pruned after accounts refresh", () => {
  const selected = new Set(["account-1", "removed-account"]);
  const pruned = pruneSelectedAccountKeys(selected, [account()]);

  assert.deepEqual([...pruned], ["account-1"]);
});

test("filtered header reports indeterminate state", () => {
  const first = account();
  const second = account({ accountId: "account-2", email: "two@example.com" });

  assert.deepEqual(
    getFilteredSelectionState(new Set(["account-1"]), [first, second]),
    {
      checked: false,
      indeterminate: true,
      selectedCount: 1,
      totalCount: 2,
    },
  );
  assert.deepEqual(
    getFilteredSelectionState(new Set(["account-1", "account-2"]), [
      first,
      second,
    ]),
    {
      checked: true,
      indeterminate: false,
      selectedCount: 2,
      totalCount: 2,
    },
  );
});

test("row toggle uses the canonical key for legacy accounts", () => {
  const legacy = account({ accountId: "  ", email: " legacy@example.com " });
  const selected = toggleAccountSelection(new Set(), legacy);

  assert.deepEqual([...selected], ["legacy@example.com"]);
  assert.equal(toggleAccountSelection(selected, legacy).size, 0);
});
