# Codex Login Flow Research Report

## Executive Summary

Implementation hiện tại của auto-login feature **ĐÚNG** với OAuth flow chuẩn của OpenAI/Codex. Flow bao gồm:
1. Generate OAuth auth URL với PKCE
2. Browser automation để login
3. Extract callback code/state
4. Exchange code để lấy tokens
5. Create account trong Sub2API

## Research Methodology

- **Nguồn 1**: Sub2API source code (Go backend)
- **Nguồn 2**: OpenAI OAuth implementation trong Sub2API
- **Nguồn 3**: JnmBrowser auto_service::openai::login module (Rust implementation)

## OpenAI OAuth Flow Chi Tiết

### 1. OAuth Configuration

```go
// From: sub2api/backend/internal/pkg/openai/oauth.go
const (
  ClientID       = "app_EMoamEEZ73f0CkXaXp7hrann"  // Codex CLI official
  AuthorizeURL   = "https://auth.openai.com/oauth/authorize"
  TokenURL       = "https://auth.openai.com/oauth/token"
  DefaultRedirectURI = "http://localhost:1455/auth/callback"
  DefaultScopes  = "openid profile email offline_access"
)
```

**Key Points:**
- Client ID: `app_EMoamEEZ73f0CkXaXp7hrann` (Codex CLI official client)
- Redirect URI: `http://localhost:1455/auth/callback`
- PKCE: Code verifier dùng **hex encoding** (không phải base64url), code challenge dùng S256
- Đặc biệt: Có parameter `codex_cli_simplified_flow=true` cho Codex flow

### 2. PKCE Implementation

```go
// GenerateCodeVerifier generates a PKCE code verifier (64 bytes -> hex for OpenAI)
// OpenAI uses hex encoding instead of base64url
func GenerateCodeVerifier() (string, error) {
  bytes, err := GenerateRandomBytes(64)
  if err != nil {
    return nil, err
  }
  return hex.EncodeToString(bytes), nil  // HEX encoding, not base64url!
}

// GenerateCodeChallenge generates a PKCE code challenge using S256 method
func GenerateCodeChallenge(verifier string) string {
  hash := sha256.Sum256([]byte(verifier))
  return base64URLEncode(hash[:])  // Base64url for challenge
}
```

### 3. Authorization URL Structure

```
https://auth.openai.com/oauth/authorize?
  response_type=code&
  client_id=app_EMoamEEZ73f0CkXaXp7hrann&
  redirect_uri=http://localhost:1455/auth/callback&
  scope=openid profile email offline_access&
  state=<random_state>&
  code_challenge=<sha256(code_verifier)>&
  code_challenge_method=S256&
  id_token_add_organizations=true&
  codex_cli_simplified_flow=true  // Đặc biệt cho Codex
```

### 4. Complete OAuth Flow

```
┌─────────────────────────────────────────────────────────────┐
│ Step 1: Generate Auth URL (Sub2API Backend)                 │
│ - Generate state (random 32 bytes hex)                      │
│ - Generate code_verifier (64 bytes hex)                     │
│ - Generate code_challenge (SHA256 + base64url)              │
│ - Store session: {state, code_verifier, redirect_uri}       │
│ - Return: {auth_url, session_id}                            │
└─────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 2: Browser Automation (JnmBrowser)                     │
│ - Launch ephemeral browser profile                          │
│ - Navigate to auth_url                                      │
│ - Fill email → click continue                               │
│ - Fill password → click continue                            │
│ - If 2FA required: generate TOTP → fill → submit            │
│ - If phone verification: rent SMS → fill → poll OTP → verify│
└─────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 3: Extract Callback (JnmBrowser)                       │
│ - Wait for redirect to localhost:1455/auth/callback          │
│ - Extract code and state from URL query params              │
│ - Example: ?code=xyz123&state=abc456                        │
└─────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 4: Exchange Code for Tokens (Sub2API Backend)          │
│ - Validate state matches session                            │
│ - POST to https://auth.openai.com/oauth/token with:         │
│   {                                                         │
│     grant_type: "authorization_code",                       │
│     client_id: "app_EMoamEEZ73f0CkXaXp7hrann",             │
│     code: "<extracted_code>",                               │
│     redirect_uri: "http://localhost:1455/auth/callback",    │
│     code_verifier: "<stored_verifier>"                      │
│   }                                                         │
│ - Receive: {access_token, refresh_token, id_token, expires_in}│
│ - Parse ID token để lấy user info (email, account_id, etc.) │
└─────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 5: Create Account in Sub2API                           │
│ - POST /api/v1/admin/openai/create-from-oauth with:         │
│   {                                                         │
│     session_id: "<session_id>",                             │
│     code: "<code>",                                         │
│     state: "<state>",                                       │
│     redirect_uri: "http://localhost:1455/auth/callback",    │
│     proxy_id: <optional>,                                   │
│     name: "<email>",                                        │
│     concurrency: 3,                                         │
│     priority: 50,                                           │
│     group_ids: [<optional>]                                 │
│   }                                                         │
│ - Sub2API exchanges code (if not done yet)                  │
│ - Creates account with credentials:                         │
│   {                                                         │
│     access_token: "...",                                    │
│     refresh_token: "...",                                   │
│     client_id: "app_EMoamEEZ73f0CkXaXp7hrann",             │
│     expires_at: <timestamp>                                 │
│   }                                                         │
│ - Returns: {id, name, platform, type, credentials, status}  │
└─────────────────────────────────────────────────────────────┘
```

## Comparison: Our Implementation vs Standard Flow

### ✅ Đúng

1. **Generate Auth URL**: Gọi Sub2API `/api/v1/admin/openai/generate-auth-url` → nhận `auth_url` + `session_id`
2. **Browser Automation**: Navigate → email → password → 2FA → phone verification
3. **Extract Callback**: Parse URL để lấy `code` và `state`
4. **Create Account**: Gọi `/api/v1/admin/openai/create-from-oauth` với đúng parameters

### ⚠️ Cần Kiểm Tra

1. **Redirect URI**: Mặc định Sub2API dùng `http://localhost:1455/auth/callback`
   - Browser automation có thể redirect về URL này không?
   - Có cần custom redirect URI không?

2. **State Validation**: Sub2API validate state để chống CSRF
   - Chúng ta có truyền đúng state từ auth URL generation không?

3. **Session Expiry**: Sub2API sessions có TTL 30 phút
   - Batch login có thể mất bao lâu? Có cần refresh session không?

## Sub2API Endpoints

### Endpoint 1: create-from-oauth (OAuth Flow)

```
POST /api/v1/admin/openai/create-from-oauth
Headers: x-api-key: <admin_api_key>
Body: {
  "session_id": "abc123",
  "code": "xyz789",
  "state": "state_value",
  "redirect_uri": "http://localhost:1455/auth/callback",
  "proxy_id": 1,
  "name": "user@example.com",
  "concurrency": 3,
  "priority": 50,
  "group_ids": [1, 2]
}
```

**Response:**
```json
{
  "code": 0,
  "message": "success",
  "data": {
    "id": 123,
    "name": "user@example.com",
    "platform": "openai",
    "type": "oauth",
    "credentials": {
      "access_token": "eyJhbGc...",
      "refresh_token": "rt_xxx",
      "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
      "expires_at": 1234567890
    },
    "status": "active"
  }
}
```

### Endpoint 2: create-from-codex-pat (PAT Flow - KHÔNG DÙNG)

```
POST /api/v1/admin/openai/create-from-codex-pat
Headers: x-api-key: <admin_api_key>
Body: {
  "access_token": "at-xxx...",  // Codex Personal Access Token
  "name": "user@example.com",
  "proxy_id": 1,
  "concurrency": 3,
  "priority": 50,
  "group_ids": [1, 2]
}
```

**Note:** Endpoint này dùng cho Codex PAT (token bắt đầu bằng `at-`), KHÔNG dùng cho OAuth flow.

## Testing Checklist

### Pre-Test
- [ ] Sub2API server đang chạy
- [ ] Admin API key đã cấu hình trong JnmBrowser settings
- [ ] VI-OTP token đã cấu hình (nếu cần phone verification)
- [ ] Browser (Chromium/Camoufox) đã cài đặt

### Test Cases

#### TC1: Single Account Login (Happy Path)
```
Input: user@example.com|password123|JBSWY3DPEHPK3PXP
Expected:
  1. Auth URL generated
  2. Browser navigates to auth URL
  3. Email filled and submitted
  4. Password filled and submitted
  5. 2FA code generated and submitted
  6. Redirect to localhost captured
  7. Account created in Sub2API
  8. Login result saved with success=true
```

#### TC2: Account with Phone Verification
```
Input: user@example.com|password123|
Expected:
  1. Auth URL generated
  2. Browser navigates to auth URL
  3. Email filled and submitted
  4. Password filled and submitted
  5. Phone verification page detected
  6. SMS number rented from VI-OTP
  7. Phone number filled and submitted
  8. OTP polled and filled
  9. Redirect to localhost captured
  10. Account created in Sub2API
```

#### TC3: Batch Login (Multiple Accounts)
```
Input:
  user1@example.com|pass1|SECRET1
  user2@example.com|pass2|SECRET2
  user3@example.com|pass3|SECRET3
Expected:
  1. 3 auth URLs generated (sequential)
  2. 3 browser sessions (one at a time)
  3. Each account logged in successfully
  4. 3 accounts created in Sub2API
  5. All results saved
```

#### TC4: Invalid Credentials
```
Input: user@example.com|wrong_password|SECRET
Expected:
  1. Auth URL generated
  2. Browser navigates to auth URL
  3. Email filled and submitted
  4. Password filled and submitted
  5. Error page detected (invalid credentials)
  6. Login result saved with success=false
  7. Retry logic triggered (max_retries=3)
```

#### TC5: 2FA Required but Not Provided
```
Input: user@example.com|password123|
Expected:
  1. Auth URL generated
  2. Browser navigates to auth URL
  3. Email filled and submitted
  4. Password filled and submitted
  5. 2FA page detected
  6. Error: "2FA required but no TOTP secret provided"
  7. Login result saved with success=false
```

## Potential Issues & Solutions

### Issue 1: Redirect URI Mismatch

**Problem:** Sub2API expects redirect to `http://localhost:1455/auth/callback`, but browser might redirect elsewhere.

**Solution:**
```rust
// In sub2api.rs, pass redirect_uri when generating auth URL
pub async fn generate_auth_url(
  &self,
  proxy_id: Option<i64>,
  redirect_uri: Option<String>,  // Add this parameter
) -> Result<AuthUrlResponse, String> {
  let url = format!("{}/api/v1/admin/openai/generate-auth-url", self.base_url);
  let body = serde_json::json!({
    "proxy_id": proxy_id,
    "redirect_uri": redirect_uri.unwrap_or_else(|| "http://localhost:1455/auth/callback".to_string())
  });
  // ...
}
```

### Issue 2: Session Expiry During Batch

**Problem:** Sub2API sessions expire after 30 minutes. If batch takes longer, session becomes invalid.

**Solution:**
```rust
// In engine.rs, generate auth URL for each account (not once for batch)
for (idx, credential) in credentials.iter().enumerate() {
  // Generate fresh auth URL for each account
  let auth_resp = self.sub2api_client.generate_auth_url(proxy_id, None).await?;
  // ... proceed with login
}
```

### Issue 3: State Validation

**Problem:** Sub2API validates that `state` in callback matches `state` in session. If we modify URL, state might not match.

**Solution:**
```rust
// In engine.rs, preserve original state from auth URL
let auth_url = auth_resp.auth_url.clone();
let original_state = extract_state_from_url(&auth_url);

// After redirect, extract state from callback URL
let callback_state = extract_state_from_url(&callback_url);

// Pass original state to create_from_oauth
let create_req = CreateFromOAuthRequest {
  session_id: auth_resp.session_id,
  code: callback_code,
  state: original_state,  // Use original state, not callback state
  // ...
};
```

## Conclusion

Implementation hiện tại **ĐÚNG** với OAuth flow chuẩn. Các điểm cần lưu ý:

1. ✅ Flow đúng: generate-auth-url → browser login → extract callback → create-from-oauth
2. ✅ PKCE handled bởi Sub2API backend (không cần implement ở client)
3. ✅ State validation handled bởi Sub2API
4. ⚠️ Cần test kỹ redirect URI và state preservation
5. ⚠️ Cần handle session expiry cho batch login

## Next Steps

1. **Manual Testing**: Test với 1 account thực để verify flow
2. **Recording**: Nếu có vấn đề, anh recording lại quá trình login để debug
3. **Batch Testing**: Test với 3-5 accounts để verify batch logic
4. **Error Handling**: Test các trường hợp lỗi (wrong password, 2FA required, etc.)

## References

- Sub2API OAuth Implementation: `backend/internal/pkg/openai/oauth.go`
- Sub2API OAuth Service: `backend/internal/service/openai_oauth_service.go`
- Sub2API OAuth Handler: `backend/internal/handler/admin/openai_oauth_handler.go`
- JnmBrowser Auto Login: `src-tauri/src/auto_service/openai/login/`
