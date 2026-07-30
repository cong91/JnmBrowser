use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tauri::AppHandle;

use super::types::{NetworkMode, RegistrationConfig};
use crate::profile::BrowserProfile;
use crate::profile_runtime::{
  LaunchPolicy, LeaseCleanupGuard, LeaseError, LeaseRegistry, RuntimeLease,
};

const SELECTED_PROFILE_BUSY_ERROR: &str =
  "Selected profile is busy; wait for the current automation task to finish or stop it before retrying";

type CleanupFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
type CleanupAction = Arc<dyn Fn() -> CleanupFuture + Send + Sync>;

/// Selected profile resolved and leased before task publication.
pub(super) struct PreparedSelectedProfileRuntime {
  runtime: SelectedProfileRuntime,
  lease_registry: &'static LeaseRegistry,
  lease_guard: Option<LeaseCleanupGuard>,
}

impl std::fmt::Debug for PreparedSelectedProfileRuntime {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("PreparedSelectedProfileRuntime")
      .field("profile_id", &self.runtime.profile.id)
      .finish_non_exhaustive()
  }
}

impl PreparedSelectedProfileRuntime {
  pub(super) fn runtime(&self) -> SelectedProfileRuntime {
    self.runtime.clone()
  }

  pub(super) fn cleanup_guard(&self, app_handle: AppHandle) -> SelectedProfileRuntimeCleanupGuard {
    SelectedProfileRuntimeCleanupGuard::new(app_handle, self.runtime.clone(), self.lease_registry)
  }

  pub(super) fn disarm_preparation_guard(mut self) {
    if let Some(guard) = self.lease_guard.take() {
      let _ = guard.into_lease_id();
    }
  }

  pub(super) fn profile_id(&self) -> uuid::Uuid {
    self.runtime.profile.id
  }
}

/// Read-only source profile, task-local launch policy, and full-lifecycle lease.
#[derive(Clone)]
pub(super) struct SelectedProfileRuntime {
  profile: BrowserProfile,
  policy: LaunchPolicy,
  lease: RuntimeLease,
}

impl SelectedProfileRuntime {
  pub(super) fn new(
    config: &RegistrationConfig,
    profile: BrowserProfile,
    lease: RuntimeLease,
  ) -> Self {
    let mut policy = LaunchPolicy::for_source_profile(profile.id.to_string());
    policy.data_mode = config.data_mode;
    policy.fingerprint_mode = config.fingerprint_mode;
    match config.network_mode {
      NetworkMode::Proxy => policy.proxy_id = config.effective_proxy_id(),
      NetworkMode::Vpn => policy.vpn_id = config.effective_vpn_id(),
      NetworkMode::None | NetworkMode::Nord => policy.clear_network = true,
    }
    Self {
      profile,
      policy,
      lease,
    }
  }

  pub(super) fn profile(&self) -> &BrowserProfile {
    &self.profile
  }

  pub(super) fn with_slot_vpn(&self, slot_vpn_id: Option<&str>) -> Self {
    let mut runtime = self.clone();
    if let Some(vpn_id) = slot_vpn_id {
      runtime.policy.proxy_id = None;
      runtime.policy.vpn_id = Some(vpn_id.to_string());
      runtime.policy.clear_network = false;
    }
    runtime
  }

  pub(super) async fn launch(&mut self, app_handle: &AppHandle) -> Result<BrowserProfile, String> {
    crate::browser_runner::BrowserRunner::instance()
      .launch_browser_with_policy(
        app_handle.clone(),
        &self.profile,
        Some("about:blank".into()),
        None,
        &self.policy,
        &mut self.lease,
      )
      .await
      .map_err(|error| format!("Launch: {error}"))
  }

  pub(super) async fn kill(
    &mut self,
    app_handle: &AppHandle,
    profile: &BrowserProfile,
  ) -> Result<(), String> {
    crate::browser_runner::BrowserRunner::instance()
      .kill_runtime_browser(app_handle.clone(), profile, &self.policy, &mut self.lease)
      .await
      .map_err(|error| error.to_string())
  }

  #[cfg(test)]
  pub(super) fn lease_id(&self) -> &str {
    &self.lease.lease_id
  }

  #[cfg(test)]
  fn policy(&self) -> &LaunchPolicy {
    &self.policy
  }
}

pub(super) fn prepare_selected_profile_runtime(
  config: &RegistrationConfig,
  profiles_dir: &std::path::Path,
  lease_registry: &'static LeaseRegistry,
) -> Result<Option<PreparedSelectedProfileRuntime>, String> {
  let Some(profile) = resolve_selected_profile(config, profiles_dir)? else {
    return Ok(None);
  };
  let lease = lease_registry
    .try_acquire(&profile.id.to_string())
    .map_err(selected_profile_lease_error)?;
  let lease_guard = LeaseCleanupGuard::with_registry(lease_registry, lease.clone());
  Ok(Some(PreparedSelectedProfileRuntime {
    runtime: SelectedProfileRuntime::new(config, profile, lease),
    lease_registry,
    lease_guard: Some(lease_guard),
  }))
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

struct CleanupContext {
  runtime: SelectedProfileRuntime,
  lease_registry: &'static LeaseRegistry,
}

/// Final selected-profile cleanup. The lease is released only after cleanup succeeds.
pub(super) struct SelectedProfileRuntimeCleanupGuard {
  action: Option<CleanupAction>,
}

impl SelectedProfileRuntimeCleanupGuard {
  fn new(
    app_handle: AppHandle,
    runtime: SelectedProfileRuntime,
    lease_registry: &'static LeaseRegistry,
  ) -> Self {
    let context = Arc::new(Mutex::new(CleanupContext {
      runtime,
      lease_registry,
    }));
    let action = Arc::new(move || {
      let app_handle = app_handle.clone();
      let context = context.clone();
      Box::pin(async move { cleanup_selected_runtime(&app_handle, &context).await })
        as CleanupFuture
    });
    Self {
      action: Some(action),
    }
  }

  #[cfg(test)]
  fn with_action(action: CleanupAction) -> Self {
    Self {
      action: Some(action),
    }
  }

  pub(super) async fn close(&mut self) -> Result<(), String> {
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
          log::error!("Dropped registration cleanup runtime creation failed; retrying");
          std::thread::sleep(std::time::Duration::from_secs(1));
        }
      }
    };
    let mut delay = std::time::Duration::from_millis(100);
    while runtime.block_on(action()).is_err() {
      log::error!("Dropped registration runtime cleanup failed; retrying");
      std::thread::sleep(delay);
      delay = (delay * 2).min(std::time::Duration::from_secs(5));
    }
  });
}

impl Drop for SelectedProfileRuntimeCleanupGuard {
  fn drop(&mut self) {
    if let Some(action) = self.action.take() {
      retry_dropped_cleanup(action);
    }
  }
}

async fn cleanup_selected_runtime(
  app_handle: &AppHandle,
  context: &Arc<Mutex<CleanupContext>>,
) -> Result<(), String> {
  let (profile, policy, mut lease) = {
    let context = context
      .lock()
      .map_err(|_| "registration cleanup context lock poisoned".to_string())?;
    (
      context.runtime.profile.clone(),
      context.runtime.policy.clone(),
      context.runtime.lease.clone(),
    )
  };

  crate::browser_runner::BrowserRunner::instance()
    .kill_runtime_browser(app_handle.clone(), &profile, &policy, &mut lease)
    .await
    .map_err(|error| format!("Registration runtime cleanup failed: {error}"))?;

  let mut context = context
    .lock()
    .map_err(|_| "registration cleanup context lock poisoned".to_string())?;
  context.runtime.lease = lease;
  context
    .lease_registry
    .release(&context.runtime.lease.lease_id);
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::profile_runtime::{DataMode, FingerprintMode};

  fn config() -> RegistrationConfig {
    serde_json::from_value(serde_json::json!({ "cdks": ["GMAIL-TEST"] }))
      .expect("minimal registration config")
  }

  fn write_selected_profile(
    profiles_dir: &std::path::Path,
    browser: &str,
  ) -> (BrowserProfile, std::path::PathBuf, Vec<u8>) {
    let profile = BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: "selected-registration-profile".to_string(),
      browser: browser.to_string(),
      version: "1.0.0".to_string(),
      host_os: Some(crate::profile::types::get_host_os()),
      ..BrowserProfile::default()
    };
    let profile_dir = profiles_dir.join(profile.id.to_string());
    std::fs::create_dir_all(&profile_dir).expect("create profile directory");
    let metadata_path = profile_dir.join("metadata.json");
    let metadata = serde_json::to_vec_pretty(&profile).expect("serialize profile");
    std::fs::write(&metadata_path, &metadata).expect("write profile metadata");
    (profile, metadata_path, metadata)
  }

  fn leaked_registry() -> &'static LeaseRegistry {
    Box::leak(Box::new(LeaseRegistry::new()))
  }

  #[test]
  fn selected_profile_preparation_is_read_only() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let (profile, metadata_path, metadata) = write_selected_profile(temp.path(), "chromium");
    let mut config = config();
    config.profile_id = Some(profile.id.to_string());

    let prepared = prepare_selected_profile_runtime(&config, temp.path(), leaked_registry())
      .expect("selected profile should prepare")
      .expect("selected runtime expected");

    assert_eq!(std::fs::read(metadata_path).unwrap(), metadata);
    drop(prepared);
  }

  #[test]
  fn selected_profile_preparation_rejects_missing_profile() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let mut config = config();
    config.profile_id = Some(uuid::Uuid::new_v4().to_string());

    let error = prepare_selected_profile_runtime(&config, temp.path(), leaked_registry())
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
    let mut config = config();
    config.profile_id = Some(profile.id.to_string());

    let error = prepare_selected_profile_runtime(&config, temp.path(), leaked_registry())
      .expect_err("running selected profile must fail");

    assert!(error.contains("running"));
  }

  #[test]
  fn selected_profile_preparation_rejects_ephemeral_camoufox() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let (profile, _, _) = write_selected_profile(temp.path(), "camoufox");
    let mut config = config();
    config.profile_id = Some(profile.id.to_string());
    config.browser_type = "camoufox".to_string();

    let error = prepare_selected_profile_runtime(&config, temp.path(), leaked_registry())
      .expect_err("ephemeral Camoufox reuse must fail closed");

    assert!(error.contains("ephemeral data"));
  }

  #[test]
  fn selected_profile_preparation_fails_fast_with_stable_busy_error() {
    let temp = tempfile::tempdir().expect("temp profiles");
    let (profile, _, _) = write_selected_profile(temp.path(), "chromium");
    let registry = leaked_registry();
    let mut config = config();
    config.profile_id = Some(profile.id.to_string());

    let first = prepare_selected_profile_runtime(&config, temp.path(), registry)
      .expect("first preparation should acquire lease");
    let error = prepare_selected_profile_runtime(&config, temp.path(), registry)
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
    let mut config = config();
    config.profile_id = Some(profile_id.clone());

    let prepared = prepare_selected_profile_runtime(&config, temp.path(), registry)
      .expect("selected profile should prepare");
    assert!(registry.is_leased(&profile_id));

    drop(prepared);
    assert!(!registry.is_leased(&profile_id));
  }

  #[test]
  fn selected_worker_policy_is_task_local_and_not_owned() {
    let mut config = config();
    config.data_mode = DataMode::Persistent;
    config.fingerprint_mode = FingerprintMode::Stable;
    config.network_mode = NetworkMode::Proxy;
    config.proxy_id = Some("proxy-task".to_string());
    let profile = BrowserProfile {
      id: uuid::Uuid::new_v4(),
      ..BrowserProfile::default()
    };
    let registry = leaked_registry();
    let lease = registry
      .try_acquire(&profile.id.to_string())
      .expect("selected profile lease");

    let runtime = SelectedProfileRuntime::new(&config, profile, lease.clone());
    let policy = runtime.policy();

    assert_eq!(
      policy.source_profile_id.as_deref(),
      Some(lease.source_profile_id.as_str())
    );
    assert_eq!(policy.data_mode, DataMode::Persistent);
    assert_eq!(policy.fingerprint_mode, FingerprintMode::Stable);
    assert_eq!(policy.proxy_id.as_deref(), Some("proxy-task"));
    assert_eq!(policy.vpn_id, None);
    assert!(!policy.clear_network);
    assert!(!policy.owns_generated_worker);
    assert!(!policy.persist_process_to_source);
    registry.release(&lease.lease_id);
  }

  #[test]
  fn selected_worker_policy_uses_slot_vpn_and_clears_stored_network_for_direct_mode() {
    let profile = BrowserProfile {
      id: uuid::Uuid::new_v4(),
      ..BrowserProfile::default()
    };
    let registry = leaked_registry();
    let lease = registry
      .try_acquire(&profile.id.to_string())
      .expect("selected profile lease");
    let mut vpn_config = config();
    vpn_config.network_mode = NetworkMode::Vpn;
    vpn_config.vpn_id = Some("vpn-base".to_string());

    let runtime = SelectedProfileRuntime::new(&vpn_config, profile, lease.clone());
    let slot_runtime = runtime.with_slot_vpn(Some("vpn-slot"));
    assert_eq!(slot_runtime.policy().vpn_id.as_deref(), Some("vpn-slot"));
    assert_eq!(slot_runtime.policy().proxy_id, None);
    assert!(!slot_runtime.policy().clear_network);

    let direct_profile = BrowserProfile {
      id: uuid::Uuid::new_v4(),
      ..BrowserProfile::default()
    };
    let direct_lease = registry
      .try_acquire(&direct_profile.id.to_string())
      .expect("direct selected profile lease");
    let direct_runtime =
      SelectedProfileRuntime::new(&config(), direct_profile, direct_lease.clone());
    assert!(direct_runtime.policy().clear_network);
    assert_eq!(direct_runtime.policy().proxy_id, None);
    assert_eq!(direct_runtime.policy().vpn_id, None);

    registry.release(&lease.lease_id);
    registry.release(&direct_lease.lease_id);
  }

  #[test]
  fn selected_lease_releases_only_after_cleanup_succeeds() {
    fn release_after_cleanup(
      registry: &LeaseRegistry,
      lease_id: &str,
      cleanup_result: Result<(), String>,
    ) -> Result<(), String> {
      cleanup_result?;
      registry.release(lease_id);
      Ok(())
    }

    let registry = leaked_registry();
    let profile_id = uuid::Uuid::new_v4().to_string();
    let lease = registry.try_acquire(&profile_id).expect("acquire lease");

    let error = release_after_cleanup(registry, &lease.lease_id, Err("cleanup failed".to_string()))
      .expect_err("failed cleanup must retain lease");
    assert_eq!(error, "cleanup failed");
    assert!(registry.is_leased(&profile_id));

    release_after_cleanup(registry, &lease.lease_id, Ok(()))
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
        if attempt <= 2 {
          Err(format!("cleanup attempt {attempt} failed"))
        } else {
          let _ = retried_tx.send(());
          Ok(())
        }
      }) as CleanupFuture
    });
    let mut guard = SelectedProfileRuntimeCleanupGuard::with_action(action);

    assert!(guard.close().await.is_err());
    drop(guard);

    retried_rx
      .recv_timeout(std::time::Duration::from_secs(2))
      .expect("drop should retry cleanup");
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
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
      }) as CleanupFuture
    });
    let mut guard = SelectedProfileRuntimeCleanupGuard::with_action(action);

    guard.close().await.expect("cleanup should succeed");
    drop(guard);

    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
  }
}
