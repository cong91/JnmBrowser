//! Concrete production-browser lifecycle for 2FA backfill.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use tauri::AppHandle;
use tokio::sync::Mutex as AsyncMutex;

use super::commands::{BackfillBrowser, BackfillNetworkConfig};
use super::engine::{BackfillBrowserFactory, BackfillBrowserSession};
use crate::auto_service::openai::browser::{attach_browser_session, BrowserSession};
use crate::auto_service::openai::chatgpt_auth::{BrowserAuthAdapter, ExistingAccountAuthAdapter};
use crate::auto_service::openai::chatgpt_two_factor::{BrowserTwoFactorAdapter, TwoFactorAdapter};

trait ProductionBrowserPlatform: Send + Sync {
  fn installed_version(&self, browser: &BackfillBrowser) -> Result<String, String>;
  fn validate_network(&self, network: &BackfillNetworkConfig) -> Result<(), String>;
  fn note_profile_creation(&self) {}
}

struct SystemProductionBrowserPlatform;

impl ProductionBrowserPlatform for SystemProductionBrowserPlatform {
  fn installed_version(&self, browser: &BackfillBrowser) -> Result<String, String> {
    let browser_name = match browser {
      BackfillBrowser::Chromium => "chromium",
      BackfillBrowser::Camoufox => "camoufox",
    };
    let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
    let mut versions = registry.get_downloaded_versions(browser_name);
    versions.sort_by(|a, b| {
      crate::api_client::VersionComponent::parse(b)
        .cmp(&crate::api_client::VersionComponent::parse(a))
    });
    versions
      .into_iter()
      .find(|version| registry.is_browser_downloaded(browser_name, version))
      .ok_or_else(|| {
        format!(
          "No installed {browser_name} version is available. Install the selected browser first."
        )
      })
  }

  fn validate_network(&self, network: &BackfillNetworkConfig) -> Result<(), String> {
    match network {
      BackfillNetworkConfig::None => Ok(()),
      BackfillNetworkConfig::Proxy { proxy_id } => {
        let proxy_id = proxy_id.trim();
        if proxy_id.is_empty() {
          return Err("Selected proxy ID is empty".into());
        }
        crate::proxy_manager::PROXY_MANAGER
          .get_proxy_settings_by_id(proxy_id)
          .map(|_| ())
          .ok_or_else(|| format!("Selected proxy ID '{proxy_id}' does not exist"))
      }
      BackfillNetworkConfig::Vpn { vpn_id } => {
        let vpn_id = vpn_id.trim();
        if vpn_id.is_empty() {
          return Err("Selected VPN ID is empty".into());
        }
        crate::vpn::VPN_STORAGE
          .lock()
          .map_err(|_| "VPN storage lock is poisoned".to_string())?
          .load_config(vpn_id)
          .map(|_| ())
          .map_err(|error| format!("Selected VPN ID '{vpn_id}' is unavailable: {error}"))
      }
    }
  }
}

fn release_selected_lease_after_cleanup(
  registry: &'static crate::profile_runtime::LeaseRegistry,
  lease_id: &str,
  cleanup_result: Result<(), String>,
) -> Result<(), String> {
  cleanup_result?;
  registry.release(lease_id);
  Ok(())
}

#[derive(Clone)]
pub(crate) struct SelectedBrowserRuntime {
  state: Arc<AsyncMutex<SelectedBrowserRuntimeState>>,
  lease_registry: &'static crate::profile_runtime::LeaseRegistry,
}

struct SelectedBrowserRuntimeState {
  profile: crate::profile::BrowserProfile,
  policy: crate::profile_runtime::LaunchPolicy,
  lease: crate::profile_runtime::RuntimeLease,
}

impl SelectedBrowserRuntime {
  pub(crate) fn new(
    profile: crate::profile::BrowserProfile,
    policy: crate::profile_runtime::LaunchPolicy,
    lease: crate::profile_runtime::RuntimeLease,
    lease_registry: &'static crate::profile_runtime::LeaseRegistry,
  ) -> Self {
    Self {
      state: Arc::new(AsyncMutex::new(SelectedBrowserRuntimeState {
        profile,
        policy,
        lease,
      })),
      lease_registry,
    }
  }

  fn validate_ownership(&self, state: &SelectedBrowserRuntimeState) -> Result<(), String> {
    validate_selected_runtime_ownership(&state.profile, &state.policy, &state.lease)?;
    if self
      .lease_registry
      .holder_lease_id(&state.lease.source_profile_id)
      .as_deref()
      != Some(state.lease.lease_id.as_str())
    {
      return Err("refusing to clean a selected profile runtime owned by another lease".into());
    }
    Ok(())
  }

  async fn snapshot(
    &self,
  ) -> (
    crate::profile::BrowserProfile,
    crate::profile_runtime::LaunchPolicy,
    crate::profile_runtime::RuntimeLease,
  ) {
    let state = self.state.lock().await;
    (
      state.profile.clone(),
      state.policy.clone(),
      state.lease.clone(),
    )
  }

  async fn launch(
    &self,
    app_handle: &AppHandle<tauri::Wry>,
  ) -> Result<crate::profile::BrowserProfile, String> {
    let mut state = self.state.lock().await;
    self.validate_ownership(&state)?;
    let profile = state.profile.clone();
    let policy = state.policy.clone();
    crate::browser_runner::BrowserRunner::instance()
      .launch_browser_with_policy(
        app_handle.clone(),
        &profile,
        Some("about:blank".into()),
        None,
        &policy,
        &mut state.lease,
      )
      .await
      .map_err(|error| format!("Launch selected 2FA backfill profile: {error}"))
  }

  async fn cleanup(&self, app_handle: &AppHandle<tauri::Wry>) -> Result<(), String> {
    let mut state = self.state.lock().await;
    self.validate_ownership(&state)?;
    let profile = state.profile.clone();
    let policy = state.policy.clone();
    crate::browser_runner::BrowserRunner::instance()
      .kill_runtime_browser(app_handle.clone(), &profile, &policy, &mut state.lease)
      .await
      .map_err(|error| format!("Clean selected 2FA backfill profile runtime: {error}"))
  }

  async fn close(&self, app_handle: &AppHandle<tauri::Wry>) -> Result<(), String> {
    let (_, _, lease) = self.snapshot().await;
    release_selected_lease_after_cleanup(
      self.lease_registry,
      &lease.lease_id,
      self.cleanup(app_handle).await,
    )
  }
}

fn validate_selected_runtime_ownership(
  profile: &crate::profile::BrowserProfile,
  policy: &crate::profile_runtime::LaunchPolicy,
  lease: &crate::profile_runtime::RuntimeLease,
) -> Result<(), String> {
  let profile_id = profile.id.to_string();
  if policy.source_profile_id.as_deref() != Some(profile_id.as_str())
    || lease.source_profile_id != profile_id
  {
    return Err("refusing to clean a selected profile runtime owned by another lease".into());
  }
  if policy.owns_generated_worker || policy.persist_process_to_source {
    return Err("selected profile runtime policy has invalid ownership flags".into());
  }
  Ok(())
}

pub(crate) struct ProductionBrowserFactory {
  platform: Arc<dyn ProductionBrowserPlatform>,
  selected_runtime: Option<SelectedBrowserRuntime>,
}

impl ProductionBrowserFactory {
  pub(crate) fn new(selected_runtime: Option<SelectedBrowserRuntime>) -> Self {
    Self {
      platform: Arc::new(SystemProductionBrowserPlatform),
      selected_runtime,
    }
  }

  pub(crate) fn selected_runtime_cleanup_guard(
    &self,
    app_handle: AppHandle<tauri::Wry>,
  ) -> Option<BrowserCleanupGuard> {
    let runtime = self.selected_runtime.clone()?;
    Some(BrowserCleanupGuard::new(Arc::new(move || {
      let runtime = runtime.clone();
      let app_handle = app_handle.clone();
      Box::pin(async move { runtime.close(&app_handle).await })
    })))
  }

  #[cfg(test)]
  fn with_platform(platform: Arc<dyn ProductionBrowserPlatform>) -> Self {
    Self {
      platform,
      selected_runtime: None,
    }
  }
}

type BrowserCleanupFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
type BrowserCleanupAction = Arc<dyn Fn() -> BrowserCleanupFuture + Send + Sync>;

pub(crate) struct BrowserCleanupGuard {
  action: Option<BrowserCleanupAction>,
}

impl BrowserCleanupGuard {
  fn new(action: BrowserCleanupAction) -> Self {
    Self {
      action: Some(action),
    }
  }

  pub(crate) async fn close(&mut self) -> Result<(), String> {
    let Some(action) = self.action.as_ref().cloned() else {
      return Ok(());
    };
    action().await?;
    self.action.take();
    Ok(())
  }
}

impl Drop for BrowserCleanupGuard {
  fn drop(&mut self) {
    let Some(action) = self.action.take() else {
      return;
    };
    match tokio::runtime::Handle::try_current() {
      Ok(handle) => {
        handle.spawn(async move {
          if let Err(error) = action().await {
            log::error!("Dropped 2FA backfill browser cleanup failed: {error}");
          }
        });
      }
      Err(error) => {
        std::thread::spawn(move || {
          let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
          {
            Ok(runtime) => runtime,
            Err(runtime_error) => {
              log::error!(
                "Dropped 2FA backfill browser session without a Tokio runtime ({error}); fallback runtime creation failed: {runtime_error}"
              );
              return;
            }
          };
          if let Err(cleanup_error) = runtime.block_on(action()) {
            log::error!("Fallback 2FA backfill browser cleanup failed: {cleanup_error}");
          }
        });
      }
    }
  }
}

struct ProductionBrowserSession {
  browser: Option<BrowserSession>,
  device_id: String,
  cleanup: BrowserCleanupGuard,
}

#[async_trait]
impl BackfillBrowserSession for ProductionBrowserSession {
  fn make_auth_adapter(&mut self) -> Box<dyn ExistingAccountAuthAdapter + Send + '_> {
    let browser = self
      .browser
      .as_mut()
      .expect("production browser session must be live before authentication");
    Box::new(BrowserAuthAdapter::new(browser, &self.device_id))
  }

  fn make_two_factor_adapter(&mut self) -> Box<dyn TwoFactorAdapter + Send + '_> {
    let browser = self
      .browser
      .as_mut()
      .expect("production browser session must be live before 2FA setup");
    Box::new(BrowserTwoFactorAdapter::new(browser))
  }

  async fn close(&mut self) -> Result<(), String> {
    self.browser.take();
    self.cleanup.close().await
  }
}

#[async_trait]
impl BackfillBrowserFactory<tauri::Wry> for ProductionBrowserFactory {
  fn preflight(
    &self,
    browser: &BackfillBrowser,
    network: &BackfillNetworkConfig,
  ) -> Result<(), String> {
    if self.selected_runtime.is_none() {
      self.platform.installed_version(browser)?;
    }
    self.platform.validate_network(network)
  }

  async fn launch(
    &self,
    app_handle: &AppHandle<tauri::Wry>,
    browser: &BackfillBrowser,
    network: &BackfillNetworkConfig,
    device_id: &str,
    login_email: &str,
    operation_id: &str,
  ) -> Result<Box<dyn BackfillBrowserSession + Send>, String> {
    self.platform.validate_network(network)?;
    if let Some(selected_runtime) = &self.selected_runtime {
      return launch_selected_browser_session(app_handle, selected_runtime, device_id, login_email)
        .await;
    }

    let version = self.platform.installed_version(browser)?;
    self.platform.note_profile_creation();

    let browser_name = match browser {
      BackfillBrowser::Chromium => "chromium",
      BackfillBrowser::Camoufox => "camoufox",
    };
    let (proxy_id, vpn_id) = match network {
      BackfillNetworkConfig::None => (None, None),
      BackfillNetworkConfig::Proxy { proxy_id } => (Some(proxy_id.trim().to_string()), None),
      BackfillNetworkConfig::Vpn { vpn_id } => (None, Some(vpn_id.trim().to_string())),
    };
    let camoufox_config = matches!(browser, BackfillBrowser::Camoufox).then(|| {
      crate::camoufox_manager::CamoufoxConfig {
        fingerprint: None,
        randomize_fingerprint_on_launch: Some(true),
        geoip: Some(serde_json::Value::Bool(true)),
        ..Default::default()
      }
    });
    let chromium_config = matches!(browser, BackfillBrowser::Chromium).then(|| {
      crate::chromium_manager::ChromiumConfig {
        fingerprint: None,
        randomize_fingerprint_on_launch: Some(true),
        ..Default::default()
      }
    });
    let release_type =
      if crate::api_client::is_browser_version_nightly(browser_name, &version, None) {
        "nightly"
      } else {
        "stable"
      };
    let profile_name = format!("twofa-backfill-worker-{}", operation_id);
    let profile = crate::profile::manager::create_browser_profile_with_group(
      app_handle.clone(),
      profile_name,
      browser_name.to_string(),
      version,
      release_type.to_string(),
      proxy_id,
      vpn_id,
      camoufox_config,
      chromium_config,
      None,
      true,
      None,
      None,
    )
    .await
    .map_err(|error| format!("Create 2FA backfill worker profile: {error}"))?;

    let launched = match crate::browser_runner::BrowserRunner::instance()
      .launch_browser(
        app_handle.clone(),
        &profile,
        Some("about:blank".into()),
        None,
      )
      .await
    {
      Ok(launched) => launched,
      Err(error) => {
        let rollback = cleanup_failed_launch(app_handle, &profile, operation_id).await;
        return match rollback {
          Ok(()) => Err(format!("Launch 2FA backfill worker profile: {error}")),
          Err(cleanup_error) => Err(format!(
            "Launch 2FA backfill worker profile: {error}; rollback failed: {cleanup_error}"
          )),
        };
      }
    };

    let mut attached = match attach_browser_session(&launched).await {
      Ok(attached) => attached,
      Err(error) => {
        if let Err(cleanup_error) = cleanup_failed_launch(app_handle, &launched, operation_id).await
        {
          return Err(format!("{error}; rollback failed: {cleanup_error}"));
        }
        return Err(error);
      }
    };
    if let Err(error) = attached
      .prepare_existing_account_login(device_id, login_email)
      .await
    {
      drop(attached);
      let rollback = cleanup_failed_launch(app_handle, &launched, operation_id).await;
      return match rollback {
        Ok(()) => Err(format!("Prepare existing-account login: {error}")),
        Err(cleanup_error) => Err(format!(
          "Prepare existing-account login: {error}; rollback failed: {cleanup_error}"
        )),
      };
    }

    let cleanup_app_handle = app_handle.clone();
    let cleanup_profile = launched.clone();
    let cleanup_operation_id = operation_id.to_string();
    let cleanup = BrowserCleanupGuard::new(Arc::new(move || {
      let app_handle = cleanup_app_handle.clone();
      let profile = cleanup_profile.clone();
      let operation_id = cleanup_operation_id.clone();
      Box::pin(async move { cleanup_worker_profile(&app_handle, &profile, &operation_id).await })
    }));
    Ok(Box::new(ProductionBrowserSession {
      browser: Some(attached),
      device_id: device_id.to_string(),
      cleanup,
    }))
  }
}

async fn launch_selected_browser_session(
  app_handle: &AppHandle<tauri::Wry>,
  runtime: &SelectedBrowserRuntime,
  device_id: &str,
  login_email: &str,
) -> Result<Box<dyn BackfillBrowserSession + Send>, String> {
  let launched = runtime.launch(app_handle).await?;
  let mut attached = match attach_browser_session(&launched).await {
    Ok(attached) => attached,
    Err(error) => {
      if let Err(cleanup_error) = runtime.cleanup(app_handle).await {
        return Err(format!("{error}; rollback failed: {cleanup_error}"));
      }
      return Err(error);
    }
  };
  if let Err(error) = attached
    .prepare_existing_account_login(device_id, login_email)
    .await
  {
    drop(attached);
    return match runtime.cleanup(app_handle).await {
      Ok(()) => Err(format!("Prepare existing-account login: {error}")),
      Err(cleanup_error) => Err(format!(
        "Prepare existing-account login: {error}; rollback failed: {cleanup_error}"
      )),
    };
  }

  let cleanup_runtime = runtime.clone();
  let cleanup_app_handle = app_handle.clone();
  let cleanup = BrowserCleanupGuard::new(Arc::new(move || {
    let runtime = cleanup_runtime.clone();
    let app_handle = cleanup_app_handle.clone();
    Box::pin(async move { runtime.cleanup(&app_handle).await })
  }));
  Ok(Box::new(ProductionBrowserSession {
    browser: Some(attached),
    device_id: device_id.to_string(),
    cleanup,
  }))
}

async fn cleanup_worker_profile(
  app_handle: &AppHandle<tauri::Wry>,
  profile: &crate::profile::BrowserProfile,
  operation_id: &str,
) -> Result<(), String> {
  if profile.name != format!("twofa-backfill-worker-{operation_id}") {
    return Err("refusing to clean a worker profile owned by another operation".into());
  }
  if !profile.ephemeral {
    return Err("refusing to clean a non-ephemeral 2FA worker profile".into());
  }
  crate::browser_runner::BrowserRunner::instance()
    .kill_browser_process(app_handle.clone(), profile)
    .await
    .map_err(|error| format!("terminate worker profile {}: {error}", profile.id))?;
  crate::profile::ProfileManager::instance()
    .delete_profile(app_handle, &profile.id.to_string())
    .map_err(|error| format!("delete worker profile {}: {error}", profile.id))
}

async fn cleanup_failed_launch(
  app_handle: &AppHandle<tauri::Wry>,
  profile: &crate::profile::BrowserProfile,
  operation_id: &str,
) -> Result<(), String> {
  let result = cleanup_worker_profile(app_handle, profile, operation_id).await;
  if let Err(error) = &result {
    log::error!(
      "2FA backfill launch rollback could not clean up worker profile {}; profile retained: {error}",
      profile.id
    );
  }
  result
}

#[cfg(test)]
mod tests {
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex};

  use super::*;

  struct RecordingProductionPlatform {
    profile_creations: Mutex<usize>,
  }

  impl RecordingProductionPlatform {
    fn with_missing_route() -> Self {
      Self {
        profile_creations: Mutex::new(0),
      }
    }

    fn profile_creation_count(&self) -> usize {
      *self.profile_creations.lock().unwrap()
    }
  }

  impl ProductionBrowserPlatform for RecordingProductionPlatform {
    fn installed_version(&self, _browser: &BackfillBrowser) -> Result<String, String> {
      Ok("test-version".into())
    }

    fn validate_network(&self, _network: &BackfillNetworkConfig) -> Result<(), String> {
      Err("selected route does not exist".into())
    }

    fn note_profile_creation(&self) {
      *self.profile_creations.lock().unwrap() += 1;
    }
  }

  #[test]
  fn missing_explicit_routes_fail_preflight_before_profile_creation() {
    for network in [
      BackfillNetworkConfig::Proxy {
        proxy_id: "missing-proxy".into(),
      },
      BackfillNetworkConfig::Vpn {
        vpn_id: "missing-vpn".into(),
      },
    ] {
      let platform = Arc::new(RecordingProductionPlatform::with_missing_route());
      let factory = ProductionBrowserFactory::with_platform(platform.clone());

      let result = <ProductionBrowserFactory as BackfillBrowserFactory<tauri::Wry>>::preflight(
        &factory,
        &BackfillBrowser::Chromium,
        &network,
      );

      assert!(result.is_err());
      assert_eq!(
        platform.profile_creation_count(),
        0,
        "preflight must reject a missing route before profile creation"
      );
    }
  }

  #[test]
  fn selected_runtime_refuses_profile_or_policy_ownership_mismatch() {
    let profile = crate::profile::BrowserProfile {
      id: uuid::Uuid::new_v4(),
      ..crate::profile::BrowserProfile::default()
    };
    let registry = Box::leak(Box::new(crate::profile_runtime::LeaseRegistry::new()));
    let lease = registry
      .try_acquire(&profile.id.to_string())
      .expect("selected lease");
    let valid = crate::profile_runtime::LaunchPolicy::for_source_profile(profile.id.to_string());
    assert!(validate_selected_runtime_ownership(&profile, &valid, &lease).is_ok());

    let foreign_policy =
      crate::profile_runtime::LaunchPolicy::for_source_profile(uuid::Uuid::new_v4().to_string());
    assert!(
      validate_selected_runtime_ownership(&profile, &foreign_policy, &lease)
        .unwrap_err()
        .contains("another lease")
    );

    let foreign_lease = registry
      .try_acquire(&uuid::Uuid::new_v4().to_string())
      .expect("foreign lease");
    assert!(
      validate_selected_runtime_ownership(&profile, &valid, &foreign_lease)
        .unwrap_err()
        .contains("another lease")
    );
    registry.release(&lease.lease_id);
    registry.release(&foreign_lease.lease_id);
  }

  #[tokio::test]
  async fn selected_runtime_refuses_a_stale_lease_holder() {
    let profile = crate::profile::BrowserProfile {
      id: uuid::Uuid::new_v4(),
      ..crate::profile::BrowserProfile::default()
    };
    let profile_id = profile.id.to_string();
    let registry = Box::leak(Box::new(crate::profile_runtime::LeaseRegistry::new()));
    let stale_lease = registry.try_acquire(&profile_id).expect("stale lease");
    let policy = crate::profile_runtime::LaunchPolicy::for_source_profile(profile_id.clone());
    let runtime = SelectedBrowserRuntime::new(profile, policy, stale_lease.clone(), registry);
    registry.release(&stale_lease.lease_id);
    let replacement = registry
      .try_acquire(&profile_id)
      .expect("replacement lease");

    let state = runtime.state.lock().await;
    assert!(runtime
      .validate_ownership(&state)
      .unwrap_err()
      .contains("another lease"));
    drop(state);
    registry.release(&replacement.lease_id);
  }

  #[test]
  fn selected_lease_releases_only_after_cleanup_succeeds() {
    let registry = Box::leak(Box::new(crate::profile_runtime::LeaseRegistry::new()));
    let profile_id = uuid::Uuid::new_v4().to_string();
    let lease = registry.try_acquire(&profile_id).expect("selected lease");

    let error =
      release_selected_lease_after_cleanup(registry, &lease.lease_id, Err("cleanup failed".into()))
        .expect_err("failed cleanup must retain lease");
    assert_eq!(error, "cleanup failed");
    assert!(registry.is_leased(&profile_id));

    release_selected_lease_after_cleanup(registry, &lease.lease_id, Ok(()))
      .expect("successful cleanup releases lease");
    assert!(!registry.is_leased(&profile_id));
  }

  #[tokio::test]
  async fn browser_cleanup_guard_drop_schedules_cleanup_and_successful_close_disarms() {
    let drop_calls = Arc::new(AtomicUsize::new(0));
    let drop_calls_for_action = Arc::clone(&drop_calls);
    let guard = BrowserCleanupGuard::new(Arc::new(move || {
      let calls = Arc::clone(&drop_calls_for_action);
      Box::pin(async move {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
      })
    }));
    drop(guard);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
      while drop_calls.load(Ordering::SeqCst) != 1 {
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("Drop must schedule browser cleanup on the current runtime");

    let close_calls = Arc::new(AtomicUsize::new(0));
    let close_calls_for_action = Arc::clone(&close_calls);
    let mut guard = BrowserCleanupGuard::new(Arc::new(move || {
      let calls = Arc::clone(&close_calls_for_action);
      Box::pin(async move {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
      })
    }));
    guard.close().await.unwrap();
    drop(guard);
    tokio::task::yield_now().await;
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);

    let retry_calls = Arc::new(AtomicUsize::new(0));
    let retry_calls_for_action = Arc::clone(&retry_calls);
    let mut guard = BrowserCleanupGuard::new(Arc::new(move || {
      let calls = Arc::clone(&retry_calls_for_action);
      Box::pin(async move {
        let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt == 1 {
          Err("first cleanup failed".into())
        } else {
          Ok(())
        }
      })
    }));
    assert!(guard.close().await.is_err());
    drop(guard);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
      while retry_calls.load(Ordering::SeqCst) != 2 {
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("Drop must retry a failed cleanup action");
    assert_eq!(retry_calls.load(Ordering::SeqCst), 2);
  }
}
