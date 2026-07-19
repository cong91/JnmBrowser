# Auto-Login ChatGPT/Codex + Sub2API Integration

## Mục tiêu
Batch login existing OpenAI accounts (format: `ACCOUNT|PASSWORD|2FA`) qua OAuth flow, extract tokens, và import vào sub2api.

## Flow tổng thể

```
Input: batch credentials (một dòng = ACCOUNT|PASSWORD|2FA)
  ↓
1. Sub2API: POST /admin/openai/generate-auth-url → auth_url + session_id
  ↓
2. Browser automation:
   - Navigate tới auth_url
   - Nhập email → password → 2FA (TOTP)
   - Phone verification (VI-OTP VINAPHONE nếu cần)
   - Bắt localhost redirect → extract code + state
  ↓
3. Sub2API: POST /admin/openai/create-from-oauth {session_id, code, state}
  ↓
4. Lưu kết quả: local store + optional push sub2api
```

## Kiến trúc module mới

### Rust: `src-tauri/src/auto_login/`

| File | Nội dung |
|------|----------|
| `mod.rs` | Module exports |
| `types.rs` | LoginConfig, LoginCredential, LoginStep (14 variants), LoginResult, LoginProgress |
| `engine.rs` | Login engine: browser automation + sub2api calls (tái dụng BrowserSession pattern từ auto_register) |
| `sub2api.rs` | HTTP client cho sub2api admin API (generate-auth-url, create-from-oauth, exchange-code, import-codex-session) |
| `commands.rs` | Tauri commands: start_auto_login, cancel_login, list_login_results_cmd, delete_login_result_cmd |
| `store.rs` | JSON file store (data_dir/login_results/) |
| `task.rs` | Task registry với cancel flag |

### Frontend

| File | Nội dung |
|------|----------|
| `src/components/account-login-dialog.tsx` | Dialog 3 tab: Login / Progress / Stored |
| `src/hooks/use-login-events.ts` | Event listener cho "login-progress", state management |

### i18n
- Thêm namespace `autoLogin.*` vào tất cả 7 locale files

### Settings
- Lưu sub2api_url và sub2api_api_key (encrypted) trong SettingsManager

## Chi tiết từng Task

### Task 1: Types (`auto_login/types.rs`)
- `LoginConfig`: credentials vec, browser_type, max_retries, headless, concurrency, sub2api_url/api_key/proxy_id/group_ids, push_to_sub2api, sms_* fields, network_mode
- `LoginCredential`: email, password, totp_secret (parse từ "A|P|2FA")
- `LoginStep`: Idle → GeneratingAuthUrl → LaunchingBrowser → EnteringEmail → EnteringPassword → Entering2FA → RequestingSmsOtp → PollingSmsOtp → VerifyingSmsOtp → ExtractingCallback → ExchangingCode → CreatingAccount → Completed → Failed
- `LoginResult`: success, email, account_id, access_token, refresh_token, sub2api_account_id, error_message, step_logs, created_at, phone_number
- `LoginProgress`: task_id, credential_index, total_credentials, step, message, timestamp, result

### Task 2: Sub2API Client (`auto_login/sub2api.rs`)
- `Sub2ApiClient { base_url, api_key, client }`
- `generate_auth_url()` → POST /admin/openai/generate-auth-url → {auth_url, session_id, state}
- `create_from_oauth()` → POST /admin/openai/create-from-oauth → Sub2ApiAccount
- `exchange_code()` → POST /admin/openai/exchange-code → TokenInfo
- `import_codex_session()` → POST /admin/accounts/import/codex-session (alternative path)
- Handle errors, 30s timeout, never log secrets

### Task 3: Login Engine (`auto_login/engine.rs`) — LARGEST TASK
- Tái dụng BrowserSession pattern (CDP + Camoufox dual-kernel)
- Tái dụng human_type(), fill_input(), click_selector(), TOTP generation
- **Single login flow:**
  1. GenerateAuthUrl: gọi sub2api → auth_url + session_id
  2. LaunchBrowser: ephemeral profile
  3. Navigate → auth_url
  4. EnterEmail: detect email input → fill → submit
  5. EnterPassword: detect password input → fill → submit
  6. Enter2FA: generate TOTP → fill → submit
  7. Phone (nếu add-phone page): rent SMS → fill phone → poll OTP → verify
  8. ExtractCallback: monitor URL cho localhost redirect → parse code + state
  9. ExchangeCode: gọi sub2api create-from-oauth
  10. Save result
- **Page detection** (URL-based): identifier/login, password, mfa/totp/2fa, add-phone, consent, callback/localhost
- **One phone per account**: track used phone numbers, reject reuse
- **Callback extraction**: Poll current_url() in loop, detect localhost or code= param

### Task 4: Store + Commands
- `store.rs`: JSON file store trong data_dir/login_results/, CRUD ops
- `commands.rs`: 5 Tauri commands (start, cancel, list, delete, export)
- Register trong `lib.rs`

### Task 5: Module Registration
- `auto_login/mod.rs`: pub mod all submodules
- `lib.rs`: pub mod auto_login + register commands trong invoke_handler

### Task 6: Frontend Hook (`use-login-events.ts`)
- Mirror use-registration-events.ts pattern
- Listen "login-progress" event
- State: progressMap, accounts, loading
- Functions: startLogin, cancelLogin, refreshLoginResults, deleteLoginResult

### Task 7: Frontend Dialog (`account-login-dialog.tsx`)
- 3 tabs: Login | Progress | Stored
- Login tab: credentials textarea, browser type, sub2api settings (URL + API key + push toggle), SMS settings, network mode, start button
- Progress tab: per-credential cards
- Stored tab: results table với export/delete

### Task 8: Frontend Wiring
- `page.tsx`: add autoLoginDialogOpen state + render dialog
- `home-header.tsx`: add "ChatGPT/Codex Auto Login" menu item

### Task 9: i18n (7 locales)
- Namespace `autoLogin.*`: ~30 keys cho dialog, steps, errors, buttons

### Task 10: Settings Integration
- SettingsManager: sub2api_url, sub2api_api_key (encrypted)
- Tauri commands: get_sub2api_settings, set_sub2api_settings

## Task Dependencies & Waves

```
Wave 1: Task 1 (types)
Wave 2: Task 2 (sub2api client)
Wave 3: Task 3 (engine) — largest
Wave 4: Tasks 4-5 (store, commands, module reg)
Wave 5: Tasks 6-8 (frontend hook, dialog, wiring)
Wave 6: Tasks 9-10 (i18n, settings) — can parallel Wave 5
Wave 7: Verify (format + lint + test)
```

## Reuse từ codebase hiện tại
- `BrowserSession` pattern từ `auto_register/engine.rs` (CDP + Camoufox)
- `SmsService` trait + `ViotpService` từ `sms/`
- TOTP generation từ `auto_register/totp.rs`
- `human_typing.rs` cho human-like input
- `browser_actions` cho human clicks
- `profile` module cho ephemeral browser profiles
- `settings_manager.rs` cho encrypted token storage
- Store pattern từ `auto_register/store.rs`

## Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Module | New `auto_login/` (không extend `auto_register/`) | Flow khác: login existing vs create new |
| Sub2API auth | Admin API key (x-api-key header) | Đơn giản nhất |
| Phone | Reuse SmsService + VI-OTP | Đã có sẵn |
| 2FA | Reuse TOTP từ auto_register | Đã implement |
| Event | "login-progress" (tách biệt) | Không conflict với registration |
| Format | ACCOUNT\|PASSWORD\|2FA | User specified |

## Done Criteria
- [ ] Batch login ACCOUNT|PASSWORD|2FA hoạt động
- [ ] OAuth flow qua sub2api thành công
- [ ] 2FA TOTP generation đúng
- [ ] Phone verification via VI-OTP khi cần
- [ ] 1 phone = 1 account
- [ ] Local store + export
- [ ] Push sub2api tạo account
- [ ] Real-time progress events
- [ ] Cancel support
- [ ] 7 locales translated
- [ ] format + lint + test green