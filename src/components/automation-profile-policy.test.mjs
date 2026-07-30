import assert from "node:assert/strict";
import test from "node:test";
import {
  accountCheckerProfilePolicyPayload,
  automationErrorTranslationKey,
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

test("automation errors map to safe translated categories", () => {
  assert.equal(
    automationErrorTranslationKey(
      "Selected profile is busy; wait for the current automation task",
    ),
    "automationProfile.errors.profileBusy",
  );
  assert.equal(
    automationErrorTranslationKey(
      "Selected source profile is running; stop it before starting",
    ),
    "automationProfile.errors.profileBusy",
  );
  assert.equal(
    automationErrorTranslationKey("Auto Login runtime cleanup failed: locked"),
    "automationProfile.errors.cleanupFailed",
  );
  assert.equal(
    automationErrorTranslationKey("browser_cleanup_failed"),
    "automationProfile.errors.cleanupFailed",
  );
  assert.equal(
    automationErrorTranslationKey("Selected source profile was not found"),
    "automationProfile.errors.invalidProfile",
  );
  assert.equal(
    automationErrorTranslationKey("password=secret@example.test"),
    "automationProfile.errors.startFailed",
  );
  assert.equal(
    automationErrorTranslationKey(
      "password=secret@example.test",
      "automationProfile.errors.operationFailed",
    ),
    "automationProfile.errors.operationFailed",
  );
});
