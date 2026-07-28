use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::Emitter;

use super::engine;
use super::store;
use super::task;
use super::types::AccountCheckResult;
use crate::browser_runner::BrowserRunner;

/// Start a new account check batch. Returns task_id.
/// Creates ephemeral worker profile, launches browser, checks all credentials, then cleans up.
#[tauri::command]
pub async fn start_openai_account_check(
  app_handle: tauri::AppHandle,
  credentials_text: String,
) -> Result<String, String> {
  let credentials =
    crate::auto_service::openai::login::types::LoginCredential::parse_batch(&credentials_text);

  if credentials.is_empty() {
    return Err("No valid credentials found".into());
  }

  let task_id = uuid::Uuid::new_v4().to_string();
  let cancel_flag = Arc::new(AtomicBool::new(false));
  let total = credentials.len();

  task::register_task(
    task_id.clone(),
    super::task::AccountCheckTaskHandle {
      cancel_flag: cancel_flag.clone(),
    },
  );

  // Spawn async task that owns the browser lifecycle

  let app_handle_for_task = app_handle.clone();
  let app_handle_for_emit = app_handle.clone();
  let task_id_clone = task_id.clone();
  let cancel_flag_clone = cancel_flag.clone();

  tokio::spawn(async move {

    let result = run_check_batch(
      app_handle_for_task,
      &task_id_clone,
      credentials,
      cancel_flag_clone,
    )
    .await;

    if let Err(error) = result {
      log::error!("Account check batch failed: {error}");

    }

    // Emit batch-complete terminal event
    let _ = app_handle_for_emit.emit(
      "openai-account-check-progress",
      super::types::CheckProgress {
        task_id: task_id_clone.clone(),
        account_key: String::new(),
        credential_index: 0,
        total_credentials: total,
        step: super::types::CheckStep::Completed,
        outcome: None,
        reason_code: None,
        terminal: true,
      },
    );
  });

  Ok(task_id)
}

async fn run_check_batch(
  app_handle: tauri::AppHandle,
  task_id: &str,
  credentials: Vec<crate::auto_service::openai::login::types::LoginCredential>,
  cancel_flag: Arc<AtomicBool>,
) -> Result<(), String> {
  use crate::auto_service::openai::browser::attach_browser_session;
  use crate::chromium_manager::ChromiumConfig;
  use crate::downloaded_browsers_registry::DownloadedBrowsersRegistry;
  use crate::profile::manager::create_browser_profile_with_group;

  let operation_id = task_id.to_string();
  let profile_name = format!("acct-check-worker-{operation_id}");

  // ── Chromium version ──────────────────────────────────
  let registry = DownloadedBrowsersRegistry::instance();
  let _ = registry.load();

  let mut versions = registry.get_downloaded_versions("chromium");
  if versions.is_empty() {
    let dir = crate::app_dirs::data_dir().join("binaries").join("fingerprint-chromium");
    if let Ok(entries) = std::fs::read_dir(&dir) {
      for entry in entries.flatten() {
        if entry.path().is_dir() {
          if let Some(v) = entry.file_name().to_str() {
            versions.push(v.to_string());
          }
        }
      }
    }
  }
  versions.sort_by(|a, b| {
    crate::api_client::VersionComponent::parse(b).cmp(&crate::api_client::VersionComponent::parse(a))
  });
  let version = versions.into_iter().next()
    .ok_or_else(|| "No Chromium version available".to_string())?;
  let release_type = if crate::api_client::is_browser_version_nightly("chromium", &version, None) {
    "nightly" } else { "stable" };

  // ── VPN configs ───────────────────────────────────────
  let vpn_configs = refresh_nord_configs(&app_handle).await.unwrap_or_else(|e| {
    log::warn!("Nord refresh failed: {e}");
    Vec::new()
  });

  // ── One ephemeral profile for all accounts ────────────
  let profile = create_browser_profile_with_group(
    app_handle.clone(), profile_name.clone(), "chromium".to_string(),
    version, release_type.to_string(),
    None, None, None,
    Some(ChromiumConfig { fingerprint: None, randomize_fingerprint_on_launch: Some(true), ..Default::default() }),
    None, true, None, None,
  ).await.map_err(|e| format!("Create profile: {e}"))?;

  // ── Process each account (kill + relaunch per account) ─
  let total = credentials.len();
  for (index, cred) in credentials.into_iter().enumerate() {
    if cancel_flag.load(Ordering::SeqCst) { break; }

    let vpn_id = if vpn_configs.is_empty() { None }
      else { Some(vpn_configs[index % vpn_configs.len()].id.clone()) };

    // Launch browser (fingerprint randomized on each launch)
    let launched = match BrowserRunner::instance()
      .launch_browser(app_handle.clone(), &profile, Some("about:blank".into()), None).await
    {
      Ok(l) => l,
      Err(e) => {
        let msg = format!("{e}");
        // Retry without VPN if VPN worker crashed
        if msg.contains("VPN worker") && vpn_id.is_some() {
          log::warn!("VPN failed, retrying without: {msg}");
          match BrowserRunner::instance()
            .launch_browser(app_handle.clone(), &profile, Some("about:blank".into()), None).await
          {
            Ok(l) => l,
            Err(e2) => { log::error!("Launch fallback: {e2}"); continue; }
          }
        } else { log::error!("Launch: {e}"); continue; }
      }
    };

    let mut session = match attach_browser_session(&launched).await {
      Ok(s) => s, Err(e) => { log::error!("Attach: {e}"); continue; }
    };

    // Navigate to login
    let _ = session.clear_all_site_data().await;
    let _ = session.navigate("https://chatgpt.com/auth/login", 25).await;
    tokio::time::sleep(std::time::Duration::from_millis(4000)).await;
    let _ = session.evaluate("window.scrollBy(0, 200)", false).await;
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    let device_id = uuid::Uuid::new_v4().to_string();
    let _ = engine::check_one(
      &app_handle, task_id, &cred.email, &cred.password, &cred.totp_secret,
      index, total, &cancel_flag, &mut session, &device_id,
    ).await;

    // Kill browser after each account (fingerprint resets on relaunch)
    drop(session);
    let _ = BrowserRunner::instance()
      .kill_browser_process(app_handle.clone(), &launched).await;

    if index + 1 < total {
      tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
  }

  // ── Final cleanup ─────────────────────────────────────
  let _ = BrowserRunner::instance()
    .kill_browser_process(app_handle.clone(), &profile).await;
  let _ = crate::profile::ProfileManager::instance()
    .delete_profile(&app_handle, &profile.id.to_string());

  Ok(())
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
    .map_err(|e| format!("Nord token: {e}"))?
    .ok_or_else(|| "No Nord access token configured".to_string())?;

  let creds = fetch_nord_wireguard_credentials(&token).await?;
  let servers = list_nord_wireguard_servers(None, Some(5)).await?;
  if servers.is_empty() {
    return Err("No Nord WireGuard servers available".into());
  }

  let mut configs = Vec::new();
  let storage = VPN_STORAGE.lock().map_err(|e| format!("Lock: {e}"))?;

  for (i, server) in servers.iter().enumerate() {
    let conf = build_nord_wireguard_conf(&creds.nordlynx_private_key, server);
    let name = format!("Nord-{}-{}", server.hostname.split('.').next().unwrap_or("wg"), i);
    match storage.create_config_manual(&name, VpnType::WireGuard, &conf) {
      Ok(cfg) => configs.push(cfg),
      Err(e) => log::warn!("Save Nord config failed: {e}"),
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
  let lines: Vec<String> = passed.iter().map(|r| r.credential_line()).collect();
  Ok(lines.join("\n"))
}

/// Export deactivated accounts as email|password|2fa text.
#[tauri::command]
pub fn export_deactivated_accounts() -> Result<String, String> {
  let deactivated = store::current_deactivated();
  let lines: Vec<String> = deactivated.iter().map(|r| r.credential_line()).collect();
  Ok(lines.join("\n"))
}
