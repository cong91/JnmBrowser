import type { RegistrationProgress } from "@/hooks/use-registration-events";

export function registrationProgressLiveRegion(progress: RegistrationProgress) {
  const failed = progress.terminal?.success === false;
  return {
    role: failed ? ("alert" as const) : ("status" as const),
    ariaLive: failed ? ("assertive" as const) : ("polite" as const),
  };
}

export function isTerminalRegistrationProgress(
  progress: RegistrationProgress,
): boolean {
  return progress.terminal != null;
}

export function isRegistrationBatchSummary(
  progress: RegistrationProgress,
): boolean {
  return progress.eventKind === "batch";
}

export function registrationProgressKey(
  progress: RegistrationProgress,
): string {
  if (isRegistrationBatchSummary(progress)) {
    return `${progress.taskId}:summary`;
  }

  return `${progress.taskId}:cdk:${progress.cdkIndex}`;
}

export function upsertRegistrationProgress(
  progressMap: Map<string, RegistrationProgress>,
  progress: RegistrationProgress,
): Map<string, RegistrationProgress> {
  const next = new Map(progressMap);
  next.set(registrationProgressKey(progress), progress);
  return next;
}

export function selectRegistrationProgressList(
  progressMap: Map<string, RegistrationProgress>,
): RegistrationProgress[] {
  return Array.from(progressMap.values())
    .filter((progress) => !isRegistrationBatchSummary(progress))
    .sort((a, b) => {
      if (a.cdkIndex !== b.cdkIndex) return a.cdkIndex - b.cdkIndex;
      return a.aliasIndex - b.aliasIndex;
    });
}
