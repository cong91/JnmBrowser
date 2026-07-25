import assert from "node:assert/strict";
import test from "node:test";
import {
  isTerminalRegistrationProgress,
  registrationProgressKey,
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
    ...overrides,
  };
}

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

test("batch summary has a separate key, is terminal, and is not a CDK block", () => {
  let progressMap = new Map();
  progressMap = upsertRegistrationProgress(progressMap, progress());
  const summary = progress({
    step: "completed",
    message: "Done",
    result: null,
  });
  progressMap = upsertRegistrationProgress(progressMap, summary);

  assert.equal(registrationProgressKey(summary), "task-1:summary");
  assert.equal(isTerminalRegistrationProgress(summary), true);
  assert.deepEqual(selectRegistrationProgressList(progressMap), [progress()]);
});
