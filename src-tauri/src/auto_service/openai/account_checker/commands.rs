use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::Emitter;

use super::engine;
use super::store;
use super::task;
use super::types::{AccountCheckConfig, AccountCheckResult, CheckProgress, CheckStep, ReasonCode};
use crate::browser_runner::BrowserRunner;
use crate::profile::BrowserProfile;
use crate::profile_runtime::{
  DataMode, FingerprintMode, LaunchPolicy, LeaseError, LeaseRegistry, RuntimeLease,
};

const CLEANUP_ERROR_PREFIX: &str = "cleanup error: ";
const BUSY_PROFILE_ERROR: &str =
  "Selected profile is busy; wait for the current automation task to finish or stop it before retrying";

type CleanupFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
type CleanupAction = Arc<dyn Fn() -> CleanupFuture + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchRunOutcome {
  Completed,
  Cancelled,
}

fn batch_terminal_state(
  result: &Result<BatchRunOutcome, String>,
) -> (CheckStep, Option<ReasonCode>) {
  match result {
    Ok(BatchRunOutcome::Completed) => (CheckStep::Completed, None),
    Ok(BatchRunOutcome::Cancelled) => (CheckStep::Cancelled, Some(ReasonCode::Cancelled)),
    Err(error) if error.starts_with(CLEANUP_ERROR_PREFIX) => {
      (CheckStep::Failed, Some(ReasonCode::CleanupError))
    }
    Err(_) => (CheckStep::Failed, Some(ReasonCode::BrowserError)),
  }
}

/// Start a new account check batch. Returns task_id.
#[tauri::command]
pub async fn start_openai_account_check(
  app_handle: tauri::AppHandle,
  config: AccountCheckConfig,
) -> Result<String, String> {
  let credentials = parse_credentials(&config)?;
  let selected = validate_selected_profile(config.source_profile_id.as_deref())?;
  let selected_lease = if let Some((_profile, profile_id)) = selected.as_ref() {
    Some(
      LeaseRegistry::global()
        .try_acquire(profile_id)
        .map_err(selected_profile_lease_error)?,
    )
  } else {
    None
  };

  let task_id = uuid::Uuid::new_v4().to_string();
  let cancel_flag = Arc::new(AtomicBool::new(false));
  let total = credentials.len();
  let task_id_for_task = task_id.clone();
  let task_id_for_emit = task_id.clone();
  let app_handle_for_task = app_handle.clone();
  let app_handle_for_emit = app_handle.clone();
  let cancel_flag_for_task = cancel_flag.clone();
  let config_for_task = config.clone();
  let selected_profile = selected.map(|(profile, _)| profile);
  let selected_lease_for_task = selected_lease.clone();

  if let Err(error) = task::spawn_registered(task_id.clone(), cancel_flag.clone(), async move {
    let result = run_check_batch(
      app_handle_for_task,
      &task_id_for_task,
      config_for_task,
      credentials,
      cancel_flag_for_task,
      selected_profile,
      selected_lease_for_task,
    )
    .await;

    let (step, reason_code) = batch_terminal_state(&result);
    if let Err(error) = &result {
      if error.starts_with(CLEANUP_ERROR_PREFIX) {
        log::error!("Account check cleanup failed");
      } else {
        log::error!("Account check task failed");
      }
    }

    let _ = app_handle_for_emit.emit(
      "openai-account-check-progress",
      CheckProgress {
        task_id: task_id_for_emit,
        account_key: String::new(),
        credential_index: 0,
        total_credentials: total,
        step,
        outcome: None,
        reason_code,
        terminal: true,
      },
    );
  }) {
    if let Some(lease) = selected_lease {
      LeaseRegistry::global().release(&lease.lease_id);
    }
    return Err(error);
  }

  Ok(task_id)
}

fn parse_credentials(
  config: &AccountCheckConfig,
) -> Result<Vec<super::types::LoginCredential>, String> {
  let credentials = super::types::LoginCredential::parse_batch(&config.credentials_text);
  if credentials.is_empty() {
    return Err("No valid credentials found".to_string());
  }
  Ok(credentials)
}

fn validate_selected_profile(
  source_profile_id: Option<&str>,
) -> Result<Option<(BrowserProfile, String)>, String> {
  validate_selected_profile_in(
    source_profile_id,
    &crate::profile::ProfileManager::instance().get_profiles_dir(),
  )
}

fn validate_selected_profile_in(
  source_profile_id: Option<&str>,
  profiles_dir: &std::path::Path,
) -> Result<Option<(BrowserProfile, String)>, String> {
  let Some(source_profile_id) = source_profile_id.map(str::trim).filter(|id| !id.is_empty()) else {
    return Ok(None);
  };

  let profile_id = uuid::Uuid::parse_str(source_profile_id)
    .map_err(|_| "Selected source profile ID is not a valid UUID".to_string())?;
  let metadata_path = profiles_dir
    .join(profile_id.to_string())
    .join("metadata.json");
  let metadata = std::fs::read_to_string(&metadata_path).map_err(|error| match error.kind() {
    std::io::ErrorKind::NotFound => "Selected source profile was not found".to_string(),
    _ => format!("Failed to read selected source profile: {error}"),
  })?;
  let profile: BrowserProfile = serde_json::from_str(&metadata)
    .map_err(|error| format!("Selected source profile metadata is invalid: {error}"))?;
  if profile.id != profile_id {
    return Err("Selected source profile metadata ID does not match its directory".to_string());
  }

  if profile
    .process_id
    .is_some_and(crate::proxy_storage::is_process_running)
  {
    return Err(
      "Selected source profile is running; stop it before starting Account Checker".into(),
    );
  }
  if !crate::browser::is_chromium_browser_name(&profile.browser) {
    return Err("Selected source profile is not Chromium-compatible".into());
  }

  Ok(Some((profile, profile_id.to_string())))
}

fn selected_profile_lease_error(error: LeaseError) -> String {
  match error {
    LeaseError::Busy { .. } => BUSY_PROFILE_ERROR.to_string(),
    LeaseError::InvalidSourceProfileId => "Selected source profile ID is invalid".to_string(),
  }
}

fn effective_vpn_for_index(
  fixed_vpn_id: Option<&str>,
  refreshed_vpn_ids: &[String],
  account_index: usize,
) -> Option<String> {
  if let Some(vpn_id) = fixed_vpn_id.map(str::trim).filter(|id| !id.is_empty()) {
    return Some(vpn_id.to_string());
  }
  refreshed_vpn_ids
    .get(account_index % refreshed_vpn_ids.len().max(1))
    .cloned()
}

fn launch_policy_for_profile(
  config: &AccountCheckConfig,
  profile_id: &str,
  owns_generated_worker: bool,
) -> LaunchPolicy {
  let mut policy = if owns_generated_worker {
    LaunchPolicy::for_generated_worker()
  } else {
    LaunchPolicy::for_source_profile(profile_id)
  };
  if !owns_generated_worker {
    policy.source_profile_id = Some(profile_id.to_string());
  }
  policy.data_mode = config.data_mode;
  policy.fingerprint_mode = config.fingerprint_mode;
  policy.persist_process_to_source = false;
  policy
}

fn should_delete_profile(owned_generated_profile_id: Option<&str>, profile_id: &str) -> bool {
  owned_generated_profile_id == Some(profile_id)
}

struct CleanupContext {
  profile: BrowserProfile,
  policy: LaunchPolicy,
  lease: RuntimeLease,
  owned_generated_profile_id: Option<String>,
}

struct AccountCheckCleanupGuard {
  action: Option<CleanupAction>,
}

impl AccountCheckCleanupGuard {
  fn new(action: CleanupAction) -> Self {
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

fn retry_dropped_cleanup(action: CleanupAction) {
  std::thread::spawn(move || {
    let runtime = loop {
      match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
      {
        Ok(runtime) => break runtime,
        Err(_) => {
          log::error!("Dropped account check cleanup runtime creation failed; retrying");
          std::thread::sleep(std::time::Duration::from_secs(1));
        }
      }
    };
    let mut delay = std::time::Duration::from_millis(100);
    while runtime.block_on(action()).is_err() {
      log::error!("Dropped account check cleanup failed; retrying");
      std::thread::sleep(delay);
      delay = (delay * 2).min(std::time::Duration::from_secs(5));
    }
  });
}

impl Drop for AccountCheckCleanupGuard {
  fn drop(&mut self) {
    if let Some(action) = self.action.take() {
      retry_dropped_cleanup(action);
    }
  }
}

async fn run_check_batch(
  app_handle: tauri::AppHandle,
  task_id: &str,
  config: AccountCheckConfig,
  credentials: Vec<super::types::LoginCredential>,
  cancel_flag: Arc<AtomicBool>,
  selected_profile: Option<BrowserProfile>,
  selected_lease: Option<RuntimeLease>,
) -> Result<BatchRunOutcome, String> {
  use crate::chromium_manager::ChromiumConfig;
  use crate::profile::manager::create_browser_profile_with_group;

  let profile_and_lease = match (selected_profile, selected_lease) {
    (Some(profile), Some(lease)) => (profile, lease, false),
    (None, None) => {
      let version = resolve_chromium_version()?;
      let release_type =
        if crate::api_client::is_browser_version_nightly("chromium", &version, None) {
          "nightly"
        } else {
          "stable"
        };
      let operation_id = task_id.to_string();
      let profile = create_browser_profile_with_group(
        app_handle.clone(),
        format!("acct-check-worker-{operation_id}"),
        "chromium".to_string(),
        version,
        release_type.to_string(),
        None,
        None,
        None,
        Some(ChromiumConfig {
          fingerprint: None,
          randomize_fingerprint_on_launch: Some(
            config.fingerprint_mode == FingerprintMode::RandomPerLaunch,
          ),
          ..Default::default()
        }),
        None,
        config.data_mode == DataMode::Ephemeral,
        None,
        None,
      )
      .await
      .map_err(|error| format!("Create account checker worker: {error}"))?;
      let profile_id = profile.id.to_string();
      let lease = match LeaseRegistry::global().try_acquire(&profile_id) {
        Ok(lease) => lease,
        Err(error) => {
          let delete_result = crate::profile::ProfileManager::instance()
            .delete_profile(&app_handle, &profile_id)
            .map_err(|delete_error| {
              cleanup_error(format!("delete generated worker: {delete_error}"))
            });
          return match delete_result {
            Ok(()) => Err(selected_profile_lease_error(error)),
            Err(cleanup_error) => Err(cleanup_error),
          };
        }
      };
      (profile, lease, true)
    }
    _ => return Err("Account checker runtime lease state was incomplete".into()),
  };

  let (profile, lease, owns_generated_worker) = profile_and_lease;
  let profile_id = profile.id.to_string();
  let base_policy = launch_policy_for_profile(&config, &profile_id, owns_generated_worker);
  let owned_generated_profile_id = owns_generated_worker.then(|| profile_id.clone());
  let cleanup_context = Arc::new(Mutex::new(CleanupContext {
    profile: profile.clone(),
    policy: base_policy.clone(),
    lease: lease.clone(),
    owned_generated_profile_id,
  }));
  let cleanup_context_for_action = cleanup_context.clone();
  let cleanup_app_handle = app_handle.clone();
  let cleanup = Arc::new(move || {
    let context = cleanup_context_for_action.clone();
    let app_handle = cleanup_app_handle.clone();
    Box::pin(async move { cleanup_runtime(&app_handle, &context).await }) as CleanupFuture
  });
  let mut cleanup_guard = AccountCheckCleanupGuard::new(cleanup);

  let refreshed_vpn_ids = if config.vpn_id.is_some() {
    Vec::new()
  } else {
    refresh_nord_configs(&app_handle)
      .await
      .unwrap_or_else(|_| Vec::new())
      .into_iter()
      .map(|vpn| vpn.id)
      .collect()
  };

  let total = credentials.len();
  let account_result = run_accounts(
    &app_handle,
    task_id,
    &profile,
    lease,
    &base_policy,
    config.vpn_id.as_deref(),
    &refreshed_vpn_ids,
    credentials,
    cancel_flag,
    total,
  )
  .await;
  let cleanup_result = cleanup_guard.close().await;

  if let Err(error) = cleanup_result {
    return Err(cleanup_error(error));
  }
  account_result
}

fn resolve_chromium_version() -> Result<String, String> {
  let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
  let _ = registry.load();
  let mut versions = registry.get_downloaded_versions("chromium");
  if versions.is_empty() {
    let dir = crate::app_dirs::data_dir()
      .join("binaries")
      .join("fingerprint-chromium");
    if let Ok(entries) = std::fs::read_dir(dir) {
      versions.extend(entries.flatten().filter_map(|entry| {
        entry
          .path()
          .is_dir()
          .then(|| entry.file_name().to_string_lossy().into_owned())
      }));
    }
  }
  versions.sort_by(|a, b| {
    crate::api_client::VersionComponent::parse(b)
      .cmp(&crate::api_client::VersionComponent::parse(a))
  });
  versions
    .into_iter()
    .next()
    .ok_or_else(|| "No Chromium version available".to_string())
}

#[allow(clippy::too_many_arguments)]
async fn run_accounts(
  app_handle: &tauri::AppHandle,
  task_id: &str,
  profile: &BrowserProfile,
  mut lease: RuntimeLease,
  base_policy: &LaunchPolicy,
  fixed_vpn_id: Option<&str>,
  refreshed_vpn_ids: &[String],
  credentials: Vec<super::types::LoginCredential>,
  cancel_flag: Arc<AtomicBool>,
  total: usize,
) -> Result<BatchRunOutcome, String> {
  for (index, credential) in credentials.into_iter().enumerate() {
    if cancel_flag.load(Ordering::SeqCst) {
      return Ok(BatchRunOutcome::Cancelled);
    }

    let vpn_id = effective_vpn_for_index(fixed_vpn_id, refreshed_vpn_ids, index);
    let mut active_policy = base_policy.clone();
    active_policy.vpn_id = vpn_id;
    active_policy.clear_network = false;

    let launched = match BrowserRunner::instance()
      .launch_browser_with_policy(
        app_handle.clone(),
        profile,
        Some("about:blank".to_string()),
        None,
        &active_policy,
        &mut lease,
      )
      .await
    {
      Ok(launched) => launched,
      Err(error) if active_policy.vpn_id.is_some() && is_vpn_launch_failure(error.as_ref()) => {
        cleanup_runtime_browser(app_handle, profile, &active_policy, &mut lease).await?;
        let mut clear_policy = active_policy.clone();
        clear_policy.vpn_id = None;
        clear_policy.clear_network = true;
        active_policy = clear_policy;
        BrowserRunner::instance()
          .launch_browser_with_policy(
            app_handle.clone(),
            profile,
            Some("about:blank".to_string()),
            None,
            &active_policy,
            &mut lease,
          )
          .await
          .map_err(|launch_error| format!("Browser launch failed: {launch_error}"))?
      }
      Err(error) => {
        cleanup_runtime_browser(app_handle, profile, &active_policy, &mut lease).await?;
        return Err(format!("Browser launch failed: {error}"));
      }
    };

    let mut session = match super::super::browser::attach_browser_session(&launched).await {
      Ok(session) => session,
      Err(error) => {
        cleanup_runtime_browser(app_handle, profile, &active_policy, &mut lease).await?;
        log::warn!("Account checker browser attach failed");
        if cancel_flag.load(Ordering::SeqCst) {
          return Ok(BatchRunOutcome::Cancelled);
        }
        let _ = error;
        continue;
      }
    };

    let _ = session.clear_all_site_data().await;
    let _ = session.navigate("https://chatgpt.com/auth/login", 25).await;
    tokio::time::sleep(std::time::Duration::from_millis(4000)).await;
    let _ = session.evaluate("window.scrollBy(0, 200)", false).await;
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    let device_id = uuid::Uuid::new_v4().to_string();
    let _ = engine::check_one(
      app_handle,
      task_id,
      &credential.email,
      &credential.password,
      &credential.totp_secret,
      index,
      total,
      &cancel_flag,
      &mut session,
      &device_id,
    )
    .await;

    drop(session);
    cleanup_runtime_browser(app_handle, profile, &active_policy, &mut lease).await?;

    if index + 1 < total {
      tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
  }
  Ok(BatchRunOutcome::Completed)
}

async fn cleanup_runtime(
  app_handle: &tauri::AppHandle,
  context: &Arc<Mutex<CleanupContext>>,
) -> Result<(), String> {
  let (profile, policy, mut lease, owned_generated_profile_id) = {
    let context = context
      .lock()
      .map_err(|_| "cleanup context lock poisoned".to_string())?;
    (
      context.profile.clone(),
      context.policy.clone(),
      context.lease.clone(),
      context.owned_generated_profile_id.clone(),
    )
  };

  cleanup_runtime_browser(app_handle, &profile, &policy, &mut lease).await?;
  let profile_id = profile.id.to_string();
  if should_delete_profile(owned_generated_profile_id.as_deref(), &profile_id) {
    crate::profile::ProfileManager::instance()
      .delete_profile(app_handle, &profile_id)
      .map_err(|error| format!("delete generated worker: {error}"))?;
  }
  LeaseRegistry::global().release(&lease.lease_id);
  Ok(())
}

async fn cleanup_runtime_browser(
  app_handle: &tauri::AppHandle,
  profile: &BrowserProfile,
  policy: &LaunchPolicy,
  lease: &mut RuntimeLease,
) -> Result<(), String> {
  BrowserRunner::instance()
    .kill_runtime_browser(app_handle.clone(), profile, policy, lease)
    .await
    .map_err(|error| cleanup_error(error.to_string()))
}

fn cleanup_error(error: impl Into<String>) -> String {
  let error = error.into();
  if error.starts_with(CLEANUP_ERROR_PREFIX) {
    error
  } else {
    format!("{CLEANUP_ERROR_PREFIX}{error}")
  }
}

fn is_vpn_launch_failure(error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
  error.to_string().contains("VPN worker")
}

/// Fetch fresh WireGuard configs from NordVPN API.
async fn refresh_nord_configs(
  app_handle: &tauri::AppHandle,
) -> Result<Vec<crate::vpn::VpnConfig>, String> {
  use crate::settings_manager::SettingsManager;
  use crate::vpn::nord::{
    build_nord_wireguard_conf, fetch_nord_wireguard_credentials, list_nord_wireguard_servers,
  };
  use crate::vpn::{VpnType, VPN_STORAGE};

  let token = SettingsManager::instance()
    .get_nord_access_token(app_handle)
    .await
    .map_err(|error| format!("Nord token: {error}"))?
    .ok_or_else(|| "No Nord access token configured".to_string())?;

  let creds = fetch_nord_wireguard_credentials(&token).await?;
  let servers = list_nord_wireguard_servers(None, Some(5)).await?;
  if servers.is_empty() {
    return Err("No Nord WireGuard servers available".into());
  }

  let mut configs = Vec::new();
  let storage = VPN_STORAGE
    .lock()
    .map_err(|error| format!("Lock: {error}"))?;

  for (index, server) in servers.iter().enumerate() {
    let conf = build_nord_wireguard_conf(&creds.nordlynx_private_key, server);
    let name = format!(
      "Nord-{}-{index}",
      server.hostname.split('.').next().unwrap_or("wg")
    );
    match storage.create_config_manual(&name, VpnType::WireGuard, &conf) {
      Ok(config) => configs.push(config),
      Err(error) => log::warn!("Save Nord config failed: {error}"),
    }
  }
  log::info!("Created {} fresh Nord VPN configs", configs.len());
  Ok(configs)
}

/// Cancel a running account check task.
#[tauri::command]
pub fn cancel_openai_account_check(task_id: String) -> Result<(), String> {
  if task::cancel_task(&task_id) {
    Ok(())
  } else {
    Err(format!("Task {task_id} not found"))
  }
}

/// List all stored account check results.
#[tauri::command]
pub fn list_openai_account_check_results() -> Result<Vec<AccountCheckResult>, String> {
  Ok(store::current_results())
}

/// Delete a stored account check result by email.
#[tauri::command]
pub fn delete_openai_account_check_result(email: String) -> Result<(), String> {
  if store::remove_result(&email) {
    Ok(())
  } else {
    Err(format!("Account {email} not found"))
  }
}

/// Export passed accounts as email|password|2fa text.
#[tauri::command]
pub fn export_passed_accounts() -> Result<String, String> {
  let passed = store::current_passed();
  let lines: Vec<String> = passed
    .iter()
    .map(|result| result.credential_line())
    .collect();
  Ok(lines.join("\n"))
}

/// Export deactivated accounts as email|password|2fa text.
#[tauri::command]
pub fn export_deactivated_accounts() -> Result<String, String> {
  let deactivated = store::current_deactivated();
  let lines: Vec<String> = deactivated
    .iter()
    .map(|result| result.credential_line())
    .collect();
  Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn config() -> AccountCheckConfig {
    AccountCheckConfig {
      credentials_text: "user@example.com|password".to_string(),
      source_profile_id: None,
      data_mode: DataMode::Ephemeral,
      fingerprint_mode: FingerprintMode::RandomPerLaunch,
      vpn_id: None,
      browser_type: "chromium".to_string(),
      headless: false,
    }
  }

  #[test]
  fn invalid_credentials_are_rejected() {
    let mut config = config();
    config.credentials_text = "\ninvalid\n|missing-email\n".to_string();
    let error = parse_credentials(&config).expect_err("invalid credentials must fail");
    assert_eq!(error, "No valid credentials found");
  }

  #[test]
  fn selected_profile_validation_is_read_only() {
    let temp = tempfile::tempdir().unwrap();
    let profile = BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: "selected".to_string(),
      browser: "chromium".to_string(),
      version: "1.0.0".to_string(),
      ..BrowserProfile::default()
    };
    let profile_dir = temp.path().join(profile.id.to_string());
    std::fs::create_dir_all(profile_dir.join("data")).unwrap();
    let metadata_path = profile_dir.join("metadata.json");
    let metadata = serde_json::to_vec_pretty(&profile).unwrap();
    std::fs::write(&metadata_path, &metadata).unwrap();
    let marker_path = profile_dir.join("data").join("marker.txt");
    std::fs::write(&marker_path, b"persistent-data").unwrap();

    let (loaded, profile_id) =
      validate_selected_profile_in(Some(&profile.id.to_string()), temp.path())
        .unwrap()
        .expect("selected profile");

    assert_eq!(loaded.id, profile.id);
    assert_eq!(profile_id, profile.id.to_string());
    assert_eq!(std::fs::read(&metadata_path).unwrap(), metadata);
    assert_eq!(std::fs::read(&marker_path).unwrap(), b"persistent-data");
  }

  #[test]
  fn selected_profile_busy_error_is_stable_and_actionable() {
    let error = selected_profile_lease_error(LeaseError::Busy {
      source_profile_id: "profile-id".to_string(),
      holder_lease_id: "lease-id".to_string(),
    });
    assert_eq!(error, BUSY_PROFILE_ERROR);
    assert!(!error.contains("lease-id"));
  }

  #[test]
  fn effective_vpn_uses_fixed_config_first() {
    let vpn_ids = vec!["rotating-a".to_string(), "rotating-b".to_string()];
    assert_eq!(
      effective_vpn_for_index(Some("fixed"), &vpn_ids, 1).as_deref(),
      Some("fixed")
    );
  }

  #[test]
  fn effective_vpn_rotates_refreshed_configs() {
    let vpn_ids = vec!["rotating-a".to_string(), "rotating-b".to_string()];
    assert_eq!(
      effective_vpn_for_index(None, &vpn_ids, 0).as_deref(),
      Some("rotating-a")
    );
    assert_eq!(
      effective_vpn_for_index(None, &vpn_ids, 3).as_deref(),
      Some("rotating-b")
    );
  }

  #[test]
  fn effective_vpn_is_none_without_fixed_or_refreshed_config() {
    assert_eq!(effective_vpn_for_index(None, &[], 5), None);
  }

  #[test]
  fn selected_profile_policy_is_immutable_and_not_owned() {
    let mut config = config();
    config.data_mode = DataMode::Persistent;
    config.fingerprint_mode = FingerprintMode::Stable;
    let policy = launch_policy_for_profile(&config, "selected-id", false);

    assert_eq!(policy.source_profile_id.as_deref(), Some("selected-id"));
    assert_eq!(policy.data_mode, DataMode::Persistent);
    assert_eq!(policy.fingerprint_mode, FingerprintMode::Stable);
    assert!(!policy.owns_generated_worker);
    assert!(!policy.persist_process_to_source);
    assert!(!should_delete_profile(None, "selected-id"));
  }

  #[test]
  fn generated_profile_policy_preserves_ownership_and_process_tracking() {
    let policy = launch_policy_for_profile(&config(), "generated-id", true);

    assert_eq!(policy.source_profile_id, None);
    assert!(policy.owns_generated_worker);
    assert!(!policy.persist_process_to_source);
    assert!(should_delete_profile(Some("generated-id"), "generated-id"));
    assert!(!should_delete_profile(Some("generated-id"), "different-id"));
  }

  #[tokio::test]
  async fn cleanup_guard_retains_owner_after_repeated_drop_failures() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_action = attempts.clone();
    let (completed_tx, completed_rx) = std::sync::mpsc::channel();
    let action = Arc::new(move || {
      let attempts = attempts_for_action.clone();
      let completed_tx = completed_tx.clone();
      Box::pin(async move {
        let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt <= 2 {
          Err(format!("cleanup attempt {attempt} failed"))
        } else {
          let _ = completed_tx.send(());
          Ok(())
        }
      }) as CleanupFuture
    });
    let mut guard = AccountCheckCleanupGuard::new(action);

    assert!(guard.close().await.is_err());
    drop(guard);

    completed_rx
      .recv_timeout(std::time::Duration::from_secs(2))
      .expect("drop owner must retry until cleanup succeeds");
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
  }

  #[test]
  fn batch_terminal_state_reports_cancellation_and_cleanup_failure() {
    assert_eq!(
      batch_terminal_state(&Ok(BatchRunOutcome::Cancelled)),
      (CheckStep::Cancelled, Some(ReasonCode::Cancelled))
    );
    assert_eq!(
      batch_terminal_state(&Err(cleanup_error("runtime cleanup failed"))),
      (CheckStep::Failed, Some(ReasonCode::CleanupError))
    );
    assert_eq!(
      batch_terminal_state(&Err("browser launch failed".to_string())),
      (CheckStep::Failed, Some(ReasonCode::BrowserError))
    );
  }

  #[test]
  fn cleanup_error_prefix_is_idempotent() {
    let marked = cleanup_error("failed");
    assert_eq!(cleanup_error(marked.clone()), marked);
  }
}
