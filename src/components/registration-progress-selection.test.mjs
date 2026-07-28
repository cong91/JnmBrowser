import assert from "node:assert/strict";
import test from "node:test";
import {
  isTerminalRegistrationProgress,
  registrationProgressKey,
  registrationProgressLiveRegion,
  selectRegistrationProgressList,
  upsertRegistrationProgress,
} from "./registration-progress-selection.ts";

function progress(overrides = {}) {
  return {
    taskId: "task-1",
    cdkIndex: 0,
    aliasIndex: 0,
    totalCdks: 2,
    step: "launchingBrowser",
    message: "[CDK 1/2 Alias 1/1] Launching browser...",
    timestamp: "2026-07-24T00:00:00Z",
    eventKind: "account",
    ...overrides,
  };
}

test("registration live region escalates only terminal failures", () => {
  assert.deepEqual(registrationProgressLiveRegion(progress()), {
    role: "status",
    ariaLive: "polite",
  });
  assert.deepEqual(
    registrationProgressLiveRegion(
      progress({ terminal: { success: false, statusCode: "failed" } }),
    ),
    { role: "alert", ariaLive: "assertive" },
  );
});

test("registration progress preserves a distinct latest block per CDK", () => {
  let progressMap = new Map();
  progressMap = upsertRegistrationProgress(progressMap, progress());
  progressMap = upsertRegistrationProgress(
    progressMap,
    progress({
      cdkIndex: 1,
      message: "[CDK 2/2 Alias 1/1] Enabling 2FA...",
    }),
  );
  progressMap = upsertRegistrationProgress(
    progressMap,
    progress({ step: "creatingAccount", message: "[CDK 1/2] Creating..." }),
  );

  assert.equal(progressMap.size, 2);
  assert.deepEqual(
    selectRegistrationProgressList(progressMap).map((entry) => entry.message),
    ["[CDK 1/2] Creating...", "[CDK 2/2 Alias 1/1] Enabling 2FA..."],
  );
});

test("failed batch summary stays terminal and hidden from CDK cards", () => {
  const summary = progress({
    step: "failed",
    message: "failed",
    eventKind: "batch",
    terminal: { success: false, statusCode: "failed" },
  });

  assert.equal(registrationProgressKey(summary), "task-1:summary");
  assert.equal(isTerminalRegistrationProgress(summary), true);
  assert.deepEqual(
    selectRegistrationProgressList(
      upsertRegistrationProgress(new Map(), summary),
    ),
    [],
  );
});
