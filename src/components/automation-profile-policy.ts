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
