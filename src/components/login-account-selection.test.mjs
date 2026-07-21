import assert from "node:assert/strict";
import test from "node:test";
import {
  isAccountReadyForExport,
  toggleAccountSelection,
} from "./login-account-selection.ts";

function account(overrides = {}) {
  return {
    success: true,
    email: "ready@example.com",
    accountId: "account-1",
    accessToken: "token",
    refreshToken: "refresh-token",
    errorMessage: "",
    stepLogs: [],
    createdAt: "2026-07-20T00:00:00Z",
    phoneNumber: "",
    status: "available",
    note: "",
    ...overrides,
  };
}

test("ready-for-export excludes exported and errored accounts", () => {
  assert.equal(isAccountReadyForExport(account()), true);
  assert.equal(isAccountReadyForExport(account({ status: "used" })), true);
  assert.equal(isAccountReadyForExport(account({ status: "exported" })), false);
  assert.equal(
    isAccountReadyForExport(account({ exportedAt: "2026-07-20T01:00:00Z" })),
    false,
  );
  assert.equal(isAccountReadyForExport(account({ status: "invalid" })), false);
  assert.equal(
    isAccountReadyForExport(account({ pushError: "Push failed" })),
    false,
  );
  assert.equal(
    isAccountReadyForExport(account({ sub2apiAccountId: 123 })),
    false,
  );
  assert.equal(isAccountReadyForExport(account({ success: false })), false);
  assert.equal(isAccountReadyForExport(account({ accessToken: "" })), false);
});

test("manual selection accepts an errored account", () => {
  const failed = account({
    success: false,
    status: "invalid",
    email: "failed@example.com",
  });

  const selected = toggleAccountSelection(new Set(), failed);

  assert.deepEqual([...selected], ["failed@example.com"]);
});
