# ChatGPT Auto-Registration

Automatically create ChatGPT accounts using Gmail CDK codes, browser automation, and anti-detect fingerprinting.

## Overview

The auto-registration feature automates the entire ChatGPT signup flow:

1. **Email provider**: Choose **gmail.123452026.xyz** or **sms.iosmq.xyz** quota-card API
2. **Mailbox acquisition**:
   - gmail.123452026.xyz: redeem CDK → base Gmail, then `user+{random}@gmail.com` aliases (up to 6)
   - sms.iosmq.xyz: `POST /api/v1/redeem` with `MAIL-…` card to claim the base mailbox, then poll `GET /api/v1/order/lookup?code=…&poll=true` for the mailbox and newest OTP; generate `user+{random}@gmail.com` aliases (up to 6) against the same card lookup
3. **Browser Automation**: Reuses one worker Chromium/Camoufox profile per batch; each account relaunches with a renewed fingerprint (+ optional proxy)
4. **Registration Flow**: Automates the ChatGPT signup via CDP / Playwright
5. **OTP Retrieval**: Polls the selected email provider for the verification code
6. **Token Extraction**: Extracts access token, session token, and account credentials
7. **Free-trial gate + 2FA**: Keeps free-trial eligible accounts and enables authenticator when possible
8. **Credential Storage**: Persists inventory JSON for export/resale
9. **Network**: Optional static proxy, **WireGuard VPN** from Proxies & VPNs (preferred), or NordVPN CLI backup (system-wide rotate)

## Prerequisites

- A valid email card:
  - gmail.123452026.xyz: `GMAIL-XXXX-XXXX-XXXX-XXXX` (provider `gmail.123452026.xyz`)
  - sms.iosmq.xyz: `MAIL-XXXX-XXXX-XXXX` (provider `sms.iosmq.xyz`); redeem the MAIL card, then poll `order/lookup` for the mailbox and newest OTP
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
3. Choose **Email provider** (`gmail.123452026.xyz` or `sms.iosmq.xyz`)
4. Enter your card/CDK code(s)
5. Configure browser type, retries, and accounts per CDK (1–6 aliases for either provider)
6. Choose **Network**:
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
| `vpn` | WireGuard `vpnId` base from **Proxies & VPNs** (private key source) | Operator chooses CDK concurrency; effective concurrency is capped by the detected Nord session budget (maximum 6). At Start, spawn that many ephemeral Nord peers. Process all CDKs in waves. Per-slot rotate hops only that worker’s peer. Ephemeral confs are deleted when the batch ends. |
| `nord` | no profile proxy/VPN | **backup** system-wide Nord CLI; after every **N successful free-trial saves**, disconnect → connect → verify public IP |

**Important:** 
**Nord session budget:** Fixed product policy **max 6** concurrent WireGuard sessions for auto-reg (Nord plans allow up to ~10 devices; 6 is the safe parallel cap). When you create a VPN via Access Token, JnmBrowser stores the detected `max_sessions` on that config. The operator chooses CDK concurrency in the registration form; effective concurrency is `min(operator concurrency, detected session budget, 6)`.

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
| `targetAccounts` | Cumulative account slots requested for this card |
| `attempted` | Cumulative alias slots that finished |
| `freeTrialYes` | free-trial eligible successes |
| `freeTrialNo` | registered but **no** free trial (saved, status invalid) |
| `failed` | hard failures after retries |
| `remaining` | Free slots left for this card (**ledger-backed**, 0–6; recomputed on list) |
| `accounts[]` | per-email detail (success / free trial / error) |

UI: Auto Registration → **CDK stats** tab. Commands: `list_cdk_inventory_cmd`, `delete_cdk_inventory_cmd`.

Each canonical card (`trim` + case-insensitive) has a lifetime budget of six logical account slots. Active tasks reserve slots atomically so the same card cannot be over-allocated concurrently; full-flow retries reuse one slot and one account identity. The usage ledger stores only one-way SHA-256 card identifiers and is the **quota source of truth** (`remaining = 6 − used − reserved`).

#### Continue / Top-up (partial CDK)

When a batch ends early (`attempted` below what you wanted, or free slots left under the 6-cap):

1. Open **CDK stats** and check **Remaining**.
2. Click **Top-up** on a row with remaining > 0 (disabled while `status=running` or remaining is 0).
3. The Register tab prefills that CDK and clamps **accounts per CDK** to remaining. History (`accounts[]`, free-trial counts, base email) is **kept**.
4. Start again — the engine still clamps via `cdk_remaining_capacity` / slot reservation.

**Delete** on a CDK stats row is a **full reset**: it removes the stats history **and** clears that card’s usage-ledger entry so quota returns to 6 free slots. Prefer Top-up when you only need more accounts without wiping history.

Raw multi-line CDK entry still works for brand-new cards and multi-CDK batches.

### Via Tauri Commands

```typescript
import { invoke } from "@tauri-apps/api/core";

// Proxy mode + Gmail CDK (default email provider)
const taskId = await invoke("start_auto_registration", {
  config: {
    cdks: ["GMAIL-K4L5-EUW5-PHBV-A6KW"],
    emailProvider: "gmail.123452026.xyz",
    browserType: "chromium",
    networkMode: "proxy",
    proxyId: "my-proxy-id",
    maxRetries: 3,
    accountsPerCdk: 1,
    headless: false,
    concurrency: 2, // max parallel CDKs
  },
});

// sms.iosmq.xyz MAIL cards — redeem + order/lookup OTP
const smsIosmqTask = await invoke("start_auto_registration", {
  config: {
    cdks: ["MAIL-XXXX-XXXX-XXXX"],
    emailProvider: "sms.iosmq.xyz",
    browserType: "chromium",
    networkMode: "none",
    maxRetries: 3,
    accountsPerCdk: 1, // 1–6 aliases per card/CDK
    headless: false,
    concurrency: 1,
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
                                     ├── Gmail123452026Service (HTTP API)
                                     ├── SmsIosmqService (HTTP API)
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

Registration uses browser-backed API and UI steps. The About You name and birthdate step is always completed by filling the visible form and submitting it; it does not fall back to the `create_account` API.
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
- gmail.123452026.xyz: redeem the CDK and poll the Gmail mailbox API for OTP
- sms.iosmq.xyz: redeem the MAIL card, then poll `order/lookup` for the mailbox and newest OTP

### Browser Launch Fails
- Ensure the browser binary is downloaded
- Check proxy connectivity

### OTP Never Arrives
- The selected provider API may be rate-limited or temporarily unavailable
- Confirm the selected domain provider is reachable and the card/CDK is active
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

1. **Primary (live-proven):** open largest `data-testid=accounts-profile-button` chip → `data-testid=settings-menu-item` → **Security and login** (`data-testid=security-tab`)
2. **Secondary:** set in-page hash `#settings/Security` (no CDP full navigation — hash-only navigate does not fire `loadEventFired` and used to burn 20s)
3. Toggle **Authenticator app** (`data-testid=mfa-authenticator-toggle`)
4. Click the underlined reveal-secret link (`button.interactive-label-accent.underline`) so the base32 secret is shown
5. Click **Copy code** (`button[aria-label="Copy code"]`)
6. Scrape the base32 secret from the dialog (same value that was copied)
7. Generate a local TOTP code from that secret (RFC 6238, HMAC-SHA1, 30s, 6 digits)
8. Fill `#totp_otp` with the 6-digit code and confirm

Token extract retries `/api/auth/session` up to 5 times with human jitter (session cookies often land after first home paint).

Policy:
- Only the 2FA step is retried (default 3 attempts) inside the same browser session
- If 2FA still fails, the registration remains successful with `twoFaEnabled=false` and an error note
- On success, `totpSecret` is persisted with the account for later login/automation

Reference recordings: `register_1.json` / `register_2.json` (signup), `enable2FA.json` (2FA-only).
Recipe sketch: `src-tauri/src/auto_service/openai/register/recipes/enable_2fa_recipe.json`.

## Selected-account 2FA Backfill

Selected-account 2FA backfill is a repair workflow for existing stored accounts whose local inventory says 2FA is Off. It logs back into the selected account, handles the provider email OTP when required, enables authenticator 2FA only when the remote Security page is confirmed Off, and patches the same inventory record with the recovered TOTP secret. It does **not** create accounts or change registration outcomes.

### Operator flow

1. In **Stored Accounts**, select the target rows and choose **Activate 2FA**.
2. Run `preview_two_factor_backfill` through the dialog preview before starting. The backend, not the table filter, decides eligibility and returns per-account reasons.
3. Choose a mode:
   - **Canary**: exactly one selected account. Required for a provider / browser / network combination before Bulk is allowed.
   - **Bulk**: multiple selected accounts. The backend opens this only after matching non-secret canary evidence exists.
4. Choose the browser kernel and an explicit network route (**None**, **Proxy**, or **VPN**). Backfill does not infer or reuse an old proxy/VPN because historical account records do not store reliable network provenance.
5. Start with `start_auto_registration` using the existing-account operation payload; the compatibility `start_two_factor_backfill` command remains available for older integrations. Stop a running task with `cancel_two_factor_backfill`.

### Existing-account autoreg request

The Auto Registration command accepts an operation-tagged existing-account payload. Its request contains selected stored-account keys plus the backfill browser, explicit network, Canary/Bulk mode, and policy acknowledgement fields. Credentials and the historical provider card remain backend-only. This dispatch happens before new-registration CDK validation and does not construct the registration engine, resolve SMS registration tokens, redeem cards, reserve quota, update CDK inventory, or write login/Sub2API credentials.

```json
{
  "config": {
    "operation": "existingAccount",
    "selectedAccountKeys": ["account-key"],
    "browser": "chromium",
    "network": { "kind": "none" },
    "mode": "canary",
    "allowFreeTrialNo": false,
    "acknowledgeLegacyAccess": false
  }
}
```

The normal new-account payload remains unchanged and still requires non-empty CDKs.

### Eligibility defaults


- registration result is successful, email and password are present, inventory `status` is `available`, `twoFaEnabled=false`, `totpSecret` is empty, and the CDK/provider can be resolved;
- `Invalid` is blocked unless the stored structured outcome reason is `free_trial_no` and the operator explicitly opts into that override;
- `Sold`, `Exported`, `Reserved`, local 2FA flag/secret conflicts, unknown-invalid rows, and known-locked rows are blocked;
- legacy `GMAIL-*` and `MAIL-*` CDKs may be migrated once to provider provenance (`gmail.123452026.xyz` or `sms.iosmq.xyz`) by exact prefix inference; unknown prefixes stay manual-review and must not trigger provider network calls;
- legacy rows whose lock state is unknown require explicit acknowledgement and Canary mode. Do not bulk-run historical locked rows or rows previously associated with lockout/abuse signals.

### Semantics and safety

- Backfill may enter through the `start_auto_registration` existing-account operation boundary, but it never enters the new-account registration engine, never creates accounts, never reserves or releases CDK quota, never mutates CDK inventory counts, and never writes to the login/Sub2API credential store.
- Execution is serial (`concurrency=1`) by design. There is no operator-facing concurrency override.
- Progress events contain safe task/account keys, index/total, step/outcome/error code, retryability, and timestamps only. They must not include passwords, raw CDKs, email verification codes, TOTP secrets/codes, access tokens, or raw provider responses.
- Cancellation is cooperative: it is checked before each account and propagated through existing-account authentication and built-in provider OTP polling. The active HTTP request may still run until its request timeout, and browser/profile cleanup always finishes before the terminal `cancelled` event; partial successes remain persisted and later accounts are untouched.
- A timed-out live harness must request cancellation and wait for that terminal event instead of exiting the process. If an older interrupted run left an owned `InProgress` record, the guarded stale-recovery path may CAS-finalize it as `Cancelled` and remove only its verified auto-created worker profile; recovery refuses secret-bearing journals and ownership mismatches.
- A lock discovered while authenticating is persisted as `two_factor_backfill_access_state=locked`, records a failed per-account outcome, and skips to the next selected account. Rate limits, Cloudflare/challenge escalation, repeated 401, and cleanup failures remain batch-level safety pauses.

### Journal, recovery, and reconciliation

The backend stores a private recovery journal while enabling 2FA. The journal captures the pending authenticator secret before submitting the TOTP code, then is marked complete only after the remote Security page is reopened and verified On and the original inventory record is patched atomically.

- If the app crashes or the task is canceled after secret capture, use the journal for operator recovery; do not expose raw secrets in UI events or logs.
- If remote MFA is already On while the local record says Off, backfill returns a reconciliation/manual-review outcome. It must not toggle MFA Off or disable/re-enable authenticator to force a fresh secret.
- If local inventory is Off but the remote account is On, reconcile the local record only from a verified recovery path or manual operator evidence.
- Terminal cleanup must close browser resources, release task/account locks, remove completed handles, and delete completed journals. Failed or incomplete journals stay available for manual recovery.

### Developer notes

- The command surface is `preview_two_factor_backfill`, `start_auto_registration` with `operation=existingAccount`, compatibility `start_two_factor_backfill`, `cancel_two_factor_backfill`, `list_two_factor_backfill_recovery`, and `recover_two_factor_backfill_journal`.
- `list_two_factor_backfill_recovery` returns only secret-free journal summaries. `recover_two_factor_backfill_journal` performs no remote operation: it deletes a journal only when the account already proves the exact completed local state; otherwise it records manual review and releases an owned `InProgress` record to reconciliation-required through the normal revision-checked account patch.
- Provider migration is additive provenance on the existing account record; it is not a quota or registration ledger operation.
- The shared ChatGPT 2FA automation may be reused, but backfill must not call OAuth/Codex PKCE, phone enrollment, registration, or Sub2API login-store collaborators.
- Remote state inspection is fail-closed: only a positive remote-Off signal may proceed to setup. Remote-On/local-Off is reconciliation, and selector ambiguity is a safe failure.
- Keep canary evidence non-secret: workflow/selector version, browser kernel, email provider, and network configuration are enough to gate Bulk without storing account secrets.


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
