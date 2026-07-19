use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Sub2API HTTP client for admin operations.
pub struct Sub2ApiClient {
  base_url: String,
  api_key: String,
  client: reqwest::Client,
}

/// Response from generate-auth-url endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AuthUrlResponse {
  pub auth_url: String,
  pub session_id: String,
  pub state: String,
  #[serde(default)]
  pub redirect_uri: String,
}

/// Request for create-from-oauth endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateFromOAuthRequest {
  pub session_id: String,
  pub code: String,
  pub state: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub redirect_uri: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub proxy_id: Option<i64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub concurrency: Option<i32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub priority: Option<i32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub group_ids: Option<Vec<i64>>,
}

/// Response from exchange-code endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TokenInfo {
  pub access_token: String,
  #[serde(default)]
  pub refresh_token: String,
  #[serde(default)]
  pub id_token: String,
  #[serde(default)]
  pub token_type: String,
  #[serde(default)]
  pub expires_in: Option<i64>,
  #[serde(default)]
  pub email: String,
  #[serde(default)]
  pub chatgpt_account_id: String,
  #[serde(default)]
  pub chatgpt_user_id: String,
  #[serde(default)]
  pub plan_type: String,
}

/// Account created in Sub2API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Sub2ApiAccount {
  pub id: i64,
  pub name: String,
  pub platform: String,
  #[serde(alias = "type")]
  pub account_type: String,
  #[serde(default)]
  pub credentials: serde_json::Value,
  #[serde(default)]
  pub extra: serde_json::Value,
  #[serde(default)]
  pub status: String,
}

/// Generic API response wrapper from Sub2API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse<T> {
  pub code: i32,
  #[serde(default)]
  pub message: String,
  pub data: Option<T>,
}

/// Request for importing Codex session tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CodexSessionImportRequest {
  pub content: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub notes: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub group_ids: Option<Vec<i64>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub proxy_id: Option<i64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub concurrency: Option<i32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub priority: Option<i32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub update_existing: Option<bool>,
}

/// Result of Codex session import.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexImportResult {
  pub total: i32,
  pub created: i32,
  pub updated: i32,
  pub skipped: i32,
  pub failed: i32,
  #[serde(default)]
  pub items: Vec<serde_json::Value>,
  #[serde(default)]
  pub warnings: Vec<serde_json::Value>,
  #[serde(default)]
  pub errors: Vec<serde_json::Value>,
}

impl Sub2ApiClient {
  pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
    let base_url = base_url.into().trim_end_matches('/').to_string();
    let client = reqwest::Client::builder()
      .timeout(Duration::from_secs(30))
      .build()
      .expect("Failed to build reqwest client");
    Self {
      base_url,
      api_key: api_key.into(),
      client,
    }
  }

  fn auth_header(&self) -> String {
    self.api_key.clone()
  }

  /// POST /api/v1/admin/openai/generate-auth-url
  pub async fn generate_auth_url(&self, proxy_id: Option<i64>) -> Result<AuthUrlResponse, String> {
    let url = format!("{}/api/v1/admin/openai/generate-auth-url", self.base_url);
    let body = serde_json::json!({
      "proxy_id": proxy_id,
    });

    let resp = self
      .client
      .post(&url)
      .header("x-api-key", self.auth_header())
      .header("Content-Type", "application/json")
      .json(&body)
      .send()
      .await
      .map_err(|e| format!("Failed to generate auth URL: {e}"))?;

    let status = resp.status();
    let text = resp
      .text()
      .await
      .map_err(|e| format!("Failed to read response: {e}"))?;

    if !status.is_success() {
      return Err(format!(
        "Generate auth URL failed (HTTP {}): {}",
        status, text
      ));
    }

    let api_resp: ApiResponse<AuthUrlResponse> = serde_json::from_str(&text)
      .map_err(|e| format!("Failed to parse response: {e} — body: {text}"))?;

    if api_resp.code != 0 && api_resp.code != 200 {
      return Err(format!(
        "Generate auth URL error (code {}): {}",
        api_resp.code, api_resp.message
      ));
    }

    api_resp
      .data
      .ok_or_else(|| "Generate auth URL returned no data".to_string())
  }

  /// POST /api/v1/admin/openai/create-from-oauth
  pub async fn create_from_oauth(
    &self,
    req: CreateFromOAuthRequest,
  ) -> Result<Sub2ApiAccount, String> {
    let url = format!("{}/api/v1/admin/openai/create-from-oauth", self.base_url);

    let resp = self
      .client
      .post(&url)
      .header("x-api-key", self.auth_header())
      .header("Content-Type", "application/json")
      .json(&req)
      .send()
      .await
      .map_err(|e| format!("Failed to create account from OAuth: {e}"))?;

    let status = resp.status();
    let text = resp
      .text()
      .await
      .map_err(|e| format!("Failed to read response: {e}"))?;

    if !status.is_success() {
      return Err(format!(
        "Create from OAuth failed (HTTP {}): {}",
        status, text
      ));
    }

    let api_resp: ApiResponse<Sub2ApiAccount> = serde_json::from_str(&text)
      .map_err(|e| format!("Failed to parse response: {e} — body: {text}"))?;

    if api_resp.code != 0 && api_resp.code != 200 {
      return Err(format!(
        "Create from OAuth error (code {}): {}",
        api_resp.code, api_resp.message
      ));
    }

    api_resp
      .data
      .ok_or_else(|| "Create from OAuth returned no data".to_string())
  }

  /// POST /api/v1/admin/openai/exchange-code
  pub async fn exchange_code(
    &self,
    session_id: &str,
    code: &str,
    state: &str,
    redirect_uri: Option<&str>,
    proxy_id: Option<i64>,
  ) -> Result<TokenInfo, String> {
    let url = format!("{}/api/v1/admin/openai/exchange-code", self.base_url);
    let body = serde_json::json!({
      "session_id": session_id,
      "code": code,
      "state": state,
      "redirect_uri": redirect_uri,
      "proxy_id": proxy_id,
    });

    let resp = self
      .client
      .post(&url)
      .header("x-api-key", self.auth_header())
      .header("Content-Type", "application/json")
      .json(&body)
      .send()
      .await
      .map_err(|e| format!("Failed to exchange code: {e}"))?;

    let status = resp.status();
    let text = resp
      .text()
      .await
      .map_err(|e| format!("Failed to read response: {e}"))?;

    if !status.is_success() {
      return Err(format!("Exchange code failed (HTTP {}): {}", status, text));
    }

    let api_resp: ApiResponse<TokenInfo> = serde_json::from_str(&text)
      .map_err(|e| format!("Failed to parse response: {e} — body: {text}"))?;

    if api_resp.code != 0 && api_resp.code != 200 {
      return Err(format!(
        "Exchange code error (code {}): {}",
        api_resp.code, api_resp.message
      ));
    }

    api_resp
      .data
      .ok_or_else(|| "Exchange code returned no data".to_string())
  }

  /// POST /api/v1/admin/accounts/import/codex-session
  /// Alternative path: import access_token directly as Codex session.
  pub async fn import_codex_session(
    &self,
    req: CodexSessionImportRequest,
  ) -> Result<CodexImportResult, String> {
    let url = format!(
      "{}/api/v1/admin/accounts/import/codex-session",
      self.base_url
    );

    let resp = self
      .client
      .post(&url)
      .header("x-api-key", self.auth_header())
      .header("Content-Type", "application/json")
      .json(&req)
      .send()
      .await
      .map_err(|e| format!("Failed to import Codex session: {e}"))?;

    let status = resp.status();
    let text = resp
      .text()
      .await
      .map_err(|e| format!("Failed to read response: {e}"))?;

    if !status.is_success() {
      return Err(format!(
        "Import Codex session failed (HTTP {}): {}",
        status, text
      ));
    }

    let api_resp: ApiResponse<CodexImportResult> = serde_json::from_str(&text)
      .map_err(|e| format!("Failed to parse response: {e} — body: {text}"))?;

    if api_resp.code != 0 && api_resp.code != 200 {
      return Err(format!(
        "Import Codex session error (code {}): {}",
        api_resp.code, api_resp.message
      ));
    }

    api_resp
      .data
      .ok_or_else(|| "Import Codex session returned no data".to_string())
  }

  /// Import OAuth tokens as OpenAI account
  /// Uses the same endpoint as codex-session import but with OAuth token format
  pub async fn import_oauth_tokens(
    &self,
    access_token: &str,
    refresh_token: &str,
    email: &str,
    proxy_id: Option<i64>,
    group_ids: Option<Vec<i64>>,
    name: Option<&str>,
  ) -> Result<CodexImportResult, String> {
    // Build token content in Codex session format
    let token_content = serde_json::json!({
      "access_token": access_token,
      "refresh_token": refresh_token,
      "email": email,
    });

    let req = CodexSessionImportRequest {
      content: serde_json::to_string(&token_content)
        .map_err(|e| format!("Failed to serialize token content: {}", e))?,
      name: name.map(|s| s.to_string()),
      notes: Some("Imported from JnmBrowser auto-login".to_string()),
      group_ids,
      proxy_id,
      concurrency: Some(10),
      priority: Some(1),
      update_existing: Some(true),
    };

    self.import_codex_session(req).await
  }

  /// Extract code and state from a callback URL.
  pub fn parse_callback_url(url: &str) -> Option<(String, String)> {
    // URL format: http://localhost:1455/callback?code=...&state=...
    // or: http://localhost:1455/?code=...&state=...
    let url_lower = url.to_lowercase();
    if !url_lower.contains("localhost") && !url_lower.contains("127.0.0.1") {
      return None;
    }

    // Parse query parameters
    let query_start = url.find('?')?;
    let query = &url[query_start + 1..];

    let mut code = None;
    let mut state = None;

    for param in query.split('&') {
      let mut parts = param.splitn(2, '=');
      let key = parts.next().unwrap_or("");
      let value = parts.next().unwrap_or("");

      match key.to_lowercase().as_str() {
        "code" => code = Some(urldecode(value)),
        "state" => state = Some(urldecode(value)),
        _ => {}
      }
    }

    match (code, state) {
      (Some(c), Some(s)) if !c.is_empty() => Some((c, s)),
      _ => None,
    }
  }
}

/// Simple URL decoding.
fn urldecode(s: &str) -> String {
  let mut result = String::with_capacity(s.len());
  let mut chars = s.chars();
  while let Some(c) = chars.next() {
    match c {
      '%' => {
        let hex: String = chars.by_ref().take(2).collect();
        if hex.len() == 2 {
          if let Ok(byte) = u8::from_str_radix(&hex, 16) {
            result.push(byte as char);
          } else {
            result.push('%');
            result.push_str(&hex);
          }
        } else {
          result.push('%');
          result.push_str(&hex);
        }
      }
      '+' => result.push(' '),
      _ => result.push(c),
    }
  }
  result
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_callback_url_full() {
    let url = "http://localhost:1455/callback?code=abc123&state=xyz789";
    let (code, state) = Sub2ApiClient::parse_callback_url(url).unwrap();
    assert_eq!(code, "abc123");
    assert_eq!(state, "xyz789");
  }

  #[test]
  fn parse_callback_url_auth_path() {
    // Real OpenAI Codex redirect URI path.
    let url = "http://localhost:1455/auth/callback?code=abc123&state=xyz789";
    let (code, state) = Sub2ApiClient::parse_callback_url(url).unwrap();
    assert_eq!(code, "abc123");
    assert_eq!(state, "xyz789");
  }

  #[test]
  fn parse_callback_url_127_0_0_1() {
    let url = "http://127.0.0.1:1455/auth/callback?code=abc123&state=xyz789";
    let (code, state) = Sub2ApiClient::parse_callback_url(url).unwrap();
    assert_eq!(code, "abc123");
    assert_eq!(state, "xyz789");
  }

  #[test]
  fn parse_callback_url_encoded() {
    let url = "http://localhost:1455/?code=abc%2B123&state=xyz%3D789";
    let (code, state) = Sub2ApiClient::parse_callback_url(url).unwrap();
    assert_eq!(code, "abc+123");
    assert_eq!(state, "xyz=789");
  }

  #[test]
  fn parse_callback_url_no_localhost() {
    let url = "https://example.com/callback?code=abc123&state=xyz789";
    assert!(Sub2ApiClient::parse_callback_url(url).is_none());
  }

  #[test]
  fn parse_callback_url_missing_code() {
    let url = "http://localhost:1455/callback?state=xyz789";
    assert!(Sub2ApiClient::parse_callback_url(url).is_none());
  }

  #[test]
  fn parse_callback_url_no_query() {
    let url = "http://localhost:1455/callback";
    assert!(Sub2ApiClient::parse_callback_url(url).is_none());
  }

  #[test]
  fn urldecode_basic() {
    assert_eq!(urldecode("hello%20world"), "hello world");
    assert_eq!(urldecode("hello+world"), "hello world");
    assert_eq!(urldecode("abc%2B123"), "abc+123");
  }
}
