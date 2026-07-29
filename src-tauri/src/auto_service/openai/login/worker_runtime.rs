use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tauri::AppHandle;

use super::types::{LoginConfig, LoginNetworkMode};
use crate::profile::BrowserProfile;
use crate::profile_runtime::{LaunchPolicy, LeaseRegistry, RuntimeLease};

type CleanupFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
type CleanupAction = Arc<dyn Fn() -> CleanupFuture + Send + Sync>;

/// Resolved Auto Login worker profile, launch policy, and full-lifecycle lease.
#[derive(Clone)]
pub(super) struct WorkerRuntime {
  profile: BrowserProfile,
  policy: LaunchPolicy,
  lease: RuntimeLease,
}

impl WorkerRuntime {
  pub(super) fn new(config: &LoginConfig, profile: BrowserProfile, lease: RuntimeLease) -> Self {
    let policy = launch_policy(config, &profile.id.to_string());
    Self {
      profile,
      policy,
      lease,
    }
  }

  pub(super) fn profile(&self) -> &BrowserProfile {
    &self.profile
  }

  pub(super) fn ephemeral_runtime_key(&self) -> Option<&str> {
    self.lease.ephemeral_runtime_key.as_deref()
  }

  #[cfg(test)]
  pub(super) fn lease_id(&self) -> &str {
    &self.lease.lease_id
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
      .map_err(|error| error.to_string())
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
}

fn launch_policy(config: &LoginConfig, profile_id: &str) -> LaunchPolicy {
  let mut policy = LaunchPolicy::for_source_profile(profile_id);
  policy.data_mode = config.data_mode;
  policy.fingerprint_mode = config.fingerprint_mode;
  match config.network_mode {
    LoginNetworkMode::Proxy => policy.proxy_id = config.proxy_id.clone(),
    LoginNetworkMode::Vpn => policy.vpn_id = config.effective_vpn_id(),
    LoginNetworkMode::None | LoginNetworkMode::Nord => policy.clear_network = true,
  }
  policy
}

struct CleanupContext {
  runtime: WorkerRuntime,
  lease_registry: &'static LeaseRegistry,
}

/// Final runtime cleanup. Explicit close disarms Drop only after cleanup and release succeed.
pub(super) struct WorkerRuntimeCleanupGuard {
  action: Option<CleanupAction>,
}

impl WorkerRuntimeCleanupGuard {
  pub(super) fn new(
    app_handle: AppHandle,
    runtime: WorkerRuntime,
    lease_registry: &'static LeaseRegistry,
  ) -> Self {
    let context = Arc::new(Mutex::new(CleanupContext {
      runtime,
      lease_registry,
    }));
    let action = Arc::new(move || {
      let app_handle = app_handle.clone();
      let context = context.clone();
      Box::pin(async move { cleanup_worker_runtime(&app_handle, &context).await }) as CleanupFuture
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

impl Drop for WorkerRuntimeCleanupGuard {
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

async fn cleanup_worker_runtime(
  app_handle: &AppHandle,
  context: &Arc<Mutex<CleanupContext>>,
) -> Result<(), String> {
  let (profile, policy, mut lease) = {
    let context = context
      .lock()
      .map_err(|_| "Auto Login cleanup context lock poisoned".to_string())?;
    (
      context.runtime.profile.clone(),
      context.runtime.policy.clone(),
      context.runtime.lease.clone(),
    )
  };
  crate::browser_runner::BrowserRunner::instance()
    .kill_runtime_browser(app_handle.clone(), &profile, &policy, &mut lease)
    .await
    .map_err(|error| format!("Auto Login runtime cleanup failed: {error}"))?;

  let mut context = context
    .lock()
    .map_err(|_| "Auto Login cleanup context lock poisoned".to_string())?;
  context.runtime.lease = lease;
  context
    .lease_registry
    .release(&context.runtime.lease.lease_id);
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn base_config() -> LoginConfig {
    let mut config: LoginConfig = serde_json::from_value(serde_json::json!({
      "credentialsText": "user@example.com|password"
    }))
    .expect("minimal login config must deserialize");
    config.parse_credentials();
    config.normalize();
    config
  }

  #[test]
  fn selected_login_policy_is_task_local_and_not_owned() {
    let mut config: LoginConfig = serde_json::from_value(serde_json::json!({
      "credentialsText": "user@example.com|password",
      "dataMode": "persistent",
      "fingerprintMode": "stable",
      "networkMode": "proxy",
      "proxyId": "proxy-task"
    }))
    .unwrap();
    config.parse_credentials();
    config.normalize();

    let policy = launch_policy(&config, "selected-profile");

    assert_eq!(
      policy.source_profile_id.as_deref(),
      Some("selected-profile")
    );
    assert_eq!(
      policy.data_mode,
      crate::profile_runtime::DataMode::Persistent
    );
    assert_eq!(
      policy.fingerprint_mode,
      crate::profile_runtime::FingerprintMode::Stable
    );
    assert_eq!(policy.proxy_id.as_deref(), Some("proxy-task"));
    assert_eq!(policy.vpn_id, None);
    assert!(!policy.clear_network);
    assert!(!policy.owns_generated_worker);
    assert!(!policy.persist_process_to_source);
  }

  #[test]
  fn direct_login_policy_clears_stored_network() {
    let config = base_config();

    let policy = launch_policy(&config, "worker-profile");

    assert!(policy.clear_network);
    assert_eq!(policy.proxy_id, None);
    assert_eq!(policy.vpn_id, None);
  }

  #[test]
  fn worker_runtime_encapsulates_profile_policy_and_lease() {
    let config = base_config();
    let profile = BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: "selected".to_string(),
      browser: "chromium".to_string(),
      version: "1.0.0".to_string(),
      ..BrowserProfile::default()
    };
    let registry = LeaseRegistry::new();
    let lease = registry
      .try_acquire(&profile.id.to_string())
      .expect("worker lease");

    let runtime = WorkerRuntime::new(&config, profile.clone(), lease.clone());

    assert_eq!(runtime.profile.id, profile.id);
    assert_eq!(
      runtime.policy.source_profile_id.as_deref(),
      Some(profile.id.to_string().as_str())
    );
    assert_eq!(runtime.lease.lease_id, lease.lease_id);
    registry.release(&lease.lease_id);
  }

  #[test]
  fn worker_lease_releases_only_after_cleanup_succeeds() {
    fn release_after_cleanup(
      registry: &LeaseRegistry,
      lease_id: &str,
      cleanup_result: Result<(), String>,
    ) -> Result<(), String> {
      cleanup_result?;
      registry.release(lease_id);
      Ok(())
    }

    let registry = LeaseRegistry::new();
    let profile_id = uuid::Uuid::new_v4().to_string();
    let lease = registry.try_acquire(&profile_id).expect("acquire lease");

    let error = release_after_cleanup(
      &registry,
      &lease.lease_id,
      Err("cleanup failed".to_string()),
    )
    .expect_err("failed cleanup must retain lease");
    assert_eq!(error, "cleanup failed");
    assert!(registry.is_leased(&profile_id));

    release_after_cleanup(&registry, &lease.lease_id, Ok(()))
      .expect("successful retry releases lease");
    assert!(!registry.is_leased(&profile_id));
  }

  #[tokio::test]
  async fn cleanup_guard_retries_after_explicit_failure() {
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
      }) as CleanupFuture
    });
    let mut guard = WorkerRuntimeCleanupGuard::with_action(action);

    assert!(guard.close().await.is_err());
    drop(guard);

    retried_rx
      .recv_timeout(std::time::Duration::from_secs(2))
      .expect("drop should retry cleanup");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
  }

  #[tokio::test]
  async fn successful_close_disarms_drop() {
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
    let mut guard = WorkerRuntimeCleanupGuard::with_action(action);

    guard
      .close()
      .await
      .expect("explicit cleanup should succeed");
    drop(guard);
    std::thread::sleep(std::time::Duration::from_millis(50));

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
  }
}
