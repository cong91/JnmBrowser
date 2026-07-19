"use client";

import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuCopy, LuRefreshCw, LuSmartphone } from "react-icons/lu";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import type {
  SmsHistoryEntry,
  SmsNetwork,
  SmsNumberInfo,
  SmsOtpInfo,
  SmsServiceInfo,
} from "@/types";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

const POLL_TIMEOUT_SECS = 120;

export function SmsVerificationDialog({ open, onOpenChange }: Props) {
  const { t } = useTranslation();
  const [token, setToken] = useState("");
  const [tokenLoaded, setTokenLoaded] = useState(false);
  const [savingToken, setSavingToken] = useState(false);
  const [balance, setBalance] = useState<number | null>(null);
  const [country, setCountry] = useState("vn");
  const [networks, setNetworks] = useState<SmsNetwork[]>([]);
  const [selectedNetworks, setSelectedNetworks] = useState<string[]>([]);
  const [services, setServices] = useState<SmsServiceInfo[]>([]);
  const [serviceId, setServiceId] = useState<string>("");
  const [loadingMeta, setLoadingMeta] = useState(false);
  const [requesting, setRequesting] = useState(false);
  const [polling, setPolling] = useState(false);
  const [numberInfo, setNumberInfo] = useState<SmsNumberInfo | null>(null);
  const [otpInfo, setOtpInfo] = useState<SmsOtpInfo | null>(null);
  const [history, setHistory] = useState<SmsHistoryEntry[]>([]);
  const [loadingHistory, setLoadingHistory] = useState(false);
  const [activeTab, setActiveTab] = useState("rent");
  const pollAbortRef = useRef(0);

  const selectedService = useMemo(
    () => services.find((s) => String(s.id) === serviceId) ?? null,
    [services, serviceId],
  );

  const networkParam = useMemo(() => {
    if (selectedNetworks.length === 0) return undefined;
    return `${selectedNetworks.join("|")}|`;
  }, [selectedNetworks]);

  const loadToken = useCallback(async () => {
    try {
      const stored = await invoke<string | null>("get_sms_api_token");
      setToken(stored ?? "");
    } catch (e) {
      console.error("Failed to load SMS token:", e);
    } finally {
      setTokenLoaded(true);
    }
  }, []);

  const saveToken = useCallback(async () => {
    setSavingToken(true);
    try {
      await invoke("set_sms_api_token", { token });
      toast.success(t("sms.tokenSaved"));
    } catch (e) {
      toast.error(String(e));
    } finally {
      setSavingToken(false);
    }
  }, [token, t]);

  const clearToken = useCallback(async () => {
    setSavingToken(true);
    try {
      await invoke("remove_sms_api_token");
      setToken("");
      setBalance(null);
      toast.success(t("sms.tokenCleared"));
    } catch (e) {
      toast.error(String(e));
    } finally {
      setSavingToken(false);
    }
  }, [t]);

  const requireToken = useCallback((): string | null => {
    const trimmed = token.trim();
    if (!trimmed) {
      toast.error(t("sms.tokenRequired"));
      return null;
    }
    return trimmed;
  }, [token, t]);

  const refreshBalance = useCallback(async () => {
    const tok = requireToken();
    if (!tok) return;
    try {
      const bal = await invoke<number>("sms_get_balance", { token: tok });
      setBalance(bal);
    } catch (e) {
      toast.error(String(e));
    }
  }, [requireToken]);

  const loadMeta = useCallback(async () => {
    const tok = requireToken();
    if (!tok) return;
    setLoadingMeta(true);
    try {
      const [nets, svcs] = await Promise.all([
        invoke<SmsNetwork[]>("sms_get_networks", {
          token: tok,
          country,
        }),
        invoke<SmsServiceInfo[]>("sms_get_services", {
          token: tok,
          country,
        }),
      ]);
      setNetworks(nets);
      setServices(svcs);
      setSelectedNetworks([]);
      if (svcs.length > 0) {
        setServiceId(String(svcs[0].id));
      } else {
        setServiceId("");
      }
      void refreshBalance();
    } catch (e) {
      toast.error(String(e));
    } finally {
      setLoadingMeta(false);
    }
  }, [country, refreshBalance, requireToken]);

  const loadHistory = useCallback(async () => {
    const tok = requireToken();
    if (!tok) return;
    setLoadingHistory(true);
    try {
      const rows = await invoke<SmsHistoryEntry[]>("sms_get_history", {
        token: tok,
        limit: 50,
      });
      setHistory(rows);
    } catch (e) {
      toast.error(String(e));
    } finally {
      setLoadingHistory(false);
    }
  }, [requireToken]);

  const toggleNetwork = (name: string) => {
    setSelectedNetworks((prev) =>
      prev.includes(name) ? prev.filter((n) => n !== name) : [...prev, name],
    );
  };

  const requestNumber = useCallback(async () => {
    const tok = requireToken();
    if (!tok) return;
    if (!serviceId) {
      toast.error(t("sms.serviceRequired"));
      return;
    }
    setRequesting(true);
    setOtpInfo(null);
    try {
      const info = await invoke<SmsNumberInfo>("sms_request_number", {
        token: tok,
        serviceId: Number(serviceId),
        network: networkParam,
        country,
      });
      setNumberInfo(info);
      if (info.balance != null) {
        setBalance(info.balance);
      }
      toast.success(t("sms.numberRented"));
    } catch (e) {
      toast.error(String(e));
    } finally {
      setRequesting(false);
    }
  }, [country, networkParam, requireToken, serviceId, t]);

  const pollOtp = useCallback(async () => {
    const tok = requireToken();
    if (!tok || !numberInfo?.requestId) {
      toast.error(t("sms.numberRequiredFirst"));
      return;
    }
    const generation = ++pollAbortRef.current;
    setPolling(true);
    setOtpInfo(null);
    try {
      const info = await invoke<SmsOtpInfo>("sms_get_otp", {
        token: tok,
        requestId: numberInfo.requestId,
        timeoutSecs: POLL_TIMEOUT_SECS,
      });
      if (generation !== pollAbortRef.current) return;
      setOtpInfo(info);
      if (info.code) {
        toast.success(t("sms.otpReceived"));
      }
    } catch (e) {
      if (generation === pollAbortRef.current) {
        toast.error(String(e));
      }
    } finally {
      if (generation === pollAbortRef.current) {
        setPolling(false);
      }
    }
  }, [numberInfo, requireToken, t]);

  const copyText = async (value: string) => {
    try {
      await navigator.clipboard.writeText(value);
      toast.success(t("sms.copied"));
    } catch {
      toast.error(t("sms.copyFailed"));
    }
  };

  useEffect(() => {
    if (!open) {
      pollAbortRef.current += 1;
      setPolling(false);
      return;
    }
    void loadToken();
  }, [open, loadToken]);

  useEffect(() => {
    if (open && tokenLoaded && token.trim()) {
      void loadMeta();
    }
  }, [open, tokenLoaded, token, loadMeta]);

  const statusLabel = (status: number) => {
    if (status === 1) return t("sms.statusCompleted");
    if (status === 2) return t("sms.statusExpired");
    return t("sms.statusWaiting");
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <LuSmartphone className="w-5 h-5" />
            {t("sms.title")}
          </DialogTitle>
        </DialogHeader>

        <div className="space-y-3">
          <div className="space-y-2">
            <Label htmlFor="sms-token">{t("sms.apiToken")}</Label>
            <div className="flex gap-2">
              <Input
                id="sms-token"
                type="password"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder={t("sms.apiTokenPlaceholder")}
              />
              <Button
                type="button"
                variant="secondary"
                disabled={savingToken}
                onClick={() => {
                  void saveToken();
                }}
              >
                {t("common.buttons.save")}
              </Button>
              <Button
                type="button"
                variant="outline"
                disabled={savingToken || !token}
                onClick={() => {
                  void clearToken();
                }}
              >
                {t("common.buttons.clear")}
              </Button>
            </div>
            <p className="text-xs text-muted-foreground">
              {t("sms.tokenHint")}
            </p>
          </div>

          <div className="flex flex-wrap items-center gap-3">
            <div className="text-sm">
              <span className="text-muted-foreground">
                {t("sms.balance")}:{" "}
              </span>
              <span className="font-medium">
                {balance == null ? "—" : balance.toLocaleString()}
              </span>
            </div>
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => {
                void refreshBalance();
              }}
            >
              <LuRefreshCw className="w-3.5 h-3.5 mr-1" />
              {t("common.buttons.refresh")}
            </Button>
            <div className="flex items-center gap-2">
              <Label>{t("sms.country")}</Label>
              <Select value={country} onValueChange={setCountry}>
                <SelectTrigger className="w-[140px]">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="vn">{t("sms.countryVn")}</SelectItem>
                  <SelectItem value="la">{t("sms.countryLa")}</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <Button
              type="button"
              size="sm"
              variant="secondary"
              disabled={loadingMeta}
              onClick={() => {
                void loadMeta();
              }}
            >
              {loadingMeta
                ? t("common.buttons.loading")
                : t("sms.loadServices")}
            </Button>
          </div>
        </div>

        <Tabs value={activeTab} onValueChange={setActiveTab} className="mt-2">
          <TabsList>
            <TabsTrigger value="rent">{t("sms.tabRent")}</TabsTrigger>
            <TabsTrigger value="history">{t("sms.tabHistory")}</TabsTrigger>
          </TabsList>

          <TabsContent value="rent" className="space-y-4 mt-3">
            <div className="space-y-2">
              <Label>{t("sms.service")}</Label>
              <Select value={serviceId} onValueChange={setServiceId}>
                <SelectTrigger>
                  <SelectValue placeholder={t("sms.selectService")} />
                </SelectTrigger>
                <SelectContent>
                  {services.map((s) => (
                    <SelectItem key={s.id} value={String(s.id)}>
                      {s.name} ({s.price})
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              {selectedService ? (
                <p className="text-xs text-muted-foreground">
                  {t("sms.servicePrice", { price: selectedService.price })}
                </p>
              ) : null}
            </div>

            <div className="space-y-2">
              <Label>{t("sms.networks")}</Label>
              {networks.length === 0 ? (
                <p className="text-sm text-muted-foreground">
                  {t("sms.networksEmpty")}
                </p>
              ) : (
                <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
                  {networks.map((n) => {
                    const checked = selectedNetworks.includes(n.name);
                    const inputId = `sms-network-${n.id}`;
                    return (
                      <div
                        key={n.id}
                        className="flex items-center gap-2 text-sm"
                      >
                        <Checkbox
                          id={inputId}
                          checked={checked}
                          onCheckedChange={() => {
                            toggleNetwork(n.name);
                          }}
                        />
                        <label htmlFor={inputId} className="cursor-pointer">
                          {n.name}
                        </label>
                      </div>
                    );
                  })}
                </div>
              )}
              <p className="text-xs text-muted-foreground">
                {t("sms.networksHint")}
              </p>
            </div>

            <div className="flex flex-wrap gap-2">
              <Button
                type="button"
                disabled={requesting || !serviceId}
                onClick={() => {
                  void requestNumber();
                }}
              >
                {requesting ? t("sms.requesting") : t("sms.requestNumber")}
              </Button>
              <Button
                type="button"
                variant="secondary"
                disabled={polling || !numberInfo}
                onClick={() => {
                  void pollOtp();
                }}
              >
                {polling ? t("sms.pollingOtp") : t("sms.pollOtp")}
              </Button>
            </div>

            {numberInfo ? (
              <div className="rounded-md border border-border p-3 space-y-2 bg-card">
                <div className="flex items-center justify-between gap-2">
                  <div>
                    <div className="text-xs text-muted-foreground">
                      {t("sms.phoneNumber")}
                    </div>
                    <div className="text-lg font-semibold tracking-wide">
                      {numberInfo.phoneNumber}
                    </div>
                  </div>
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    onClick={() => {
                      void copyText(numberInfo.phoneNumber);
                    }}
                  >
                    <LuCopy className="w-3.5 h-3.5 mr-1" />
                    {t("common.buttons.copy")}
                  </Button>
                </div>
                <div className="text-xs text-muted-foreground">
                  {t("sms.requestId")}: {numberInfo.requestId}
                </div>
                {numberInfo.rePhoneNumber ? (
                  <div className="text-xs text-muted-foreground">
                    {t("sms.rePhoneNumber")}: {numberInfo.rePhoneNumber}
                  </div>
                ) : null}
              </div>
            ) : null}

            {otpInfo ? (
              <div className="rounded-md border border-border p-3 space-y-2 bg-card">
                <div className="flex items-center justify-between gap-2">
                  <div>
                    <div className="text-xs text-muted-foreground">
                      {t("sms.otpCode")}
                    </div>
                    <div className="text-2xl font-bold tracking-widest">
                      {otpInfo.code ?? "—"}
                    </div>
                  </div>
                  {otpInfo.code ? (
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      onClick={() => {
                        void copyText(otpInfo.code ?? "");
                      }}
                    >
                      <LuCopy className="w-3.5 h-3.5 mr-1" />
                      {t("common.buttons.copy")}
                    </Button>
                  ) : null}
                </div>
                <div className="text-xs text-muted-foreground">
                  {t("sms.status")}: {statusLabel(otpInfo.status)}
                </div>
                {otpInfo.smsContent ? (
                  <p className="text-xs text-muted-foreground break-all">
                    {otpInfo.smsContent}
                  </p>
                ) : null}
              </div>
            ) : null}
          </TabsContent>

          <TabsContent value="history" className="space-y-3 mt-3">
            <div className="flex justify-end">
              <Button
                type="button"
                size="sm"
                variant="outline"
                disabled={loadingHistory}
                onClick={() => {
                  void loadHistory();
                }}
              >
                <LuRefreshCw className="w-3.5 h-3.5 mr-1" />
                {loadingHistory
                  ? t("common.buttons.loading")
                  : t("common.buttons.refresh")}
              </Button>
            </div>
            {history.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                {t("sms.historyEmpty")}
              </p>
            ) : (
              <div className="space-y-2 max-h-[360px] overflow-y-auto">
                {history.map((row) => (
                  <div
                    key={row.id}
                    className="rounded-md border border-border p-2 text-sm"
                  >
                    <div className="flex justify-between gap-2">
                      <span className="font-medium">{row.phone}</span>
                      <span className="text-muted-foreground">
                        {statusLabel(row.status)}
                      </span>
                    </div>
                    <div className="text-xs text-muted-foreground">
                      {row.serviceName ?? "—"} · {row.code ?? "—"} ·{" "}
                      {row.createdTime ?? "—"}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </TabsContent>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}
