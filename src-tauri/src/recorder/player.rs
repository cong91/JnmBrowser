//! Raw event-stream replay for recordings.
//!
//! - Chromium: CDP `Input.dispatchMouseEvent` / `Input.dispatchKeyEvent` / `Page.navigate`
//! - Camoufox: Playwright mouse/keyboard/goto APIs

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::camoufox_manager::CamoufoxManager;
use crate::recorder::capture::{
  canonical_browser, get_chromium_cdp_port, get_page_ws_url, get_running_profile, profile_data_path,
};
use crate::recorder::types::{RecordedEvent, Recording};

/// Replay a saved recording onto a running profile. Sleeps between events using
/// relative `t_ms` offsets so human pacing is roughly preserved.
pub async fn replay_recording(profile_id: &str, recording: &Recording) -> Result<(), String> {
  let profile = get_running_profile(profile_id)?;
  let browser = canonical_browser(&profile.browser)?;
  match browser {
    "chromium" => replay_chromium(&profile, recording).await,
    "camoufox" => replay_camoufox(&profile, recording).await,
    other => Err(format!("Unsupported browser for replay: {other}")),
  }
}

async fn replay_chromium(
  profile: &crate::profile::types::BrowserProfile,
  recording: &Recording,
) -> Result<(), String> {
  let port = get_chromium_cdp_port(profile).await?;
  let ws_url = get_page_ws_url(port).await?;
  let (mut ws, _) = connect_async(&ws_url)
    .await
    .map_err(|e| format!("Failed to connect CDP for replay: {e}"))?;

  let mut cmd_id: u64 = 0;
  let mut last_t: u64 = 0;

  // Enable Input domain is not required for dispatch, but Page helps navigations.
  send_cmd(&mut ws, &mut cmd_id, "Page.enable", serde_json::json!({})).await?;

  for event in &recording.events {
    let delay = event.t_ms.saturating_sub(last_t);
    if delay > 0 {
      // Cap extreme gaps so a long idle pause doesn't hang the player.
      let capped = delay.min(10_000);
      tokio::time::sleep(Duration::from_millis(capped)).await;
    }
    last_t = event.t_ms;
    dispatch_chromium_event(&mut ws, &mut cmd_id, event).await?;
  }
  Ok(())
}

async fn dispatch_chromium_event(
  ws: &mut tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
  >,
  cmd_id: &mut u64,
  event: &RecordedEvent,
) -> Result<(), String> {
  match event.kind.as_str() {
    "navigate" => {
      let url = event
        .payload
        .get("to")
        .or_else(|| event.payload.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
      if !url.is_empty() {
        send_cmd(
          ws,
          cmd_id,
          "Page.navigate",
          serde_json::json!({ "url": url }),
        )
        .await?;
      }
    }
    "click" => {
      let (x, y) = click_coords(event);
      send_cmd(
        ws,
        cmd_id,
        "Input.dispatchMouseEvent",
        serde_json::json!({
          "type": "mousePressed",
          "x": x,
          "y": y,
          "button": "left",
          "clickCount": 1,
        }),
      )
      .await?;
      send_cmd(
        ws,
        cmd_id,
        "Input.dispatchMouseEvent",
        serde_json::json!({
          "type": "mouseReleased",
          "x": x,
          "y": y,
          "button": "left",
          "clickCount": 1,
        }),
      )
      .await?;
    }
    "input" => {
      // Prefer setting the value via Runtime.evaluate for reliability on
      // controlled inputs; also type keystrokes if no target.
      if let Some(value) = event.payload.get("value").and_then(|v| v.as_str()) {
        if value == "<password>" {
          return Ok(());
        }
        if let Some(css) = preferred_css(event) {
          let expr = format!(
            r#"(function(){{const el=document.querySelector({sel});if(!el)return false;el.focus();el.value={val};el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));return true;}})()"#,
            sel = serde_json::to_string(&css).unwrap_or_else(|_| "null".to_string()),
            val = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string()),
          );
          send_cmd(
            ws,
            cmd_id,
            "Runtime.evaluate",
            serde_json::json!({ "expression": expr, "returnByValue": true }),
          )
          .await?;
        } else {
          type_text_cdp(ws, cmd_id, value).await?;
        }
      }
    }
    "keydown" => {
      let key = event
        .payload
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
      let code = event
        .payload
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or("");
      // Skip plain printable keys that are already covered by input events.
      if key.len() == 1
        && !event
          .payload
          .get("ctrlKey")
          .and_then(|v| v.as_bool())
          .unwrap_or(false)
        && !event
          .payload
          .get("metaKey")
          .and_then(|v| v.as_bool())
          .unwrap_or(false)
        && !event
          .payload
          .get("altKey")
          .and_then(|v| v.as_bool())
          .unwrap_or(false)
      {
        return Ok(());
      }
      let modifiers = key_modifiers(event);
      send_cmd(
        ws,
        cmd_id,
        "Input.dispatchKeyEvent",
        serde_json::json!({
          "type": "keyDown",
          "key": key,
          "code": code,
          "modifiers": modifiers,
          "windowsVirtualKeyCode": event.payload.get("keyCode").and_then(|v| v.as_i64()).unwrap_or(0),
        }),
      )
      .await?;
      send_cmd(
        ws,
        cmd_id,
        "Input.dispatchKeyEvent",
        serde_json::json!({
          "type": "keyUp",
          "key": key,
          "code": code,
          "modifiers": modifiers,
          "windowsVirtualKeyCode": event.payload.get("keyCode").and_then(|v| v.as_i64()).unwrap_or(0),
        }),
      )
      .await?;
    }
    "scroll" => {
      let x = event
        .payload
        .get("scrollX")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
      let y = event
        .payload
        .get("scrollY")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
      let expr = format!("window.scrollTo({x}, {y})");
      send_cmd(
        ws,
        cmd_id,
        "Runtime.evaluate",
        serde_json::json!({ "expression": expr, "returnByValue": true }),
      )
      .await?;
    }
    "submit" => {
      if let Some(css) = preferred_css(event) {
        let expr = format!(
          r#"(function(){{const el=document.querySelector({sel});if(!el)return false;if(typeof el.requestSubmit==='function')el.requestSubmit();else if(typeof el.submit==='function')el.submit();else el.dispatchEvent(new Event('submit',{{bubbles:true,cancelable:true}}));return true;}})()"#,
          sel = serde_json::to_string(&css).unwrap_or_else(|_| "null".to_string()),
        );
        send_cmd(
          ws,
          cmd_id,
          "Runtime.evaluate",
          serde_json::json!({ "expression": expr, "returnByValue": true }),
        )
        .await?;
      }
    }
    _ => {}
  }
  Ok(())
}

async fn type_text_cdp(
  ws: &mut tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
  >,
  cmd_id: &mut u64,
  text: &str,
) -> Result<(), String> {
  for ch in text.chars() {
    let s = ch.to_string();
    send_cmd(
      ws,
      cmd_id,
      "Input.dispatchKeyEvent",
      serde_json::json!({
        "type": "keyDown",
        "text": s,
        "key": s,
        "unmodifiedText": s,
      }),
    )
    .await?;
    send_cmd(
      ws,
      cmd_id,
      "Input.dispatchKeyEvent",
      serde_json::json!({
        "type": "keyUp",
        "key": s,
      }),
    )
    .await?;
  }
  Ok(())
}

async fn send_cmd(
  ws: &mut tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
  >,
  cmd_id: &mut u64,
  method: &str,
  params: serde_json::Value,
) -> Result<serde_json::Value, String> {
  *cmd_id += 1;
  let id = *cmd_id;
  let cmd = serde_json::json!({ "id": id, "method": method, "params": params });
  ws.send(Message::Text(cmd.to_string().into()))
    .await
    .map_err(|e| format!("Failed to send {method}: {e}"))?;
  // Best-effort drain of the matching response (ignore intervening events).
  let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
  loop {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
      // Don't fail the whole replay on a slow command.
      return Ok(serde_json::json!({}));
    }
    match tokio::time::timeout(remaining, ws.next()).await {
      Ok(Some(Ok(Message::Text(text)))) => {
        let resp: serde_json::Value = serde_json::from_str(text.as_str()).unwrap_or_default();
        if resp.get("id") == Some(&serde_json::json!(id)) {
          if let Some(error) = resp.get("error") {
            log::warn!("Replay CDP error for {method}: {error}");
          }
          return Ok(resp.get("result").cloned().unwrap_or(serde_json::json!({})));
        }
      }
      Ok(Some(Ok(_))) => continue,
      Ok(Some(Err(e))) => return Err(format!("WebSocket error during replay: {e}")),
      Ok(None) => return Err("WebSocket closed during replay".to_string()),
      Err(_) => return Ok(serde_json::json!({})),
    }
  }
}

async fn replay_camoufox(
  profile: &crate::profile::types::BrowserProfile,
  recording: &Recording,
) -> Result<(), String> {
  let path = profile_data_path(profile);
  let page = CamoufoxManager::instance()
    .get_active_page(&path)
    .await
    .map_err(|e| format!("Failed to get Camoufox page for replay: {e}"))?;

  let mut last_t: u64 = 0;
  for event in &recording.events {
    let delay = event.t_ms.saturating_sub(last_t).min(10_000);
    if delay > 0 {
      tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    last_t = event.t_ms;
    dispatch_camoufox_event(&page, event).await?;
  }
  Ok(())
}

async fn dispatch_camoufox_event(
  page: &playwright::api::Page,
  event: &RecordedEvent,
) -> Result<(), String> {
  match event.kind.as_str() {
    "navigate" => {
      let url = event
        .payload
        .get("to")
        .or_else(|| event.payload.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
      if !url.is_empty() {
        page
          .goto_builder(url)
          .goto()
          .await
          .map_err(|e| format!("Camoufox navigate failed: {e}"))?;
      }
    }
    "click" => {
      if let Some(css) = preferred_css(event) {
        page
          .click_builder(&css)
          .click()
          .await
          .map_err(|e| format!("Camoufox click failed: {e}"))?;
      } else {
        let (x, y) = click_coords(event);
        let expr = format!(
          r#"(function(){{const el=document.elementFromPoint({x},{y});if(!el)return false;el.dispatchEvent(new MouseEvent('click',{{bubbles:true,clientX:{x},clientY:{y}}}));return true;}})()"#
        );
        let _ = page.eval::<serde_json::Value>(&expr).await;
      }
    }
    "input" => {
      if let Some(value) = event.payload.get("value").and_then(|v| v.as_str()) {
        if value == "<password>" {
          return Ok(());
        }
        if let Some(css) = preferred_css(event) {
          let expr = format!(
            r#"(function(){{const el=document.querySelector({sel});if(!el)return false;el.focus();el.value={val};el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));return true;}})()"#,
            sel = serde_json::to_string(&css).unwrap_or_else(|_| "null".to_string()),
            val = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string()),
          );
          let _ = page.eval::<serde_json::Value>(&expr).await;
        } else {
          page
            .keyboard
            .r#type(value, None)
            .await
            .map_err(|e| format!("Camoufox type failed: {e}"))?;
        }
      }
    }
    "keydown" => {
      let key = event
        .payload
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
      if key.len() == 1
        && !event
          .payload
          .get("ctrlKey")
          .and_then(|v| v.as_bool())
          .unwrap_or(false)
        && !event
          .payload
          .get("metaKey")
          .and_then(|v| v.as_bool())
          .unwrap_or(false)
      {
        return Ok(());
      }
      if !key.is_empty() {
        let _ = page.keyboard.down(key).await;
        let _ = page.keyboard.up(key).await;
      }
    }
    "scroll" => {
      let x = event
        .payload
        .get("scrollX")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
      let y = event
        .payload
        .get("scrollY")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
      let expr = format!("window.scrollTo({x}, {y})");
      let _ = page.eval::<serde_json::Value>(&expr).await;
    }
    "submit" => {
      if let Some(css) = preferred_css(event) {
        let expr = format!(
          r#"(function(){{const el=document.querySelector({sel});if(!el)return false;if(typeof el.requestSubmit==='function')el.requestSubmit();else if(typeof el.submit==='function')el.submit();else el.dispatchEvent(new Event('submit',{{bubbles:true,cancelable:true}}));return true;}})()"#,
          sel = serde_json::to_string(&css).unwrap_or_else(|_| "null".to_string()),
        );
        let _ = page.eval::<serde_json::Value>(&expr).await;
      }
    }
    _ => {}
  }
  Ok(())
}

fn click_coords(event: &RecordedEvent) -> (f64, f64) {
  if let Some(target) = &event.target {
    if let Some(rect) = &target.rect {
      return (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
    }
  }
  let x = event
    .payload
    .get("clientX")
    .and_then(|v| v.as_f64())
    .unwrap_or(0.0);
  let y = event
    .payload
    .get("clientY")
    .and_then(|v| v.as_f64())
    .unwrap_or(0.0);
  (x, y)
}

fn preferred_css(event: &RecordedEvent) -> Option<String> {
  let target = event.target.as_ref()?;
  if let Some(id) = &target.id {
    if !id.is_empty() {
      return Some(format!("#{id}"));
    }
  }
  target
    .locators
    .iter()
    .find(|l| l.by == "css")
    .map(|l| l.value.clone())
    .or_else(|| {
      target
        .locators
        .iter()
        .find(|l| l.by == "testid")
        .map(|l| format!("[data-testid=\"{}\"]", l.value))
    })
}

fn key_modifiers(event: &RecordedEvent) -> i32 {
  let mut mods = 0;
  if event
    .payload
    .get("altKey")
    .and_then(|v| v.as_bool())
    .unwrap_or(false)
  {
    mods |= 1;
  }
  if event
    .payload
    .get("ctrlKey")
    .and_then(|v| v.as_bool())
    .unwrap_or(false)
  {
    mods |= 2;
  }
  if event
    .payload
    .get("metaKey")
    .and_then(|v| v.as_bool())
    .unwrap_or(false)
  {
    mods |= 4;
  }
  if event
    .payload
    .get("shiftKey")
    .and_then(|v| v.as_bool())
    .unwrap_or(false)
  {
    mods |= 8;
  }
  mods
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::recorder::types::{LocatorCandidate, RecordedTarget, Rect};

  #[test]
  fn test_click_coords_from_rect() {
    let event = RecordedEvent {
      t_ms: 0,
      kind: "click".to_string(),
      target: Some(RecordedTarget {
        tag: Some("button".to_string()),
        id: None,
        locators: vec![],
        rect: Some(Rect {
          x: 10.0,
          y: 20.0,
          width: 40.0,
          height: 10.0,
        }),
        attributes: serde_json::json!({}),
      }),
      payload: serde_json::json!({}),
    };
    let (x, y) = click_coords(&event);
    assert!((x - 30.0).abs() < 0.01);
    assert!((y - 25.0).abs() < 0.01);
  }

  #[test]
  fn test_preferred_css() {
    let event = RecordedEvent {
      t_ms: 0,
      kind: "input".to_string(),
      target: Some(RecordedTarget {
        tag: Some("input".to_string()),
        id: Some("email".to_string()),
        locators: vec![LocatorCandidate {
          by: "css".to_string(),
          value: "input.email".to_string(),
          nth: None,
          exact: None,
          preferred: false,
        }],
        rect: None,
        attributes: serde_json::json!({}),
      }),
      payload: serde_json::json!({}),
    };
    assert_eq!(preferred_css(&event).as_deref(), Some("#email"));
  }
}
