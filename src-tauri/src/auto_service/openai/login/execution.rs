use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tauri::AppHandle;

use super::engine::LoginEngine;
use super::types::{LoginConfig, LoginResult};
use crate::profile::BrowserProfile;
use crate::profile_runtime::{LeaseCleanupGuard, LeaseError, LeaseRegistry, RuntimeLease};
use crate::settings_manager::SettingsManager;
use crate::sms::viotp::ViotpService;
use crate::sms::SmsService;

const SELECTED_PROFILE_BUSY_ERROR: &str =
  "Selected profile is busy; wait for the current automation task to finish or stop it before retrying";

type RuntimeCleanupFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
type RuntimeCleanupAction = Arc<dyn Fn() -> RuntimeCleanupFuture + Send + Sync>;

struct LoginRuntimeCleanupGuard {
  action: Option<RuntimeCleanupAction>,
}

impl LoginRuntimeCleanupGuard {
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

impl Drop for LoginRuntimeCleanupGuard {
  fn drop(&mut self) {
    let Some(action) = self.action.take() else {
      return;
    };
    std::thread::spawn(move || {
      let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
      else {
        log::error!("Dropped Auto Login cleanup without a runtime");
        return;
      };
      if runtime.block_on(action()).is_err() {
        log::error!("Dropped Auto Login runtime cleanup failed");
      }
    });
  }
}

struct WorkerRuntimeCleanupContext {
  profile: BrowserProfile,
  policy: crate::profile_runtime::LaunchPolicy,
  lease: RuntimeLease,
  lease_registry: &'static LeaseRegistry,
}

fn release_worker_lease_after_cleanup(
  registry: &'static LeaseRegistry,
  lease_id: &str,
  cleanup_result: Result<(), String>,
) -> Result<(), String> {
  cleanup_result?;
  registry.release(lease_id);
  Ok(())
}

async fn cleanup_worker_runtime(
  app_handle: &AppHandle,
  context: &Arc<Mutex<WorkerRuntimeCleanupContext>>,
) -> Result<(), String> {
  let (profile, policy, mut lease) = {
    let context = context
      .lock()
      .map_err(|_| "Auto Login cleanup context lock poisoned".to_string())?;
    (
      context.profile.clone(),
      context.policy.clone(),
      context.lease.clone(),
    )
  };
  crate::browser_runner::BrowserRunner::instance()
    .kill_runtime_browser(app_handle.clone(), &profile, &policy, &mut lease)
    .await
    .map_err(|error| format!("Auto Login runtime cleanup failed: {error}"))?;

  let mut context = context
    .lock()
    .map_err(|_| "Auto Login cleanup context lock poisoned".to_string())?;
  context.lease = lease;
  release_worker_lease_after_cleanup(context.lease_registry, &context.lease.lease_id, Ok(()))
}

/// A validated login run ready to be moved onto its dedicated runtime.
pub struct PreparedLogin {
  engine: LoginEngine,
  sms_token: Option<String>,
  worker_profile: BrowserProfile,
  runtime_lease: RuntimeLease,
  lease_registry: &'static LeaseRegistry,
  _lease_guard: LeaseCleanupGuard,
}

impl std::fmt::Debug for PreparedLogin {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("PreparedLogin")
      .field("task_id", &self.task_id())
      .field("worker_profile_id", &self.worker_profile.id)
      .finish_non_exhaustive()
  }
}

impl PreparedLogin {
  pub fn task_id(&self) -> &str {
    self.engine.task_id()
  }
}

fn selected_profile_lease_error(error: LeaseError) -> String {
  match error {
    LeaseError::Busy { .. } => SELECTED_PROFILE_BUSY_ERROR.to_string(),
    LeaseError::InvalidSourceProfileId => "Selected source profile ID is invalid".to_string(),
  }
}

fn default_worker_name(browser: &str) -> String {
  format!("auto-login-worker-{browser}")
}

fn select_exact_default_worker(
  profiles: &[BrowserProfile],
  browser: &str,
) -> Option<BrowserProfile> {
  let stable_name = default_worker_name(browser);
  profiles
    .iter()
    .find(|profile| {
      profile.browser.eq_ignore_ascii_case(browser)
        && profile.name.eq_ignore_ascii_case(&stable_name)
    })
    .cloned()
}

fn validate_login_profile(
  config: &LoginConfig,
  profile: BrowserProfile,
  explicit_selection: bool,
) -> Result<BrowserProfile, String> {
  if profile
    .process_id
    .is_some_and(crate::proxy_storage::is_process_running)
  {
    return Err("Selected source profile is running; stop it before starting Auto Login".into());
  }
  if profile.is_cross_os() {
    return Err("Selected source profile was created on a different operating system".into());
  }
  if profile.browser != config.browser_type {
    return Err(format!(
      "Selected source profile uses {}, but Auto Login requested {}",
      profile.browser, config.browser_type
    ));
  }
  if explicit_selection
    && config.data_mode == crate::profile_runtime::DataMode::Ephemeral
    && matches!(profile.browser.as_str(), "camoufox" | "firefox")
  {
    return Err(
      "Selected Camoufox/Firefox profiles do not support ephemeral data for Auto Login".into(),
    );
  }
  Ok(profile)
}

fn read_login_profiles(profiles_dir: &std::path::Path) -> Result<Vec<BrowserProfile>, String> {
  if !profiles_dir.exists() {
    return Ok(Vec::new());
  }
  let mut profiles = Vec::new();
  for entry in std::fs::read_dir(profiles_dir)
    .map_err(|error| format!("Failed to read browser profiles: {error}"))?
  {
    let entry = entry.map_err(|error| format!("Failed to read browser profile entry: {error}"))?;
    let metadata_path = entry.path().join("metadata.json");
    if !metadata_path.is_file() {
      continue;
    }
    let metadata = std::fs::read(&metadata_path)
      .map_err(|error| format!("Failed to read browser profile metadata: {error}"))?;
    let profile = serde_json::from_slice(&metadata)
      .map_err(|error| format!("Browser profile metadata is invalid: {error}"))?;
    profiles.push(profile);
  }
  Ok(profiles)
}

fn resolve_login_browser_version(
  browser: &str,
  profiles: &[BrowserProfile],
) -> Result<(String, String), String> {
  if let Some(profile) = profiles
    .iter()
    .find(|profile| profile.browser.eq_ignore_ascii_case(browser) && !profile.version.is_empty())
  {
    return Ok((profile.version.clone(), profile.release_type.clone()));
  }

  let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
  let _ = registry.load();
  let mut versions = registry.get_downloaded_versions(browser);
  versions.sort_by(|left, right| {
    crate::api_client::VersionComponent::parse(right)
      .cmp(&crate::api_client::VersionComponent::parse(left))
  });
  if let Some(version) = versions.into_iter().next() {
    return Ok((version, "stable".to_string()));
  }
  if browser == "camoufox" {
    return Ok(("v135.0.1-beta.24".to_string(), "stable".to_string()));
  }
  if browser == "firefox" {
    return Ok(("firefox".to_string(), "stable".to_string()));
  }
  Err(format!(
    "No downloaded {browser} version found. Install the browser in JnmBrowser first."
  ))
}

async fn create_default_login_worker(
  app_handle: &AppHandle,
  config: &LoginConfig,
  profiles: &[BrowserProfile],
) -> Result<BrowserProfile, String> {
  let browser = crate::browser::BrowserType::from_str(&config.browser_type)
    .map_err(|error| format!("Invalid browser type: {error}"))?;
  let (version, release_type) = resolve_login_browser_version(&config.browser_type, profiles)?;
  let camoufox_config = matches!(config.browser_type.as_str(), "camoufox" | "firefox").then(|| {
    crate::camoufox_manager::CamoufoxConfig {
      fingerprint: None,
      randomize_fingerprint_on_launch: Some(true),
      geoip: Some(serde_json::Value::Bool(true)),
      ..Default::default()
    }
  });
  let chromium_config =
    (config.browser_type == "chromium").then(|| crate::chromium_manager::ChromiumConfig {
      fingerprint: None,
      randomize_fingerprint_on_launch: Some(true),
      ..Default::default()
    });

  crate::profile::manager::create_browser_profile_with_group(
    app_handle.clone(),
    default_worker_name(&config.browser_type),
    browser.as_str().to_string(),
    version,
    release_type,
    None,
    None,
    camoufox_config,
    chromium_config,
    None,
    true,
    None,
    None,
  )
  .await
  .map_err(|error| format!("Create Auto Login worker: {error}"))
}

fn resolve_explicit_login_profile(
  config: &LoginConfig,
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
    return Err("Selected source profile metadata ID does not match its directory".into());
  }
  validate_login_profile(config, profile, true).map(Some)
}

fn prepare_login_runtime(
  config: LoginConfig,
  cancel_flag: Arc<AtomicBool>,
  worker_profile: BrowserProfile,
  lease_registry: &'static LeaseRegistry,
) -> Result<PreparedLogin, String> {
  let runtime_lease = lease_registry
    .try_acquire(&worker_profile.id.to_string())
    .map_err(selected_profile_lease_error)?;
  let lease_guard = LeaseCleanupGuard::with_registry(lease_registry, runtime_lease.clone());
  let sms_token = config.sms_token.clone();
  let engine = LoginEngine::with_cancel_flag(config, cancel_flag);
  Ok(PreparedLogin {
    engine,
    sms_token,
    worker_profile,
    runtime_lease,
    lease_registry,
    _lease_guard: lease_guard,
  })
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
  config.browser_type = config.browser_type.trim().to_ascii_lowercase();
  config.profile_id = normalized_optional(config.profile_id.as_deref());
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

  let profile_manager = crate::profile::ProfileManager::instance();
  let profiles_dir = profile_manager.get_profiles_dir();
  let (worker_profile, mut preparation_guard) =
    if let Some(profile) = resolve_explicit_login_profile(&config, &profiles_dir)? {
      (profile, None)
    } else {
      let preparation_source = format!("auto-login-default:{}", config.browser_type);
      let preparation_lease = LeaseRegistry::global()
        .try_acquire(&preparation_source)
        .map_err(selected_profile_lease_error)?;
      let preparation_guard = LeaseCleanupGuard::new(preparation_lease);
      let profiles = read_login_profiles(&profiles_dir)?;
      let profile =
        if let Some(profile) = select_exact_default_worker(&profiles, &config.browser_type) {
          validate_login_profile(&config, profile, false)?
        } else {
          create_default_login_worker(app_handle, &config, &profiles).await?
        };
      (profile, Some(preparation_guard))
    };

  let prepared =
    prepare_login_runtime(config, cancel_flag, worker_profile, LeaseRegistry::global())?;
  if let Some(guard) = preparation_guard.as_mut() {
    guard.close();
  }
  Ok(prepared)
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

  let worker_profile = prepared.worker_profile.clone();
  let runtime_lease = prepared.runtime_lease.clone();
  prepared
    .engine
    .install_worker_runtime(worker_profile.clone(), runtime_lease.clone());
  let policy = prepared
    .engine
    .worker_runtime()
    .map(|(_, policy, _)| policy)
    .ok_or_else(|| "Auto Login worker runtime state is incomplete".to_string())?;
  let _ = prepared._lease_guard.into_lease_id();
  let cleanup_context = Arc::new(Mutex::new(WorkerRuntimeCleanupContext {
    profile: worker_profile,
    policy,
    lease: runtime_lease,
    lease_registry: prepared.lease_registry,
  }));
  let cleanup_context_for_action = cleanup_context.clone();
  let cleanup_app_handle = app_handle.clone();
  let cleanup_action = Arc::new(move || {
    let context = cleanup_context_for_action.clone();
    let app_handle = cleanup_app_handle.clone();
    Box::pin(async move { cleanup_worker_runtime(&app_handle, &context).await })
      as RuntimeCleanupFuture
  }) as RuntimeCleanupAction;
  let mut cleanup_guard = LoginRuntimeCleanupGuard::new(cleanup_action);

  let results =
    runtime.block_on(async { prepared.engine.run(app_handle.clone(), sms_service).await });
  if let Err(error) = runtime.block_on(cleanup_guard.close()) {
    prepared
      .engine
      .emit_deferred_batch_terminal(&app_handle, &results, false);
    return Err(error);
  }
  prepared
    .engine
    .emit_deferred_batch_terminal(&app_handle, &results, true);
  Ok(results)
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

  fn write_profile(
    profiles_dir: &std::path::Path,
    name: &str,
    browser: &str,
  ) -> (crate::profile::BrowserProfile, std::path::PathBuf, Vec<u8>) {
    let profile = crate::profile::BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: name.to_string(),
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
  fn default_worker_selection_uses_exact_name_only() {
    let exact = crate::profile::BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: "auto-login-worker-chromium".to_string(),
      browser: "chromium".to_string(),
      ..crate::profile::BrowserProfile::default()
    };
    let foreign = crate::profile::BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: "auto-login-worker-foreign-purpose".to_string(),
      browser: "chromium".to_string(),
      ..crate::profile::BrowserProfile::default()
    };

    let selected = select_exact_default_worker(&[foreign.clone(), exact.clone()], "chromium")
      .expect("exact default worker should be selected");
    assert_eq!(selected.id, exact.id);
    assert!(select_exact_default_worker(&[foreign], "chromium").is_none());
  }

  #[test]
  fn explicit_profile_resolution_is_read_only() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let (profile, metadata_path, metadata) = write_profile(temp.path(), "selected", "chromium");
    let mut config = base_config();
    config.profile_id = Some(profile.id.to_string());
    config.parse_credentials();
    config.normalize();

    let resolved = resolve_explicit_login_profile(&config, temp.path())
      .expect("selected profile should resolve")
      .expect("explicit profile expected");

    assert_eq!(resolved.id, profile.id);
    assert_eq!(std::fs::read(metadata_path).unwrap(), metadata);
  }

  #[test]
  fn explicit_profile_resolution_rejects_missing_profile() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let mut config = base_config();
    config.profile_id = Some(uuid::Uuid::new_v4().to_string());
    config.parse_credentials();
    config.normalize();

    let error = resolve_explicit_login_profile(&config, temp.path())
      .expect_err("missing explicit profile must fail");
    assert!(error.contains("not found"));
  }

  #[test]
  fn explicit_profile_resolution_rejects_ephemeral_camoufox() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let (profile, _, _) = write_profile(temp.path(), "selected", "camoufox");
    let mut config = base_config();
    config.profile_id = Some(profile.id.to_string());
    config.browser_type = "camoufox".to_string();
    config.parse_credentials();
    config.normalize();

    let error = resolve_explicit_login_profile(&config, temp.path())
      .expect_err("ephemeral selected Camoufox must fail closed");
    assert!(error.contains("ephemeral data"));
  }

  #[test]
  fn prepared_login_fails_fast_when_profile_is_busy() {
    let profile = crate::profile::BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: "selected".to_string(),
      browser: "chromium".to_string(),
      version: "1.0.0".to_string(),
      ..crate::profile::BrowserProfile::default()
    };
    let registry = leaked_registry();
    let mut first_config = base_config();
    first_config.parse_credentials();
    first_config.normalize();
    let second_config = first_config.clone();

    let first = prepare_login_runtime(
      first_config,
      Arc::new(AtomicBool::new(false)),
      profile.clone(),
      registry,
    )
    .expect("first preparation should acquire lease");
    let error = prepare_login_runtime(
      second_config,
      Arc::new(AtomicBool::new(false)),
      profile,
      registry,
    )
    .expect_err("second preparation must fail immediately");

    assert_eq!(error, SELECTED_PROFILE_BUSY_ERROR);
    assert!(!error.contains("lease"));
    drop(first);
  }

  #[test]
  fn dropping_prepared_login_releases_worker_lease() {
    let profile = crate::profile::BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: "selected".to_string(),
      browser: "chromium".to_string(),
      version: "1.0.0".to_string(),
      ..crate::profile::BrowserProfile::default()
    };
    let profile_id = profile.id.to_string();
    let registry = leaked_registry();
    let mut config = base_config();
    config.parse_credentials();
    config.normalize();

    let prepared =
      prepare_login_runtime(config, Arc::new(AtomicBool::new(false)), profile, registry)
        .expect("worker runtime should prepare");
    assert!(registry.is_leased(&profile_id));

    drop(prepared);
    assert!(!registry.is_leased(&profile_id));
  }

  #[test]
  fn worker_lease_releases_only_after_cleanup_succeeds() {
    let registry = leaked_registry();
    let profile_id = uuid::Uuid::new_v4().to_string();
    let lease = registry.try_acquire(&profile_id).expect("acquire lease");

    let error = release_worker_lease_after_cleanup(
      registry,
      &lease.lease_id,
      Err("cleanup failed".to_string()),
    )
    .expect_err("failed cleanup must retain lease");
    assert_eq!(error, "cleanup failed");
    assert!(registry.is_leased(&profile_id));

    release_worker_lease_after_cleanup(registry, &lease.lease_id, Ok(()))
      .expect("successful retry releases lease");
    assert!(!registry.is_leased(&profile_id));
  }

  #[tokio::test]
  async fn runtime_cleanup_guard_retries_after_explicit_failure() {
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
    let mut guard = LoginRuntimeCleanupGuard::new(action);

    assert!(guard.close().await.is_err());
    drop(guard);

    retried_rx
      .recv_timeout(std::time::Duration::from_secs(2))
      .expect("drop should retry cleanup");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
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
