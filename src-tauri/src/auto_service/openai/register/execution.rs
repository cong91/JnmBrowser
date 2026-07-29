use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tauri::AppHandle;

use super::engine::RegistrationEngine;
use super::types::{RegistrationConfig, RegistrationResult};
use crate::email::{build_email_service, EmailProvider};
use crate::profile::BrowserProfile;
use crate::profile_runtime::{LeaseCleanupGuard, LeaseError, LeaseRegistry, RuntimeLease};
use crate::sms::viotp::ViotpService;
use crate::sms::SmsService;

const SELECTED_PROFILE_BUSY_ERROR: &str =
  "Selected profile is busy; wait for the current automation task to finish or stop it before retrying";

type RuntimeCleanupFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
type RuntimeCleanupAction = Arc<dyn Fn() -> RuntimeCleanupFuture + Send + Sync>;

struct RegistrationRuntimeCleanupGuard {
  action: Option<RuntimeCleanupAction>,
}

impl RegistrationRuntimeCleanupGuard {
  fn new(action: RuntimeCleanupAction) -> Self {
    Self {
      action: Some(action),
    }
  }

  async fn close(&mut self) -> Result<(), String> {
    let Some(action) = self.action.as_ref().cloned() else {
      return Ok(());
    };
    action().await?;
    self.action.take();
    Ok(())
  }
}

impl Drop for RegistrationRuntimeCleanupGuard {
  fn drop(&mut self) {
    let Some(action) = self.action.take() else {
      return;
    };
    std::thread::spawn(move || {
      let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
      else {
        log::error!("Dropped registration cleanup without a runtime");
        return;
      };
      if runtime.block_on(action()).is_err() {
        log::error!("Dropped registration runtime cleanup failed");
      }
    });
  }
}

struct SelectedRuntimeCleanupContext {
  profile: BrowserProfile,
  policy: crate::profile_runtime::LaunchPolicy,
  lease: RuntimeLease,
  lease_registry: &'static LeaseRegistry,
}

fn release_selected_lease_after_cleanup(
  registry: &'static LeaseRegistry,
  lease_id: &str,
  cleanup_result: Result<(), String>,
) -> Result<(), String> {
  cleanup_result?;
  registry.release(lease_id);
  Ok(())
}

async fn cleanup_selected_runtime(
  app_handle: &AppHandle,
  context: &Arc<Mutex<SelectedRuntimeCleanupContext>>,
) -> Result<(), String> {
  let (profile, policy, mut lease) = {
    let context = context
      .lock()
      .map_err(|_| "registration cleanup context lock poisoned".to_string())?;
    (
      context.profile.clone(),
      context.policy.clone(),
      context.lease.clone(),
    )
  };

  crate::browser_runner::BrowserRunner::instance()
    .kill_runtime_browser(app_handle.clone(), &profile, &policy, &mut lease)
    .await
    .map_err(|error| format!("Registration runtime cleanup failed: {error}"))?;

  let mut context = context
    .lock()
    .map_err(|_| "registration cleanup context lock poisoned".to_string())?;
  context.lease = lease;
  release_selected_lease_after_cleanup(context.lease_registry, &context.lease.lease_id, Ok(()))
}

/// A validated registration run ready to be moved onto its dedicated runtime.
pub struct PreparedRegistration {
  engine: RegistrationEngine,
  email_provider: EmailProvider,
  sms_token: Option<String>,
  selected_profile: Option<BrowserProfile>,
  selected_lease: Option<RuntimeLease>,
  lease_registry: &'static LeaseRegistry,
  _lease_guard: Option<LeaseCleanupGuard>,
}

impl std::fmt::Debug for PreparedRegistration {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("PreparedRegistration")
      .field("task_id", &self.task_id())
      .field(
        "selected_profile_id",
        &self.selected_profile.as_ref().map(|profile| profile.id),
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

fn selected_profile_lease_error(error: LeaseError) -> String {
  match error {
    LeaseError::Busy { .. } => SELECTED_PROFILE_BUSY_ERROR.to_string(),
    LeaseError::InvalidSourceProfileId => "Selected source profile ID is invalid".to_string(),
  }
}

fn resolve_selected_profile(
  config: &RegistrationConfig,
  profiles_dir: &std::path::Path,
) -> Result<Option<BrowserProfile>, String> {
  let Some(profile_id) = config.profile_id.as_deref() else {
    return Ok(None);
  };
  let profile_uuid = uuid::Uuid::parse_str(profile_id)
    .map_err(|_| "Selected source profile ID is not a valid UUID".to_string())?;
  let metadata_path = profiles_dir
    .join(profile_uuid.to_string())
    .join("metadata.json");
  let metadata = std::fs::read(&metadata_path).map_err(|error| match error.kind() {
    std::io::ErrorKind::NotFound => "Selected source profile was not found".to_string(),
    _ => format!("Failed to read selected source profile: {error}"),
  })?;
  let profile: BrowserProfile = serde_json::from_slice(&metadata)
    .map_err(|error| format!("Selected source profile metadata is invalid: {error}"))?;
  if profile.id != profile_uuid {
    return Err("Selected source profile metadata ID does not match its directory".to_string());
  }
  if profile
    .process_id
    .is_some_and(crate::proxy_storage::is_process_running)
  {
    return Err(
      "Selected source profile is running; stop it before starting Auto Registration".to_string(),
    );
  }
  if profile.is_cross_os() {
    return Err("Selected source profile was created on a different operating system".to_string());
  }
  if profile.browser != config.browser_type {
    return Err(format!(
      "Selected source profile uses {}, but Auto Registration requested {}",
      profile.browser, config.browser_type
    ));
  }
  if config.data_mode == crate::profile_runtime::DataMode::Ephemeral
    && matches!(profile.browser.as_str(), "camoufox" | "firefox")
  {
    return Err(
      "Selected Camoufox/Firefox profiles do not support ephemeral data for Auto Registration"
        .to_string(),
    );
  }

  Ok(Some(profile))
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
  let selected_profile = resolve_selected_profile(&config, profiles_dir)?;
  let selected_lease = selected_profile
    .as_ref()
    .map(|profile| {
      lease_registry
        .try_acquire(&profile.id.to_string())
        .map_err(selected_profile_lease_error)
    })
    .transpose()?;
  let lease_guard = selected_lease
    .as_ref()
    .cloned()
    .map(|lease| LeaseCleanupGuard::with_registry(lease_registry, lease));
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
    selected_profile,
    selected_lease,
    lease_registry,
    _lease_guard: lease_guard,
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

  let lease_registry = prepared.lease_registry;
  let cleanup_context = match (
    prepared.selected_profile.take(),
    prepared.selected_lease.take(),
    prepared._lease_guard.take(),
  ) {
    (Some(profile), Some(lease), Some(lease_guard)) => {
      prepared
        .engine
        .install_selected_runtime(profile.clone(), lease.clone());
      let policy = prepared
        .engine
        .selected_runtime()
        .map(|(_, policy, _)| policy)
        .ok_or_else(|| "Selected registration runtime state is incomplete".to_string())?;
      let _ = lease_guard.into_lease_id();
      Some(Arc::new(Mutex::new(SelectedRuntimeCleanupContext {
        profile,
        policy,
        lease,
        lease_registry,
      })))
    }
    (None, None, None) => None,
    _ => return Err("Selected registration runtime state is incomplete".to_string()),
  };

  let cleanup_action = cleanup_context.as_ref().map(|context| {
    let context = context.clone();
    let app_handle = app_handle.clone();
    Arc::new(move || {
      let context = context.clone();
      let app_handle = app_handle.clone();
      Box::pin(async move { cleanup_selected_runtime(&app_handle, &context).await })
        as RuntimeCleanupFuture
    }) as RuntimeCleanupAction
  });
  let mut cleanup_guard = cleanup_action.map(RegistrationRuntimeCleanupGuard::new);

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

  fn write_selected_profile(
    profiles_dir: &std::path::Path,
    browser: &str,
  ) -> (crate::profile::BrowserProfile, std::path::PathBuf, Vec<u8>) {
    let profile = crate::profile::BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: "selected-registration-profile".to_string(),
      browser: browser.to_string(),
      version: "1.0.0".to_string(),
      host_os: Some(crate::profile::types::get_host_os()),
      ..crate::profile::BrowserProfile::default()
    };
    let profile_dir = profiles_dir.join(profile.id.to_string());
    std::fs::create_dir_all(&profile_dir).expect("create profile directory");
    let metadata_path = profile_dir.join("metadata.json");
    let metadata = serde_json::to_vec_pretty(&profile).expect("serialize profile");
    std::fs::write(&metadata_path, &metadata).expect("write profile metadata");
    (profile, metadata_path, metadata)
  }

  fn leaked_registry() -> &'static crate::profile_runtime::LeaseRegistry {
    Box::leak(Box::new(crate::profile_runtime::LeaseRegistry::new()))
  }

  #[test]
  fn selected_profile_preparation_is_read_only() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let (profile, metadata_path, metadata) = write_selected_profile(temp.path(), "chromium");
    let mut config = base_config();
    config.profile_id = Some(profile.id.to_string());

    let prepared = prepare_registration_in(
      config,
      Arc::new(AtomicBool::new(false)),
      temp.path(),
      leaked_registry(),
    )
    .expect("selected profile should prepare");

    assert_eq!(std::fs::read(metadata_path).unwrap(), metadata);
    drop(prepared);
  }

  #[test]
  fn selected_profile_preparation_rejects_missing_profile() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let mut config = base_config();
    config.profile_id = Some(uuid::Uuid::new_v4().to_string());

    let error = prepare_registration_in(
      config,
      Arc::new(AtomicBool::new(false)),
      temp.path(),
      leaked_registry(),
    )
    .expect_err("missing selected profile must fail");

    assert!(error.contains("not found"));
  }

  #[test]
  fn selected_profile_preparation_rejects_running_profile() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let (mut profile, metadata_path, _) = write_selected_profile(temp.path(), "chromium");
    profile.process_id = Some(std::process::id());
    std::fs::write(
      metadata_path,
      serde_json::to_vec_pretty(&profile).expect("serialize running profile"),
    )
    .expect("write running profile");
    let mut config = base_config();
    config.profile_id = Some(profile.id.to_string());

    let error = prepare_registration_in(
      config,
      Arc::new(AtomicBool::new(false)),
      temp.path(),
      leaked_registry(),
    )
    .expect_err("running selected profile must fail");

    assert!(error.contains("running"));
  }

  #[test]
  fn selected_profile_preparation_rejects_ephemeral_camoufox() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let (profile, _, _) = write_selected_profile(temp.path(), "camoufox");
    let mut config = base_config();
    config.profile_id = Some(profile.id.to_string());
    config.browser_type = "camoufox".to_string();

    let error = prepare_registration_in(
      config,
      Arc::new(AtomicBool::new(false)),
      temp.path(),
      leaked_registry(),
    )
    .expect_err("ephemeral Camoufox reuse must fail closed");

    assert!(error.contains("ephemeral data"));
  }

  #[test]
  fn selected_profile_preparation_fails_fast_with_stable_busy_error() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let (profile, _, _) = write_selected_profile(temp.path(), "chromium");
    let registry = leaked_registry();
    let mut first_config = base_config();
    first_config.profile_id = Some(profile.id.to_string());
    let mut second_config = base_config();
    second_config.profile_id = Some(profile.id.to_string());

    let first = prepare_registration_in(
      first_config,
      Arc::new(AtomicBool::new(false)),
      temp.path(),
      registry,
    )
    .expect("first preparation should acquire lease");
    let error = prepare_registration_in(
      second_config,
      Arc::new(AtomicBool::new(false)),
      temp.path(),
      registry,
    )
    .expect_err("second preparation should fail immediately");

    assert_eq!(error, SELECTED_PROFILE_BUSY_ERROR);
    assert!(!error.contains("lease"));
    drop(first);
  }

  #[test]
  fn dropping_prepared_registration_releases_selected_profile_lease() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let (profile, _, _) = write_selected_profile(temp.path(), "chromium");
    let registry = leaked_registry();
    let profile_id = profile.id.to_string();
    let mut config = base_config();
    config.profile_id = Some(profile_id.clone());

    let prepared = prepare_registration_in(
      config,
      Arc::new(AtomicBool::new(false)),
      temp.path(),
      registry,
    )
    .expect("selected profile should prepare");
    assert!(registry.is_leased(&profile_id));

    drop(prepared);
    assert!(!registry.is_leased(&profile_id));
  }

  #[test]
  fn selected_lease_releases_only_after_cleanup_succeeds() {
    let registry = leaked_registry();
    let profile_id = uuid::Uuid::new_v4().to_string();
    let lease = registry.try_acquire(&profile_id).expect("acquire lease");

    let error = release_selected_lease_after_cleanup(
      registry,
      &lease.lease_id,
      Err("cleanup failed".to_string()),
    )
    .expect_err("failed cleanup must retain lease");
    assert_eq!(error, "cleanup failed");
    assert!(registry.is_leased(&profile_id));

    release_selected_lease_after_cleanup(registry, &lease.lease_id, Ok(()))
      .expect("successful retry releases lease");
    assert!(!registry.is_leased(&profile_id));
  }

  #[tokio::test]
  async fn runtime_cleanup_guard_retries_after_explicit_cleanup_failure() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_action = attempts.clone();
    let (retried_tx, retried_rx) = std::sync::mpsc::channel();
    let action = Arc::new(move || {
      let attempts = attempts_for_action.clone();
      let retried_tx = retried_tx.clone();
      Box::pin(async move {
        let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt == 1 {
          Err("first cleanup failed".to_string())
        } else {
          let _ = retried_tx.send(());
          Ok(())
        }
      }) as RuntimeCleanupFuture
    });
    let mut guard = RegistrationRuntimeCleanupGuard::new(action);

    assert!(guard.close().await.is_err());
    drop(guard);

    retried_rx
      .recv_timeout(std::time::Duration::from_secs(2))
      .expect("drop should retry cleanup");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
  }

  #[tokio::test]
  async fn runtime_cleanup_guard_does_not_repeat_successful_close() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_action = attempts.clone();
    let action = Arc::new(move || {
      let attempts = attempts_for_action.clone();
      Box::pin(async move {
        attempts.fetch_add(1, Ordering::SeqCst);
        Ok(())
      }) as RuntimeCleanupFuture
    });
    let mut guard = RegistrationRuntimeCleanupGuard::new(action);

    guard.close().await.expect("cleanup should succeed");
    drop(guard);

    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
  }

  #[test]
  fn prepared_registration_exposes_engine_task_id() {
    let prepared = prepare_registration(base_config(), Arc::new(AtomicBool::new(false)))
      .expect("config should prepare");

    assert!(!prepared.task_id().is_empty());
  }
}
