use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use tauri::AppHandle;

use super::engine::RegistrationEngine;
use super::selected_profile_runtime::{
  prepare_selected_profile_runtime, PreparedSelectedProfileRuntime,
};
use super::types::{RegistrationConfig, RegistrationResult};
use crate::email::{build_email_service, EmailProvider};
use crate::profile_runtime::LeaseRegistry;
use crate::sms::viotp::ViotpService;
use crate::sms::SmsService;

/// A validated registration run ready to be moved onto its dedicated runtime.
pub struct PreparedRegistration {
  engine: RegistrationEngine,
  email_provider: EmailProvider,
  sms_token: Option<String>,
  selected_runtime: Option<PreparedSelectedProfileRuntime>,
}

impl std::fmt::Debug for PreparedRegistration {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("PreparedRegistration")
      .field("task_id", &self.task_id())
      .field(
        "selected_profile_id",
        &self
          .selected_runtime
          .as_ref()
          .map(PreparedSelectedProfileRuntime::profile_id),
      )
      .finish_non_exhaustive()
  }
}

impl PreparedRegistration {
  pub fn task_id(&self) -> &str {
    self.engine.task_id()
  }
}

fn trim_optional(value: &mut Option<String>) {
  *value = value
    .take()
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty());
}

/// Normalize and validate every startup path before the engine can reserve work.
pub fn normalize_and_validate_registration(config: &mut RegistrationConfig) -> Result<(), String> {
  let sms_configuration_present = config.sms_provider.is_some()
    || config.sms_token.is_some()
    || config.sms_service_id.is_some()
    || config.sms_network.is_some()
    || config.sms_country.is_some();

  config.cdks = config
    .cdks
    .drain(..)
    .map(|cdk| cdk.trim().to_string())
    .collect();
  config.browser_type = config.browser_type.trim().to_ascii_lowercase();
  trim_optional(&mut config.profile_id);
  trim_optional(&mut config.proxy_id);
  trim_optional(&mut config.vpn_id);
  trim_optional(&mut config.nord_group);
  trim_optional(&mut config.nord_server_name);
  trim_optional(&mut config.nord_cli_path);
  trim_optional(&mut config.sms_provider);
  trim_optional(&mut config.sms_token);
  trim_optional(&mut config.sms_network);
  trim_optional(&mut config.sms_country);
  if let Some(provider) = config.sms_provider.as_mut() {
    provider.make_ascii_lowercase();
  }
  if let Some(country) = config.sms_country.as_mut() {
    country.make_ascii_lowercase();
  }

  if config.cdks.is_empty() {
    return Err("Auto-registration requires at least one CDK/card".into());
  }
  config.validate_cdks()?;

  if !matches!(
    config.browser_type.as_str(),
    "chromium" | "camoufox" | "firefox"
  ) {
    return Err(format!(
      "Unsupported browser '{}'; expected chromium, camoufox, or firefox",
      config.browser_type
    ));
  }

  config.normalize_network();
  config.validate_network()?;

  if config.profile_id.is_some() && config.concurrency > 1 {
    return Err("profileId cannot be used with concurrency greater than 1".into());
  }

  match config.sms_provider.as_deref() {
    None if sms_configuration_present => {
      return Err("SMS configuration requires smsProvider".into());
    }
    None => {}
    Some("viotp") => {
      if config.sms_token.is_none() {
        return Err("VI-OTP SMS configuration requires a non-empty token".into());
      }
      if !matches!(config.sms_service_id, Some(service_id) if service_id > 0) {
        return Err("VI-OTP SMS configuration requires a positive smsServiceId".into());
      }
      if let Some(country) = config.sms_country.as_deref() {
        if !matches!(country, "vn" | "la") {
          return Err("VI-OTP smsCountry must be 'vn' or 'la'".into());
        }
      }
    }
    Some(provider) => {
      return Err(format!(
        "Unsupported SMS provider '{provider}'; expected viotp"
      ));
    }
  }

  Ok(())
}

/// Prepare the engine once so the command can publish its real task ID before execution.
pub fn prepare_registration(
  config: RegistrationConfig,
  cancel_flag: Arc<AtomicBool>,
) -> Result<PreparedRegistration, String> {
  prepare_registration_in(
    config,
    cancel_flag,
    &crate::profile::ProfileManager::instance().get_profiles_dir(),
    LeaseRegistry::global(),
  )
}

fn prepare_registration_in(
  mut config: RegistrationConfig,
  cancel_flag: Arc<AtomicBool>,
  profiles_dir: &std::path::Path,
  lease_registry: &'static LeaseRegistry,
) -> Result<PreparedRegistration, String> {
  normalize_and_validate_registration(&mut config)?;
  let selected_runtime = prepare_selected_profile_runtime(&config, profiles_dir, lease_registry)?;
  let email_provider = config.email_provider;
  let sms_token = config.sms_provider.as_ref().map(|_| {
    config
      .sms_token
      .clone()
      .expect("validated VI-OTP configuration must contain a token")
  });
  let engine = RegistrationEngine::with_cancel_flag(config, cancel_flag);

  Ok(PreparedRegistration {
    engine,
    email_provider,
    sms_token,
    selected_runtime,
  })
}

/// Construct provider services and run the engine on a runtime dedicated to this task.
pub fn run_prepared_registration(
  app_handle: AppHandle,
  mut prepared: PreparedRegistration,
) -> Result<RegistrationResult, String> {
  let email_service = build_email_service(prepared.email_provider);
  let viotp = prepared.sms_token.map(ViotpService::new);
  let sms_service: Option<&dyn SmsService> =
    viotp.as_ref().map(|service| service as &dyn SmsService);
  let runtime = tokio::runtime::Runtime::new()
    .map_err(|error| format!("Failed to create registration runtime: {error}"))?;

  let mut cleanup_guard = if let Some(selected_runtime) = prepared.selected_runtime.take() {
    prepared
      .engine
      .install_selected_runtime(selected_runtime.runtime());
    let guard = selected_runtime.cleanup_guard(app_handle.clone());
    selected_runtime.disarm_preparation_guard();
    Some(guard)
  } else {
    None
  };

  let result = runtime.block_on(async {
    prepared
      .engine
      .run(app_handle.clone(), email_service.as_ref(), sms_service)
      .await
  });

  if let Some(guard) = cleanup_guard.as_mut() {
    if let Err(error) = runtime.block_on(guard.close()) {
      prepared
        .engine
        .emit_deferred_batch_terminal(&app_handle, &result, false);
      return Err(error);
    }
    prepared
      .engine
      .emit_deferred_batch_terminal(&app_handle, &result, true);
  }
  Ok(result)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::auto_service::openai::register::types::NetworkMode;

  fn base_config() -> RegistrationConfig {
    serde_json::from_value(serde_json::json!({ "cdks": ["GMAIL-TEST"] }))
      .expect("minimal registration config must deserialize")
  }

  #[test]
  fn shared_preparation_normalizes_product_defaults() {
    let mut config = base_config();
    config.browser_type = " Chromium ".into();
    config.proxy_id = Some(" proxy-1 ".into());

    normalize_and_validate_registration(&mut config).expect("config should be valid");

    assert_eq!(config.browser_type, "chromium");
    assert_eq!(config.network_mode, NetworkMode::Proxy);
    assert_eq!(config.proxy_id.as_deref(), Some("proxy-1"));
    assert_eq!(config.max_retries, 3);
    assert_eq!(config.concurrency, 1);
  }

  #[test]
  fn shared_preparation_rejects_empty_cdk_list() {
    let mut config = base_config();
    config.cdks.clear();

    assert!(normalize_and_validate_registration(&mut config)
      .unwrap_err()
      .contains("at least one CDK"));
  }

  #[test]
  fn shared_preparation_rejects_unknown_browser() {
    let mut config = base_config();
    config.browser_type = "edge".into();

    assert!(normalize_and_validate_registration(&mut config)
      .unwrap_err()
      .contains("Unsupported browser"));
  }

  #[test]
  fn shared_preparation_keeps_no_sms_path_valid() {
    let mut config = base_config();
    assert!(normalize_and_validate_registration(&mut config).is_ok());
  }

  #[test]
  fn shared_preparation_requires_complete_supported_sms_configuration() {
    let mut config = base_config();
    config.sms_provider = Some("viotp".into());
    assert!(normalize_and_validate_registration(&mut config)
      .unwrap_err()
      .contains("non-empty token"));

    let mut config = base_config();
    config.sms_provider = Some("other".into());
    config.sms_token = Some("token".into());
    config.sms_service_id = Some(1);
    assert!(normalize_and_validate_registration(&mut config)
      .unwrap_err()
      .contains("Unsupported SMS provider"));

    let mut config = base_config();
    config.sms_provider = Some("VIOTP".into());
    config.sms_token = Some(" token ".into());
    config.sms_service_id = Some(1);
    config.sms_country = Some("VN".into());
    assert!(normalize_and_validate_registration(&mut config).is_ok());
    assert_eq!(config.sms_provider.as_deref(), Some("viotp"));
    assert_eq!(config.sms_token.as_deref(), Some("token"));
    assert_eq!(config.sms_country.as_deref(), Some("vn"));
  }

  #[test]
  fn shared_preparation_rejects_sms_fields_without_provider() {
    let mut config = base_config();
    config.sms_service_id = Some(1);

    assert!(normalize_and_validate_registration(&mut config)
      .unwrap_err()
      .contains("requires smsProvider"));
  }

  #[test]
  fn shared_preparation_rejects_shared_profile_concurrency() {
    let mut config = base_config();
    config.profile_id = Some("profile-1".into());
    config.concurrency = 2;

    assert!(normalize_and_validate_registration(&mut config)
      .unwrap_err()
      .contains("profileId"));
  }

  #[test]
  fn prepared_registration_exposes_engine_task_id() {
    let prepared = prepare_registration(base_config(), Arc::new(AtomicBool::new(false)))
      .expect("config should prepare");

    assert!(!prepared.task_id().is_empty());
  }
}
