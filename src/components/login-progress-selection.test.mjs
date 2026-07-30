import assert from "node:assert/strict";
import test from "node:test";
import {
  isTerminalLoginProgress,
  loginProgressKey,
  loginProgressLiveRegion,
  selectLoginProgressList,
  upsertLoginProgress,
} from "./login-progress-selection.ts";

function progress(overrides = {}) {
  return {
    taskId: "task-1",
    credentialIndex: 0,
    totalCredentials: 2,
    step: "enteringPassword",
    message: "Entering password",
    timestamp: "2026-07-26T00:00:00Z",
    eventKind: "account",
    ...overrides,
  };
}

test("login live region escalates only terminal failures", () => {
  assert.deepEqual(loginProgressLiveRegion(progress()), {
    role: "status",
    ariaLive: "polite",
  });
  assert.deepEqual(
    loginProgressLiveRegion(
      progress({ terminal: { success: false, statusCode: "failed" } }),
    ),
    { role: "alert", ariaLive: "assertive" },
  );
});

test("safe terminal summary does not need a credential result", () => {
  const terminal = progress({
    step: "completed",
    message: "completed",
    terminal: { success: true, statusCode: "completed" },
  });

  assert.equal(isTerminalLoginProgress(terminal), true);
  assert.equal("result" in terminal, false);
});

test("batch summary does not overwrite account progress", () => {
  const account = progress({
    terminal: { success: true, statusCode: "completed" },
  });
  const batch = progress({
    eventKind: "batch",
    terminal: { success: true, statusCode: "completed" },
  });

  let progressMap = upsertLoginProgress(new Map(), account);
  progressMap = upsertLoginProgress(progressMap, batch);

  assert.equal(progressMap.size, 2);
  assert.equal(loginProgressKey(batch), "task-1:summary");
  assert.deepEqual(selectLoginProgressList(progressMap), [account]);
});
