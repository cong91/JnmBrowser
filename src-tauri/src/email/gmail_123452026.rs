use std::time::{Duration, Instant};

use super::{
  EmailAliasGenerator, EmailInfo, EmailService, EmailServiceError, VerificationCodeResponse,
};

const API_BASE: &str = "http://gmail.123452026.xyz/api";
const POLL_INTERVAL_SECS: u64 = 3;

/// Email service for `gmail.123452026.xyz`.
/// Redeems cards for Gmail addresses and polls for verification codes.
/// Uses `reqwest` (async) internally — calls are bridged via `tokio::runtime::Handle`.
pub struct Gmail123452026Service {
  aliases: EmailAliasGenerator,
}

impl Gmail123452026Service {
  pub fn new() -> Self {
    Self {
      aliases: EmailAliasGenerator::default(),
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
            .expect("gmail.123452026.xyz nested runtime thread panicked")
        })
      }
      Err(_) => {
        let rt = tokio::runtime::Builder::new_current_thread()
          .enable_all()
          .build()
          .expect("Failed to create gmail.123452026.xyz runtime");
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
}

impl EmailService for Gmail123452026Service {
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
    self.aliases.generate(base_email)
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

impl Default for Gmail123452026Service {
  fn default() -> Self {
    Self::new()
  }
}
