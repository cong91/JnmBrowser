import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useCallback, useEffect, useRef, useState } from "react";
import type {
  AutomationDataMode,
  AutomationFingerprintMode,
} from "@/components/automation-profile-policy";
import {
  isTerminalRegistrationProgress,
  upsertRegistrationProgress,
} from "@/components/registration-progress-selection";
import type { EmailProvider } from "@/lib/email-providers";

export interface RegistrationProgress {
  taskId: string;
  cdkIndex: number;
  aliasIndex: number;
  totalCdks: number;
  step: string;
  message: string;
  timestamp: string;
  eventKind: "account" | "batch";
  terminal?: {
    success: boolean;
    statusCode: string;
  } | null;
}

export type NetworkMode = "none" | "proxy" | "vpn" | "nord";

export type { EmailProvider };

export interface RegistrationConfig {
  cdks: string[];
  profileId?: string;
  dataMode: AutomationDataMode;
  fingerprintMode: AutomationFingerprintMode;
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
  const listenerPromiseRef = useRef<Promise<void> | null>(null);
  const listenerGenerationRef = useRef(0);
  const unlistenRef = useRef<UnlistenFn | null>(null);
  const disposedRef = useRef(false);

  const ensureListener = useCallback(async () => {
    if (!listenerPromiseRef.current) {
      const generation = ++listenerGenerationRef.current;
      listenerPromiseRef.current = listen<RegistrationProgress>(
        "registration-progress",
        (event) => {
          const progress = event.payload;
          setProgressMap((prev) => upsertRegistrationProgress(prev, progress));
          if (isTerminalRegistrationProgress(progress)) {
            void invoke<RegistrationResult[]>("list_registered_accounts_cmd")
              .then(setAccounts)
              .catch(() => {});
            void invoke<CdkInventoryRecord[]>("list_cdk_inventory_cmd")
              .then(setCdkInventory)
              .catch(() => {});
          }
        },
      )
        .then((unlisten) => {
          if (
            disposedRef.current ||
            generation !== listenerGenerationRef.current
          ) {
            unlisten();
          } else {
            unlistenRef.current = unlisten;
          }
        })
        .catch((error) => {
          if (generation === listenerGenerationRef.current) {
            listenerPromiseRef.current = null;
          }
          throw error;
        });
    }
    await listenerPromiseRef.current;
  }, []);

  useEffect(() => {
    disposedRef.current = false;
    void ensureListener().catch(() => {});

    return () => {
      disposedRef.current = true;
      listenerGenerationRef.current += 1;
      unlistenRef.current?.();
      unlistenRef.current = null;
      listenerPromiseRef.current = null;
    };
  }, [ensureListener]);

  const startRegistration = useCallback(
    async (config: RegistrationConfig): Promise<string> => {
      setLoading(true);
      try {
        await ensureListener();
        const taskId = await invoke<string>("start_auto_registration", {
          config,
        });
        return taskId;
      } finally {
        setLoading(false);
      }
    },
    [ensureListener],
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
