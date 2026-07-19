use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngExt;
use sha2::{Digest, Sha256};

/// PKCE (Proof Key for Code Exchange) implementation for OpenAI OAuth
/// Follows RFC 7636 specification
/// Encode bytes to hex string
fn bytes_to_hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Generate a random state parameter (32 bytes hex encoded)
pub fn generate_state() -> String {
  let mut rng = rand::rng();
  let state: Vec<u8> = (0..32).map(|_| rng.random::<u8>()).collect();
  bytes_to_hex(&state)
}

/// Generate a PKCE code verifier (64 bytes hex encoded for OpenAI)
/// OpenAI uses hex encoding instead of base64url
pub fn generate_code_verifier() -> String {
  let mut rng = rand::rng();
  let verifier: Vec<u8> = (0..64).map(|_| rng.random::<u8>()).collect();
  bytes_to_hex(&verifier)
}

/// Generate a PKCE code challenge from verifier using S256 method
/// Challenge is base64url encoded SHA256 hash of the verifier
pub fn generate_code_challenge(verifier: &str) -> String {
  let mut hasher = Sha256::new();
  hasher.update(verifier.as_bytes());
  let hash = hasher.finalize();
  URL_SAFE_NO_PAD.encode(hash)
}

/// Build OpenAI OAuth authorization URL
pub fn build_authorization_url(
  state: &str,
  code_challenge: &str,
  redirect_uri: &str,
  client_id: &str,
) -> String {
  let params = vec![
    ("response_type", "code"),
    ("client_id", client_id),
    ("redirect_uri", redirect_uri),
    ("scope", "openid profile email offline_access"),
    ("state", state),
    ("code_challenge", code_challenge),
    ("code_challenge_method", "S256"),
    ("id_token_add_organizations", "true"),
    ("codex_cli_simplified_flow", "true"),
  ];

  let query_string: String = params
    .iter()
    .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
    .collect::<Vec<_>>()
    .join("&");

  format!("https://auth.openai.com/oauth/authorize?{}", query_string)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_generate_state() {
    let state = generate_state();
    assert_eq!(state.len(), 64); // 32 bytes hex = 64 chars
    assert!(state.chars().all(|c| c.is_ascii_hexdigit()));
  }

  #[test]
  fn test_generate_code_verifier() {
    let verifier = generate_code_verifier();
    assert_eq!(verifier.len(), 128); // 64 bytes hex = 128 chars
    assert!(verifier.chars().all(|c| c.is_ascii_hexdigit()));
  }

  #[test]
  fn test_generate_code_challenge() {
    let verifier = "test_verifier_123";
    let challenge = generate_code_challenge(verifier);

    // Should be base64url encoded (no padding)
    assert!(!challenge.contains('='));
    assert!(!challenge.contains('+'));
    assert!(!challenge.contains('/'));

    // Should be deterministic
    let challenge2 = generate_code_challenge(verifier);
    assert_eq!(challenge, challenge2);
  }

  #[test]
  fn test_build_authorization_url() {
    let url = build_authorization_url(
      "test_state",
      "test_challenge",
      "http://localhost:1455/auth/callback",
      "app_EMoamEEZ73f0CkXaXp7hrann",
    );

    assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
    assert!(url.contains("response_type=code"));
    assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
    assert!(url.contains("state=test_state"));
    assert!(url.contains("code_challenge=test_challenge"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("codex_cli_simplified_flow=true"));
  }
}
