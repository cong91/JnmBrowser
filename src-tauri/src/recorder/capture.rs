//! Capture tasks for Chromium (CDP console harvest) and Camoufox (Playwright console).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{oneshot, watch, Mutex as AsyncMutex};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::camoufox_manager::CamoufoxManager;
use crate::chromium_manager::ChromiumManager;
use crate::profile::manager::ProfileManager;
use crate::profile::types::BrowserProfile;
use crate::recorder::inject_script::recorder_script;
use crate::recorder::types::{LocatorCandidate, RecordedEvent, RecordedTarget, Rect};
use crate::recorder::{new_shared, push_event, set_last_error, RECORDER_TAG};

use playwright::api::Page;

/// Resolve a running profile by id.
pub fn get_running_profile(profile_id: &str) -> Result<BrowserProfile, String> {
  let profiles = ProfileManager::instance()
    .list_profiles()
    .map_err(|e| format!("Failed to list profiles: {e}"))?;
  let profile = profiles
    .into_iter()
    .find(|p| p.id.to_string() == profile_id)
    .ok_or_else(|| format!("Profile {profile_id} not found"))?;
  if profile.process_id.is_none() {
    return Err(format!(
      "Profile '{}' is not running. Launch it before recording.",
      profile.name
    ));
  }
  Ok(profile)
}

/// Canonical browser label for a recording ("chromium" | "camoufox").
pub fn canonical_browser(browser: &str) -> Result<&'static str, String> {
  if crate::browser::is_chromium_browser_name(browser) {
    Ok("chromium")
  } else if crate::browser::normalize_browser_name(browser) == "camoufox" {
    Ok("camoufox")
  } else {
    Err(format!(
      "Action recording only supports chromium and camoufox profiles (got '{browser}')"
    ))
  }
}

pub fn profile_data_path(profile: &BrowserProfile) -> String {
  let profiles_dir = ProfileManager::instance().get_profiles_dir();
  crate::ephemeral_dirs::get_effective_profile_path(profile, &profiles_dir)
    .to_string_lossy()
    .to_string()
}

pub async fn get_chromium_cdp_port(profile: &BrowserProfile) -> Result<u16, String> {
  let path = profile_data_path(profile);
  for attempt in 0..15 {
    if attempt > 0 {
      tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if let Some(port) = ChromiumManager::instance().get_cdp_port(&path).await {
      return Ok(port);
    }
  }
  Err(format!(
    "No CDP port available for profile '{}'. Browser may not be running.",
    profile.name
  ))
}

pub async fn get_page_ws_url(port: u16) -> Result<String, String> {
  let url = format!("http://127.0.0.1:{port}/json");
  let client = reqwest::Client::new();
  for attempt in 0..15 {
    if attempt > 0 {
      tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if let Ok(resp) = client
      .get(&url)
      .timeout(std::time::Duration::from_secs(3))
      .send()
      .await
    {
      if let Ok(targets) = resp.json::<Vec<serde_json::Value>>().await {
        if let Some(ws_url) = targets
          .iter()
          .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
          .and_then(|t| t.get("webSocketDebuggerUrl"))
          .and_then(|v| v.as_str())
        {
          return Ok(ws_url.to_string());
        }
      }
    }
  }
  Err("Failed to get CDP page WebSocket URL".to_string())
}

/// Fetch the current page URL via Runtime.evaluate.
pub async fn get_current_url_chromium(ws_url: &str) -> String {
  match send_cdp_oneshot(
    ws_url,
    "Runtime.evaluate",
    serde_json::json!({
      "expression": "location.href",
      "returnByValue": true
    }),
  )
  .await
  {
    Ok(result) => result
      .pointer("/result/value")
      .and_then(|v| v.as_str())
      .unwrap_or("")
      .to_string(),
    Err(_) => String::new(),
  }
}

async fn send_cdp_oneshot(
  ws_url: &str,
  method: &str,
  params: serde_json::Value,
) -> Result<serde_json::Value, String> {
  let (mut ws, _) = connect_async(ws_url)
    .await
    .map_err(|e| format!("CDP connect failed: {e}"))?;
  let cmd = serde_json::json!({ "id": 1, "method": method, "params": params });
  ws.send(Message::Text(cmd.to_string().into()))
    .await
    .map_err(|e| format!("CDP send failed: {e}"))?;
  let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
  loop {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
      return Err(format!("Timeout waiting for {method}"));
    }
    match tokio::time::timeout(remaining, ws.next()).await {
      Ok(Some(Ok(Message::Text(text)))) => {
        let resp: serde_json::Value = serde_json::from_str(text.as_str()).unwrap_or_default();
        if resp.get("id") == Some(&serde_json::json!(1)) {
          if let Some(error) = resp.get("error") {
            return Err(format!("CDP error for {method}: {error}"));
          }
          return Ok(resp.get("result").cloned().unwrap_or(serde_json::json!({})));
        }
      }
      Ok(Some(Ok(_))) => continue,
      Ok(Some(Err(e))) => return Err(format!("WebSocket error: {e}")),
      Ok(None) => return Err("WebSocket closed".to_string()),
      Err(_) => return Err(format!("Timeout waiting for {method}")),
    }
  }
}

/// Chromium capture loop: inject script + harvest Runtime.consoleAPICalled.
pub async fn run_chromium_recorder(
  profile_id: String,
  ws_url: String,
  shared: Arc<AsyncMutex<crate::recorder::RecorderShared>>,
  mut cancel_rx: watch::Receiver<bool>,
  ready_tx: oneshot::Sender<Result<(), String>>,
) {
  let mut ready_tx = Some(ready_tx);
  let notify = |ready_tx: &mut Option<oneshot::Sender<Result<(), String>>>,
                result: Result<(), String>| {
    if let Some(tx) = ready_tx.take() {
      let _ = tx.send(result);
    }
  };

  let connect = connect_async(&ws_url).await;
  let (mut ws_stream, _) = match connect {
    Ok(pair) => pair,
    Err(e) => {
      let msg = format!("Failed to connect CDP for recording profile {profile_id}: {e}");
      set_last_error(&shared, msg.clone()).await;
      notify(&mut ready_tx, Err(msg));
      return;
    }
  };

  let mut cmd_id: u64 = 0;
  let mut pending_events: Vec<serde_json::Value> = Vec::new();

  async fn send_cmd(
    ws: &mut tokio_tungstenite::WebSocketStream<
      tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    cmd_id: &mut u64,
    pending_events: &mut Vec<serde_json::Value>,
    method: &str,
    params: serde_json::Value,
  ) -> Result<serde_json::Value, String> {
    *cmd_id += 1;
    let id = *cmd_id;
    let cmd = serde_json::json!({ "id": id, "method": method, "params": params });
    ws.send(Message::Text(cmd.to_string().into()))
      .await
      .map_err(|e| format!("Failed to send {method}: {e}"))?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
      let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
      if remaining.is_zero() {
        return Err(format!("Timeout waiting for {method}"));
      }
      match tokio::time::timeout(remaining, ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
          let resp: serde_json::Value = serde_json::from_str(text.as_str()).unwrap_or_default();
          if resp.get("id") == Some(&serde_json::json!(id)) {
            if let Some(error) = resp.get("error") {
              return Err(format!("CDP error for {method}: {error}"));
            }
            return Ok(resp.get("result").cloned().unwrap_or(serde_json::json!({})));
          }
          if resp.get("method").is_some() {
            pending_events.push(resp);
          }
        }
        Ok(Some(Ok(_))) => continue,
        Ok(Some(Err(e))) => return Err(format!("WebSocket error: {e}")),
        Ok(None) => return Err("WebSocket closed".to_string()),
        Err(_) => return Err(format!("Timeout waiting for {method}")),
      }
    }
  }

  let script = recorder_script();
  let setup = [
    ("Page.enable", serde_json::json!({})),
    ("Runtime.enable", serde_json::json!({})),
    (
      "Page.addScriptToEvaluateOnNewDocument",
      serde_json::json!({ "source": script, "runImmediately": true }),
    ),
    (
      "Runtime.evaluate",
      serde_json::json!({ "expression": script, "returnByValue": true }),
    ),
  ];

  for (method, params) in setup {
    if let Err(e) = send_cmd(
      &mut ws_stream,
      &mut cmd_id,
      &mut pending_events,
      method,
      params,
    )
    .await
    {
      let msg = format!("Recorder setup failed ({method}) for {profile_id}: {e}");
      set_last_error(&shared, msg.clone()).await;
      notify(&mut ready_tx, Err(msg));
      return;
    }
  }

  // Drain any events buffered during setup.
  for event in pending_events.drain(..) {
    handle_cdp_console_event(&shared, &event).await;
  }

  notify(&mut ready_tx, Ok(()));
  log::info!("Recorder: Chromium capture armed for profile {profile_id}");

  loop {
    tokio::select! {
      _ = cancel_rx.changed() => {
        if *cancel_rx.borrow() {
          break;
        }
      }
      msg = ws_stream.next() => {
        match msg {
          Some(Ok(Message::Text(text))) => {
            let value: serde_json::Value = serde_json::from_str(text.as_str()).unwrap_or_default();
            handle_cdp_console_event(&shared, &value).await;
          }
          Some(Ok(_)) => {}
          Some(Err(e)) => {
            set_last_error(&shared, format!("Recorder WS error: {e}")).await;
            break;
          }
          None => break,
        }
      }
    }
  }

  log::info!("Recorder: Chromium capture stopped for profile {profile_id}");
}

async fn handle_cdp_console_event(
  shared: &Arc<AsyncMutex<crate::recorder::RecorderShared>>,
  value: &serde_json::Value,
) {
  if value.get("method").and_then(|m| m.as_str()) != Some("Runtime.consoleAPICalled") {
    return;
  }
  let Some(params) = value.get("params") else {
    return;
  };
  let Some(args) = params.get("args").and_then(|a| a.as_array()) else {
    return;
  };
  for arg in args {
    let text = arg
      .get("value")
      .and_then(|v| v.as_str())
      .or_else(|| arg.get("description").and_then(|v| v.as_str()))
      .unwrap_or("");
    if let Some(event) = parse_tagged_console_message(text) {
      push_event(shared, event).await;
    }
  }
}

/// Camoufox capture loop via Playwright page console events.
pub async fn run_camoufox_recorder(
  profile_id: String,
  profile_path: String,
  shared: Arc<AsyncMutex<crate::recorder::RecorderShared>>,
  mut cancel_rx: watch::Receiver<bool>,
  ready_tx: oneshot::Sender<Result<(), String>>,
) {
  let mut ready_tx = Some(ready_tx);
  let notify = |ready_tx: &mut Option<oneshot::Sender<Result<(), String>>>,
                result: Result<(), String>| {
    if let Some(tx) = ready_tx.take() {
      let _ = tx.send(result);
    }
  };

  // Retry get_active_page for a few seconds — the automation session may
  // need a moment to stabilize even after start_recording's own retry loop,
  // e.g. if a tab navigation transiently makes pages() return empty.
  let page = {
    let mut last_err: Option<String> = None;
    let mut resolved: Option<Page> = None;
    for attempt in 0..10 {
      if attempt > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
      }
      match CamoufoxManager::instance()
        .get_active_page(&profile_path)
        .await
      {
        Ok(page) => {
          resolved = Some(page);
          break;
        }
        Err(e) => {
          last_err = Some(format!("{e}"));
        }
      }
    }
    match resolved {
      Some(page) => page,
      None => {
        let msg = format!(
          "Failed to get Camoufox page for {profile_id}: {}",
          last_err.unwrap_or_else(|| "unknown error".to_string())
        );
        set_last_error(&shared, msg.clone()).await;
        notify(&mut ready_tx, Err(msg));
        return;
      }
    }
  };

  let script = recorder_script();
  if let Err(e) = page.add_init_script(&script).await {
    let msg = format!("Failed to add Camoufox init script for {profile_id}: {e}");
    set_last_error(&shared, msg.clone()).await;
    notify(&mut ready_tx, Err(msg));
    return;
  }
  // Apply to current document as well.
  if let Err(e) = page.eval::<serde_json::Value>(&script).await {
    log::warn!("Recorder: Camoufox evaluate inject failed (continuing): {e}");
  }

  let mut stream = match page.subscribe_event() {
    Ok(stream) => stream,
    Err(e) => {
      let msg = format!("Failed to subscribe Camoufox console for {profile_id}: {e}");
      set_last_error(&shared, msg.clone()).await;
      notify(&mut ready_tx, Err(msg));
      return;
    }
  };

  notify(&mut ready_tx, Ok(()));
  log::info!("Recorder: Camoufox capture armed for profile {profile_id}");

  loop {
    tokio::select! {
      _ = cancel_rx.changed() => {
        if *cancel_rx.borrow() {
          break;
        }
      }
      event = stream.next() => {
        let Some(event) = event else { break; };
        match event {
          Ok(playwright::api::page::Event::Console(message)) => {
            let text = message.text().unwrap_or_default();
            if let Some(ev) = parse_tagged_console_message(&text) {
              push_event(&shared, ev).await;
            }
          }
          Ok(_) => {}
          Err(e) => {
            set_last_error(&shared, format!("Camoufox stream error: {e}")).await;
            break;
          }
        }
      }
    }
  }

  log::info!("Recorder: Camoufox capture stopped for profile {profile_id}");
}

/// Parse a console message that may carry a tagged recorder payload.
pub fn parse_tagged_console_message(text: &str) -> Option<RecordedEvent> {
  let payload = text.strip_prefix(RECORDER_TAG)?;
  let value: serde_json::Value = serde_json::from_str(payload).ok()?;
  parse_recorded_event(&value)
}

fn parse_recorded_event(value: &serde_json::Value) -> Option<RecordedEvent> {
  let kind = value.get("kind")?.as_str()?.to_string();
  let t_ms = value
    .get("t_ms")
    .and_then(|v| v.as_u64())
    .or_else(|| value.get("t_ms").and_then(|v| v.as_f64()).map(|f| f as u64))
    .unwrap_or(0);
  let target = value.get("target").and_then(parse_target);
  let payload = value
    .get("payload")
    .cloned()
    .unwrap_or(serde_json::Value::Null);
  Some(RecordedEvent {
    t_ms,
    kind,
    target,
    payload,
  })
}

fn parse_target(value: &serde_json::Value) -> Option<RecordedTarget> {
  if value.is_null() {
    return None;
  }
  let locators = value
    .get("locators")
    .and_then(|v| v.as_array())
    .map(|arr| {
      arr
        .iter()
        .filter_map(|item| {
          let by = item.get("by")?.as_str()?.to_string();
          let value = item
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
          if value.is_empty() && by != "role" {
            return None;
          }
          // role may store name separately; fold into value as "role|name" if present.
          let value = if by == "role" {
            if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
              if !name.is_empty() {
                format!("{value}|{name}")
              } else {
                value
              }
            } else {
              value
            }
          } else {
            value
          };
          Some(LocatorCandidate {
            by,
            value,
            nth: item.get("nth").and_then(|v| v.as_u64()).map(|n| n as usize),
            exact: item.get("exact").and_then(|v| v.as_bool()),
            preferred: item
              .get("preferred")
              .and_then(|v| v.as_bool())
              .unwrap_or(false),
          })
        })
        .collect()
    })
    .unwrap_or_default();

  let rect = value.get("rect").and_then(|r| {
    Some(Rect {
      x: r.get("x")?.as_f64()?,
      y: r.get("y")?.as_f64()?,
      width: r.get("width")?.as_f64()?,
      height: r.get("height")?.as_f64()?,
    })
  });

  Some(RecordedTarget {
    tag: value
      .get("tag")
      .and_then(|v| v.as_str())
      .map(ToString::to_string),
    id: value
      .get("id")
      .and_then(|v| v.as_str())
      .map(ToString::to_string),
    locators,
    rect,
    attributes: value
      .get("attributes")
      .cloned()
      .unwrap_or(serde_json::Value::Null),
  })
}

pub fn now_unix_secs() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0)
}

/// Helper re-export for commands that need a fresh shared buffer without
/// knowing the private type layout.
pub fn allocate_shared() -> Arc<AsyncMutex<crate::recorder::RecorderShared>> {
  new_shared()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_tagged_click() {
    let msg = r##"__REC__:{"t_ms":120,"kind":"click","target":{"tag":"button","id":"ok","locators":[{"by":"css","value":"#ok","preferred":true}],"rect":{"x":1,"y":2,"width":3,"height":4},"attributes":{}},"payload":{"button":0,"clientX":10,"clientY":20}}"##;
    let event = parse_tagged_console_message(msg).expect("parse");
    assert_eq!(event.kind, "click");
    assert_eq!(event.t_ms, 120);
    let target = event.target.expect("target");
    assert_eq!(target.tag.as_deref(), Some("button"));
    assert_eq!(target.locators[0].by, "css");
  }

  #[test]
  fn test_parse_ignores_non_tagged() {
    assert!(parse_tagged_console_message("hello world").is_none());
  }

  #[test]
  fn test_canonical_browser() {
    assert_eq!(canonical_browser("chromium").unwrap(), "chromium");
    assert_eq!(canonical_browser("camoufox").unwrap(), "camoufox");
    assert!(canonical_browser("firefox").is_err());
  }
}
