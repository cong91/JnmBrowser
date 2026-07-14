//! Type definitions for the action recorder.

use serde::{Deserialize, Serialize};

/// Current recording format version.
pub const RECORDING_FORMAT_VERSION: u32 = 1;

/// A complete recording: header metadata + the ordered list of recorded events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recording {
  pub header: RecordingHeader,
  pub events: Vec<RecordedEvent>,
}

/// Metadata header persisted alongside the events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingHeader {
  pub version: u32,
  pub id: String,
  pub created_at: String,
  pub profile_id: String,
  pub profile_name: String,
  /// "chromium" or "camoufox".
  pub browser: String,
  pub start_url: String,
  pub viewport: Option<Viewport>,
  pub event_count: u32,
}

/// A single recorded user interaction. The Rust capture task pushes these into
/// the shared buffer; the player / recipe exporter consumes them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedEvent {
  /// Offset from recording start, in milliseconds.
  pub t_ms: u64,
  /// Event kind: "click","input","keydown","keyup","scroll","submit",
  /// "navigate","tab_open","tab_close","tab_switch","wait".
  pub kind: String,
  /// Element target info for events that originate from a DOM element.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub target: Option<RecordedTarget>,
  /// Raw event payload (key, code, value, deltaX/Y, url, etc.).
  #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
  pub payload: serde_json::Value,
}

/// Element info captured at the moment of interaction, used both for raw replay
/// (rect fallback) and for semantic recipe export (locators).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedTarget {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tag: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub id: Option<String>,
  #[serde(default)]
  pub locators: Vec<LocatorCandidate>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub rect: Option<Rect>,
  #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
  pub attributes: serde_json::Value,
}

/// A ranked selector candidate for the target element.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocatorCandidate {
  /// "testid" | "role" | "label" | "text" | "css" | "xpath" | "placeholder"
  pub by: String,
  pub value: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub nth: Option<usize>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub exact: Option<bool>,
  /// Heuristic best match for semantic export.
  #[serde(default)]
  pub preferred: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rect {
  pub x: f64,
  pub y: f64,
  pub width: f64,
  pub height: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Viewport {
  pub width: u32,
  pub height: u32,
}

/// Lightweight summary used by `list_recordings` (omits the full event list).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingSummary {
  pub id: String,
  pub created_at: String,
  pub profile_id: String,
  pub profile_name: String,
  pub browser: String,
  pub start_url: String,
  pub event_count: u32,
  /// Duration in milliseconds (the t_ms of the last event, 0 if no events).
  pub duration_ms: u64,
}

impl From<&Recording> for RecordingSummary {
  fn from(r: &Recording) -> Self {
    let duration_ms = r.events.last().map(|e| e.t_ms).unwrap_or(0);
    RecordingSummary {
      id: r.header.id.clone(),
      created_at: r.header.created_at.clone(),
      profile_id: r.header.profile_id.clone(),
      profile_name: r.header.profile_name.clone(),
      browser: r.header.browser.clone(),
      start_url: r.header.start_url.clone(),
      event_count: r.header.event_count,
      duration_ms,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_recording_round_trip() {
    let rec = Recording {
      header: RecordingHeader {
        version: RECORDING_FORMAT_VERSION,
        id: "rec_test".to_string(),
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
        target: Some(RecordedTarget {
          tag: Some("button".to_string()),
          id: Some("submit".to_string()),
          locators: vec![LocatorCandidate {
            by: "css".to_string(),
            value: "#submit".to_string(),
            nth: None,
            exact: None,
            preferred: true,
          }],
          rect: Some(Rect {
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
          }),
          attributes: serde_json::json!({}),
        }),
        payload: serde_json::json!({ "button": 0, "clientX": 10, "clientY": 20 }),
      }],
    };
    let s = serde_json::to_string(&rec).unwrap();
    let back: Recording = serde_json::from_str(&s).unwrap();
    assert_eq!(back.events.len(), 1);
    assert_eq!(back.events[0].kind, "click");
    assert_eq!(back.header.version, RECORDING_FORMAT_VERSION);
  }

  #[test]
  fn test_summary_from_recording() {
    let rec = Recording {
      header: RecordingHeader {
        version: 1,
        id: "rec_1".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        profile_id: "p1".to_string(),
        profile_name: "Profile".to_string(),
        browser: "chromium".to_string(),
        start_url: "https://example.com".to_string(),
        viewport: None,
        event_count: 2,
      },
      events: vec![
        RecordedEvent {
          t_ms: 100,
          kind: "navigate".to_string(),
          target: None,
          payload: serde_json::json!({}),
        },
        RecordedEvent {
          t_ms: 5000,
          kind: "click".to_string(),
          target: None,
          payload: serde_json::json!({}),
        },
      ],
    };
    let s = RecordingSummary::from(&rec);
    assert_eq!(s.id, "rec_1");
    assert_eq!(s.event_count, 2);
    assert_eq!(s.duration_ms, 5000);
  }
}
