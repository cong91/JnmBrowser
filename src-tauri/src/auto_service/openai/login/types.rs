use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Parsed credential line: ACCOUNT|PASSWORD|2FA
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginCredential {
  pub email: String,
  pub password: String,
  pub totp_secret: String,
}

impl LoginCredential {
  /// Parse a credential line in format "ACCOUNT|PASSWORD|2FA".
  /// Returns None if the line is empty or malformed.
  pub fn parse(line: &str) -> Option<Self> {
    let line = line.trim();
    if line.is_empty() {
      return None;
    }
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() < 2 {
      return None;
    }
    let email = parts[0].trim().to_string();
    let password = parts[1].trim().to_string();
    let totp_secret = parts.get(2).map(|s| s.trim()).unwrap_or("").to_string();

    if email.is_empty() || password.is_empty() {
      return None;
    }

    Some(Self {
      email,
      password,
      totp_secret,
    })
  }

  /// Parse multiple credential lines (newline-separated).
  pub fn parse_batch(text: &str) -> Vec<Self> {
    text.lines().filter_map(Self::parse).collect()
  }
}

/// How auto-login should handle network.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum LoginNetworkMode {
  #[default]
  None,
  Proxy,
  /// Inventory WireGuard / Nord conf (per-profile vpn-worker). Preferred.
  Vpn,
  /// Legacy system-wide Nord CLI (not preferred for batch).
  Nord,
}

/// Configuration for a batch login task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginConfig {
  /// Raw credentials text (one per line: ACCOUNT|PASSWORD|2FA).
  /// Parsed into `credentials` on validation.
  #[serde(default)]
  pub credentials_text: String,

  /// Parsed credentials (populated from credentials_text if empty).
  #[serde(default)]
  pub credentials: Vec<LoginCredential>,

  /// Browser engine: "chromium" or "camoufox"
  #[serde(default = "default_browser_type")]
  pub browser_type: String,

  /// Max retries per account
  #[serde(default = "default_max_retries")]
  pub max_retries: u32,

  /// Run browser headless
  #[serde(default)]
  pub headless: bool,

  /// Max concurrent logins (default 1)
  #[serde(default = "default_concurrency")]
  pub concurrency: u32,

  /// Sub2API base URL (e.g. "http://localhost:3000")
  #[serde(default)]
  pub sub2api_url: String,

  /// Sub2API admin API key
  #[serde(default)]
  pub sub2api_api_key: String,

  /// Optional proxy ID for sub2api account
  #[serde(default)]
  pub sub2api_proxy_id: Option<i64>,

  /// Optional group IDs for sub2api account
  #[serde(default)]
  pub sub2api_group_ids: Option<Vec<i64>>,

  /// Push to sub2api after login (false = local store only)
  #[serde(default)]
  pub push_to_sub2api: bool,

  /// SMS provider for phone verification (e.g. "viotp")
  #[serde(default)]
  pub sms_provider: Option<String>,

  /// SMS API token
  #[serde(default)]
  pub sms_token: Option<String>,

  /// SMS service ID for OpenAI
  #[serde(default)]
  pub sms_service_id: Option<u32>,

  /// SMS network (e.g. "VINAPHONE")
  #[serde(default)]
  pub sms_network: Option<String>,

  /// SMS country (default "vn")
  #[serde(default)]
  pub sms_country: Option<String>,

  /// Optional proxy ID for browser (when network_mode is Proxy)
  #[serde(default)]
  pub proxy_id: Option<String>,

  /// Inventory WireGuard / Nord conf id (when network_mode is Vpn)
  #[serde(default)]
  pub vpn_id: Option<String>,

  /// Rotate WireGuard peer after every N successful logins (0 = never).
  /// VPN mode defaults to 1 (new IP after each success).
  #[serde(default)]
  pub rotate_every_n: u32,

  /// Network mode for browser
  #[serde(default)]
  pub network_mode: LoginNetworkMode,
}

fn default_browser_type() -> String {
  "chromium".into()
}
fn default_max_retries() -> u32 {
  3
}
fn default_concurrency() -> u32 {
  1
}

/// Whether success_count warrants a peer rotate (same rule as auto-reg).
pub fn should_rotate(success_count: u32, every_n: u32) -> bool {
  every_n > 0 && success_count > 0 && success_count.is_multiple_of(every_n)
}

impl LoginConfig {
  fn non_empty(opt: &Option<String>) -> bool {
    opt.as_ref().is_some_and(|s| !s.trim().is_empty())
  }

  /// Parse credentials_text into credentials if credentials is empty.
  pub fn parse_credentials(&mut self) {
    if self.credentials.is_empty() && !self.credentials_text.is_empty() {
      self.credentials = LoginCredential::parse_batch(&self.credentials_text);
    }
  }

  /// Normalize bare proxyId / vpnId into network_mode and default rotate.
  pub fn normalize(&mut self) {
    let has_vpn = Self::non_empty(&self.vpn_id);
    let has_proxy = Self::non_empty(&self.proxy_id);
    if self.network_mode == LoginNetworkMode::None && has_vpn {
      self.network_mode = LoginNetworkMode::Vpn;
    } else if self.network_mode == LoginNetworkMode::None && has_proxy {
      self.network_mode = LoginNetworkMode::Proxy;
    }
    // VPN login: rotate peer after every success unless user set a custom cadence.
    if self.network_mode == LoginNetworkMode::Vpn && self.rotate_every_n == 0 {
      self.rotate_every_n = 1;
    }
  }

  pub fn effective_vpn_id(&self) -> Option<String> {
    match self.network_mode {
      LoginNetworkMode::Vpn => self
        .vpn_id
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()),
      _ => None,
    }
  }

  /// Validate configuration before starting.
  pub fn validate(&self) -> Result<(), String> {
    if self.credentials.is_empty() {
      return Err("No valid credentials provided. Format: ACCOUNT|PASSWORD|2FA".into());
    }
    if self.push_to_sub2api {
      if self.sub2api_url.trim().is_empty() {
        return Err("Sub2API URL is required when push is enabled".into());
      }
      if self.sub2api_api_key.trim().is_empty() {
        return Err("Sub2API API key is required when push is enabled".into());
      }
    }
    match self.network_mode {
      LoginNetworkMode::None => {}
      LoginNetworkMode::Proxy => {
        if !Self::non_empty(&self.proxy_id) {
          return Err("proxy mode requires proxyId".into());
        }
        if Self::non_empty(&self.vpn_id) {
          return Err("proxy mode cannot be combined with vpnId".into());
        }
      }
      LoginNetworkMode::Vpn => {
        if !Self::non_empty(&self.vpn_id) {
          return Err("vpn mode requires vpnId".into());
        }
        if Self::non_empty(&self.proxy_id) {
          return Err("vpn mode cannot be combined with proxyId".into());
        }
        let vpn_id = self.vpn_id.as_ref().map(|s| s.trim()).unwrap_or_default();
        let storage = crate::vpn::VPN_STORAGE
          .lock()
          .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
        storage
          .load_config(vpn_id)
          .map_err(|e| format!("vpnId not found or invalid: {e}"))?;
      }
      LoginNetworkMode::Nord => {
        if Self::non_empty(&self.proxy_id) {
          return Err("nord CLI mode cannot be combined with proxyId".into());
        }
        if Self::non_empty(&self.vpn_id) {
          return Err("nord CLI mode cannot be combined with vpnId (use networkMode vpn)".into());
        }
      }
    }
    Ok(())
  }
}

/// Login step tracking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LoginStep {
  Idle,
  GeneratingAuthUrl,
  LaunchingBrowser,
  NavigatingToAuth,
  EnteringEmail,
  EnteringPassword,
  Entering2FA,
  RequestingSmsOtp,
  PollingSmsOtp,
  VerifyingSmsOtp,
  ConfirmingAuthorization,
  WaitingForCallback,
  ExtractingCallback,
  ExchangingCode,
  CreatingAccount,
  Completed,
  Failed,
}

impl LoginStep {
  #[allow(dead_code)]
  pub fn label(&self) -> &'static str {
    match self {
      Self::Idle => "Idle",
      Self::GeneratingAuthUrl => "Generating auth URL",
      Self::LaunchingBrowser => "Launching browser",
      Self::NavigatingToAuth => "Navigating to auth page",
      Self::EnteringEmail => "Entering email",
      Self::EnteringPassword => "Entering password",
      Self::Entering2FA => "Entering 2FA code",
      Self::RequestingSmsOtp => "Requesting SMS number",
      Self::PollingSmsOtp => "Waiting for SMS OTP",
      Self::VerifyingSmsOtp => "Verifying SMS OTP",
      Self::ConfirmingAuthorization => "Confirming authorization",
      Self::WaitingForCallback => "Waiting for callback",
      Self::ExtractingCallback => "Extracting callback",
      Self::ExchangingCode => "Exchanging code",
      Self::CreatingAccount => "Creating account",
      Self::Completed => "Completed",
      Self::Failed => "Failed",
    }
  }
}

/// Result of a single login.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResult {
  pub success: bool,
  pub email: String,
  pub account_id: String,
  pub access_token: String,
  pub refresh_token: String,
  pub sub2api_account_id: Option<i64>,
  pub error_message: String,
  /// Non-fatal Sub2API push failure after local login succeeded.
  #[serde(default)]
  pub push_error: String,
  pub step_logs: Vec<String>,
  pub created_at: DateTime<Utc>,
  pub phone_number: String,
  /// Inventory status for tracking.
  #[serde(default)]
  pub status: LoginResultStatus,
  /// Free-form note.
  #[serde(default)]
  pub note: String,
  /// When exported.
  #[serde(default)]
  pub exported_at: Option<DateTime<Utc>>,
  /// Account password (kept so failed/expired rows can be re-logged without re-paste).
  /// Not included in Sub2API export mapping.
  #[serde(default)]
  pub password: String,
  /// Optional TOTP secret for re-login.
  #[serde(default)]
  pub totp_secret: String,
}

impl LoginResult {
  /// `email|password|totp` line for restarting login.
  pub fn credential_line(&self) -> Option<String> {
    if self.email.is_empty() || self.password.is_empty() {
      return None;
    }
    if self.totp_secret.is_empty() {
      Some(format!("{}|{}", self.email, self.password))
    } else {
      Some(format!(
        "{}|{}|{}",
        self.email, self.password, self.totp_secret
      ))
    }
  }
}

/// Status for login results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum LoginResultStatus {
  #[default]
  Available,
  Exported,
  Used,
  Invalid,
}

impl LoginResultStatus {
  pub fn as_str(&self) -> &'static str {
    match self {
      Self::Available => "available",
      Self::Exported => "exported",
      Self::Used => "used",
      Self::Invalid => "invalid",
    }
  }
}

/// Progress event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginProgress {
  pub task_id: String,
  pub credential_index: u32,
  pub total_credentials: u32,
  pub step: LoginStep,
  pub message: String,
  pub timestamp: DateTime<Utc>,
  pub result: Option<LoginResult>,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_credential_full() {
    let c = LoginCredential::parse("user@example.com|password123|JBSWY3DPEHPK3PXP").unwrap();
    assert_eq!(c.email, "user@example.com");
    assert_eq!(c.password, "password123");
    assert_eq!(c.totp_secret, "JBSWY3DPEHPK3PXP");
  }

  #[test]
  fn parse_credential_no_2fa() {
    let c = LoginCredential::parse("user@example.com|password123").unwrap();
    assert_eq!(c.email, "user@example.com");
    assert_eq!(c.password, "password123");
    assert_eq!(c.totp_secret, "");
  }

  #[test]
  fn parse_credential_with_spaces() {
    let c = LoginCredential::parse("  user@example.com  |  password123  |  SECRET  ").unwrap();
    assert_eq!(c.email, "user@example.com");
    assert_eq!(c.password, "password123");
    assert_eq!(c.totp_secret, "SECRET");
  }

  #[test]
  fn parse_credential_empty_line() {
    assert!(LoginCredential::parse("").is_none());
    assert!(LoginCredential::parse("   ").is_none());
  }

  #[test]
  fn parse_credential_no_password() {
    assert!(LoginCredential::parse("user@example.com").is_none());
  }

  #[test]
  fn parse_credential_empty_email() {
    assert!(LoginCredential::parse("|password123|SECRET").is_none());
  }

  #[test]
  fn parse_batch_multiple() {
    let text = "a@x.com|p1|s1\nb@x.com|p2\n\n  \nc@x.com|p3|s3";
    let creds = LoginCredential::parse_batch(text);
    assert_eq!(creds.len(), 3);
    assert_eq!(creds[0].email, "a@x.com");
    assert_eq!(creds[1].email, "b@x.com");
    assert_eq!(creds[2].email, "c@x.com");
  }

  #[test]
  fn config_validate_empty_credentials() {
    let config = LoginConfig {
      credentials_text: String::new(),
      credentials: vec![],
      browser_type: "chromium".into(),
      max_retries: 3,
      headless: false,
      concurrency: 1,
      sub2api_url: String::new(),
      sub2api_api_key: String::new(),
      sub2api_proxy_id: None,
      sub2api_group_ids: None,
      push_to_sub2api: false,
      sms_provider: None,
      sms_token: None,
      sms_service_id: None,
      sms_network: None,
      sms_country: None,
      proxy_id: None,
      vpn_id: None,
      rotate_every_n: 0,
      network_mode: LoginNetworkMode::None,
    };
    assert!(config.validate().is_err());
  }

  #[test]
  fn config_validate_push_without_url() {
    let config = LoginConfig {
      credentials_text: String::new(),
      credentials: vec![LoginCredential {
        email: "a@x.com".into(),
        password: "p".into(),
        totp_secret: String::new(),
      }],
      browser_type: "chromium".into(),
      max_retries: 3,
      headless: false,
      concurrency: 1,
      sub2api_url: String::new(),
      sub2api_api_key: String::new(),
      sub2api_proxy_id: None,
      sub2api_group_ids: None,
      push_to_sub2api: true,
      sms_provider: None,
      sms_token: None,
      sms_service_id: None,
      sms_network: None,
      sms_country: None,
      proxy_id: None,
      vpn_id: None,
      rotate_every_n: 0,
      network_mode: LoginNetworkMode::None,
    };
    assert!(config.validate().is_err());
  }

  #[test]
  fn config_validate_ok() {
    let config = LoginConfig {
      credentials_text: String::new(),
      credentials: vec![LoginCredential {
        email: "a@x.com".into(),
        password: "p".into(),
        totp_secret: String::new(),
      }],
      browser_type: "chromium".into(),
      max_retries: 3,
      headless: false,
      concurrency: 1,
      sub2api_url: "http://localhost:3000".into(),
      sub2api_api_key: "admin-key".into(),
      sub2api_proxy_id: None,
      sub2api_group_ids: None,
      push_to_sub2api: true,
      sms_provider: None,
      sms_token: None,
      sms_service_id: None,
      sms_network: None,
      sms_country: None,
      proxy_id: None,
      vpn_id: None,
      rotate_every_n: 0,
      network_mode: LoginNetworkMode::None,
    };
    assert!(config.validate().is_ok());
  }

  #[test]
  fn vpn_normalize_defaults_rotate_every_n() {
    let mut c = LoginConfig {
      credentials_text: String::new(),
      credentials: vec![LoginCredential {
        email: "a@x.com".into(),
        password: "p".into(),
        totp_secret: String::new(),
      }],
      browser_type: "chromium".into(),
      max_retries: 1,
      headless: false,
      concurrency: 1,
      sub2api_url: String::new(),
      sub2api_api_key: String::new(),
      sub2api_proxy_id: None,
      sub2api_group_ids: None,
      push_to_sub2api: false,
      sms_provider: None,
      sms_token: None,
      sms_service_id: None,
      sms_network: None,
      sms_country: None,
      proxy_id: None,
      vpn_id: Some("wg-1".into()),
      rotate_every_n: 0,
      network_mode: LoginNetworkMode::None,
    };
    c.normalize();
    assert_eq!(c.network_mode, LoginNetworkMode::Vpn);
    assert_eq!(c.rotate_every_n, 1);
    assert_eq!(c.effective_vpn_id().as_deref(), Some("wg-1"));
  }

  #[test]
  fn should_rotate_every_n_successes() {
    assert!(!should_rotate(0, 1));
    assert!(should_rotate(1, 1));
    assert!(should_rotate(2, 1));
    assert!(!should_rotate(1, 2));
    assert!(should_rotate(2, 2));
    assert!(!should_rotate(2, 0));
  }
}
