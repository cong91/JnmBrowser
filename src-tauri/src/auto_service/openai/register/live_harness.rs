//! Live end-to-end auto-registration notes.
//!
//! Prefer the dedicated binary (avoids Tauri MockRuntime vs Wry AppHandle mismatch):
//! ```text
//! cargo run --manifest-path src-tauri/Cargo.toml --bin auto-reg-live -- \
//!   --cdk GMAIL-XXXX --browser camoufox --profile-id <uuid>
//! ```
//!
//! Or with env for historical harness style (binary path recommended).

#[cfg(test)]
mod tests {
  use crate::auto_service::openai::register::types::{NetworkMode, RegistrationConfig};

  /// Smoke: config used by live runs deserializes / builds with dual-network defaults.
  #[test]
  fn live_config_defaults_include_network_mode() {
    let cdk = std::env::var("AUTO_REG_CDK").unwrap_or_else(|_| "GMAIL-TEST".into());
    let browser_type = std::env::var("AUTO_REG_BROWSER").unwrap_or_else(|_| "camoufox".into());
    let profile_id = std::env::var("AUTO_REG_PROFILE_ID").ok();

    let config = RegistrationConfig {
      cdks: vec![cdk],
      profile_id,
      proxy_id: None,
      vpn_id: None,
      browser_type,
      max_retries: 1,
      accounts_per_cdk: 1,
      headless: false,
      concurrency: 1,
      nord_max_sessions: 6,
      network_mode: NetworkMode::None,
      rotate_every_n: 0,
      nord_group: None,
      nord_server_name: None,
      nord_cli_path: None,
      sms_provider: None,
      sms_token: None,
      sms_service_id: None,
      sms_network: None,
      sms_country: None,
    };

    assert!(config.validate_network().is_ok());
    assert!(config.effective_proxy_id().is_none());
    assert!(config.effective_vpn_id().is_none());
    assert!(!config.uses_nord_cli());
  }
}
