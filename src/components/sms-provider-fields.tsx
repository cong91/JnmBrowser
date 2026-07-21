"use client";

import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuRefreshCw } from "react-icons/lu";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Combobox } from "@/components/ui/combobox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { SmsNetwork, SmsServiceInfo } from "@/types";

interface SmsProviderFieldsProps {
  enabled: boolean;
  country: string;
  onCountryChange: (country: string) => void;
  serviceId: string;
  onServiceIdChange: (serviceId: string) => void;
  network: string;
  onNetworkChange: (network: string) => void;
  tokenOverride: string;
  onTokenOverrideChange: (token: string) => void;
  hasSavedToken: boolean;
}

/** Fallback carriers when /networks/get is unreachable. */
const FALLBACK_NETWORKS: SmsNetwork[] = [
  { id: 1, name: "VINAPHONE" },
  { id: 2, name: "VIETTEL" },
  { id: 3, name: "MOBIFONE" },
  { id: 4, name: "VIETNAMOBILE" },
  { id: 5, name: "ITELECOM" },
];

function parseNetworkSelection(value: string): string[] {
  return value
    .split("|")
    .map((part) => part.trim())
    .filter(Boolean);
}

function redactSecrets(message: string): string {
  return message
    .replace(/token=[^&\s"']+/gi, "token=[redacted]")
    .replace(/[0-9a-f]{24,}/gi, "[redacted]");
}

function scoreService(name: string): number {
  const n = name.toLowerCase();
  if (/(^|[^a-z])openai([^a-z]|$)/.test(n) && /chatgpt/.test(n)) return 100;
  if (/(^|[^a-z])chatgpt([^a-z]|$)/.test(n)) return 95;
  if (/(^|[^a-z])openai([^a-z]|$)/.test(n)) return 90;
  if (/(^|[^a-z])codex([^a-z]|$)/.test(n)) return 85;
  if (/gpt/.test(n)) return 70;
  return 0;
}

function pickDefaultServiceId(
  services: SmsServiceInfo[],
  preferredId: string,
): string {
  if (preferredId && services.some((s) => String(s.id) === preferredId)) {
    return preferredId;
  }

  let best: SmsServiceInfo | null = null;
  let bestScore = 0;
  for (const service of services) {
    const score = scoreService(service.name);
    if (
      score > bestScore ||
      (score === bestScore && best && score > 0 && service.price < best.price)
    ) {
      best = service;
      bestScore = score;
    }
  }
  if (best && bestScore > 0) {
    return String(best.id);
  }
  return services[0] ? String(services[0].id) : "";
}

export function SmsProviderFields({
  enabled,
  country,
  onCountryChange,
  serviceId,
  onServiceIdChange,
  network,
  onNetworkChange,
  tokenOverride,
  onTokenOverrideChange,
  hasSavedToken,
}: SmsProviderFieldsProps) {
  const { t } = useTranslation();
  const [services, setServices] = useState<SmsServiceInfo[]>([]);
  const [networks, setNetworks] = useState<SmsNetwork[]>([]);
  const [loadingMeta, setLoadingMeta] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [usingFallbackNetworks, setUsingFallbackNetworks] = useState(false);

  const serviceIdRef = useRef(serviceId);
  const networkRef = useRef(network);
  const onServiceIdChangeRef = useRef(onServiceIdChange);
  const onNetworkChangeRef = useRef(onNetworkChange);

  useEffect(() => {
    serviceIdRef.current = serviceId;
  }, [serviceId]);

  useEffect(() => {
    networkRef.current = network;
  }, [network]);

  useEffect(() => {
    onServiceIdChangeRef.current = onServiceIdChange;
  }, [onServiceIdChange]);

  useEffect(() => {
    onNetworkChangeRef.current = onNetworkChange;
  }, [onNetworkChange]);

  const selectedNetworks = useMemo(
    () => parseNetworkSelection(network),
    [network],
  );

  const serviceOptions = useMemo(
    () =>
      services.map((s) => ({
        value: String(s.id),
        label: `${s.name} · ${s.price}`,
        description: `ID ${s.id}`,
      })),
    [services],
  );

  const selectedService = useMemo(
    () => services.find((s) => String(s.id) === serviceId) ?? null,
    [services, serviceId],
  );

  const loadMeta = useCallback(async () => {
    if (!enabled) return;

    const override = tokenOverride.trim();
    let token = override;
    if (!token) {
      try {
        const stored = await invoke<string | null>("get_sms_api_token");
        token = stored?.trim() || "";
      } catch {
        token = "";
      }
    }

    if (!token) {
      setServices([]);
      setNetworks([]);
      setLoadError(null);
      setUsingFallbackNetworks(false);
      return;
    }

    setLoadingMeta(true);
    setLoadError(null);

    // Load independently so network endpoint failure does not block service list.
    const servicesResult = await invoke<SmsServiceInfo[]>("sms_get_services", {
      token,
      country,
    })
      .then((svcs) => ({ ok: true as const, svcs }))
      .catch((e) => ({
        ok: false as const,
        error: redactSecrets(e instanceof Error ? e.message : String(e)),
      }));

    const networksResult = await invoke<SmsNetwork[]>("sms_get_networks", {
      token,
      country,
    })
      .then((nets) => ({ ok: true as const, nets }))
      .catch((e) => ({
        ok: false as const,
        error: redactSecrets(e instanceof Error ? e.message : String(e)),
      }));

    if (servicesResult.ok) {
      const svcs = servicesResult.svcs;
      setServices(svcs);
      const preferredId = serviceIdRef.current;
      const nextServiceId = pickDefaultServiceId(svcs, preferredId);
      if (nextServiceId && nextServiceId !== preferredId) {
        onServiceIdChangeRef.current(nextServiceId);
      }
    } else {
      setServices([]);
      setLoadError(servicesResult.error);
      toast.error(servicesResult.error);
    }

    if (networksResult.ok) {
      setNetworks(networksResult.nets);
      setUsingFallbackNetworks(false);
      const available = new Set(networksResult.nets.map((n) => n.name));
      const current = parseNetworkSelection(networkRef.current);
      const kept = current.filter((name) => available.has(name));
      if (kept.length !== current.length) {
        onNetworkChangeRef.current(kept.join("|"));
      }
    } else {
      // Keep UI usable: show common VN carriers even if network API is down.
      setNetworks(FALLBACK_NETWORKS);
      setUsingFallbackNetworks(true);
      if (!servicesResult.ok) {
        // already toasted service error; avoid double toast spam
      } else {
        toast.error(
          t("sms.networksLoadFailed", {
            error: networksResult.error,
          }),
        );
      }
    }

    setLoadingMeta(false);
  }, [country, enabled, t, tokenOverride]);

  // Dependency size/order must stay constant across HMR reloads.
  // country is covered via loadMeta identity.
  useEffect(() => {
    if (!enabled) {
      setServices([]);
      setNetworks([]);
      setLoadError(null);
      setUsingFallbackNetworks(false);
      return;
    }
    if (!hasSavedToken && !tokenOverride.trim()) {
      setServices([]);
      setNetworks([]);
      setLoadError(null);
      setUsingFallbackNetworks(false);
      return;
    }
    void loadMeta();
  }, [enabled, hasSavedToken, tokenOverride, loadMeta]);

  const toggleNetwork = (name: string) => {
    const next = selectedNetworks.includes(name)
      ? selectedNetworks.filter((n) => n !== name)
      : [...selectedNetworks, name];
    onNetworkChange(next.join("|"));
  };

  if (!enabled) return null;

  return (
    <div className="space-y-3">
      <div className="grid grid-cols-2 gap-2">
        <div className="space-y-1">
          <Label className="text-xs">{t("registration.smsCountry")}</Label>
          <Select value={country} onValueChange={onCountryChange}>
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="vn">{t("sms.countryVn")}</SelectItem>
              <SelectItem value="la">{t("sms.countryLa")}</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <div className="space-y-1">
          <Label className="text-xs">
            {t("registration.smsTokenOverride")}
          </Label>
          <Input
            type="password"
            value={tokenOverride}
            onChange={(e) => onTokenOverrideChange(e.target.value)}
            placeholder={
              hasSavedToken
                ? t("registration.smsTokenOverridePlaceholder")
                : t("sms.apiTokenPlaceholder")
            }
          />
        </div>
      </div>

      <div className="flex items-center justify-between gap-2">
        <Label className="text-xs">{t("sms.service")}</Label>
        <Button
          type="button"
          size="sm"
          variant="outline"
          disabled={loadingMeta || (!hasSavedToken && !tokenOverride.trim())}
          onClick={() => {
            void loadMeta();
          }}
        >
          <LuRefreshCw
            className={`mr-1 h-3.5 w-3.5 ${loadingMeta ? "animate-spin" : ""}`}
          />
          {loadingMeta ? t("common.buttons.loading") : t("sms.loadServices")}
        </Button>
      </div>

      <Combobox
        options={serviceOptions}
        value={serviceId}
        onValueChange={onServiceIdChange}
        placeholder={t("sms.selectService")}
        searchPlaceholder={t("common.buttons.search")}
        disabled={loadingMeta || serviceOptions.length === 0}
      />
      {selectedService ? (
        <p className="text-xs text-muted-foreground">
          {t("sms.servicePrice", { price: selectedService.price })} · ID{" "}
          {selectedService.id}
          {scoreService(selectedService.name) >= 90
            ? ` · ${t("sms.autoSelectedOpenAI")}`
            : ""}
        </p>
      ) : (
        <p className="text-xs text-muted-foreground">
          {loadError
            ? t("sms.servicesLoadFailed", { error: loadError })
            : hasSavedToken || tokenOverride.trim()
              ? t("sms.serviceRequired")
              : t("registration.smsTokenMissing")}
        </p>
      )}

      <div className="space-y-2">
        <Label className="text-xs">{t("sms.networks")}</Label>
        {networks.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            {t("sms.networksEmpty")}
          </p>
        ) : (
          <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
            {networks.map((n) => {
              const checked = selectedNetworks.includes(n.name);
              const inputId = `auto-sms-network-${n.id}`;
              return (
                <label
                  key={n.id}
                  htmlFor={inputId}
                  className="flex cursor-pointer items-center gap-2 rounded-md border border-border px-2 py-1.5 text-xs"
                >
                  <input
                    id={inputId}
                    type="checkbox"
                    className="h-3.5 w-3.5"
                    checked={checked}
                    onChange={() => toggleNetwork(n.name)}
                  />
                  <span className="truncate">{n.name}</span>
                </label>
              );
            })}
          </div>
        )}
        <p className="text-xs text-muted-foreground">
          {usingFallbackNetworks
            ? t("sms.networksFallbackHint")
            : t("sms.networksHint")}
        </p>
      </div>
    </div>
  );
}
