//! Integration-style tests for action-recorder persistence.
//!
//! Full browser launch/kill is environment-dependent; these tests verify the
//! durable save path that the kill auto-save UX relies on (storage flush).
//! Isolation uses JNMBROWSER_DATA_DIR + serial execution (env is process-global).

use donutbrowser_lib::recorder::storage;
use donutbrowser_lib::recorder::types::{
  RecordedEvent, Recording, RecordingHeader, RECORDING_FORMAT_VERSION,
};
use serial_test::serial;
use std::path::PathBuf;

struct EnvDataDirGuard {
  previous: Option<String>,
  dir: PathBuf,
}

impl EnvDataDirGuard {
  fn set(prefix: &str) -> Self {
    let dir = std::env::temp_dir().join(format!(
      "jnmbrowser-recorder-integration-{}-{}-{}",
      prefix,
      std::process::id(),
      rand::random::<u32>()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let previous = std::env::var("JNMBROWSER_DATA_DIR").ok();
    // SAFETY: tests in this file are #[serial]; env is restored on drop.
    unsafe {
      std::env::set_var("JNMBROWSER_DATA_DIR", &dir);
    }
    Self { previous, dir }
  }
}

impl Drop for EnvDataDirGuard {
  fn drop(&mut self) {
    unsafe {
      match &self.previous {
        Some(prev) => std::env::set_var("JNMBROWSER_DATA_DIR", prev),
        None => std::env::remove_var("JNMBROWSER_DATA_DIR"),
      }
    }
    let _ = std::fs::remove_dir_all(&self.dir);
  }
}

fn sample_recording(id: &str, event_count: u32) -> Recording {
  let mut events = Vec::new();
  for i in 0..event_count {
    events.push(RecordedEvent {
      t_ms: (i as u64 + 1) * 100,
      kind: if i == 0 {
        "navigate".to_string()
      } else {
        "click".to_string()
      },
      target: None,
      payload: serde_json::json!({ "i": i }),
    });
  }
  Recording {
    header: RecordingHeader {
      version: RECORDING_FORMAT_VERSION,
      id: id.to_string(),
      created_at: "2026-07-14T00:00:00Z".to_string(),
      profile_id: "profile-test".to_string(),
      profile_name: "Test Profile".to_string(),
      browser: "chromium".to_string(),
      start_url: "https://example.com".to_string(),
      viewport: None,
      event_count,
    },
    events,
  }
}

/// Simulates the kill-path auto-save: a recording buffer is flushed to disk
/// and then discoverable via list_recordings / get_recording.
#[test]
#[serial]
fn test_auto_save_recording_is_listed_and_loadable() {
  let _guard = EnvDataDirGuard::set("autosave");
  let rec = sample_recording("rec_kill_autosave", 3);
  storage::save_recording(&rec).expect("save_recording failed");

  let listed = storage::list_recordings();
  assert!(
    listed.iter().any(|s| s.id == "rec_kill_autosave"),
    "saved recording must appear in list_recordings: {listed:?}"
  );
  assert_eq!(
    listed
      .iter()
      .find(|s| s.id == "rec_kill_autosave")
      .map(|s| s.event_count),
    Some(3)
  );

  let loaded = storage::get_recording("rec_kill_autosave").expect("get_recording");
  assert_eq!(loaded.events.len(), 3);
  assert_eq!(loaded.header.browser, "chromium");
  assert_eq!(loaded.events[0].kind, "navigate");
}

/// Ensures delete removes the artifact the UI list depends on.
#[test]
#[serial]
fn test_delete_recording_removes_from_list() {
  let _guard = EnvDataDirGuard::set("delete");
  let rec = sample_recording("rec_to_delete", 1);
  storage::save_recording(&rec).unwrap();
  assert!(storage::delete_recording("rec_to_delete"));
  assert!(storage::get_recording("rec_to_delete").is_none());
  assert!(!storage::list_recordings()
    .iter()
    .any(|s| s.id == "rec_to_delete"));
}

/// Multiple recordings sort newest-first — matches RecorderDialog ordering.
#[test]
#[serial]
fn test_list_recordings_newest_first_for_dialog() {
  let _guard = EnvDataDirGuard::set("sort");
  let mut older = sample_recording("rec_old", 1);
  older.header.created_at = "2026-01-01T00:00:00Z".to_string();
  let mut newer = sample_recording("rec_new", 2);
  newer.header.created_at = "2026-07-14T12:00:00Z".to_string();
  storage::save_recording(&older).unwrap();
  storage::save_recording(&newer).unwrap();
  let list = storage::list_recordings();
  let ids: Vec<_> = list.iter().map(|s| s.id.as_str()).collect();
  assert!(
    ids.contains(&"rec_old") && ids.contains(&"rec_new"),
    "expected both recordings in list: {ids:?}"
  );
  let pos_new = ids.iter().position(|id| *id == "rec_new").unwrap();
  let pos_old = ids.iter().position(|id| *id == "rec_old").unwrap();
  assert!(pos_new < pos_old, "newest first: {ids:?}");
}
