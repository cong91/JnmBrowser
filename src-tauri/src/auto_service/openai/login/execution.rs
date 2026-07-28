use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use tauri::AppHandle;

use super::engine::LoginEngine;
use super::types::{LoginConfig, LoginResult};
use crate::settings_manager::SettingsManager;
use crate::sms::viotp::ViotpService;
use crate::sms::SmsService;

/// A validated login run ready to be moved onto its dedicated runtime.
pub struct PreparedLogin {
  engine: LoginEngine,
  sms_token: Option<String>,
}

impl PreparedLogin {
  pub fn task_id(&self) -> &str {
    self.engine.task_id()
  }
}

fn normalized_optional(value: Option<&str>) -> Option<String> {
  value
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .map(str::to_string)
}

fn select_login_sms_token(
  uses_viotp: bool,
  override_token: Option<&str>,
  stored_token: Option<&str>,
) -> Option<String> {
  if !uses_viotp {
    return None;
  }

  normalized_optional(override_token).or_else(|| normalized_optional(stored_token))
}

fn apply_login_settings(
  config: &mut LoginConfig,
  stored_sms_token: Option<&str>,
  stored_sub2api: Option<(&str, &str)>,
) {
  config.sms_token = select_login_sms_token(
    config.uses_viotp(),
    config.sms_token.as_deref(),
    stored_sms_token,
  );

  if let Some((stored_url, stored_api_key)) = stored_sub2api {
    if config.sub2api_url.trim().is_empty() {
      config.sub2api_url = stored_url.trim().to_string();
    }
    if config.sub2api_api_key.trim().is_empty() {
      config.sub2api_api_key = stored_api_key.trim().to_string();
    }
  }
}

/// Parse, normalize, resolve persisted settings, validate and construct the engine.
pub async fn prepare_login(
  app_handle: &AppHandle,
  mut config: LoginConfig,
  cancel_flag: Arc<AtomicBool>,
) -> Result<PreparedLogin, String> {
  config.parse_credentials();
  config.normalize();

  let manager = SettingsManager::instance();
  let stored_sms_token =
    if config.uses_viotp() && normalized_optional(config.sms_token.as_deref()).is_none() {
      manager.get_sms_api_token(app_handle).await.ok().flatten()
    } else {
      None
    };
  let stored_sub2api =
    if config.sub2api_url.trim().is_empty() || config.sub2api_api_key.trim().is_empty() {
      Some(manager.get_sub2api_settings(app_handle).await)
    } else {
      None
    };

  apply_login_settings(
    &mut config,
    stored_sms_token.as_deref(),
    stored_sub2api
      .as_ref()
      .map(|(url, api_key)| (url.as_str(), api_key.as_str())),
  );
  config.validate()?;

  let sms_token = config.sms_token.clone();
  let engine = LoginEngine::with_cancel_flag(config, cancel_flag);
  Ok(PreparedLogin { engine, sms_token })
}

/// Construct provider services and run the engine on a runtime dedicated to this task.
pub fn run_prepared_login(
  app_handle: AppHandle,
  mut prepared: PreparedLogin,
) -> Result<Vec<LoginResult>, String> {
  let viotp = prepared.sms_token.map(ViotpService::new);
  let sms_service: Option<&dyn SmsService> =
    viotp.as_ref().map(|service| service as &dyn SmsService);
  let runtime = tokio::runtime::Runtime::new()
    .map_err(|error| format!("Failed to create login runtime: {error}"))?;

  Ok(runtime.block_on(async { prepared.engine.run(app_handle, sms_service).await }))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn base_config() -> LoginConfig {
    serde_json::from_value(serde_json::json!({
      "credentialsText": "user@example.com|password|secret"
    }))
    .expect("minimal login config must deserialize")
  }

  #[test]
  fn shared_settings_prefer_override_then_encrypted_sms_token() {
    let mut config = base_config();
    config.sms_provider = Some("VIOTP".into());
    config.sms_token = Some(" override ".into());
    config.sms_service_id = Some(1);
    config.normalize();

    apply_login_settings(&mut config, Some("stored"), None);
    assert_eq!(config.sms_token.as_deref(), Some("override"));

    config.sms_token = Some(" ".into());
    apply_login_settings(&mut config, Some(" stored "), None);
    assert_eq!(config.sms_token.as_deref(), Some("stored"));
  }

  #[test]
  fn shared_settings_fill_only_missing_sub2api_values() {
    let mut config = base_config();
    config.sub2api_url = " https://configured.example ".into();

    apply_login_settings(
      &mut config,
      None,
      Some(("https://stored.example", " stored-key ")),
    );

    assert_eq!(config.sub2api_url, " https://configured.example ");
    assert_eq!(config.sub2api_api_key, "stored-key");
  }

  #[test]
  fn shared_preparation_parses_and_validates_credentials() {
    let mut config = base_config();
    config.parse_credentials();
    config.normalize();
    apply_login_settings(&mut config, None, None);

    assert_eq!(config.credentials.len(), 1);
    assert!(config.validate().is_ok());
  }
}
