import type { LoginProgress } from "@/hooks/use-login-events";

export function loginProgressLiveRegion(progress: LoginProgress) {
  const failed = progress.terminal?.success === false;
  return {
    role: failed ? ("alert" as const) : ("status" as const),
    ariaLive: failed ? ("assertive" as const) : ("polite" as const),
  };
}

export function isTerminalLoginProgress(progress: LoginProgress): boolean {
  return progress.terminal != null;
}

export function loginProgressKey(progress: LoginProgress): string {
  return progress.eventKind === "batch"
    ? `${progress.taskId}:summary`
    : `${progress.taskId}:credential:${progress.credentialIndex}`;
}

export function upsertLoginProgress(
  progressMap: Map<string, LoginProgress>,
  progress: LoginProgress,
): Map<string, LoginProgress> {
  const next = new Map(progressMap);
  next.set(loginProgressKey(progress), progress);
  return next;
}

export function selectLoginProgressList(
  progressMap: Map<string, LoginProgress>,
): LoginProgress[] {
  return Array.from(progressMap.values())
    .filter((progress) => progress.eventKind !== "batch")
    .sort((left, right) => left.credentialIndex - right.credentialIndex);
}
