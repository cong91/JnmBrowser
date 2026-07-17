use rand::Rng;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{EmailInfo, EmailService, EmailServiceError, VerificationCodeResponse};

const API_BASE: &str = "http://gmail.123452026.xyz/api";
const POLL_INTERVAL_SECS: u64 = 3;
const MAX_ALIASES_PER_EMAIL: usize = 6;

/// Gmail CDK email service.
/// Redeems CDK codes for Gmail addresses and polls for verification codes.
/// Uses `reqwest` (async) internally — calls are bridged via `tokio::runtime::Handle`.
pub struct GmailCdkService {
  /// Tracks used aliases per base email to stay within the 6-alias budget.
  used_aliases: Mutex<Vec<(String, HashSet<String>)>>,
}

impl GmailCdkService {
  pub fn new() -> Self {
    Self {
      used_aliases: Mutex::new(Vec::new()),
    }
  }

  /// Helper to run an async future safely from sync trait methods.
  ///
  /// Production auto-register runs these methods on a dedicated blocking
  /// runtime thread. Nested `Handle::block_on` panics when already inside a
  /// runtime worker, so we either:
  /// - enter a fresh runtime when no handle is active, or
  /// - offload onto `spawn_blocking` + `block_on` when one already is.
  fn block_on<F, T>(future: F) -> T
  where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
  {
    match tokio::runtime::Handle::try_current() {
      Ok(handle) => {
        // Already inside a runtime — never call block_on on the same worker.
        std::thread::scope(|s| {
          s.spawn(move || handle.block_on(future))
            .join()
            .expect("gmail cdk nested runtime thread panicked")
        })
      }
      Err(_) => {
        let rt = tokio::runtime::Builder::new_current_thread()
          .enable_all()
          .build()
          .expect("Failed to create gmail cdk runtime");
        rt.block_on(future)
      }
    }
  }

  /// Build a reqwest client with default timeout.
  fn client() -> reqwest::Client {
    reqwest::Client::builder()
      .timeout(Duration::from_secs(30))
      .build()
      .expect("Failed to build reqwest Client")
  }

  /// Extract the username portion from a Gmail address.
  fn extract_username(email: &str) -> &str {
    email.split('@').next().unwrap_or(email)
  }

  /// Check if an alias is already used for a given base email.
  fn is_alias_used(&self, base_email: &str, alias: &str) -> bool {
    let guard = self.used_aliases.lock().unwrap();
    for (email, aliases) in guard.iter() {
      if email == base_email {
        return aliases.contains(alias);
      }
    }
    false
  }

  /// Register an alias as used for a base email.
  fn mark_alias_used(&self, base_email: &str, alias: &str) {
    let mut guard = self.used_aliases.lock().unwrap();
    for (email, aliases) in guard.iter_mut() {
      if email == base_email {
        aliases.insert(alias.to_string());
        return;
      }
    }
    let mut set = HashSet::new();
    set.insert(alias.to_string());
    guard.push((base_email.to_string(), set));
  }

  /// Count used aliases for a base email.
  fn used_alias_count(&self, base_email: &str) -> usize {
    let guard = self.used_aliases.lock().unwrap();
    guard
      .iter()
      .find(|(e, _)| e == base_email)
      .map(|(_, aliases)| aliases.len())
      .unwrap_or(0)
  }
}

impl EmailService for GmailCdkService {
  fn redeem_cdk(&self, cdk: &str) -> Result<EmailInfo, EmailServiceError> {
    let url = format!("{API_BASE}/mailbox/redeem");
    let body = serde_json::json!({ "cdk": cdk });

    let client = Self::client();
    let response = Self::block_on(async move {
      client
        .post(&url)
        .header("accept", "*/*")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    })
    .map_err(|e| EmailServiceError::Network(format!("redeem request failed: {e}")))?;

    let status_code = response.status();
    let text = Self::block_on(async move { response.text().await })
      .map_err(|e| EmailServiceError::Network(format!("failed to read redeem response: {e}")))?;

    if !status_code.is_success() {
      return Err(EmailServiceError::Network(format!(
        "redeem HTTP {status_code}: {text}"
      )));
    }

    let data: serde_json::Value = serde_json::from_str(&text)
      .map_err(|e| EmailServiceError::Internal(format!("failed to parse redeem response: {e}")))?;

    let raw_status = data["status"].as_str().unwrap_or("unknown").to_lowercase();

    if raw_status != "active" {
      if raw_status.contains("invalid") || raw_status.contains("expired") {
        return Err(EmailServiceError::CdkInvalid(format!(
          "CDK status: {raw_status}"
        )));
      }
      return Err(EmailServiceError::Internal(format!(
        "unexpected CDK status: {raw_status}"
      )));
    }

    let email = data["emailAddress"].as_str().unwrap_or("").to_string();
    if email.is_empty() {
      return Err(EmailServiceError::EmailInvalid(
        "no email address in redeem response".into(),
      ));
    }

    let expires_at = data["expiresAt"].as_str().map(|s| s.to_string());
    let remaining_uses = data["remainingUses"].as_u64().map(|n| n as u32);

    Ok(EmailInfo {
      email,
      expires_at,
      status: raw_status,
      remaining_uses,
    })
  }

  fn generate_alias(&self, base_email: &str) -> Result<String, EmailServiceError> {
    let username = Self::extract_username(base_email);
    let domain = base_email.split('@').nth(1).unwrap_or("gmail.com");

    let used_count = self.used_alias_count(base_email);
    if used_count >= MAX_ALIASES_PER_EMAIL {
      return Err(EmailServiceError::EmailInvalid(format!(
        "maximum {MAX_ALIASES_PER_EMAIL} aliases already used for {base_email}"
      )));
    }

    // Generate random 5-char alphanumeric alias
    let mut rng = rand::rng();
    for _ in 0..50 {
      let alias: String = (0..5)
        .map(|_| {
          let idx = (rng.next_u32() % 36) as u8;
          if idx < 10 {
            (b'0' + idx) as char
          } else {
            (b'a' + (idx - 10)) as char
          }
        })
        .collect();

      if !self.is_alias_used(base_email, &alias) {
        self.mark_alias_used(base_email, &alias);
        return Ok(format!("{username}+{alias}@{domain}"));
      }
    }

    Err(EmailServiceError::Internal(
      "failed to generate unique alias after 50 attempts".into(),
    ))
  }

  fn poll_verification_code(
    &self,
    cdk: &str,
    timeout_secs: u64,
  ) -> Result<String, EmailServiceError> {
    let url = format!("{API_BASE}/mailbox/code");
    let body = serde_json::json!({ "cdk": cdk, "locktime": 5 });
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    loop {
      if Instant::now() >= deadline {
        return Err(EmailServiceError::Timeout(format!(
          "verification code not received within {timeout_secs}s"
        )));
      }

      let remaining = deadline.saturating_duration_since(Instant::now());
      let poll_timeout = std::cmp::min(Duration::from_secs(POLL_INTERVAL_SECS), remaining);

      let client = Self::client();
      let url_clone = url.clone();
      let body_clone = body.clone();
      let result: Result<reqwest::Response, reqwest::Error> = Self::block_on(async move {
        client
          .post(&url_clone)
          .header("accept", "*/*")
          .header("content-type", "application/json")
          .json(&body_clone)
          .timeout(Duration::from_secs(30))
          .send()
          .await
      });

      match result {
        Ok(resp) => {
          let text = Self::block_on(async move { resp.text().await }).unwrap_or_default();
          let data: Result<VerificationCodeResponse, _> = serde_json::from_str(&text);

          match data {
            Ok(vcr) => match vcr.status.as_str() {
              "success" => {
                if let Some(code) = vcr.code {
                  if !code.is_empty() {
                    return Ok(code);
                  }
                }
                // "success" without a code — keep polling
              }
              "email_invalid" => {
                return Err(EmailServiceError::EmailInvalid(
                  vcr.message.unwrap_or_else(|| "email invalid".into()),
                ));
              }
              "processing" => {
                // Expected — continue polling
              }
              other => {
                return Err(EmailServiceError::Internal(format!(
                  "unexpected poll status: {other}"
                )));
              }
            },
            Err(e) => {
              return Err(EmailServiceError::Internal(format!(
                "failed to parse poll response: {e} — body: {text}"
              )));
            }
          }
        }
        Err(e) => {
          if Instant::now() >= deadline {
            return Err(EmailServiceError::Network(format!(
              "poll request failed after deadline: {e}"
            )));
          }
          // Network error — retry on next poll cycle
        }
      }

      std::thread::sleep(poll_timeout);
    }
  }

  fn check_health(&self) -> bool {
    let url = format!("{API_BASE}/mailbox/redeem");
    let client = Self::client();
    let result: Result<reqwest::Response, reqwest::Error> = Self::block_on(async move {
      client
        .post(&url)
        .header("content-type", "application/json")
        .json(&serde_json::json!({ "cdk": "health-check" }))
        .timeout(Duration::from_secs(10))
        .send()
        .await
    });
    result.is_ok()
  }
}

impl Default for GmailCdkService {
  fn default() -> Self {
    Self::new()
  }
}
