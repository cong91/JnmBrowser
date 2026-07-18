use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How auto-registration should exit the network.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum NetworkMode {
  /// Host IP; no proxy and no Nord CLI.
  #[default]
  None,
  /// Attach static `proxy_id` to ephemeral profiles.
  Proxy,
  /// System-wide NordVPN CLI; rotate after N successes.
  Nord,
}

/// Configuration for an auto-registration task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationConfig {
  /// List of CDK codes (e.g. ["GMAIL-K4L5-EUW5-PHBV-A6KW", ...])
  pub cdks: Vec<String>,
  /// Optional existing profile ID to use (otherwise creates ephemeral)
  pub profile_id: Option<String>,
  /// Optional proxy ID to attach (used when `network_mode` is Proxy)
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
  /// Dual network mode: none | proxy | nord
  #[serde(default)]
  pub network_mode: NetworkMode,
  /// Rotate Nord IP after this many successful saves (0 = never). Nord mode only.
  #[serde(default)]
  pub rotate_every_n: u32,
  /// NordVPN group/country, e.g. "United States" (`-g`)
  #[serde(default)]
  pub nord_group: Option<String>,
  /// NordVPN server name, e.g. "United States #5" (`-n`); wins over group
  #[serde(default)]
  pub nord_server_name: Option<String>,
  /// Optional path to NordVPN.exe; default Program Files install
  #[serde(default)]
  pub nord_cli_path: Option<String>,
}

fn default_browser_type() -> String {
  "chromium".into()
}
fn default_max_retries() -> u32 {
  3
}
fn default_accounts_per_cdk() -> u32 {
  1
}
fn default_concurrency() -> u32 {
  1
}

/// Whether success_count should trigger a Nord IP rotation.
pub fn should_rotate(success_count: u32, every_n: u32) -> bool {
  every_n > 0 && success_count > 0 && success_count.is_multiple_of(every_n)
}

impl RegistrationConfig {
  /// Normalize legacy payloads: bare `proxyId` without mode → Proxy.
  /// Call before validate / run.
  pub fn normalize_network(&mut self) {
    let has_proxy = self
      .proxy_id
      .as_ref()
      .map(|s| !s.trim().is_empty())
      .unwrap_or(false);
    if self.network_mode == NetworkMode::None && has_proxy {
      self.network_mode = NetworkMode::Proxy;
    }
    // Nord without explicit rotateEveryN defaults to 2 (UI parity).
    if self.network_mode == NetworkMode::Nord && self.rotate_every_n == 0 {
      self.rotate_every_n = 2;
    }
  }

  /// Validate dual-mode network fields before starting a task.
  pub fn validate_network(&self) -> Result<(), String> {
    match self.network_mode {
      NetworkMode::None => Ok(()),
      NetworkMode::Proxy => {
        if self
          .proxy_id
          .as_ref()
          .map(|s| s.trim().is_empty())
          .unwrap_or(true)
        {
          Err("proxy mode requires proxyId".into())
        } else {
          Ok(())
        }
      }
      NetworkMode::Nord => {
        if self
          .proxy_id
          .as_ref()
          .map(|s| !s.trim().is_empty())
          .unwrap_or(false)
        {
          Err("nord mode cannot be combined with proxyId".into())
        } else {
          Ok(())
        }
      }
    }
  }

  /// Proxy to attach on profile create — Proxy mode, or legacy proxyId with None.
  pub fn effective_proxy_id(&self) -> Option<String> {
    match self.network_mode {
      NetworkMode::Proxy => self.proxy_id.clone(),
      NetworkMode::None => self.proxy_id.clone().filter(|s| !s.trim().is_empty()),
      NetworkMode::Nord => None,
    }
  }
}

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
  CheckingFreeOffer,
  Enabling2Fa,
  RotatingIp,
  Completed,
  Failed,
}

impl RegistrationStep {
  #[allow(dead_code)]
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
      Self::CheckingFreeOffer => "Checking free offer / free trial",
      Self::Enabling2Fa => "Enabling 2FA",
      Self::RotatingIp => "Rotating IP",
      Self::Completed => "Completed",
      Self::Failed => "Failed",
    }
  }
}

#[cfg(test)]
mod network_config_tests {
  use super::*;

  #[test]
  fn default_network_mode_is_none() {
    let json = r#"{"cdks":["GMAIL-X"]}"#;
    let c: RegistrationConfig = serde_json::from_str(json).unwrap();
    assert_eq!(c.network_mode, NetworkMode::None);
    assert_eq!(c.rotate_every_n, 0);
  }

  #[test]
  fn should_rotate_every_n_successes() {
    assert!(!should_rotate(0, 2));
    assert!(!should_rotate(1, 2));
    assert!(should_rotate(2, 2));
    assert!(!should_rotate(3, 2));
    assert!(should_rotate(4, 2));
    assert!(!should_rotate(2, 0));
  }

  fn base_config(mode: NetworkMode) -> RegistrationConfig {
    RegistrationConfig {
      cdks: vec!["GMAIL-X".into()],
      profile_id: None,
      proxy_id: None,
      browser_type: "chromium".into(),
      max_retries: 1,
      accounts_per_cdk: 1,
      headless: false,
      concurrency: 1,
      network_mode: mode,
      rotate_every_n: 0,
      nord_group: None,
      nord_server_name: None,
      nord_cli_path: None,
    }
  }

  #[test]
  fn validate_proxy_mode_requires_proxy_id() {
    let mut c = base_config(NetworkMode::Proxy);
    assert!(c.validate_network().is_err());
    c.proxy_id = Some("p1".into());
    assert!(c.validate_network().is_ok());
  }

  #[test]
  fn validate_nord_rejects_proxy_id() {
    let mut c = base_config(NetworkMode::Nord);
    c.proxy_id = Some("p1".into());
    c.rotate_every_n = 2;
    c.nord_group = Some("United States".into());
    assert!(c.validate_network().is_err());
  }

  #[test]
  fn effective_proxy_id_legacy_none_with_proxy_id() {
    let mut c = base_config(NetworkMode::None);
    c.proxy_id = Some("p1".into());
    // Before normalize: still honor proxy_id for safety.
    assert_eq!(c.effective_proxy_id().as_deref(), Some("p1"));
    c.normalize_network();
    assert_eq!(c.network_mode, NetworkMode::Proxy);
    assert_eq!(c.effective_proxy_id().as_deref(), Some("p1"));
    c.network_mode = NetworkMode::Nord;
    c.proxy_id = None;
    assert!(c.effective_proxy_id().is_none());
  }

  #[test]
  fn nord_normalize_default_rotate_every_n() {
    let mut c = base_config(NetworkMode::Nord);
    assert_eq!(c.rotate_every_n, 0);
    c.normalize_network();
    assert_eq!(c.rotate_every_n, 2);
  }
}

/// Inventory lifecycle for a registered ChatGPT account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum AccountInventoryStatus {
  #[default]
  Available,
  Exported,
  Sold,
  Invalid,
  Reserved,
}

impl AccountInventoryStatus {
  pub fn as_str(&self) -> &'static str {
    match self {
      Self::Available => "available",
      Self::Exported => "exported",
      Self::Sold => "sold",
      Self::Invalid => "invalid",
      Self::Reserved => "reserved",
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
  /// Base32 TOTP secret when 2FA was enabled successfully.
  #[serde(default)]
  pub totp_secret: String,
  /// True only when ChatGPT free trial / free Plus offer was detected.
  #[serde(default)]
  pub free_trial_eligible: bool,
  /// Compact plan/offer summary for UI/debug (e.g. "chatgptplusplan/trial").
  #[serde(default)]
  pub plan_type: String,
  pub cdk: String,
  pub base_email: String,
  /// Inventory status for resale workflow.
  #[serde(default)]
  pub status: AccountInventoryStatus,
  /// Free-form seller note / buyer tag.
  #[serde(default)]
  pub note: String,
  /// Last time this account was exported.
  #[serde(default)]
  pub exported_at: Option<DateTime<Utc>>,
  /// When this account was marked sold/used.
  #[serde(default)]
  pub sold_at: Option<DateTime<Utc>>,
}

/// Progress payload emitted to the frontend via Tauri events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationProgress {
  pub task_id: String,
  pub cdk_index: u32,   // which CDK in the list (0-based)
  pub alias_index: u32, // which alias for this CDK (0-based)
  pub total_cdks: u32,
  pub step: RegistrationStep,
  pub message: String,
  pub timestamp: DateTime<Utc>,
  pub result: Option<RegistrationResult>,
}
