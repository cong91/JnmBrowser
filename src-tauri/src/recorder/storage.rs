//! Persistence for completed recordings: one JSON file per recording under
//! `data_dir()/recordings/`. Mirrors `proxy_storage.rs`.

use std::fs;
use std::path::PathBuf;

use super::types::{Recording, RecordingSummary};

/// Directory where recording JSON files are stored.
pub fn recordings_dir() -> PathBuf {
  crate::app_dirs::recordings_dir()
}

/// Save a recording to disk as `<recordings_dir>/<id>.json`.
pub fn save_recording(recording: &Recording) -> Result<(), Box<dyn std::error::Error>> {
  let dir = recordings_dir();
  fs::create_dir_all(&dir)?;
  let file_path = dir.join(format!("{}.json", recording.header.id));
  let content = serde_json::to_string_pretty(recording)?;
  fs::write(&file_path, content)?;
  Ok(())
}

/// Load a recording by id. Returns `None` if the file is missing or unparsable.
pub fn get_recording(id: &str) -> Option<Recording> {
  let file_path = recordings_dir().join(format!("{id}.json"));
  if !file_path.exists() {
    return None;
  }
  match fs::read_to_string(&file_path) {
    Ok(content) => serde_json::from_str(&content).ok(),
    Err(_) => None,
  }
}

/// Delete a recording file. Returns `true` if the file existed and was removed.
pub fn delete_recording(id: &str) -> bool {
  let file_path = recordings_dir().join(format!("{id}.json"));
  if !file_path.exists() {
    return false;
  }
  fs::remove_file(&file_path).is_ok()
}

/// List all recordings (without their events). Returns summaries sorted by
/// `created_at` descending (newest first).
pub fn list_recordings() -> Vec<RecordingSummary> {
  let dir = recordings_dir();
  if !dir.exists() {
    return Vec::new();
  }
  let mut summaries = Vec::new();
  if let Ok(entries) = fs::read_dir(&dir) {
    for entry in entries.flatten() {
      let path = entry.path();
      if path.extension().is_some_and(|ext| ext == "json") {
        if let Ok(content) = fs::read_to_string(&path) {
          if let Ok(rec) = serde_json::from_str::<Recording>(&content) {
            summaries.push(RecordingSummary::from(&rec));
          }
        }
      }
    }
  }
  summaries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
  summaries
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::app_dirs;
  use crate::recorder::types::{RecordedEvent, RecordingHeader, RECORDING_FORMAT_VERSION};

  fn unique_tmp(prefix: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
      "jnmbrowser-recorder-test-{}-{}",
      prefix,
      rand::random::<u32>()
    ));
    let _ = fs::create_dir_all(&p);
    p
  }

  fn make_recording(id: &str) -> Recording {
    Recording {
      header: RecordingHeader {
        version: RECORDING_FORMAT_VERSION,
        id: id.to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        profile_id: "p1".to_string(),
        profile_name: "Profile 1".to_string(),
        browser: "chromium".to_string(),
        start_url: "https://example.com".to_string(),
        viewport: None,
        event_count: 1,
      },
      events: vec![RecordedEvent {
        t_ms: 100,
        kind: "click".to_string(),
        target: None,
        payload: serde_json::json!({}),
      }],
    }
  }

  #[test]
  fn test_save_get_delete_recording() {
    let _guard = app_dirs::set_test_data_dir(unique_tmp("save"));
    let rec = make_recording("rec_test_save");
    save_recording(&rec).expect("save failed");
    let got = get_recording("rec_test_save").expect("recording not found");
    assert_eq!(got.header.id, "rec_test_save");
    assert!(delete_recording("rec_test_save"));
    assert!(get_recording("rec_test_save").is_none());
  }

  #[test]
  fn test_list_recordings_sorted() {
    let _guard = app_dirs::set_test_data_dir(unique_tmp("list"));
    // First recording has an earlier created_at so insert order is jumbled.
    let mut a = make_recording("rec_a");
    a.header.created_at = "2026-01-01T00:00:00Z".to_string();
    let mut b = make_recording("rec_b");
    b.header.created_at = "2026-02-01T00:00:00Z".to_string();
    save_recording(&b).unwrap();
    save_recording(&a).unwrap();
    let list = list_recordings();
    assert_eq!(list.len(), 2);
    // Newest first.
    assert_eq!(list[0].id, "rec_b");
    assert_eq!(list[1].id, "rec_a");
  }

  #[test]
  fn test_list_recordings_empty_dir() {
    let _guard = app_dirs::set_test_data_dir(unique_tmp("empty"));
    let list = list_recordings();
    assert!(list.is_empty());
  }
}
