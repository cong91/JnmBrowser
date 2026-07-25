import type { RegistrationProgress } from "@/hooks/use-registration-events";

export function isTerminalRegistrationProgress(
  progress: RegistrationProgress,
): boolean {
  return (
    progress.result != null ||
    progress.step === "completed" ||
    progress.step === "failed"
  );
}

export function isRegistrationBatchSummary(
  progress: RegistrationProgress,
): boolean {
  return (
    progress.result == null &&
    progress.step === "completed" &&
    progress.cdkIndex === 0 &&
    progress.aliasIndex === 0
  );
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
