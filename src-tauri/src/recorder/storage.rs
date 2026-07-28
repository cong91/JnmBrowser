//! Persistence for completed recordings: one JSON file per recording under
//! `data_dir()/recordings/`. Mirrors `proxy_storage.rs`.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use super::types::{RecordedEvent, RecordedTarget, Recording, RecordingSummary};

const REDACTED_VALUE: &str = "<password>";

/// Directory where recording JSON files are stored.
pub fn recordings_dir() -> PathBuf {
  crate::app_dirs::recordings_dir()
}

/// Save a recording to disk as `<recordings_dir>/<id>.json`.
pub fn save_recording(recording: &Recording) -> Result<(), Box<dyn std::error::Error>> {
  let dir = recordings_dir();
  fs::create_dir_all(&dir)?;
  let file_path = dir.join(format!("{}.json", recording.header.id));
  let sanitized = sanitize_recording_for_persistence(recording);
  let content = serde_json::to_string_pretty(&sanitized)?;
  fs::write(&file_path, content)?;
  Ok(())
}

fn sanitize_recording_for_persistence(recording: &Recording) -> Recording {
  let mut sanitized = recording.clone();
  let mut sensitive_targets = HashSet::new();

  for event in &mut sanitized.events {
    let target_key = event.target.as_ref().and_then(target_key);
    let sensitive = event
      .target
      .as_ref()
      .is_some_and(target_is_obviously_sensitive)
      || event
        .payload
        .get("redacted")
        .and_then(|value| value.as_bool())
        == Some(true)
      || target_key
        .as_ref()
        .is_some_and(|key| sensitive_targets.contains(key));

    if sensitive {
      if let Some(key) = target_key {
        sensitive_targets.insert(key);
      }
    }

    if let Some(target) = &mut event.target {
      remove_target_value(target);
    }

    if sensitive {
      if event.kind == "keydown" {
        if keydown_is_printable(event) {
          event.kind.clear();
          continue;
        }
        event
          .payload
          .as_object_mut()
          .map(|payload| payload.remove("redacted"));
        continue;
      }
      redact_sensitive_payload(event);
    }
  }

  sanitized.events.retain(|event| !event.kind.is_empty());
  sanitized.header.event_count = sanitized.events.len() as u32;
  sanitized
}

fn target_key(target: &RecordedTarget) -> Option<String> {
  if let Some(id) = target.id.as_deref().filter(|id| !id.is_empty()) {
    return Some(format!("id:{id}"));
  }
  target
    .locators
    .iter()
    .find(|locator| locator.preferred)
    .or_else(|| target.locators.first())
    .map(|locator| format!("{}:{}", locator.by, locator.value))
}

fn target_is_obviously_sensitive(target: &RecordedTarget) -> bool {
  if target.tag.as_deref() != Some("input") {
    return false;
  }

  let attribute = |name: &str| {
    target
      .attributes
      .get(name)
      .and_then(|value| value.as_str())
      .unwrap_or("")
  };
  if attribute("type").eq_ignore_ascii_case("password") {
    return true;
  }
  if attribute("autocomplete")
    .split_whitespace()
    .any(|token| token.eq_ignore_ascii_case("one-time-code"))
  {
    return true;
  }

  let has_sensitive_hint = |value: &str| {
    let normalized: String = value
      .chars()
      .filter(|character| character.is_ascii_alphanumeric())
      .flat_map(char::to_lowercase)
      .collect();
    [
      "password",
      "passwd",
      "passcode",
      "onetimecode",
      "otp",
      "totp",
      "2fa",
      "twofactor",
      "verificationcode",
      "securitycode",
      "authenticationcode",
      "authcode",
    ]
    .iter()
    .any(|hint| normalized.contains(hint))
  };

  let locator_has_sensitive_hint = target.locators.iter().any(|locator| {
    matches!(locator.by.as_str(), "label" | "placeholder" | "role")
      && has_sensitive_hint(&locator.value)
  });
  if target.id.as_deref().is_some_and(has_sensitive_hint)
    || has_sensitive_hint(attribute("id"))
    || has_sensitive_hint(attribute("name"))
    || has_sensitive_hint(attribute("aria-label"))
    || has_sensitive_hint(attribute("placeholder"))
    || locator_has_sensitive_hint
  {
    return true;
  }

  let input_mode = attribute("inputmode");
  let input_type = attribute("type");
  let max_length = attribute("maxlength").parse::<u32>().ok();
  (matches!(
    input_mode.to_ascii_lowercase().as_str(),
    "numeric" | "decimal"
  ) || input_type.eq_ignore_ascii_case("tel"))
    && max_length.is_some_and(|length| (4..=10).contains(&length))
}

fn remove_target_value(target: &mut RecordedTarget) {
  if let Some(attributes) = target.attributes.as_object_mut() {
    attributes.retain(|key, _| !key.eq_ignore_ascii_case("value"));
  }
}

fn keydown_is_printable(event: &RecordedEvent) -> bool {
  let key = event.payload.get("key").and_then(|value| value.as_str());
  key.is_some_and(|key| key.chars().count() == 1)
    && !["ctrlKey", "altKey", "metaKey"].iter().any(|modifier| {
      event
        .payload
        .get(modifier)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    })
}

fn redact_sensitive_payload(event: &mut RecordedEvent) {
  let Some(payload) = event.payload.as_object_mut() else {
    event.payload = serde_json::json!({ "redacted": true });
    return;
  };
  for field in ["key", "code", "keyCode", "text", "unmodifiedText"] {
    payload.remove(field);
  }
  if event.kind == "input" || payload.contains_key("value") {
    payload.insert("value".to_string(), serde_json::json!(REDACTED_VALUE));
  }
  payload.insert("redacted".to_string(), serde_json::json!(true));
}

/// Load a recording by id. Returns `None` if the file is missing or unparsable.
pub fn get_recording(id: &str) -> Option<Recording> {
  let file_path = recordings_dir().join(format!("{id}.json"));
  if !file_path.exists() {
    return None;
  }
  match fs::read_to_string(&file_path) {
    Ok(content) => serde_json::from_str::<Recording>(&content)
      .ok()
      .map(|recording| sanitize_recording_for_persistence(&recording)),
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
  use crate::recorder::types::{
    LocatorCandidate, RecordedEvent, RecordedTarget, RecordingHeader, RECORDING_FORMAT_VERSION,
  };

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

  fn input_target(id: &str, attributes: serde_json::Value) -> RecordedTarget {
    RecordedTarget {
      tag: Some("input".to_string()),
      id: Some(id.to_string()),
      locators: vec![LocatorCandidate {
        by: "css".to_string(),
        value: format!("#{id}"),
        nth: None,
        exact: None,
        preferred: true,
      }],
      rect: None,
      attributes,
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

  #[test]
  fn test_password_target_stays_redacted_after_type_changes_to_text() {
    let mut recording = make_recording("rec_sticky_password");
    recording.events = vec![
      RecordedEvent {
        t_ms: 10,
        kind: "input".to_string(),
        target: Some(input_target(
          "account-secret",
          serde_json::json!({ "type": "password", "value": "RAW_PASSWORD_SENTINEL_1" }),
        )),
        payload: serde_json::json!({ "value": "RAW_PASSWORD_SENTINEL_1" }),
      },
      RecordedEvent {
        t_ms: 20,
        kind: "input".to_string(),
        target: Some(input_target(
          "account-secret",
          serde_json::json!({ "type": "text", "value": "RAW_PASSWORD_SENTINEL_2" }),
        )),
        payload: serde_json::json!({ "value": "RAW_PASSWORD_SENTINEL_2" }),
      },
    ];

    let sanitized = sanitize_recording_for_persistence(&recording);

    assert_eq!(sanitized.events.len(), 2);
    for event in &sanitized.events {
      assert_eq!(
        event.payload.get("value"),
        Some(&serde_json::json!("<password>"))
      );
      assert_eq!(
        event.payload.get("redacted"),
        Some(&serde_json::json!(true))
      );
      assert!(event
        .target
        .as_ref()
        .and_then(|target| target.attributes.get("value"))
        .is_none());
    }
  }

  #[test]
  fn test_otp_heuristics_redact_input_and_suppress_printable_keydown() {
    let otp_targets = [
      serde_json::json!({ "autocomplete": "one-time-code" }),
      serde_json::json!({ "name": "totp" }),
      serde_json::json!({ "id": "otp-code" }),
      serde_json::json!({ "aria-label": "Verification code" }),
      serde_json::json!({ "inputmode": "numeric", "maxlength": "6" }),
    ];

    for (index, attributes) in otp_targets.into_iter().enumerate() {
      let id = format!("field-{index}");
      let mut recording = make_recording(&format!("rec_otp_{index}"));
      recording.events = vec![
        RecordedEvent {
          t_ms: 10,
          kind: "input".to_string(),
          target: Some(input_target(&id, attributes.clone())),
          payload: serde_json::json!({ "value": "RAW_OTP_SENTINEL" }),
        },
        RecordedEvent {
          t_ms: 20,
          kind: "keydown".to_string(),
          target: Some(input_target(&id, attributes)),
          payload: serde_json::json!({
            "key": "7",
            "code": "Digit7",
            "keyCode": 55
          }),
        },
      ];

      let sanitized = sanitize_recording_for_persistence(&recording);
      assert_eq!(sanitized.events.len(), 1, "heuristic case {index}");
      assert_eq!(
        sanitized.events[0].payload.get("value"),
        Some(&serde_json::json!("<password>")),
        "heuristic case {index}"
      );
    }
  }

  #[test]
  fn test_save_recording_persists_no_raw_secret_sentinel() {
    let _guard = app_dirs::set_test_data_dir(unique_tmp("redaction"));
    let mut recording = make_recording("rec_redaction_boundary");
    recording.events = vec![RecordedEvent {
      t_ms: 10,
      kind: "input".to_string(),
      target: Some(input_target(
        "password",
        serde_json::json!({
          "type": "password",
          "value": "RAW_PERSISTENCE_SECRET_SENTINEL"
        }),
      )),
      payload: serde_json::json!({ "value": "RAW_PERSISTENCE_SECRET_SENTINEL" }),
    }];

    save_recording(&recording).expect("save failed");

    let persisted = fs::read_to_string(recordings_dir().join("rec_redaction_boundary.json"))
      .expect("read persisted recording");
    assert!(!persisted.contains("RAW_PERSISTENCE_SECRET_SENTINEL"));
    assert!(persisted.contains("<password>"));
    assert!(persisted.contains("\"redacted\": true"));
  }

  #[test]
  fn test_get_recording_sanitizes_legacy_file_without_rewriting_it() {
    let _guard = app_dirs::set_test_data_dir(unique_tmp("legacy-read"));
    let mut recording = make_recording("rec_legacy_secret");
    recording.events = vec![RecordedEvent {
      t_ms: 10,
      kind: "input".to_string(),
      target: Some(input_target(
        "legacy-password",
        serde_json::json!({
          "type": "password",
          "value": "RAW_LEGACY_SECRET_SENTINEL"
        }),
      )),
      payload: serde_json::json!({ "value": "RAW_LEGACY_SECRET_SENTINEL" }),
    }];
    fs::create_dir_all(recordings_dir()).expect("create recordings dir");
    let path = recordings_dir().join("rec_legacy_secret.json");
    fs::write(
      &path,
      serde_json::to_string_pretty(&recording).expect("serialize legacy recording"),
    )
    .expect("write legacy recording");

    let loaded = get_recording("rec_legacy_secret").expect("load legacy recording");
    assert_eq!(
      loaded.events[0].payload.get("value"),
      Some(&serde_json::json!("<password>"))
    );
    assert!(loaded.events[0]
      .target
      .as_ref()
      .and_then(|target| target.attributes.get("value"))
      .is_none());

    let unchanged = fs::read_to_string(path).expect("read unchanged legacy recording");
    assert!(unchanged.contains("RAW_LEGACY_SECRET_SENTINEL"));
  }
}
