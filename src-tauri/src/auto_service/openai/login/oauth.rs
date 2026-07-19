use serde::{Deserialize, Serialize};
use std::time::Duration;

const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// Token response from OpenAI OAuth
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
  pub access_token: String,
  pub token_type: String,
  pub expires_in: u64,
  #[serde(default)]
  pub refresh_token: String,
  #[serde(default)]
  pub scope: String,
  #[serde(default)]
  pub id_token: String,
}

/// Exchange authorization code for tokens
pub async fn exchange_code(code: &str, code_verifier: &str) -> Result<TokenResponse, String> {
  let client = reqwest::Client::builder()
    .timeout(Duration::from_secs(30))
    .build()
    .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

  let params = [
    ("grant_type", "authorization_code"),
    ("code", code),
    ("redirect_uri", REDIRECT_URI),
    ("client_id", CLIENT_ID),
    ("code_verifier", code_verifier),
  ];

  let form_body: String = params
    .iter()
    .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
    .collect::<Vec<_>>()
    .join("&");

  let response = client
    .post(TOKEN_URL)
    .header("Content-Type", "application/x-www-form-urlencoded")
    .body(form_body)
    .send()
    .await
    .map_err(|e| format!("Token exchange request failed: {}", e))?;

  let status = response.status();
  let body = response
    .text()
    .await
    .map_err(|e| format!("Failed to read response body: {}", e))?;

  if !status.is_success() {
    return Err(format!("Token exchange failed (HTTP {}): {}", status, body));
  }

  let token_response: TokenResponse = serde_json::from_str(&body)
    .map_err(|e| format!("Failed to parse token response: {} - body: {}", e, body))?;

  Ok(token_response)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_token_response_deserialize() {
    let json = r#"{
      "access_token": "eyJhbGc...",
      "token_type": "Bearer",
      "expires_in": 3600,
      "refresh_token": "rt_...",
      "scope": "openid profile email",
      "id_token": "eyJhbGc..."
    }"#;

    let response: TokenResponse = serde_json::from_str(json).unwrap();
    assert_eq!(response.token_type, "Bearer");
    assert_eq!(response.expires_in, 3600);
  }
}
