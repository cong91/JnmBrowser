use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::email::EmailProvider;

/// How auto-registration should exit the network.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum NetworkMode {
  /// Host IP; no proxy, no WireGuard VPN, no Nord CLI.
  #[default]
  None,
  /// Attach static `proxy_id` to ephemeral profiles.
  Proxy,
  /// Attach inventory WireGuard `vpn_id` (per-profile via vpn-worker). Preferred over Nord CLI.
  Vpn,
  /// System-wide NordVPN CLI; rotate after N successes. Backup / legacy path.
  Nord,
}

/// Configuration for an auto-registration task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationConfig {
  /// List of CDK codes (e.g. ["GMAIL-K4L5-EUW5-PHBV-A6KW", ...])
  pub cdks: Vec<String>,
  /// Optional existing profile ID to reuse as the batch worker.
  /// When set and found, auto-reg launches this profile (with FP renew + data wipe
  /// on each relaunch) instead of creating an ephemeral worker. Not deleted at end.
  /// When unset, one ephemeral worker is created for the whole batch and deleted once.
  pub profile_id: Option<String>,
  /// Optional proxy ID to attach (used when `network_mode` is Proxy)
  pub proxy_id: Option<String>,
  /// Optional VPN config ID from Proxies & VPNs (used when `network_mode` is Vpn)
  #[serde(default)]
  pub vpn_id: Option<String>,
  /// Browser engine: "chromium" or "camoufox"
  #[serde(default = "default_browser_type")]
  pub browser_type: String,
  /// Max full-flow retries on failure
  #[serde(default = "default_max_retries")]
  pub max_retries: u32,
  /// Number of accounts to create per card/CDK (1–6 via +aliases).
  #[serde(default = "default_accounts_per_cdk")]
  pub accounts_per_cdk: u32,
  /// Run browser in headless mode
  #[serde(default)]
  pub headless: bool,
  /// Max number of CDKs to process concurrently (default 1).
  /// In VPN mode this is capped by `nord_max_sessions` (not by CDK count).
  #[serde(default = "default_concurrency")]
  pub concurrency: u32,
  /// Nord simultaneous WireGuard session budget (device/session limit).
  /// VPN mode: concurrency is auto-set to this budget (fixed policy max **6**).
  /// Peer pool size follows it so multi-IP concurrency stays within Nord limits (~10 devices plan; we use 6).
  #[serde(default = "default_nord_max_sessions")]
  pub nord_max_sessions: u32,
  /// Network mode: none | proxy | vpn | nord
  #[serde(default)]
  pub network_mode: NetworkMode,
  /// Rotate egress after this many successful saves (0 = never).
  /// Nord CLI: system disconnect/connect. Vpn/WireGuard: new Nord peer + worker reconnect.
  #[serde(default)]
  pub rotate_every_n: u32,
  /// NordVPN group/country, e.g. "United States" (`-g`) — Nord CLI mode only
  #[serde(default)]
  pub nord_group: Option<String>,
  /// NordVPN server name, e.g. "United States #5" (`-n`); wins over group
  #[serde(default)]
  pub nord_server_name: Option<String>,
  /// Optional path to NordVPN.exe; default Program Files install
  #[serde(default)]
  pub nord_cli_path: Option<String>,
  /// SMS provider id for phone verification (e.g. `"viotp"`). None = skip SMS.
  #[serde(default)]
  pub sms_provider: Option<String>,
  /// SMS provider API token (VI-OTP token). Prefer settings store when omitted.
  #[serde(default)]
  pub sms_token: Option<String>,
  /// Provider service id to rent numbers for (required when SMS is enabled).
  #[serde(default)]
  pub sms_service_id: Option<u32>,
  /// Preferred carriers, pipe-separated (e.g. `"VIETTEL|MOBIFONE"`).
  #[serde(default)]
  pub sms_network: Option<String>,
  /// Country code for SMS rental: `"vn"` (default) or `"la"`.
  #[serde(default)]
  pub sms_country: Option<String>,
  /// Email OTP provider — domain id (`gmail.123452026.xyz` default, or `sms.iosmq.xyz`).
  #[serde(default)]
  pub email_provider: EmailProvider,
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
/// Fixed Nord WireGuard concurrent-session budget for auto-reg.
/// Nord accounts allow up to ~10 devices; we use 6 as a safe parallel WG cap.
fn default_nord_max_sessions() -> u32 {
  6
}

/// Hard cap for Nord WG parallel sessions (never above this).
pub const NORD_MAX_PARALLEL_SESSIONS: u32 = 6;

/// Whether success_count should trigger a Nord IP rotation.
pub fn should_rotate(success_count: u32, every_n: u32) -> bool {
  every_n > 0 && success_count > 0 && success_count.is_multiple_of(every_n)
}

impl RegistrationConfig {
  /// Reject blank or duplicate cards before scheduling concurrent workers.
  pub fn validate_cdks(&self) -> Result<(), String> {
    let mut seen = HashSet::with_capacity(self.cdks.len());
    for cdk in &self.cdks {
      let normalized = cdk.trim();
      if normalized.is_empty() {
        return Err("CDK list contains an empty code".into());
      }
      if !seen.insert(normalized.to_ascii_uppercase()) {
        return Err(format!("duplicate CDK/card in batch: {normalized}"));
      }
    }
    Ok(())
  }

  fn non_empty(opt: &Option<String>) -> bool {
    opt.as_ref().is_some_and(|s| !s.trim().is_empty())
  }

  /// Normalize legacy payloads: bare `proxyId` / `vpnId` without mode.
  /// Call before validate / run.
  pub fn normalize_network(&mut self) {
    let has_proxy = Self::non_empty(&self.proxy_id);
    let has_vpn = Self::non_empty(&self.vpn_id);

    // Prefer explicit inventory VPN over bare proxy when both appear under None.
    if self.network_mode == NetworkMode::None && has_vpn {
      self.network_mode = NetworkMode::Vpn;
    } else if self.network_mode == NetworkMode::None && has_proxy {
      self.network_mode = NetworkMode::Proxy;
    }

    // Nord CLI / WireGuard: rotateEveryN defaults to 2 when unset (UI parity).
    if matches!(self.network_mode, NetworkMode::Nord | NetworkMode::Vpn) && self.rotate_every_n == 0
    {
      self.rotate_every_n = 2;
    }

    // Clamp accounts/card to the selected email provider capability.
    self.accounts_per_cdk = self
      .email_provider
      .clamp_accounts_per_card(self.accounts_per_cdk);
  }

  /// Accounts to create per card after provider limits are applied.
  pub fn effective_accounts_per_cdk(&self) -> u32 {
    self
      .email_provider
      .clamp_accounts_per_card(self.accounts_per_cdk)
  }

  /// Validate network fields before starting a task.
  pub fn validate_network(&self) -> Result<(), String> {
    match self.network_mode {
      NetworkMode::None => Ok(()),
      NetworkMode::Proxy => {
        if !Self::non_empty(&self.proxy_id) {
          return Err("proxy mode requires proxyId".into());
        }
        if Self::non_empty(&self.vpn_id) {
          return Err("proxy mode cannot be combined with vpnId".into());
        }
        Ok(())
      }
      NetworkMode::Vpn => {
        if !Self::non_empty(&self.vpn_id) {
          return Err("vpn mode requires vpnId".into());
        }
        if Self::non_empty(&self.proxy_id) {
          return Err("vpn mode cannot be combined with proxyId".into());
        }
        // Ensure the WireGuard config exists in storage.
        let vpn_id = self.vpn_id.as_ref().map(|s| s.trim()).unwrap_or_default();
        let storage = crate::vpn::VPN_STORAGE
          .lock()
          .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
        storage
          .load_config(vpn_id)
          .map_err(|e| format!("vpnId not found or invalid: {e}"))?;
        Ok(())
      }
      NetworkMode::Nord => {
        if Self::non_empty(&self.proxy_id) {
          return Err("nord CLI mode cannot be combined with proxyId".into());
        }
        if Self::non_empty(&self.vpn_id) {
          return Err("nord CLI mode cannot be combined with vpnId (use networkMode vpn)".into());
        }
        Ok(())
      }
    }
  }

  /// Proxy to attach on profile create — Proxy mode, or legacy proxyId with None.
  pub fn effective_proxy_id(&self) -> Option<String> {
    match self.network_mode {
      NetworkMode::Proxy => self.proxy_id.clone(),
      NetworkMode::None => self.proxy_id.clone().filter(|s| !s.trim().is_empty()),
      NetworkMode::Vpn | NetworkMode::Nord => None,
    }
  }

  /// WireGuard VPN config to attach on profile create — Vpn mode only.
  pub fn effective_vpn_id(&self) -> Option<String> {
    match self.network_mode {
      NetworkMode::Vpn => self
        .vpn_id
        .clone()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()),
      NetworkMode::None => self
        .vpn_id
        .clone()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()),
      NetworkMode::Proxy | NetworkMode::Nord => None,
    }
  }

  /// Whether the engine should drive system-wide Nord CLI (backup/legacy).
  pub fn uses_nord_cli(&self) -> bool {
    self.network_mode == NetworkMode::Nord
  }

  /// Whether mid-batch IP rotation is active (Nord CLI or WireGuard peer hop).
  pub fn uses_ip_rotation(&self) -> bool {
    matches!(self.network_mode, NetworkMode::Nord | NetworkMode::Vpn) && self.rotate_every_n > 0
  }

  /// Cap VPN concurrency by Nord simultaneous-session budget (independent of CDK count).
  pub fn effective_vpn_concurrency(&self) -> u32 {
    let requested = self.concurrency.clamp(1, NORD_MAX_PARALLEL_SESSIONS);
    let nord_budget = self.nord_max_sessions.clamp(1, NORD_MAX_PARALLEL_SESSIONS);
    requested.min(nord_budget)
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
  RequestingSmsOtp,
  PollingSmsOtp,
  VerifyingSmsOtp,
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
      Self::RequestingSmsOtp => "Requesting SMS number",
      Self::PollingSmsOtp => "Polling for SMS OTP",
      Self::VerifyingSmsOtp => "Verifying SMS OTP",
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
  fn validate_cdks_rejects_blank_codes() {
    let mut c = base_config(NetworkMode::None);
    c.cdks = vec!["GMAIL-X".into(), "  ".into()];
    assert_eq!(
      c.validate_cdks().unwrap_err(),
      "CDK list contains an empty code"
    );
  }

  #[test]
  fn validate_cdks_rejects_normalized_duplicates() {
    let mut c = base_config(NetworkMode::None);
    c.cdks = vec![" mail-abcd ".into(), "MAIL-ABCD".into()];
    assert_eq!(
      c.validate_cdks().unwrap_err(),
      "duplicate CDK/card in batch: MAIL-ABCD"
    );
  }

  #[test]
  fn validate_cdks_accepts_distinct_codes() {
    let mut c = base_config(NetworkMode::None);
    c.cdks = vec!["MAIL-ABCD".into(), "MAIL-EFGH".into()];
    assert!(c.validate_cdks().is_ok());
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
      vpn_id: None,
      browser_type: "chromium".into(),
      max_retries: 1,
      accounts_per_cdk: 1,
      headless: false,
      concurrency: 1,
      nord_max_sessions: 6,
      network_mode: mode,
      rotate_every_n: 0,
      nord_group: None,
      nord_server_name: None,
      nord_cli_path: None,
      sms_provider: None,
      sms_token: None,
      sms_service_id: None,
      sms_network: None,
      sms_country: None,
      email_provider: EmailProvider::Gmail123452026,
    }
  }

  #[test]
  fn effective_vpn_concurrency_uses_nord_budget_not_cdk_count() {
    let mut c = base_config(NetworkMode::Vpn);
    c.concurrency = 8;
    c.nord_max_sessions = 6;
    assert_eq!(c.effective_vpn_concurrency(), 6);
    c.concurrency = 3;
    c.nord_max_sessions = 6;
    assert_eq!(c.effective_vpn_concurrency(), 3);
    c.concurrency = 8;
    c.nord_max_sessions = 2;
    assert_eq!(c.effective_vpn_concurrency(), 2);
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
  fn validate_vpn_rejects_proxy_id() {
    let mut c = base_config(NetworkMode::Vpn);
    c.vpn_id = Some("v1".into());
    c.proxy_id = Some("p1".into());
    assert!(c.validate_network().is_err());
  }

  #[test]
  fn validate_vpn_requires_vpn_id() {
    let c = base_config(NetworkMode::Vpn);
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
  fn normalize_bare_vpn_id_to_vpn_mode() {
    let mut c = base_config(NetworkMode::None);
    c.vpn_id = Some("wg-1".into());
    c.normalize_network();
    assert_eq!(c.network_mode, NetworkMode::Vpn);
    assert_eq!(c.effective_vpn_id().as_deref(), Some("wg-1"));
    assert!(c.effective_proxy_id().is_none());
    assert!(!c.uses_nord_cli());
  }

  #[test]
  fn nord_normalize_default_rotate_every_n() {
    let mut c = base_config(NetworkMode::Nord);
    assert_eq!(c.rotate_every_n, 0);
    c.normalize_network();
    assert_eq!(c.rotate_every_n, 2);
    assert!(c.uses_nord_cli());
  }

  #[test]
  fn vpn_mode_defaults_rotate_every_n() {
    let mut c = base_config(NetworkMode::Vpn);
    c.vpn_id = Some("wg-1".into());
    assert_eq!(c.rotate_every_n, 0);
    c.normalize_network();
    assert_eq!(c.rotate_every_n, 2);
    assert!(c.uses_ip_rotation());
    assert!(!c.uses_nord_cli());
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
  /// Phone number used for SMS verification (if any).
  #[serde(default)]
  pub phone_number: String,
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

/// One account attempt recorded under a CDK inventory row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CdkAccountEntry {
  pub email: String,
  #[serde(default)]
  pub account_id: String,
  pub success: bool,
  pub free_trial_eligible: bool,
  #[serde(default)]
  pub plan_type: String,
  #[serde(default)]
  pub error_message: String,
  pub created_at: DateTime<Utc>,
}

/// Aggregated stats for a CDK code (how many nicks / free-trial yes-no).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CdkInventoryRecord {
  pub cdk: String,
  #[serde(default)]
  pub base_email: String,
  /// How many accounts this batch targeted for the CDK (`accounts_per_cdk`).
  #[serde(default)]
  pub target_accounts: u32,
  /// Alias slots that finished an attempt (success, free-trial-no, or hard fail).
  #[serde(default)]
  pub attempted: u32,
  /// Free-trial eligible accounts created successfully.
  #[serde(default)]
  pub free_trial_yes: u32,
  /// Accounts that registered but failed free-trial gate (saved, not resellable).
  #[serde(default)]
  pub free_trial_no: u32,
  /// Hard failures after retries (no usable account).
  #[serde(default)]
  pub failed: u32,
  /// pending | running | done | failed | cancelled
  #[serde(default = "default_cdk_status")]
  pub status: String,
  #[serde(default)]
  pub last_error: String,
  #[serde(default)]
  pub accounts: Vec<CdkAccountEntry>,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
  #[serde(default)]
  pub task_id: String,
}

fn default_cdk_status() -> String {
  "pending".into()
}

impl CdkInventoryRecord {
  pub fn new(cdk: &str, target_accounts: u32, task_id: &str) -> Self {
    let now = Utc::now();
    Self {
      cdk: cdk.to_string(),
      base_email: String::new(),
      target_accounts,
      attempted: 0,
      free_trial_yes: 0,
      free_trial_no: 0,
      failed: 0,
      status: "pending".into(),
      last_error: String::new(),
      accounts: Vec::new(),
      created_at: now,
      updated_at: now,
      task_id: task_id.to_string(),
    }
  }

  pub fn record_result(&mut self, result: &RegistrationResult) {
    self.attempted = self.attempted.saturating_add(1);
    self.updated_at = Utc::now();
    if self.base_email.is_empty() && !result.base_email.is_empty() {
      self.base_email = result.base_email.clone();
    }
    if result.success && result.free_trial_eligible {
      self.free_trial_yes = self.free_trial_yes.saturating_add(1);
    } else if !result.email.is_empty() && !result.free_trial_eligible {
      self.free_trial_no = self.free_trial_no.saturating_add(1);
      if !result.error_message.is_empty() {
        self.last_error = result.error_message.clone();
      }
    } else {
      self.failed = self.failed.saturating_add(1);
      if !result.error_message.is_empty() {
        self.last_error = result.error_message.clone();
      }
    }
    self.accounts.push(CdkAccountEntry {
      email: result.email.clone(),
      account_id: result.account_id.clone(),
      success: result.success,
      free_trial_eligible: result.free_trial_eligible,
      plan_type: result.plan_type.clone(),
      error_message: result.error_message.clone(),
      created_at: result.created_at,
    });
  }

  pub fn record_hard_failure(&mut self, error: &str) {
    self.attempted = self.attempted.saturating_add(1);
    self.failed = self.failed.saturating_add(1);
    self.last_error = error.to_string();
    self.updated_at = Utc::now();
  }

  pub fn finalize_status(&mut self) {
    self.updated_at = Utc::now();
    if self.status == "cancelled" {
      return;
    }
    if self.free_trial_yes == 0 && self.attempted > 0 && self.failed == self.attempted {
      self.status = "failed".into();
    } else {
      self.status = "done".into();
    }
  }
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
