import assert from "node:assert/strict";
import test from "node:test";
import {
  accountCheckerProfilePolicyPayload,
  automationProfilePolicyPayload,
  DEFAULT_AUTOMATION_PROFILE_POLICY,
  registrationConcurrency,
} from "./automation-profile-policy.ts";

test("automation profile policy defaults are isolated and random per launch", () => {
  assert.deepEqual(DEFAULT_AUTOMATION_PROFILE_POLICY, {
    profileId: "",
    dataMode: "ephemeral",
    fingerprintMode: "randomPerLaunch",
  });
  assert.deepEqual(
    automationProfilePolicyPayload(DEFAULT_AUTOMATION_PROFILE_POLICY),
    {
      profileId: undefined,
      dataMode: "ephemeral",
      fingerprintMode: "randomPerLaunch",
    },
  );
});

test("automation profile payload normalizes selected UUID and preserves modes", () => {
  assert.deepEqual(
    automationProfilePolicyPayload({
      profileId: " profile-uuid ",
      dataMode: "persistent",
      fingerprintMode: "stable",
    }),
    {
      profileId: "profile-uuid",
      dataMode: "persistent",
      fingerprintMode: "stable",
    },
  );
});

test("account checker maps selected profile to sourceProfileId", () => {
  assert.deepEqual(
    accountCheckerProfilePolicyPayload({
      profileId: "source-uuid",
      dataMode: "persistent",
      fingerprintMode: "stable",
    }),
    {
      sourceProfileId: "source-uuid",
      dataMode: "persistent",
      fingerprintMode: "stable",
    },
  );
});

test("selected registration profile clamps concurrency to one", () => {
  assert.equal(registrationConcurrency("profile-uuid", 8), 1);
  assert.equal(registrationConcurrency("", 8), 8);
  assert.equal(registrationConcurrency("", 0), 1);
});
