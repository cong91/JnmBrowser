# ChatGPT Auto-Registration

Automatically create ChatGPT accounts using Gmail CDK codes, browser automation, and anti-detect fingerprinting.

## Overview

The auto-registration feature automates the entire ChatGPT signup flow:

1. **CDK Redemption**: Redeems a Gmail CDK code to obtain a disposable Gmail address
2. **Alias Generation**: Creates `user+{random}@gmail.com` aliases (up to 6 per CDK)
3. **Browser Automation**: Reuses one worker Chromium/Camoufox profile per batch; each account relaunches with a renewed fingerprint (+ optional proxy)
4. **Registration Flow**: Automates the ChatGPT signup via CDP / Playwright
5. **OTP Retrieval**: Polls the Gmail CDK API for the verification code
6. **Token Extraction**: Extracts access token, session token, and account credentials
7. **Free-trial gate + 2FA**: Keeps free-trial eligible accounts and enables authenticator when possible
8. **Credential Storage**: Persists inventory JSON for export/resale
9. **Network**: Optional static proxy, **WireGuard VPN** from Proxies & VPNs (preferred), or NordVPN CLI backup (system-wide rotate)

## Prerequisites

- A valid Gmail CDK code (format: `GMAIL-XXXX-XXXX-XXXX-XXXX`)
- Chromium or Camoufox browser installed (via JnmBrowser's downloader)
- **Network (pick one mode):**
  - **None** — host egress IP
  - **Proxy** — a proxy ID already configured in JnmBrowser
  - **VPN (WireGuard)** — a `vpnId` from Proxies & VPNs (including Nord Access Token → WG configs)
  - **NordVPN CLI (backup)** — Windows NordVPN app installed, logged in, CLI available (`NordVPN.exe` under Program Files)

## Usage

### Via UI

1. Open JnmBrowser
2. Click the **Auto Registration** button in the header
3. Enter your CDK code(s)
4. Configure browser type, retries, accounts per CDK
5. Choose **Network**:
   - **None** — no proxy / no VPN
   - **Proxy** — enter proxy ID
   - **VPN (WireGuard)** — select a config from Proxies & VPNs (preferred, per-profile)
   - **NordVPN CLI (backup)** — optional group/server, **rotate IP every N successes** (default 2, system-wide)
6. Click **Start Registration**

Progress is shown in real-time with step-by-step logs (including IP rotation messages in Nord mode).

### Dual network modes

| Mode | Profile attach | Mid-batch behavior |
|------|----------------|--------------------|
| `none` | no `proxy_id` / no `vpn_id` | host IP sticky |
| `proxy` | static `proxyId` on the reused worker profile | no mid-batch hop (v1) |
| `vpn` | WireGuard `vpnId` base from **Proxies & VPNs** (private key source) | Concurrency **auto = 6** (Nord WG policy; plan allows up to ~10 devices). At Start, spawn up to 6 ephemeral Nord peers. Process all CDKs in waves. Per-slot rotate hops only that worker’s peer. Ephemeral confs deleted when the batch ends. |
| `nord` | no profile proxy/VPN | **backup** system-wide Nord CLI; after every **N successful free-trial saves**, disconnect → connect → verify public IP |

**Important:** 
**Nord session budget:** Fixed product policy **max 6** concurrent WireGuard sessions for auto-reg (Nord plans allow up to ~10 devices; 6 is the safe parallel cap). When you create a VPN via Access Token, JnmBrowser stores `max_sessions = 6` on that config and auto-sets concurrency. You do not enter session limits or concurrency by hand for Nord WG.

Prefer **`vpn`** (WireGuard inventory / Nord Access Token configs created in Proxies & VPNs) for isolation. **`nord` is CLI backup only** — system-wide (affects the whole PC, including CDK HTTP and OTP polling). Modes are mutually exclusive (`proxyId` / `vpnId` / Nord CLI). Nord CLI stays connected after the batch finishes (no auto-disconnect); you disconnect manually when done.

### Profile lifecycle (reuse, not spam)

Auto-registration does **not** create a new profile metadata row for every account.

| Phase | Behavior |
|-------|----------|
| Per CDK worker | Create **one** ephemeral worker profile (`auto-reg-worker-{task8}-s{slot}`), **or** adopt `profileId` if provided |
| Each account (alias) | Kill → relaunch same worker: `randomize_fingerprint_on_launch` + fresh ephemeral data dir + `clear_all_site_data` + new `device_id` |
| Cloudflare authorize retry | Relaunch the **same** worker (no extra profile create) |
| CDK finished | Delete auto-created worker for that slot; **never** delete a user-provided `profileId` |

Isolation between accounts comes from relaunch fingerprint renew + ephemeral dir wipe + cookie/storage clear — not from creating unlimited profiles.

### CDK concurrency (1 CDK = 1 thread)

- `concurrency` = max number of CDKs processed in parallel (UI default 1, max 8).
- **Inside** a CDK, aliases (`accountsPerCdk`, 1–6) stay **sequential**.
- **Nord CLI mode forces concurrency = 1** (system-wide IP is not thread-safe).
- Proxy / VPN / none modes can run multiple CDK workers concurrently; each slot has its own worker profile.

### CDK inventory stats

Every CDK used is persisted under app data `cdk_inventory/`:

| Field | Meaning |
|-------|---------|
| `targetAccounts` | `accountsPerCdk` for that run |
| `attempted` | alias slots that finished |
| `freeTrialYes` | free-trial eligible successes |
| `freeTrialNo` | registered but **no** free trial (saved, status invalid) |
| `failed` | hard failures after retries |
| `accounts[]` | per-email detail (success / free trial / error) |

UI: Auto Registration → **CDK stats** tab. Commands: `list_cdk_inventory_cmd`, `delete_cdk_inventory_cmd`.

### Via Tauri Commands

```typescript
import { invoke } from "@tauri-apps/api/core";

// Proxy mode
const taskId = await invoke("start_auto_registration", {
  config: {
    cdks: ["GMAIL-K4L5-EUW5-PHBV-A6KW"],
    browserType: "chromium",
    networkMode: "proxy",
    proxyId: "my-proxy-id",
    maxRetries: 3,
    accountsPerCdk: 1,
    headless: false,
    concurrency: 2, // max parallel CDKs
  },
});

// WireGuard VPN mode (preferred; config from Proxies & VPNs)
const vpnTask = await invoke("start_auto_registration", {
  config: {
    cdks: ["GMAIL-K4L5-EUW5-PHBV-A6KW"],
    browserType: "chromium",
    networkMode: "vpn",
    vpnId: "your-wireguard-vpn-config-id",
    maxRetries: 3,
    accountsPerCdk: 1,
    concurrency: 1,
  },
});

// Nord CLI backup mode with rotate every 2 successes

const nordTask = await invoke("start_auto_registration", {
  config: {
    cdks: ["GMAIL-K4L5-EUW5-PHBV-A6KW"],
    browserType: "camoufox",
    networkMode: "nord",
    rotateEveryN: 2,
    nordGroup: "United States",
    maxRetries: 3,
    accountsPerCdk: 2,
    concurrency: 1,
  },
});

// Cancel registration
await invoke("cancel_registration", { taskId });

// List registered accounts
const accounts = await invoke("list_registered_accounts_cmd");

// Delete an account
await invoke("delete_registered_account_cmd", { accountId: "..." });
```

### Via MCP Tools (future)

```json
{
  "tool": "start_auto_register",
  "arguments": {
    "cdks": ["GMAIL-XXXX-XXXX-XXXX-XXXX"],
    "browserType": "chromium",
    "networkMode": "nord",
    "rotateEveryN": 2,
    "maxRetries": 3,
    "accountsPerCdk": 1
  }
}
```

## Architecture

```
Frontend (React) → Tauri invoke → RegistrationEngine (Rust)
                                     ├── GmailCdkService (HTTP API)
                                     ├── BrowserRunner (launch profile)
                                     ├── CDP / Playwright (automation)
                                     ├── Nord CLI helper (optional system VPN)
                                     └── CredentialStore (JSON persistence)
```

### Registration Flow

```
CDK Input → Redeem CDK → Generate Alias → Generate User Info
→ [Nord connect if mode=nord]
→ Launch Browser → Visit chatgpt.com → CSRF Token → Submit Email
→ Follow Authorize → Register User → Request OTP → Poll OTP
→ Verify OTP → Create Account → Extract Tokens → Free-trial gate → Enable 2FA
→ Save inventory → [every N successes: Nord rotate + IP verify]
→ Leave Nord connected after finish (no auto-disconnect)
```

Registration itself is API-driven (CSRF / register / email-otp / create_account).
2FA is UI-driven after a live session is available (Settings → Security → Authenticator).

## Credential Storage

Credentials are stored at:
```
{data_dir}/registered_accounts/{account_id}.json
```

Each file contains:
- Email and password
- Access token
- Account ID
- `twoFaEnabled` + `totpSecret` (base32) when authenticator setup succeeds
- Step logs for debugging

## Troubleshooting

### CDK Redeem Fails
- Verify the CDK is valid and not expired
- Check network connectivity to `http://gmail.123452026.xyz`

### Browser Launch Fails
- Ensure the browser binary is downloaded
- Check proxy connectivity

### OTP Never Arrives
- The Gmail CDK API may be rate-limited
- Increase timeout or retry

### Cloudflare Interception
- The engine automatically retries by relaunching the **same** worker profile with a new fingerprint / device id
- Consider using a different proxy or fingerprint seed / network mode

## Free Trial Gate

After tokens are extracted, registration **only succeeds** if the account is eligible for a free trial / free Plus offer.

### Verified detection (live account probe)

Primary source of truth:

`GET https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27`

(with page cookies + `Authorization: Bearer <accessToken>` + `ChatGPT-Account-ID`)

Key fields from a fresh free signup that **has** free Plus trial:

```json
{
  "accounts": {
    "<account_id>": {
      "account": {
        "plan_type": "free",
        "has_previously_paid_subscription": false
      },
      "entitlement": {
        "subscription_plan": "chatgptfreeplan",
        "has_active_subscription": false,
        "trial": null
      },
      "eligible_promo_campaigns": {
        "plus": {
          "id": "plus-1-month-free",
          "metadata": {
            "discount": { "percentage": 100 },
            "duration": { "num_periods": 1, "period": "month" },
            "plan_name": "chatgptplusplan",
            "promotion_type_label": "1-month free trial",
            "title": "Try Plus free for 1 month"
          }
        }
      },
      "is_eligible_for_yearly_plus_new_user_subscription": true
    }
  }
}
```

Rules used by engine:

1. **Eligible if** `eligible_promo_campaigns` contains a free-trial promo
   - id/title/label contains free trial / try plus free / plus-1-month-free
   - or discount percentage == 100
2. **Eligible if** `entitlement.trial` is a non-null active trial object
3. JWT claim `chatgpt_plan_type=free` alone is **not** enough (that only means free plan)
4. `eligible_offers` alone is **not** enough (always lists plus/pro plan SKUs)
5. DOM fallback: text like `claim offer` / `free offer` / `free trial`
6. If no free-offer signal → account is **skipped** (not stored as success)

Stored fields: `freeTrialEligible`, `planType`.

Secondary endpoint:

`GET /backend-api/subscriptions?account_id=<id>` (requires account_id query)

Probe helper:

```bash
cargo run --manifest-path src-tauri/Cargo.toml --bin probe-free-trial --   --profile-id <camoufox-profile-id>   --token-file <registered_account.json>
```

## 2FA Enablement

After tokens are extracted, the engine enables ChatGPT authenticator 2FA in the same browser session:

1. Open account menu → Settings (`data-testid=settings-menu-item`)
2. Open **Security** tab
3. Toggle **Authenticator app** (`data-testid=mfa-authenticator-toggle`)
4. Click the underlined reveal-secret link (`button.interactive-label-accent.underline`) so the base32 secret is shown
5. Click **Copy code** (`button[aria-label="Copy code"]`)
6. Scrape the base32 secret from the dialog (same value that was copied)
7. Generate a local TOTP code from that secret (RFC 6238, HMAC-SHA1, 30s, 6 digits)
8. Fill `#totp_otp` with the 6-digit code and confirm

Policy:
- Only the 2FA step is retried (default 3 attempts) inside the same browser session
- If 2FA still fails, the registration remains successful with `twoFaEnabled=false` and an error note
- On success, `totpSecret` is persisted with the account for later login/automation

Reference recordings: `register_1.json` / `register_2.json` (signup), `enable2FA.json` (2FA-only).
Recipe sketch: `src-tauri/src/auto_service/openai/register/recipes/enable_2fa_recipe.json`.


## Account Inventory & Export

Registered accounts are inventory records under `{data_dir}/registered_accounts/{account_id}.json`.

### Inventory status

- `available`: ready to sell/use
- `exported`: already exported
- `sold`: sold/used
- `reserved`: held for a buyer
- `invalid`: dead/banned

### Export (UI)

In **Stored Accounts**: select rows (or empty = all available), choose fields, format TXT/CSV/JSON, optional delimiter, optional auto-mark exported, then save via system dialog.

Default seller TXT line: `email|password|totpSecret`

### Commands

```typescript
await invoke("update_registered_account_status_cmd", {
  accountIds: ["..."],
  status: "sold",
  note: "buyer-A",
});
```


## Dual network troubleshooting

| Symptom | Fix |
|---------|-----|
| NordVPN CLI not found | Install NordVPN desktop app or set `nordCliPath` to NordVPN.exe |
| Connect fails / not logged in | Open NordVPN GUI, log in, then retry |
| Egress IP unchanged after rotate | Wait longer, try a country group; soft-warn continues batch |
| Free trial drop after hop | Stick to a stable region group (e.g. United States) |
| Proxy mode error without ID | Provide `proxyId` when `networkMode` is `proxy` |
