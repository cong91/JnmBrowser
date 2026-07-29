import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useCallback, useEffect, useRef, useState } from "react";

export interface CheckProgress {
  taskId: string;
  accountKey: string;
  credentialIndex: number;
  totalCredentials: number;
  step: string;
  outcome: string | null;
  reasonCode: string | null;
  terminal: boolean;
}

export interface AccountCheckResult {
  email: string;
  password: string;
  totpSecret: string;
  outcome: string;
  reasonCode: string;
  createdAt: string;
}

export interface AccountCheckConfig {
  credentialsText: string;
  sourceProfileId?: string;
  dataMode: "ephemeral" | "persistent";
  fingerprintMode: "randomPerLaunch" | "stable";
  vpnId?: string;
  browserType: string;
  headless: boolean;
}

export function useAccountCheckerEvents() {
  const [progressMap, setProgressMap] = useState<Map<string, CheckProgress>>(
    new Map(),
  );
  const [results, setResults] = useState<AccountCheckResult[]>([]);
  const [activeTaskId, setActiveTaskId] = useState<string | null>(null);
  const listenerPromiseRef = useRef<Promise<void> | null>(null);
  const unlistenRef = useRef<UnlistenFn | null>(null);

  const refreshResults = useCallback(async () => {
    try {
      const list = await invoke<AccountCheckResult[]>(
        "list_openai_account_check_results",
      );
      setResults(list);
    } catch (e) {
      console.error("Failed to refresh account check results:", e);
    }
  }, []);

  const ensureListener = useCallback(async () => {
    if (!listenerPromiseRef.current) {
      listenerPromiseRef.current = listen<CheckProgress>(
        "openai-account-check-progress",
        (event) => {
          const progress = event.payload;
          setProgressMap((prev) => {
            const next = new Map(prev);
            next.set(
              progress.accountKey || `batch-${progress.taskId}`,
              progress,
            );
            return next;
          });
          if (progress.terminal) {
            void refreshResults();
          }
        },
      )
        .then((unlisten) => {
          unlistenRef.current = unlisten;
        })
        .catch((e) => {
          console.error("Failed to set up account check listener:", e);
        });
    }
    await listenerPromiseRef.current;
  }, [refreshResults]);

  const startCheck = useCallback(
    async (config: AccountCheckConfig): Promise<string> => {
      await ensureListener();
      try {
        const taskId = await invoke<string>("start_openai_account_check", {
          config,
        });
        setActiveTaskId(taskId);
        return taskId;
      } catch (e) {
        console.error("Failed to start account check:", e);
        throw e;
      }
    },
    [ensureListener],
  );

  const cancelCheck = useCallback(async (taskId: string) => {
    try {
      await invoke("cancel_openai_account_check", { taskId });
    } catch (e) {
      console.error("Failed to cancel account check:", e);
    }
  }, []);

  const exportPassed = useCallback(async (): Promise<string> => {
    try {
      return await invoke<string>("export_passed_accounts");
    } catch (e) {
      console.error("Failed to export passed accounts:", e);
      return "";
    }
  }, []);

  const exportDeactivated = useCallback(async (): Promise<string> => {
    try {
      return await invoke<string>("export_deactivated_accounts");
    } catch (e) {
      console.error("Failed to export deactivated accounts:", e);
      return "";
    }
  }, []);

  const deleteResult = useCallback(
    async (email: string) => {
      try {
        await invoke("delete_openai_account_check_result", { email });
        await refreshResults();
      } catch (e) {
        console.error("Failed to delete account check result:", e);
      }
    },
    [refreshResults],
  );

  useEffect(() => {
    void refreshResults();
    return () => {
      if (unlistenRef.current) {
        unlistenRef.current();
        unlistenRef.current = null;
        listenerPromiseRef.current = null;
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [refreshResults]);

  const passed = results.filter((r) => r.outcome === "Passed");
  const deactivated = results.filter((r) => r.outcome === "Deactivated");
  const unresolved = results.filter((r) => r.outcome === "Unresolved");

  return {
    progressMap,
    results,
    passed,
    deactivated,
    unresolved,
    activeTaskId,
    startCheck,
    cancelCheck,
    exportPassed,
    exportDeactivated,
    deleteResult,
    refreshResults,
  };
}
