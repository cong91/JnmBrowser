import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";

export interface LoginProgress {
  taskId: string;
  credentialIndex: number;
  totalCredentials: number;
  step: string;
  message: string;
  timestamp: string;
  result?: LoginResult;
}

export interface LoginResult {
  success: boolean;
  email: string;
  accountId: string;
  accessToken: string;
  refreshToken: string;
  sub2apiAccountId?: number;
  errorMessage: string;
  stepLogs: string[];
  createdAt: string;
  phoneNumber: string;
  status: LoginResultStatus;
  note: string;
  exportedAt?: string;
  pushError?: string;
}

export type LoginResultStatus = "available" | "exported" | "used" | "invalid";

/** Matches Rust `LoginNetworkMode` (camelCase unit enum). */
export type LoginNetworkMode = "none" | "proxy" | "nord";

export interface LoginConfig {
  credentialsText: string;
  credentials: Array<{ email: string; password: string; totpSecret: string }>;
  browserType: "chromium" | "camoufox";
  maxRetries: number;
  headless: boolean;
  concurrency: number;
  sub2apiUrl: string;
  sub2apiApiKey: string;
  sub2apiProxyId?: number;
  sub2apiGroupIds?: number[];
  pushToSub2api: boolean;
  smsProvider?: string;
  smsToken?: string;
  smsServiceId?: number;
  smsNetwork?: string;
  smsCountry?: string;
  proxyId?: string;
  networkMode: LoginNetworkMode;
}

export function useLoginEvents() {
  const [progressMap, setProgressMap] = useState<Map<string, LoginProgress>>(
    new Map(),
  );
  const [accounts, setAccounts] = useState<LoginResult[]>([]);
  const [loading, setLoading] = useState(false);

  const refreshAccounts = useCallback(async () => {
    try {
      const results = await invoke<LoginResult[]>("list_login_results_cmd");
      setAccounts(results);
    } catch (e) {
      console.error("Failed to refresh login results:", e);
    }
  }, []);

  useEffect(() => {
    let unlistenFn: (() => void) | undefined;
    let cancelled = false;

    void listen<LoginProgress>("login-progress", (event) => {
      const progress = event.payload;
      setProgressMap((prev) => {
        const newMap = new Map(prev);
        newMap.set(progress.taskId, progress);
        return newMap;
      });
      if (progress.result) {
        void refreshAccounts();
      }
    }).then((fn) => {
      if (cancelled) {
        fn();
      } else {
        unlistenFn = fn;
      }
    });

    void refreshAccounts();

    return () => {
      cancelled = true;
      unlistenFn?.();
    };
  }, [refreshAccounts]);

  const startLogin = useCallback(
    async (config: LoginConfig): Promise<string> => {
      setLoading(true);
      try {
        return await invoke<string>("start_auto_login", { config });
      } finally {
        setLoading(false);
      }
    },
    [],
  );

  const cancelLogin = useCallback(async (taskId: string) => {
    await invoke("cancel_login", { taskId });
  }, []);

  const deleteAccount = useCallback(
    async (accountId: string) => {
      await invoke("delete_login_result_cmd", { accountId });
      await refreshAccounts();
    },
    [refreshAccounts],
  );

  const updateAccountStatus = useCallback(
    async (accountIds: string[], status: LoginResultStatus, note?: string) => {
      await invoke("update_login_result_status_cmd", {
        accountIds,
        status,
        note,
      });
      await refreshAccounts();
    },
    [refreshAccounts],
  );

  const updateAccountNote = useCallback(
    async (accountId: string, note: string) => {
      await invoke("update_login_result_note_cmd", { accountId, note });
      await refreshAccounts();
    },
    [refreshAccounts],
  );

  /**
   * Export login results as Sub2API JSON.
   * Defaults: includeFailed=false, markExported=false (caller marks after successful file write).
   * Empty ids → backend exports non-exported success+token only.
   */
  const exportAccountsJson = useCallback(
    async (
      accountIds: string[] = [],
      options?: { includeFailed?: boolean; markExported?: boolean },
    ): Promise<string> => {
      const json = await invoke<string>("export_login_results_cmd", {
        accountIds,
        includeFailed: options?.includeFailed ?? false,
        markExported: options?.markExported ?? false,
      });
      // Only refresh if backend marked statuses (otherwise UI marks later).
      if (options?.markExported) {
        await refreshAccounts();
      }
      return json;
    },
    [refreshAccounts],
  );

  /** Push stored successful results to Sub2API after batch login (or selected). */
  const pushAccountsToSub2api = useCallback(
    async (
      accountIds: string[] = [],
      options?: {
        sub2apiUrl?: string;
        sub2apiApiKey?: string;
        sub2apiProxyId?: number;
        sub2apiGroupIds?: number[];
      },
    ): Promise<{ pushed: number; failed: number; errors: string[] }> => {
      const res = await invoke<{
        pushed: number;
        failed: number;
        errors: string[];
      }>("push_login_results_to_sub2api_cmd", {
        accountIds,
        sub2apiUrl: options?.sub2apiUrl,
        sub2apiApiKey: options?.sub2apiApiKey,
        sub2apiProxyId: options?.sub2apiProxyId,
        sub2apiGroupIds: options?.sub2apiGroupIds,
      });
      await refreshAccounts();
      return res;
    },
    [refreshAccounts],
  );

  return {
    progressMap,
    accounts,
    loading,
    startLogin,
    cancelLogin,
    refreshAccounts,
    deleteAccount,
    updateAccountStatus,
    updateAccountNote,
    exportAccountsJson,
    pushAccountsToSub2api,
  };
}
