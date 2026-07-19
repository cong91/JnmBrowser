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

  /// Optional proxy ID for browser
  #[serde(default)]
  pub proxy_id: Option<String>,

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

impl LoginConfig {
  /// Parse credentials_text into credentials if credentials is empty.
  pub fn parse_credentials(&mut self) {
    if self.credentials.is_empty() && !self.credentials_text.is_empty() {
      self.credentials = LoginCredential::parse_batch(&self.credentials_text);
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
      network_mode: LoginNetworkMode::None,
    };
    assert!(config.validate().is_ok());
  }
}
