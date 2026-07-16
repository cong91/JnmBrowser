use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Configuration for an auto-registration task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationConfig {
  /// List of CDK codes (e.g. ["GMAIL-K4L5-EUW5-PHBV-A6KW", ...])
  pub cdks: Vec<String>,
  /// Optional existing profile ID to use (otherwise creates ephemeral)
  pub profile_id: Option<String>,
  /// Optional proxy ID to attach
  pub proxy_id: Option<String>,
  /// Browser engine: "chromium" or "camoufox"
  #[serde(default = "default_browser_type")]
  pub browser_type: String,
  /// Max full-flow retries on failure
  #[serde(default = "default_max_retries")]
  pub max_retries: u32,
  /// Number of accounts to create per CDK (1-6, via Gmail aliases)
  #[serde(default = "default_accounts_per_cdk")]
  pub accounts_per_cdk: u32,
  /// Run browser in headless mode
  #[serde(default)]
  pub headless: bool,
  /// Max number of CDKs to process concurrently (default 1)
  #[serde(default = "default_concurrency")]
  pub concurrency: u32,
}

fn default_browser_type() -> String { "chromium".into() }
fn default_max_retries() -> u32 { 3 }
fn default_accounts_per_cdk() -> u32 { 1 }
fn default_concurrency() -> u32 { 1 }

/// Tracks which step the registration is currently on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RegistrationStep {
  Idle,
  RedeemingCdk,
  GeneratingAlias,
  GeneratingUserInfo,
  LaunchingBrowser,
  VisitingHomepage,
  GettingCsrfToken,
  SubmittingEmail,
  FollowingAuthorize,
  RegisteringUser,
  SendingEmailOtp,
  PollingOtp,
  VerifyingOtp,
  CreatingAccount,
  ExtractingTokens,
  Enabling2Fa,
  Completed,
  Failed,
}

impl RegistrationStep {
  pub fn label(&self) -> &'static str {
    match self {
      Self::Idle => "Idle",
      Self::RedeemingCdk => "Redeeming CDK",
      Self::GeneratingAlias => "Generating email alias",
      Self::GeneratingUserInfo => "Generating user info",
      Self::LaunchingBrowser => "Launching browser",
      Self::VisitingHomepage => "Visiting chatgpt.com",
      Self::GettingCsrfToken => "Getting CSRF token",
      Self::SubmittingEmail => "Submitting email",
      Self::FollowingAuthorize => "Following authorize redirect",
      Self::RegisteringUser => "Registering user",
      Self::SendingEmailOtp => "Sending email OTP",
      Self::PollingOtp => "Polling for OTP code",
      Self::VerifyingOtp => "Verifying OTP code",
      Self::CreatingAccount => "Creating account",
      Self::ExtractingTokens => "Extracting tokens",
      Self::Enabling2Fa => "Enabling 2FA",
      Self::Completed => "Completed",
      Self::Failed => "Failed",
    }
  }
}

/// The result of a completed (or failed) registration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationResult {
  pub success: bool,
  pub email: String,
  pub password: String,
  pub account_id: String,
  pub access_token: String,
  pub device_id: String,
  pub error_message: String,
  pub step_logs: Vec<String>,
  pub created_at: DateTime<Utc>,
  pub two_fa_enabled: bool,
  pub cdk: String,
  pub base_email: String,
}

/// Progress payload emitted to the frontend via Tauri events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationProgress {
  pub task_id: String,
  pub cdk_index: u32,       // which CDK in the list (0-based)
  pub alias_index: u32,     // which alias for this CDK (0-based)
  pub total_cdks: u32,
  pub step: RegistrationStep,
  pub message: String,
  pub timestamp: DateTime<Utc>,
  pub result: Option<RegistrationResult>,
}
