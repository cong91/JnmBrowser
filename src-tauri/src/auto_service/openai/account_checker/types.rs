use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Reuse the same credential parse format from login: email|password|2fa
pub use super::super::login::types::LoginCredential;

/// Conclusive outcome for one checked account.
/// Unresolved = indeterminate (wrong password, Cloudflare, rate limit, cancel, etc.)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AccountOutcome {
  /// Reached a visible verification-code input — account is accessible.
  Passed,
  /// Page contains BOTH "Authentication Error" AND "error_code: account_deactivated".
  Deactivated,
  /// Everything else: wrong password, challenge, timeout, cancel, etc.
  Unresolved,
}

/// Secret-free reason code emitted in progress events.
/// Never carries passwords, raw HTML, or provider tokens.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReasonCode {
  OtpReached,
  Deactivated,
  WrongCredentials,
  RateLimited,
  Cloudflare,
  Timeout,
  Cancelled,
  Unknown,
  UnknownSurface,
  BrowserError,
  CleanupError,
}

/// Durable per-account result stored on disk.
/// Format on export: email|password|2fa (same shape as input).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountCheckResult {
  pub email: String,
  pub password: String,
  pub totp_secret: String,
  pub outcome: AccountOutcome,
  pub reason_code: ReasonCode,
  pub created_at: DateTime<Utc>,
}

impl AccountCheckResult {
  /// email|password|2fa line, matching input format.
  pub fn credential_line(&self) -> String {
    if self.totp_secret.is_empty() {
      format!("{}|{}", self.email, self.password)
    } else {
      format!("{}|{}|{}", self.email, self.password, self.totp_secret)
    }
  }

  /// Key used for dedup / store indexing.
  pub fn key(&self) -> String {
    self.email.trim().to_lowercase()
  }
}

/// Configuration for a batch account check task (from frontend).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountCheckConfig {
  pub credentials_text: String,
  pub browser_type: String,
  pub headless: bool,
  pub concurrency: u32,
}

/// Safe step enum — no credential material.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CheckStep {
  Launching,
  EnteringEmail,
  SubmittingPassword,
  Observing,
  Classifying,
  Persisting,
  Completed,
  Failed,
  Cancelled,
}

/// Per-account progress event payload — safe, no passwords.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckProgress {
  pub task_id: String,
  pub account_key: String,
  pub credential_index: usize,
  pub total_credentials: usize,
  pub step: CheckStep,
  pub outcome: Option<AccountOutcome>,
  pub reason_code: Option<ReasonCode>,
  pub terminal: bool,
}
