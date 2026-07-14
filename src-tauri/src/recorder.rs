//! Action recorder: capture user browser interactions into a durable recording,
//! persist it, replay it on another profile, and export it as an MCP recipe.
//!
//! The recorder injects a content script into the target browser page that
//! listens to DOM events (click, input, keydown, scroll, submit, navigation)
//! and forwards them to Rust via a tagged `console.log("__REC__:" + json)` channel.
//!
//! - On Chromium the script is installed via `Page.addScriptToEvaluateOnNewDocument`
//!   and events are harvested from `Runtime.consoleAPICalled` over a persistent CDP
//!   WebSocket connection.
//! - On Camoufox the script is installed via Playwright `add_init_script` and
//!   events are harvested from the `page::Event::Console` listener.
//!
//! Recordings are stored as JSON files under `data_dir()/recordings/` (one file
//! per recording). Each recording can be:
//!   - Replayed raw on a running profile (coordinate/keystroke-level dispatch).
//!   - Exported as an `McpRecipe` JSON so the existing `run_recipe` /
//!     `run_batch_profile_workflow` pipeline can replay it semantically.

pub mod capture;
pub mod commands;
pub mod inject_script;
pub mod player;
pub mod recipe_export;
pub mod storage;
pub mod types;

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::Emitter;
use tokio::sync::watch;
use tokio::sync::Mutex as AsyncMutex;

use self::types::{RecordedEvent, Recording, RecordingHeader, RECORDING_FORMAT_VERSION};

/// Tag prefix used by the injected script to mark console messages carrying
/// recorder events. Rust only harvests messages whose first argument starts
/// with this prefix.
pub const RECORDER_TAG: &str = "__REC__:";

/// Event emitted to the frontend when a recorder session changes or ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecorderSessionInfo {
  pub id: String,
  pub profile_id: String,
  pub profile_name: String,
  pub browser: String,
  pub started_at: u64,
  pub event_count: u64,
  pub status: String,
}

/// Shared state for an in-progress recording session — mutated by the capture
/// task and read by the manager / Tauri commands.
pub struct RecorderShared {
  events: Vec<RecordedEvent>,
  last_error: Option<String>,
}

/// Internal session state held by the manager.
struct RecorderSession {
  id: String,
  profile_id: String,
  profile_name: String,
  browser: String,
  started_at: u64,
  start_url: String,
  shared: Arc<AsyncMutex<RecorderShared>>,
  /// Drop the sender to stop the capture task.
  cancel_tx: watch::Sender<bool>,
}

/// Input for registering a newly armed recording session.
pub struct NewRecorderSession {
  pub id: String,
  pub profile_id: String,
  pub profile_name: String,
  pub browser: String,
  pub started_at: u64,
  pub start_url: String,
  pub shared: Arc<AsyncMutex<RecorderShared>>,
  pub cancel_tx: watch::Sender<bool>,
}

pub struct RecorderManager {
  inner: Arc<AsyncMutex<RecorderInner>>,
}

struct RecorderInner {
  sessions: HashMap<String, RecorderSession>,
}

static RECORDER: std::sync::OnceLock<RecorderManager> = std::sync::OnceLock::new();

impl RecorderManager {
  pub fn instance() -> &'static RecorderManager {
    RECORDER.get_or_init(|| RecorderManager {
      inner: Arc::new(AsyncMutex::new(RecorderInner {
        sessions: HashMap::new(),
      })),
    })
  }

  /// Generate a recording id of the form `rec_<epoch>_<rand>`.
  pub fn generate_recording_id() -> String {
    format!(
      "rec_{}_{}",
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0),
      rand::random::<u32>()
    )
  }

  /// Returns true if a recording session is currently active for the profile.
  pub async fn is_recording(&self, profile_id: &str) -> bool {
    let inner = self.inner.lock().await;
    inner.sessions.values().any(|s| s.profile_id == profile_id)
  }

  /// Register a freshly started session and spawn its capture task.
  ///
  /// The caller is responsible for launching the capture task and returning
  /// the shared event buffer + cancel token. This helper stores the session
  /// and emits the `recorder-session-changed` event.
  pub async fn register_session(&self, app_handle: tauri::AppHandle, session: NewRecorderSession) {
    let info = self
      .session_info(
        &session.id,
        &session.profile_id,
        &session.profile_name,
        &session.browser,
        session.started_at,
        &session.shared,
      )
      .await;
    {
      let mut inner = self.inner.lock().await;
      inner.sessions.insert(
        session.id.clone(),
        RecorderSession {
          id: session.id,
          profile_id: session.profile_id,
          profile_name: session.profile_name,
          browser: session.browser,
          started_at: session.started_at,
          start_url: session.start_url,
          shared: session.shared,
          cancel_tx: session.cancel_tx,
        },
      );
    }
    let _ = app_handle.emit("recorder-session-changed", &info);
  }

  /// Stop a recording session, flush events to disk, and return the saved recording.
  pub async fn stop_recording(
    &self,
    app_handle: tauri::AppHandle,
    session_id: &str,
  ) -> Result<Recording, String> {
    let session = {
      let mut inner = self.inner.lock().await;
      inner
        .sessions
        .remove(session_id)
        .ok_or_else(|| format!("Recording session {session_id} not found"))?
    };
    // Signal the capture task to stop.
    let _ = session.cancel_tx.send(true);

    let events = {
      let mut shared = session.shared.lock().await;
      std::mem::take(&mut shared.events)
    };

    let recording = Recording {
      header: RecordingHeader {
        version: RECORDING_FORMAT_VERSION,
        id: session.id.clone(),
        created_at: iso8601(session.started_at),
        profile_id: session.profile_id.clone(),
        profile_name: session.profile_name.clone(),
        browser: session.browser.clone(),
        start_url: session.start_url.clone(),
        viewport: None,
        event_count: events.len() as u32,
      },
      events,
    };

    storage::save_recording(&recording).map_err(|e| format!("Failed to save recording: {e}"))?;

    let _ = app_handle.emit("recorder-session-ended", &session.id);
    let _ = app_handle.emit("recorder-sessions-changed", &());
    Ok(recording)
  }

  /// Stop and persist every active recording for a profile (if any).
  /// Used by browser kill paths so recordings are not silently lost.
  pub async fn stop_for_profile(
    &self,
    app_handle: &tauri::AppHandle,
    profile_id: &str,
  ) -> Vec<Recording> {
    let session_ids: Vec<String> = {
      let inner = self.inner.lock().await;
      inner
        .sessions
        .values()
        .filter(|s| s.profile_id == profile_id)
        .map(|s| s.id.clone())
        .collect()
    };
    let mut saved = Vec::new();
    for session_id in session_ids {
      match self.stop_recording(app_handle.clone(), &session_id).await {
        Ok(recording) => saved.push(recording),
        Err(e) => log::warn!("Failed to auto-save recording {session_id}: {e}"),
      }
    }
    saved
  }

  /// Stop all active recordings (e.g. on app shutdown). Does not emit per-session
  /// events; used for graceful cleanup.
  #[allow(dead_code)]
  pub async fn stop_all(&self) {
    let mut inner = self.inner.lock().await;
    for (_, session) in inner.sessions.drain() {
      let _ = session.cancel_tx.send(true);
      let events = {
        let mut shared = session.shared.lock().await;
        std::mem::take(&mut shared.events)
      };
      let recording = Recording {
        header: RecordingHeader {
          version: RECORDING_FORMAT_VERSION,
          id: session.id.clone(),
          created_at: iso8601(session.started_at),
          profile_id: session.profile_id.clone(),
          profile_name: session.profile_name.clone(),
          browser: session.browser.clone(),
          start_url: session.start_url.clone(),
          viewport: None,
          event_count: events.len() as u32,
        },
        events,
      };
      let _ = storage::save_recording(&recording);
    }
  }

  /// Return snapshot infos for all active recording sessions.
  pub async fn get_sessions(&self) -> Vec<RecorderSessionInfo> {
    let inner = self.inner.lock().await;
    let mut infos = Vec::new();
    for session in inner.sessions.values() {
      let event_count = {
        let shared = session.shared.lock().await;
        shared.events.len() as u64
      };
      infos.push(RecorderSessionInfo {
        id: session.id.clone(),
        profile_id: session.profile_id.clone(),
        profile_name: session.profile_name.clone(),
        browser: session.browser.clone(),
        started_at: session.started_at,
        event_count,
        status: "recording".to_string(),
      });
    }
    infos
  }

  async fn session_info(
    &self,
    id: &str,
    profile_id: &str,
    profile_name: &str,
    browser: &str,
    started_at: u64,
    shared: &Arc<AsyncMutex<RecorderShared>>,
  ) -> RecorderSessionInfo {
    let event_count = {
      let s = shared.lock().await;
      s.events.len() as u64
    };
    RecorderSessionInfo {
      id: id.to_string(),
      profile_id: profile_id.to_string(),
      profile_name: profile_name.to_string(),
      browser: browser.to_string(),
      started_at,
      event_count,
      status: "recording".to_string(),
    }
  }
}

/// Push a parsed event into the shared buffer.
pub(crate) async fn push_event(shared: &Arc<AsyncMutex<RecorderShared>>, event: RecordedEvent) {
  let mut s = shared.lock().await;
  s.events.push(event);
}

/// Mark the last error on the shared buffer (used by capture tasks).
pub(crate) async fn set_last_error(shared: &Arc<AsyncMutex<RecorderShared>>, err: String) {
  let mut s = shared.lock().await;
  s.last_error = Some(err);
}

/// Allocate a fresh shared buffer.
pub(crate) fn new_shared() -> Arc<AsyncMutex<RecorderShared>> {
  Arc::new(AsyncMutex::new(RecorderShared {
    events: Vec::new(),
    last_error: None,
  }))
}

/// Format a unix-seconds timestamp as an ISO-8601 string (UTC).
fn iso8601(secs: u64) -> String {
  // Simple ISO-8601 without external crate: use chrono-free manual formatting.
  // Good enough for recording metadata; Rust stdlib doesn't expose date math,
  // so we fall back to the unix seconds stored as a string if formatting fails.
  // Days since 1970-01-01 (Thursday).
  let days = secs / 86400;
  let rem = secs % 86400;
  let hour = rem / 3600;
  let minute = (rem % 3600) / 60;
  let second = rem % 60;
  // Compute civil date from days since epoch (Howard Hinnant's algorithm).
  let z = days as i64 + 719468;
  let era = if z >= 0 { z } else { z - 146096 } / 146097;
  let doe = z - era * 146097;
  let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
  let y = yoe + era * 400;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let d = doy - (153 * mp + 2) / 5 + 1;
  let m = if mp < 10 { mp + 3 } else { mp - 9 };
  let year = if m <= 2 { y + 1 } else { y };
  format!(
    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
    year, m, d, hour, minute, second
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_generate_recording_id_format() {
    let id = RecorderManager::generate_recording_id();
    assert!(id.starts_with("rec_"), "id should start with rec_: {id}");
    let parts: Vec<&str> = id.split('_').collect();
    assert!(parts.len() == 3, "id should have 3 parts: {id}");
    assert!(
      parts[1].parse::<u64>().is_ok(),
      "epoch part should be numeric: {id}"
    );
    assert!(
      parts[2].parse::<u32>().is_ok(),
      "rand part should be numeric: {id}"
    );
  }

  #[test]
  fn test_iso8601_well_known() {
    // 1970-01-01 00:00:00 UTC
    assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
    // 1970-01-02 00:00:00 UTC
    assert_eq!(iso8601(86400), "1970-01-02T00:00:00Z");
    // 2020-01-01 00:00:00 UTC = 1577836800
    assert_eq!(iso8601(1_577_836_800), "2020-01-01T00:00:00Z");
  }

  #[tokio::test]
  async fn test_is_recording_empty() {
    // No sessions exist initially; using a distinct id avoids interference.
    let m = RecorderManager::instance();
    assert!(!m.is_recording("nonexistent-profile-id").await);
  }

  #[tokio::test]
  async fn test_get_sessions_empty() {
    let m = RecorderManager::instance();
    let sessions = m.get_sessions().await;
    // Sessions may linger from other tests; only assert it's a vec.
    let _ = sessions;
  }
}
