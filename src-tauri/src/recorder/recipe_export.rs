//! Semantic lift: convert a raw event-stream recording into an MCP recipe JSON
//! that can be executed via `validate_recipe` / `preview_recipe` / `run_recipe`.

use crate::recorder::types::{LocatorCandidate, RecordedEvent, Recording};

/// Exported recipe shape compatible with `McpRecipe` (camelCase fields).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedRecipe {
  pub version: u32,
  pub name: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub description: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub defaults: Option<ExportedRecipeDefaults>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub inputs: Option<serde_json::Value>,
  pub steps: Vec<ExportedRecipeStep>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedRecipeDefaults {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub timeout_ms: Option<u64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub auto_wait: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedRecipeStep {
  pub id: String,
  pub tool: String,
  pub arguments: serde_json::Value,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub on_error: Option<String>,
}

/// Convert a recording into an MCP recipe. Profile is parameterized as
/// `{{inputs.profile_id}}` so the same recipe can run on any profile.
pub fn export_to_recipe(recording: &Recording, recipe_name: Option<String>) -> ExportedRecipe {
  let name = recipe_name.unwrap_or_else(|| {
    format!(
      "Recording {} ({})",
      recording.header.id, recording.header.profile_name
    )
  });

  let mut steps: Vec<ExportedRecipeStep> = Vec::new();
  let mut step_idx: usize = 0;
  let mut pending_input: Option<PendingInput> = None;

  let flush_input = |pending: &mut Option<PendingInput>,
                     steps: &mut Vec<ExportedRecipeStep>,
                     step_idx: &mut usize| {
    if let Some(p) = pending.take() {
      if p.value.is_empty() || p.value == "<password>" {
        return;
      }
      *step_idx += 1;
      let mut args = serde_json::json!({
        "profile_id": "{{inputs.profile_id}}",
        "text": p.value,
      });
      if let Some(locator) = p.locator {
        merge_locator(&mut args, &locator);
      }
      steps.push(ExportedRecipeStep {
        id: format!("step_{step_idx}"),
        tool: "type_text".to_string(),
        arguments: args,
        on_error: Some("stop".to_string()),
      });
    }
  };

  for event in &recording.events {
    match event.kind.as_str() {
      "navigate" => {
        flush_input(&mut pending_input, &mut steps, &mut step_idx);
        let url = event
          .payload
          .get("to")
          .or_else(|| event.payload.get("url"))
          .and_then(|v| v.as_str())
          .unwrap_or("");
        // Skip empty / about:blank initial navigations with no meaningful URL.
        if url.is_empty() || url.starts_with("about:") {
          continue;
        }
        // Avoid consecutive duplicate navigates to the same URL.
        if let Some(last) = steps.last() {
          if last.tool == "navigate" {
            if let Some(prev) = last.arguments.get("url").and_then(|v| v.as_str()) {
              if prev == url {
                continue;
              }
            }
          }
        }
        step_idx += 1;
        steps.push(ExportedRecipeStep {
          id: format!("step_{step_idx}"),
          tool: "navigate".to_string(),
          arguments: serde_json::json!({
            "profile_id": "{{inputs.profile_id}}",
            "url": url,
          }),
          on_error: Some("stop".to_string()),
        });
        step_idx += 1;
        steps.push(ExportedRecipeStep {
          id: format!("step_{step_idx}"),
          tool: "wait_for_navigation".to_string(),
          arguments: serde_json::json!({
            "profile_id": "{{inputs.profile_id}}",
          }),
          on_error: Some("continue".to_string()),
        });
      }
      "click" => {
        flush_input(&mut pending_input, &mut steps, &mut step_idx);
        if let Some(locator) = best_locator(event) {
          step_idx += 1;
          let mut args = serde_json::json!({
            "profile_id": "{{inputs.profile_id}}",
          });
          merge_locator(&mut args, &locator);
          steps.push(ExportedRecipeStep {
            id: format!("step_{step_idx}"),
            tool: "click_element".to_string(),
            arguments: args,
            on_error: Some("stop".to_string()),
          });
        }
      }
      "input" => {
        let value = event
          .payload
          .get("value")
          .and_then(|v| v.as_str())
          .unwrap_or("")
          .to_string();
        let locator = best_locator(event);
        match &mut pending_input {
          Some(p) if same_target(p.locator.as_ref(), locator.as_ref()) => {
            p.value = value;
          }
          _ => {
            flush_input(&mut pending_input, &mut steps, &mut step_idx);
            pending_input = Some(PendingInput { value, locator });
          }
        }
      }
      "scroll" => {
        flush_input(&mut pending_input, &mut steps, &mut step_idx);
        let y = event
          .payload
          .get("scrollY")
          .and_then(|v| v.as_f64())
          .unwrap_or(0.0);
        let x = event
          .payload
          .get("scrollX")
          .and_then(|v| v.as_f64())
          .unwrap_or(0.0);
        step_idx += 1;
        if let Some(locator) = best_locator(event) {
          let mut args = serde_json::json!({
            "profile_id": "{{inputs.profile_id}}",
          });
          merge_locator(&mut args, &locator);
          steps.push(ExportedRecipeStep {
            id: format!("step_{step_idx}"),
            tool: "scroll_element_into_view".to_string(),
            arguments: args,
            on_error: Some("continue".to_string()),
          });
        } else {
          steps.push(ExportedRecipeStep {
            id: format!("step_{step_idx}"),
            tool: "evaluate_javascript".to_string(),
            arguments: serde_json::json!({
              "profile_id": "{{inputs.profile_id}}",
              "expression": format!("window.scrollTo({x}, {y})"),
            }),
            on_error: Some("continue".to_string()),
          });
        }
      }
      "keydown" => {
        // Only export non-printable / modifier keys as press_key steps.
        let key = event
          .payload
          .get("key")
          .and_then(|v| v.as_str())
          .unwrap_or("");
        let is_modifier = event
          .payload
          .get("ctrlKey")
          .and_then(|v| v.as_bool())
          .unwrap_or(false)
          || event
            .payload
            .get("metaKey")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
          || event
            .payload
            .get("altKey")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let is_special = key.len() > 1;
        if is_modifier || is_special {
          flush_input(&mut pending_input, &mut steps, &mut step_idx);
          step_idx += 1;
          steps.push(ExportedRecipeStep {
            id: format!("step_{step_idx}"),
            tool: "press_key".to_string(),
            arguments: serde_json::json!({
              "profile_id": "{{inputs.profile_id}}",
              "key": key,
            }),
            on_error: Some("continue".to_string()),
          });
        }
      }
      "submit" => {
        flush_input(&mut pending_input, &mut steps, &mut step_idx);
        // Navigation after submit is usually captured separately; still emit a
        // wait so recipe runners pause for the next page.
        step_idx += 1;
        steps.push(ExportedRecipeStep {
          id: format!("step_{step_idx}"),
          tool: "wait_for_navigation".to_string(),
          arguments: serde_json::json!({
            "profile_id": "{{inputs.profile_id}}",
          }),
          on_error: Some("continue".to_string()),
        });
      }
      _ => {}
    }
  }
  flush_input(&mut pending_input, &mut steps, &mut step_idx);

  ExportedRecipe {
    version: 1,
    name,
    description: Some(format!(
      "Exported from recording {} (profile {}, {} events)",
      recording.header.id, recording.header.profile_name, recording.header.event_count
    )),
    defaults: Some(ExportedRecipeDefaults {
      timeout_ms: Some(15_000),
      auto_wait: Some(true),
    }),
    inputs: Some(serde_json::json!({
      "profile_id": recording.header.profile_id,
    })),
    steps,
  }
}

struct PendingInput {
  value: String,
  locator: Option<LocatorCandidate>,
}

fn same_target(a: Option<&LocatorCandidate>, b: Option<&LocatorCandidate>) -> bool {
  match (a, b) {
    (Some(a), Some(b)) => a.by == b.by && a.value == b.value,
    (None, None) => true,
    _ => false,
  }
}

/// Prefer testid > role > label > placeholder > text > css.
fn best_locator(event: &RecordedEvent) -> Option<LocatorCandidate> {
  let target = event.target.as_ref()?;
  if let Some(preferred) = target.locators.iter().find(|l| l.preferred) {
    return Some(preferred.clone());
  }
  const ORDER: &[&str] = &[
    "testid",
    "role",
    "label",
    "placeholder",
    "text",
    "css",
    "xpath",
  ];
  for by in ORDER {
    if let Some(loc) = target.locators.iter().find(|l| l.by == *by) {
      return Some(loc.clone());
    }
  }
  if let Some(id) = &target.id {
    if !id.is_empty() {
      return Some(LocatorCandidate {
        by: "css".to_string(),
        value: format!("#{id}"),
        nth: None,
        exact: None,
        preferred: true,
      });
    }
  }
  None
}

fn merge_locator(args: &mut serde_json::Value, locator: &LocatorCandidate) {
  let obj = args.as_object_mut().expect("args object");
  // MCP tools accept either selector (css) or locator {by,value,nth,exact,name}.
  if locator.by == "css" {
    obj.insert("selector".to_string(), serde_json::json!(locator.value));
    return;
  }
  let mut loc = serde_json::json!({
    "by": locator.by,
    "value": locator.value,
  });
  if locator.by == "role" {
    // value may be "role|name" from capture parser.
    if let Some((role, name)) = locator.value.split_once('|') {
      loc["value"] = serde_json::json!(role);
      if !name.is_empty() {
        loc["name"] = serde_json::json!(name);
      }
    }
  }
  if let Some(nth) = locator.nth {
    loc["nth"] = serde_json::json!(nth);
  }
  if let Some(exact) = locator.exact {
    loc["exact"] = serde_json::json!(exact);
  }
  obj.insert("locator".to_string(), loc);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::recorder::types::{RecordedTarget, RecordingHeader, Rect, RECORDING_FORMAT_VERSION};

  fn sample_recording() -> Recording {
    Recording {
      header: RecordingHeader {
        version: RECORDING_FORMAT_VERSION,
        id: "rec_demo".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        profile_id: "p1".to_string(),
        profile_name: "Demo".to_string(),
        browser: "chromium".to_string(),
        start_url: "https://example.com".to_string(),
        viewport: None,
        event_count: 3,
      },
      events: vec![
        RecordedEvent {
          t_ms: 0,
          kind: "navigate".to_string(),
          target: None,
          payload: serde_json::json!({ "to": "https://example.com" }),
        },
        RecordedEvent {
          t_ms: 500,
          kind: "click".to_string(),
          target: Some(RecordedTarget {
            tag: Some("button".to_string()),
            id: Some("login".to_string()),
            locators: vec![LocatorCandidate {
              by: "testid".to_string(),
              value: "login-btn".to_string(),
              nth: None,
              exact: None,
              preferred: true,
            }],
            rect: Some(Rect {
              x: 0.0,
              y: 0.0,
              width: 10.0,
              height: 10.0,
            }),
            attributes: serde_json::json!({}),
          }),
          payload: serde_json::json!({ "clientX": 5, "clientY": 5 }),
        },
        RecordedEvent {
          t_ms: 800,
          kind: "input".to_string(),
          target: Some(RecordedTarget {
            tag: Some("input".to_string()),
            id: Some("email".to_string()),
            locators: vec![LocatorCandidate {
              by: "css".to_string(),
              value: "#email".to_string(),
              nth: None,
              exact: None,
              preferred: true,
            }],
            rect: None,
            attributes: serde_json::json!({}),
          }),
          payload: serde_json::json!({ "value": "a@b.com" }),
        },
        RecordedEvent {
          t_ms: 900,
          kind: "input".to_string(),
          target: Some(RecordedTarget {
            tag: Some("input".to_string()),
            id: Some("email".to_string()),
            locators: vec![LocatorCandidate {
              by: "css".to_string(),
              value: "#email".to_string(),
              nth: None,
              exact: None,
              preferred: true,
            }],
            rect: None,
            attributes: serde_json::json!({}),
          }),
          payload: serde_json::json!({ "value": "user@example.com" }),
        },
      ],
    }
  }

  #[test]
  fn test_export_basic_flow() {
    let recipe = export_to_recipe(&sample_recording(), Some("Demo recipe".to_string()));
    assert_eq!(recipe.version, 1);
    assert_eq!(recipe.name, "Demo recipe");
    // navigate + wait_for_navigation + click + type_text
    assert!(recipe.steps.len() >= 3);
    assert_eq!(recipe.steps[0].tool, "navigate");
    assert_eq!(recipe.steps[1].tool, "wait_for_navigation");
    assert_eq!(recipe.steps[2].tool, "click_element");
    // input events on same target should be coalesced into one type_text with final value
    let type_step = recipe
      .steps
      .iter()
      .find(|s| s.tool == "type_text")
      .expect("type_text");
    assert_eq!(
      type_step.arguments.get("text").and_then(|v| v.as_str()),
      Some("user@example.com")
    );
  }

  #[test]
  fn test_export_skips_about_blank() {
    let mut rec = sample_recording();
    rec.events[0].payload = serde_json::json!({ "to": "about:blank" });
    let recipe = export_to_recipe(&rec, None);
    assert!(recipe.steps.iter().all(|s| s.tool != "navigate" || {
      s.arguments.get("url").and_then(|v| v.as_str()) != Some("about:blank")
    }));
  }
}
