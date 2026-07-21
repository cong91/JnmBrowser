import type { LoginResult } from "@/hooks/use-login-events";

export function accountKey(account: LoginResult): string {
  return account.email || account.accountId;
}

export function isAccountReadyForExport(account: LoginResult): boolean {
  return Boolean(
    account.success &&
      account.accessToken &&
      account.status !== "invalid" &&
      account.status !== "exported" &&
      !account.exportedAt &&
      !account.pushError &&
      !account.sub2apiAccountId,
  );
}

export function toggleAccountSelection(
  selected: ReadonlySet<string>,
  account: LoginResult,
): Set<string> {
  const next = new Set(selected);
  const id = accountKey(account);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  return next;
}
