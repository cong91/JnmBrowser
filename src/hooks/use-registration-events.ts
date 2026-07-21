import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import type { EmailProvider } from "@/lib/email-providers";

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

export type NetworkMode = "none" | "proxy" | "vpn" | "nord";

export type { EmailProvider };

export interface RegistrationConfig {
  cdks: string[];
  profileId?: string;
  proxyId?: string;
  /** WireGuard VPN config id from Proxies & VPNs (preferred over Nord CLI) */
  vpnId?: string;
  browserType: string;
  maxRetries: number;
  accountsPerCdk: number;
  headless: boolean;
  concurrency: number;
  /** Nord simultaneous WG session budget (caps VPN concurrency; not CDK count) */
  nordMaxSessions?: number;
  networkMode?: NetworkMode;
  rotateEveryN?: number;
  nordGroup?: string;
  nordServerName?: string;
  nordCliPath?: string;
  /** SMS provider id, e.g. "viotp" */
  smsProvider?: string;
  /** Optional override; otherwise encrypted settings token is used */
  smsToken?: string;
  smsServiceId?: number;
  /** Pipe-separated carriers, e.g. "VIETTEL|MOBIFONE" */
  smsNetwork?: string;
  /** "vn" | "la" */
  smsCountry?: string;
  /** Email OTP provider domain id: gmail.123452026.xyz (default) or sms.iosmq.xyz */
  emailProvider?: EmailProvider;
}

export type AccountInventoryStatus =
  | "available"
  | "exported"
  | "sold"
  | "invalid"
  | "reserved";

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
  totpSecret?: string;
  freeTrialEligible?: boolean;
  planType?: string;
  cdk: string;
  baseEmail: string;
  phoneNumber?: string;
  status?: AccountInventoryStatus;
  note?: string;
  exportedAt?: string | null;
  soldAt?: string | null;
}

export interface CdkAccountEntry {
  email: string;
  accountId?: string;
  success: boolean;
  freeTrialEligible: boolean;
  planType?: string;
  errorMessage?: string;
  createdAt: string;
}

export interface CdkInventoryRecord {
  cdk: string;
  baseEmail: string;
  targetAccounts: number;
  attempted: number;
  freeTrialYes: number;
  freeTrialNo: number;
  failed: number;
  status: string;
  lastError: string;
  accounts: CdkAccountEntry[];
  createdAt: string;
  updatedAt: string;
  taskId: string;
  /** Ledger-backed free slots (0–6). Derived on list. */
  remaining?: number;
}

export function useRegistrationEvents() {
  const [progressMap, setProgressMap] = useState<
    Map<string, RegistrationProgress>
  >(new Map());
  const [accounts, setAccounts] = useState<RegistrationResult[]>([]);
  const [cdkInventory, setCdkInventory] = useState<CdkInventoryRecord[]>([]);
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
          // Refresh inventories when a registration emits a result.
          if (event.payload.result) {
            void invoke<RegistrationResult[]>("list_registered_accounts_cmd")
              .then(setAccounts)
              .catch(() => {});
            void invoke<CdkInventoryRecord[]>("list_cdk_inventory_cmd")
              .then(setCdkInventory)
              .catch(() => {});
          }
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

  const refreshCdkInventory = useCallback(async () => {
    try {
      const list = await invoke<CdkInventoryRecord[]>("list_cdk_inventory_cmd");
      setCdkInventory(list);
    } catch {
      // Silently fail — inventory may not be available yet
    }
  }, []);

  const deleteAccount = useCallback(async (accountId: string) => {
    await invoke("delete_registered_account_cmd", { accountId });
  }, []);

  const deleteCdkRecord = useCallback(async (cdk: string) => {
    await invoke("delete_cdk_inventory_cmd", { cdk });
  }, []);

  const updateAccountStatus = useCallback(
    async (
      accountIds: string[],
      status: AccountInventoryStatus,
      note?: string,
    ) => {
      await invoke("update_registered_account_status_cmd", {
        accountIds,
        status,
        note: note ?? null,
      });
      await refreshAccounts();
    },
    [refreshAccounts],
  );

  const updateAccountNote = useCallback(
    async (accountId: string, note: string) => {
      await invoke("update_registered_account_note_cmd", { accountId, note });
      await refreshAccounts();
    },
    [refreshAccounts],
  );

  useEffect(() => {
    void refreshAccounts();
    void refreshCdkInventory();
  }, [refreshAccounts, refreshCdkInventory]);

  return {
    progressMap,
    accounts,
    cdkInventory,
    loading,
    startRegistration,
    cancelRegistration,
    refreshAccounts,
    refreshCdkInventory,
    deleteAccount,
    deleteCdkRecord,
    updateAccountStatus,
    updateAccountNote,
  };
}
