import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";

export interface RegistrationProgress {
  taskId: string;
  cdkIndex: number;
  aliasIndex: number;
  totalCdks: number;
  step: string;
  message: string;
  timestamp: string;
  result?: RegistrationResult | null;
}

export interface RegistrationConfig {
  cdks: string[];
  profileId?: string;
  proxyId?: string;
  browserType: string;
  maxRetries: number;
  accountsPerCdk: number;
  headless: boolean;
  concurrency: number;
}

export interface RegistrationResult {
  success: boolean;
  email: string;
  password: string;
  accountId: string;
  accessToken: string;
  deviceId: string;
  errorMessage: string;
  stepLogs: string[];
  createdAt: string;
  twoFaEnabled: boolean;
  cdk: string;
  baseEmail: string;
}

export function useRegistrationEvents() {
  const [progressMap, setProgressMap] = useState<
    Map<string, RegistrationProgress>
  >(new Map());
  const [accounts, setAccounts] = useState<RegistrationResult[]>([]);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;

    const setup = async () => {
      unlisten = await listen<RegistrationProgress>(
        "registration-progress",
        (event) => {
          setProgressMap((prev) => {
            const next = new Map(prev);
            next.set(event.payload.taskId, event.payload);
            return next;
          });
        },
      );
    };

    setup();

    return () => {
      unlisten?.();
    };
  }, []);

  const startRegistration = useCallback(
    async (config: RegistrationConfig): Promise<string> => {
      setLoading(true);
      try {
        const taskId = await invoke<string>("start_auto_registration", {
          config,
        });
        return taskId;
      } finally {
        setLoading(false);
      }
    },
    [],
  );

  const cancelRegistration = useCallback(async (taskId: string) => {
    await invoke("cancel_registration", { taskId });
  }, []);

  const refreshAccounts = useCallback(async () => {
    try {
      const list = await invoke<RegistrationResult[]>(
        "list_registered_accounts_cmd",
      );
      setAccounts(list);
    } catch {
      // Silently fail — accounts may not be available yet
    }
  }, []);

  const deleteAccount = useCallback(async (accountId: string) => {
    await invoke("delete_registered_account_cmd", { accountId });
  }, []);

  useEffect(() => {
    refreshAccounts();
  }, [refreshAccounts]);

  return {
    progressMap,
    accounts,
    loading,
    startRegistration,
    cancelRegistration,
    refreshAccounts,
    deleteAccount,
  };
}
