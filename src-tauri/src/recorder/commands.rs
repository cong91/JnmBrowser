//! Tauri commands for the action recorder.

use crate::recorder::capture::{
  allocate_shared, canonical_browser, get_chromium_cdp_port, get_current_url_chromium,
  get_page_ws_url, get_running_profile, now_unix_secs, profile_data_path, run_camoufox_recorder,
  run_chromium_recorder,
};
use crate::recorder::player;
use crate::recorder::recipe_export::{export_to_recipe, ExportedRecipe};
use crate::recorder::storage;
use crate::recorder::types::{Recording, RecordingSummary};
use crate::recorder::{NewRecorderSession, RecorderManager, RecorderSessionInfo};

/// Start recording user actions on a running profile.
#[tauri::command]
pub async fn start_recording(
  app_handle: tauri::AppHandle,
  profile_id: String,
) -> Result<RecorderSessionInfo, String> {
  let manager = RecorderManager::instance();
  if manager.is_recording(&profile_id).await {
    return Err("A recording is already active for this profile".to_string());
  }

  let profile = get_running_profile(&profile_id)?;
  let browser = canonical_browser(&profile.browser)?.to_string();
  let session_id = RecorderManager::generate_recording_id();
  let started_at = now_unix_secs();
  let shared = allocate_shared();
  let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
  let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

  let start_url = match browser.as_str() {
    "chromium" => {
      let port = get_chromium_cdp_port(&profile).await?;
      let ws_url = get_page_ws_url(port).await?;
      let url = get_current_url_chromium(&ws_url).await;
      let shared_task = shared.clone();
      let pid = profile_id.clone();
      tokio::spawn(async move {
        run_chromium_recorder(pid, ws_url, shared_task, cancel_rx, ready_tx).await;
      });
      url
    }
    "camoufox" => {
      let path = profile_data_path(&profile);
      let shared_task = shared.clone();
      let pid = profile_id.clone();
      // Retry get_active_page for up to 15s — the Camoufox automation session
      // may need a moment to stabilize after launch (ephemeral dir creation,
      // Playwright context warm-up, or transient page-list failures).
      let mut start_url = String::new();
      let mut last_err: Option<String> = None;
      for attempt in 0..30 {
        if attempt > 0 {
          tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        match crate::camoufox_manager::CamoufoxManager::instance()
          .get_active_page(&path)
          .await
        {
          Ok(page) => {
            start_url = page
              .eval::<String>("location.href")
              .await
              .unwrap_or_default();
            last_err = None;
            break;
          }
          Err(e) => {
            last_err = Some(format!("{e}"));
          }
        }
      }
      if let Some(err) = last_err {
        return Err(format!(
          "Camoufox automation session not ready for profile '{}' after 15s: {err}",
          profile.name
        ));
      }
      tokio::spawn(async move {
        run_camoufox_recorder(pid, path, shared_task, cancel_rx, ready_tx).await;
      });
      start_url
    }
    other => return Err(format!("Unsupported browser: {other}")),
  };

  // Wait up to 30s for the capture task to arm.
  tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx)
    .await
    .map_err(|_| "Timed out waiting for recorder to start".to_string())?
    .map_err(|_| "Recorder task exited before becoming ready".to_string())??;

  manager
    .register_session(
      app_handle,
      NewRecorderSession {
        id: session_id.clone(),
        profile_id: profile_id.clone(),
        profile_name: profile.name.clone(),
        browser: browser.clone(),
        started_at,
        start_url,
        shared,
        cancel_tx,
      },
    )
    .await;

  let sessions = manager.get_sessions().await;
  sessions
    .into_iter()
    .find(|s| s.id == session_id)
    .ok_or_else(|| "Recording session was not registered".to_string())
}

/// Stop an active recording session and persist it to disk.
#[tauri::command]
pub async fn stop_recording(
  app_handle: tauri::AppHandle,
  session_id: String,
) -> Result<Recording, String> {
  RecorderManager::instance()
    .stop_recording(app_handle, &session_id)
    .await
}

/// List active (in-progress) recording sessions.
#[tauri::command]
pub async fn get_recorder_sessions() -> Result<Vec<RecorderSessionInfo>, String> {
  Ok(RecorderManager::instance().get_sessions().await)
}

/// List saved recordings (summaries only).
#[tauri::command]
pub async fn list_recordings() -> Result<Vec<RecordingSummary>, String> {
  Ok(storage::list_recordings())
}

/// Absolute path to the recordings directory on disk.
///
/// Ensures the directory exists so "Open folder" never fails on a missing path
/// before the first recording is saved.
#[tauri::command]
pub async fn get_recordings_dir() -> Result<String, String> {
  let dir = storage::recordings_dir();
  std::fs::create_dir_all(&dir)
    .map_err(|e| format!("Failed to create recordings directory: {e}"))?;
  Ok(dir.to_string_lossy().to_string())
}

/// Load a full saved recording by id.
#[tauri::command]
pub async fn get_recording(id: String) -> Result<Recording, String> {
  storage::get_recording(&id).ok_or_else(|| format!("Recording {id} not found"))
}

/// Delete a saved recording by id.
#[tauri::command]
pub async fn delete_recording(id: String) -> Result<bool, String> {
  Ok(storage::delete_recording(&id))
}

/// Export a saved recording as an MCP recipe JSON.
#[tauri::command]
pub async fn export_recording_as_recipe(
  id: String,
  recipe_name: Option<String>,
) -> Result<ExportedRecipe, String> {
  let recording = storage::get_recording(&id).ok_or_else(|| format!("Recording {id} not found"))?;
  Ok(export_to_recipe(&recording, recipe_name))
}

/// Replay a saved recording on a running profile (raw event player).
#[tauri::command]
pub async fn replay_recording(id: String, profile_id: String) -> Result<(), String> {
  let recording = storage::get_recording(&id).ok_or_else(|| format!("Recording {id} not found"))?;
  player::replay_recording(&profile_id, &recording).await
}
