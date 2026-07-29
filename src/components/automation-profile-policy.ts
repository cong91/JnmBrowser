export type AutomationDataMode = "ephemeral" | "persistent";
export type AutomationFingerprintMode = "randomPerLaunch" | "stable";

export interface AutomationProfilePolicy {
  profileId: string;
  dataMode: AutomationDataMode;
  fingerprintMode: AutomationFingerprintMode;
}

export interface AutomationProfilePolicyPayload {
  profileId?: string;
  dataMode: AutomationDataMode;
  fingerprintMode: AutomationFingerprintMode;
}

export interface AccountCheckerProfilePolicyPayload {
  sourceProfileId?: string;
  dataMode: AutomationDataMode;
  fingerprintMode: AutomationFingerprintMode;
}

export type AutomationErrorTranslationKey =
  | "automationProfile.errors.profileBusy"
  | "automationProfile.errors.invalidProfile"
  | "automationProfile.errors.cleanupFailed"
  | "automationProfile.errors.startFailed"
  | "automationProfile.errors.operationFailed";

export const DEFAULT_AUTOMATION_PROFILE_POLICY: AutomationProfilePolicy = {
  profileId: "",
  dataMode: "ephemeral",
  fingerprintMode: "randomPerLaunch",
};

function normalizedProfileId(profileId: string): string | undefined {
  return profileId.trim() || undefined;
}

export function automationProfilePolicyPayload(
  policy: AutomationProfilePolicy,
): AutomationProfilePolicyPayload {
  return {
    profileId: normalizedProfileId(policy.profileId),
    dataMode: policy.dataMode,
    fingerprintMode: policy.fingerprintMode,
  };
}

export function accountCheckerProfilePolicyPayload(
  policy: AutomationProfilePolicy,
): AccountCheckerProfilePolicyPayload {
  const { profileId, ...runtimePolicy } =
    automationProfilePolicyPayload(policy);
  return {
    sourceProfileId: profileId,
    ...runtimePolicy,
  };
}

export function registrationConcurrency(
  profileId: string,
  requestedConcurrency: number,
): number {
  if (normalizedProfileId(profileId)) return 1;
  return Math.max(1, requestedConcurrency);
}

export function automationErrorTranslationKey(
  error: unknown,
  fallback: AutomationErrorTranslationKey = "automationProfile.errors.startFailed",
): AutomationErrorTranslationKey {
  const message = String(error).toLowerCase();

  if (
    message.includes("selected profile is busy") ||
    message.includes("selected source profile is running")
  ) {
    return "automationProfile.errors.profileBusy";
  }
  if (
    message.includes("browser_cleanup_failed") ||
    message.includes("cleanup error") ||
    message.includes("cleanup failed") ||
    message.includes("cleanup context lock poisoned")
  ) {
    return "automationProfile.errors.cleanupFailed";
  }
  if (
    message.includes("selected source profile") ||
    message.includes("selected camoufox") ||
    message.includes("selected firefox") ||
    message.includes("profileid cannot be used")
  ) {
    return "automationProfile.errors.invalidProfile";
  }
  return fallback;
}
