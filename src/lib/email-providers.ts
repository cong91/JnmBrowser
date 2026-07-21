/** Email OTP providers for ChatGPT auto-registration — named by API domain. */

export const EMAIL_PROVIDERS = [
  "gmail.123452026.xyz",
  "sms.iosmq.xyz",
] as const;

export type EmailProvider = (typeof EMAIL_PROVIDERS)[number];

const LEGACY_ALIASES: Record<string, EmailProvider> = {
  "123452026": "gmail.123452026.xyz",
  gmail_cdk: "gmail.123452026.xyz",
  "gmail-cdk": "gmail.123452026.xyz",
  iosmq: "sms.iosmq.xyz",
  "sms.iosmq": "sms.iosmq.xyz",
  "iosmq.xyz": "sms.iosmq.xyz",
  mail: "sms.iosmq.xyz",
};

export function isEmailProvider(value: string): value is EmailProvider {
  return (EMAIL_PROVIDERS as readonly string[]).includes(value);
}

export function parseEmailProvider(
  value: string | undefined | null,
): EmailProvider {
  if (!value) {
    return "gmail.123452026.xyz";
  }
  const raw = value.trim().toLowerCase();
  if (isEmailProvider(raw)) {
    return raw;
  }
  return LEGACY_ALIASES[raw] ?? "gmail.123452026.xyz";
}

/** Max accounts that can be created from one card/CDK for the provider. */
export function maxAccountsPerCard(_provider: EmailProvider): number {
  return 6;
}

export function clampAccountsPerCard(
  provider: EmailProvider,
  requested: number,
): number {
  const max = maxAccountsPerCard(provider);
  if (!Number.isFinite(requested) || requested < 1) {
    return 1;
  }
  return Math.min(max, Math.max(1, Math.floor(requested)));
}

/** Textarea placeholder for card codes. */
export function cardCodesPlaceholder(provider: EmailProvider): string {
  switch (provider) {
    case "sms.iosmq.xyz":
      return "MAIL-XXXX-XXXX-XXXX\nMAIL-YYYY-YYYY-YYYY";
    default:
      return "GMAIL-K4L5-EUW5-PHBV-A6KW\nGMAIL-XXXX-XXXX-XXXX-XXXX";
  }
}

/** i18n key for the selected provider label. */
export function emailProviderLabelKey(provider: EmailProvider): string {
  switch (provider) {
    case "sms.iosmq.xyz":
      return "registration.emailProviderSmsIosmq";
    default:
      return "registration.emailProviderGmail123452026";
  }
}

/** i18n key for the selected provider hint. */
export function emailProviderHintKey(provider: EmailProvider): string {
  switch (provider) {
    case "sms.iosmq.xyz":
      return "registration.emailProviderSmsIosmqHint";
    default:
      return "registration.emailProviderGmail123452026Hint";
  }
}
