# ChatGPT Auto-Registration

Automatically create ChatGPT accounts using Gmail CDK codes, browser automation, and anti-detect fingerprinting.

## Overview

The auto-registration feature automates the entire ChatGPT signup flow:

1. **CDK Redemption**: Redeems a Gmail CDK code to obtain a disposable Gmail address
2. **Alias Generation**: Creates `user+{random}@gmail.com` aliases (up to 6 per CDK)
3. **Browser Automation**: Launches a Chromium/Camoufox profile with proxy + fingerprint
4. **Registration Flow**: Automates the ChatGPT signup via CDP (Chrome DevTools Protocol)
5. **OTP Retrieval**: Polls the Gmail CDK API for the verification code
6. **Token Extraction**: Extracts access token, session token, and account credentials
7. **Credential Storage**: Persists results as JSON files

## Prerequisites

- A valid Gmail CDK code (format: `GMAIL-XXXX-XXXX-XXXX-XXXX`)
- Chromium or Camoufox browser installed (via JnmBrowser's downloader)
- Optional: proxy configured for geo-matching

## Usage

### Via UI

1. Open JnmBrowser
2. Click the **Auto Registration** button in the header
3. Enter your CDK code
4. Configure browser type, proxy (optional), retries, and accounts per CDK
5. Click **Start Registration**

Progress is shown in real-time with step-by-step logs.

### Via Tauri Commands

```typescript
import { invoke } from "@tauri-apps/api/core";

// Start registration
const taskId = await invoke("start_auto_registration", {
  config: {
    cdk: "GMAIL-K4L5-EUW5-PHBV-A6KW",
    browserType: "chromium",
    proxyId: null,
    maxRetries: 3,
    accountsPerCdk: 1,
    headless: false,
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
    "cdk": "GMAIL-XXXX-XXXX-XXXX-XXXX",
    "browserType": "chromium",
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
                                     ├── CDP WebSocket (automation)
                                     └── CredentialStore (JSON persistence)
```

### Registration Flow

```
CDK Input → Redeem CDK → Generate Alias → Generate User Info
→ Launch Browser → Visit chatgpt.com → CSRF Token → Submit Email
→ Follow Authorize → Register User → Request OTP → Poll OTP
→ Verify OTP → Create Account → Extract Tokens → Enable 2FA
```

## Credential Storage

Credentials are stored at:
```
{data_dir}/registered_accounts/{account_id}.json
```

Each file contains:
- Email and password
- Access token and session token
- Account ID and workspace ID
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
- The engine automatically retries with a fresh browser session
- Consider using a different proxy or fingerprint

## 2FA Enablement (Future)

The 2FA enablement step navigates to Settings → Privacy → Enable Two-Factor Authentication.
This is implemented via action recording replay. Record the flow once, then replay for each registration.
