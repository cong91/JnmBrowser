use chrono::{Datelike, NaiveDate, Utc};
use futures_util::stream::{FuturesUnordered, StreamExt};
use futures_util::SinkExt;
use rand::prelude::IndexedRandom;
use rand::{Rng, RngExt};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tauri::Emitter;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use super::store::{
  cdk_remaining_capacity, get_cdk_inventory, put_cdk_inventory_record, reserve_cdk_slots_per,
  save_registration_result, CdkSlotReservation, MAX_ACCOUNTS_PER_CDK,
};
use super::types::{
  should_rotate, CdkInventoryRecord, EmailProviderProvenance, NetworkMode, RegistrationConfig,
  RegistrationOutcomeReason, RegistrationProgress, RegistrationResult, RegistrationStep,
  TwoFactorBackfillAccessState,
};
use crate::auto_service::openai::chatgpt_auth::ChatGptBrowser;
use crate::auto_service::openai::chatgpt_two_factor::{
  enable_authenticator_two_factor, BrowserTwoFactorAdapter,
};
use crate::email::{EmailService, EmailServiceError};
use crate::sms::{NumberRequest, SmsService, SmsServiceError};

/// Diagnostic helper used only during live About You debugging.
/// Captures a PNG screenshot of the current page via the CDP
/// `Page.captureScreenshot` command + the form HTML / input state, and
/// writes them under `<app_data>/debug/about-you-<suffix>.{png,html}`. No
/// secret material is written — only what is visible in the viewport plus
/// the form's outerHTML. If any step fails the helper returns silently; the
/// caller already has the primary error message.
async fn snapshot_about_you_page(session: &mut BrowserSession, suffix: &str) {
  use base64::Engine as _;
  let dir = crate::app_dirs::data_dir().join("debug");
  let _ = std::fs::create_dir_all(&dir);
  let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");

  // Screenshot via CDP.
  let snap_params = serde_json::json!({ "format": "png", "captureBeyondViewport": true });
  if let Ok(snap) = session
    .send_cmd("Page.captureScreenshot", snap_params)
    .await
  {
    if let Some(b64) = snap.get("data").and_then(|v| v.as_str()) {
      if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
        let _ = std::fs::write(dir.join(format!("about-you-{}-{}.png", ts, suffix)), bytes);
      }
    }
  }

  // Form HTML + inputs state.
  let form_html_js = r#"(function(){
    const form = document.querySelector('form');
    const inputs = Array.from(document.querySelectorAll('input, select, textarea'))
      .filter((el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; })
      .map((el) => ({
        tag: el.tagName,
        name: el.name || '',
        type: el.type || '',
        value: String(el.value || ''),
        disabled: el.disabled,
        aria: el.getAttribute('aria-invalid'),
        placeholder: el.placeholder || '',
      }));
    const btns = Array.from(document.querySelectorAll('button'))
      .filter((el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; })
      .map((el) => ({
        text: (el.innerText || '').trim().slice(0, 60),
        type: el.type || '',
        disabled: el.disabled,
        ariaDisabled: el.getAttribute('aria-disabled') || '',
      }));
    const errs = Array.from(document.querySelectorAll('[role="alert"], [class*="error" i], [data-error]'))
      .filter((el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; })
      .map((el) => (el.innerText || '').trim().slice(0, 160))
      .filter((s) => s.length > 0);
    return {
      formHTML: form ? form.outerHTML.slice(0, 8000) : '<no form>',
      url: window.location.href,
      title: document.title,
      inputs,
      buttons: btns,
      errors: errs,
      active: document.activeElement
        ? (document.activeElement.tagName + '#' + (document.activeElement.id || '') + ' name=' + (document.activeElement.getAttribute('name') || ''))
        : 'none',
    };
  })()"#;
  if let Ok(info) = session.evaluate(form_html_js, false).await {
    let value = info.get("value").cloned().unwrap_or_default();
    let json_text = serde_json::to_string_pretty(&value).unwrap_or_default();
    let _ = std::fs::write(
      dir.join(format!("about-you-{}-{}.html", ts, suffix)),
      format!(
        "<!-- About You diagnostic {} -->\n<pre>{}</pre>\n\n<!-- form HTML -->\n{}",
        suffix,
        json_text,
        value
          .get("formHTML")
          .and_then(|v| v.as_str())
          .unwrap_or("<none>")
      ),
    );
    eprintln!(
      "snapshot_about_you_page wrote html to {}",
      dir
        .join(format!("about-you-{}-{}.html", ts, suffix))
        .display()
    );
  }
}

// ---------------------------------------------------------------------------
// CDP connection wrapper
// ---------------------------------------------------------------------------

type CdpWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(crate) struct CdpConnection {
  ws: CdpWs,
  next_id: u64,
  /// Tracked mouse cursor position across the session (x, y in viewport coords).
  cursor_pos: (f64, f64),
}

impl CdpConnection {
  async fn connect(ws_url: &str) -> Result<Self, String> {
    let (ws, _) = connect_async(ws_url)
      .await
      .map_err(|e| format!("CDP WebSocket connect failed: {e}"))?;
    let mut conn = Self {
      ws,
      next_id: 1,
      cursor_pos: (0.0, 0.0),
    };
    conn.prepare_for_background_automation().await?;
    Ok(conn)
  }

  async fn prepare_for_background_automation(&mut self) -> Result<(), String> {
    let _ = self.send_cmd("Page.enable", serde_json::json!({})).await;
    let _ = self.send_cmd("Runtime.enable", serde_json::json!({})).await;
    let _ = self
      .send_cmd(
        "Emulation.setFocusEmulationEnabled",
        serde_json::json!({ "enabled": true }),
      )
      .await;
    let _ = self
      .send_cmd("Page.bringToFront", serde_json::json!({}))
      .await;
    if let Ok(win) = self
      .send_cmd("Browser.getWindowForTarget", serde_json::json!({}))
      .await
    {
      if let Some(window_id) = win.get("windowId").and_then(|v| v.as_i64()) {
        let state = win
          .pointer("/bounds/windowState")
          .and_then(|v| v.as_str())
          .unwrap_or("");
        if state == "minimized" || state.is_empty() {
          let (win_w, win_h) = {
            let mut rng = rand::rng();
            (rng.random_range(1280..1440), rng.random_range(800..1000))
          };
          let _ = self
            .send_cmd(
              "Browser.setWindowBounds",
              serde_json::json!({
                "windowId": window_id,
                "bounds": {
                  "windowState": "normal",
                  "width": win_w,
                  "height": win_h,
                }
              }),
            )
            .await;
        }
      }
    }
    Ok(())
  }

  async fn send_cmd(
    &mut self,
    method: &str,
    params: serde_json::Value,
  ) -> Result<serde_json::Value, String> {
    let id = self.next_id;
    self.next_id += 1;
    let cmd = serde_json::json!({ "id": id, "method": method, "params": params });
    self
      .ws
      .send(Message::Text(cmd.to_string().into()))
      .await
      .map_err(|e| format!("CDP send error: {e}"))?;

    loop {
      let msg = self
        .ws
        .next()
        .await
        .ok_or("CDP stream closed")?
        .map_err(|e| format!("CDP read error: {e}"))?;
      if let Message::Text(text) = msg {
        let v: serde_json::Value =
          serde_json::from_str(&text).map_err(|e| format!("CDP parse: {e}"))?;
        if v["id"].as_u64() == Some(id) {
          if let Some(err) = v.get("error") {
            return Err(format!("CDP error: {err}"));
          }
          return Ok(v["result"].clone());
        }
      }
    }
  }

  async fn navigate(&mut self, url: &str, timeout_secs: u64) -> Result<(), String> {
    let _ = self.prepare_for_background_automation().await;
    self
      .send_cmd("Page.navigate", serde_json::json!({ "url": url }))
      .await?;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
      if tokio::time::Instant::now() > deadline {
        return Err("Navigation timeout".into());
      }
      let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
      match tokio::time::timeout(remaining, self.ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
          let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
          if v["method"].as_str() == Some("Page.loadEventFired") {
            return Ok(());
          }
        }
        Ok(None) => return Err("CDP stream closed during navigation".into()),
        Err(_) => return Err("Navigation timeout".into()),
        _ => {}
      }
    }
  }

  async fn evaluate(
    &mut self,
    expression: &str,
    await_promise: bool,
  ) -> Result<serde_json::Value, String> {
    let params = serde_json::json!({
      "expression": expression,
      "returnByValue": true,
      "awaitPromise": await_promise,
    });
    let result = self.send_cmd("Runtime.evaluate", params).await?;
    if let Some(exception) = result.get("exceptionDetails") {
      return Err(format!("JS exception: {exception}"));
    }
    Ok(result["result"].clone())
  }

  /// Execute a fetch() via CDP and return the parsed JSON response.
  /// `extra_headers` is a JS object literal string like `{ 'x-foo': 'bar' }`.
  #[allow(dead_code)]
  async fn fetch_json_with_headers(
    &mut self,
    js_fetch_expr: &str,
    extra_headers: &str,
  ) -> Result<serde_json::Value, String> {
    let expr = format!(
      "(async () => {{ const r = await {js_fetch_expr}; const t = await r.text(); try {{ return JSON.parse(t); }} catch(_) {{ return {{ _status: r.status, _body: t }}; }} }})()"
    );
    // Embed the extra_headers into the fetch options if not already present
    let with_headers = if extra_headers.is_empty() || js_fetch_expr.contains("headers:") {
      expr
    } else {
      // Inject headers into the fetch call
      expr.replace("})", &format!(", headers: {{ {extra_headers} }} }})"))
    };
    self.evaluate(&with_headers, true).await.and_then(|r| {
      r.get("value")
        .cloned()
        .ok_or_else(|| "evaluate returned no value".into())
    })
  }

  #[allow(dead_code)]
  async fn fetch_json(&mut self, js_fetch_expr: &str) -> Result<serde_json::Value, String> {
    self.fetch_json_with_headers(js_fetch_expr, "").await
  }

  async fn current_url(&mut self) -> Result<String, String> {
    let result = self.evaluate("window.location.href", false).await?;
    result["value"]
      .as_str()
      .map(|s| s.to_string())
      .ok_or_else(|| "Failed to get current URL".into())
  }

  /// Set a cookie in the browser via CDP.
  async fn set_cookie(&mut self, name: &str, value: &str, domain: &str) -> Result<(), String> {
    let js = format!(
      "document.cookie = '{name}={value}; domain={domain}; path=/; SameSite=None; Secure'",
    );
    // Use Network.setCookie for reliability
    let params = serde_json::json!({
      "name": name,
      "value": value,
      "domain": domain,
      "path": "/",
      "secure": true,
      "sameSite": "None",
    });
    let _ = self.send_cmd("Network.setCookie", params).await;
    // Also try document.cookie as fallback
    let _ = self.evaluate(&js, false).await;
    Ok(())
  }

  async fn mouse_move(&mut self, x: f64, y: f64) -> Result<(), String> {
    self
      .send_cmd(
        "Input.dispatchMouseEvent",
        serde_json::json!({
          "type": "mouseMoved",
          "x": x,
          "y": y,
          "button": "none",
        }),
      )
      .await?;
    self.cursor_pos = (x, y);
    Ok(())
  }

  async fn mouse_click(&mut self, x: f64, y: f64) -> Result<(), String> {
    // Instant click at point (prefer humanized path via BrowserSession::human_click).
    let _ = self.mouse_move(x, y).await;
    sleep(crate::browser_actions::jitter_ms(20, 40)).await;
    self
      .send_cmd(
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
    sleep(crate::browser_actions::jitter_ms(30, 55)).await;
    self
      .send_cmd(
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
    Ok(())
  }

  async fn key_char(&mut self, ch: char) -> Result<(), String> {
    let text = ch.to_string();
    self
      .send_cmd(
        "Input.dispatchKeyEvent",
        serde_json::json!({
          "type": "keyDown",
          "text": text,
          "key": text,
          "unmodifiedText": text,
        }),
      )
      .await?;
    // Human-like key hold time: 50–150ms between keyDown and keyUp.
    sleep(crate::browser_actions::jitter_ms(50, 150)).await;
    self
      .send_cmd(
        "Input.dispatchKeyEvent",
        serde_json::json!({
          "type": "keyUp",
          "key": text,
        }),
      )
      .await?;
    Ok(())
  }

  async fn key_backspace(&mut self) -> Result<(), String> {
    self
      .send_cmd(
        "Input.dispatchKeyEvent",
        serde_json::json!({
          "type": "keyDown",
          "key": "Backspace",
          "code": "Backspace",
          "windowsVirtualKeyCode": 8,
          "nativeVirtualKeyCode": 8,
        }),
      )
      .await?;
    // Human-like key hold time: 50–150ms between keyDown and keyUp.
    sleep(crate::browser_actions::jitter_ms(50, 150)).await;
    self
      .send_cmd(
        "Input.dispatchKeyEvent",
        serde_json::json!({
          "type": "keyUp",
          "key": "Backspace",
          "code": "Backspace",
          "windowsVirtualKeyCode": 8,
          "nativeVirtualKeyCode": 8,
        }),
      )
      .await?;
    Ok(())
  }
}

// ---------------------------------------------------------------------------
// Dual-kernel browser session (Chromium CDP + Camoufox Playwright)
// ---------------------------------------------------------------------------

#[allow(clippy::large_enum_variant)]
pub(crate) enum BrowserSession {
  Cdp(CdpConnection),
  Camoufox {
    page: playwright::api::Page,
    cursor_pos: (f64, f64),
  },
}

impl BrowserSession {
  /// Forward a CDP command. Only meaningful for the Chromium kernel; for the
  /// Camoufox kernel this returns an error (diagnostics that rely on CDP
  /// commands are silently skipped in that path).
  async fn send_cmd(
    &mut self,
    method: &str,
    params: serde_json::Value,
  ) -> Result<serde_json::Value, String> {
    match self {
      Self::Cdp(cdp) => cdp.send_cmd(method, params).await,
      Self::Camoufox { .. } => Err(format!(
        "Camoufox kernel does not support CDP command {method}"
      )),
    }
  }

  async fn navigate(&mut self, url: &str, timeout_secs: u64) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => cdp.navigate(url, timeout_secs).await,
      Self::Camoufox { page, .. } => {
        // Prefer a softer wait than full load — ChatGPT home can keep network
        // busy and exceed Playwright's default 30s load timeout.
        let _ = timeout_secs;
        let _ =
          crate::camoufox_manager::CamoufoxManager::prepare_page_for_background_automation(page)
            .await;
        match page.goto_builder(url).goto().await {
          Ok(_) => {
            let _ =
              crate::camoufox_manager::CamoufoxManager::prepare_page_for_background_automation(
                page,
              )
              .await;
            Ok(())
          }
          Err(e) => {
            // If we already landed on a related origin, treat timeout as soft success.
            let current = page.url().unwrap_or_default();
            if current.starts_with(url)
              || (url.contains("chatgpt.com") && current.contains("chatgpt.com"))
              || (url.contains("auth.openai.com") && current.contains("auth.openai.com"))
            {
              let _ =
                crate::camoufox_manager::CamoufoxManager::prepare_page_for_background_automation(
                  page,
                )
                .await;
              Ok(())
            } else {
              Err(format!("Camoufox navigate failed: {e} (current={current})"))
            }
          }
        }
      }
    }
  }

  /// Returns a CDP-shaped Runtime.evaluate result object: `{ "value": ... }`.
  async fn evaluate(
    &mut self,
    expression: &str,
    await_promise: bool,
  ) -> Result<serde_json::Value, String> {
    match self {
      Self::Cdp(cdp) => cdp.evaluate(expression, await_promise).await,
      Self::Camoufox { page, .. } => {
        let _ = await_promise; // Playwright eval awaits promises by default.
        let value: serde_json::Value = page
          .eval(expression)
          .await
          .map_err(|e| format!("Camoufox evaluate failed: {e}"))?;
        Ok(serde_json::json!({ "value": value }))
      }
    }
  }

  #[allow(dead_code)]
  async fn fetch_json_with_headers(
    &mut self,
    js_fetch_expr: &str,
    extra_headers: &str,
  ) -> Result<serde_json::Value, String> {
    // IMPORTANT: never string-inject into the fetch call body — OpenAI register
    // uses JSON.stringify({...}) and naive "})" replacement pollutes the payload.
    // Page-context fetch defaults to same-origin credentials, which is enough once
    // we are on auth.openai.com / chatgpt.com.
    let expr = if extra_headers.is_empty() || js_fetch_expr.contains("headers:") {
      js_fetch_expr.to_string()
    } else {
      // Only inject headers into the outermost fetch options object by appending
      // before the final "})" of the expression when it ends with "})".
      let trimmed = js_fetch_expr.trim_end();
      if let Some(base) = trimmed.strip_suffix("})") {
        format!("{base}, headers: {{ {extra_headers} }}}})")
      } else {
        js_fetch_expr.to_string()
      }
    };
    let wrapped = format!(
      "(async () => {{ const r = await {expr}; const t = await r.text(); try {{ const j = JSON.parse(t); if (j && typeof j === 'object' && j._status === undefined) {{ j._status = r.status; }} return j; }} catch(_) {{ return {{ _status: r.status, _body: t }}; }} }})()"
    );
    self.evaluate(&wrapped, true).await.and_then(|r| {
      r.get("value")
        .cloned()
        .ok_or_else(|| "evaluate returned no value".into())
    })
  }

  async fn fetch_json(&mut self, js_fetch_expr: &str) -> Result<serde_json::Value, String> {
    self.fetch_json_with_headers(js_fetch_expr, "").await
  }

  async fn current_url(&mut self) -> Result<String, String> {
    match self {
      Self::Cdp(cdp) => cdp.current_url().await,
      Self::Camoufox { page, .. } => page
        .url()
        .map_err(|e| format!("Camoufox current_url failed: {e}")),
    }
  }

  async fn set_cookie(&mut self, name: &str, value: &str, domain: &str) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => cdp.set_cookie(name, value, domain).await,
      Self::Camoufox { page, .. } => {
        let mut cookie = playwright::api::Cookie::with_domain_path(name, value, domain, "/");
        cookie.secure = Some(true);
        cookie.same_site = Some(playwright::api::SameSite::None);
        page
          .context()
          .add_cookies(&[cookie])
          .await
          .map_err(|e| format!("Camoufox set_cookie failed: {e}"))
      }
    }
  }

  pub(crate) async fn prepare_existing_account_login(
    &mut self,
    device_id: &str,
    login_email: &str,
  ) -> Result<(), String> {
    if login_email.trim().is_empty() {
      return Err("existing-account login email is empty".into());
    }
    self.clear_all_site_data().await?;
    for domain in [
      "chatgpt.com",
      ".chatgpt.com",
      "auth.openai.com",
      ".auth.openai.com",
    ] {
      self.set_cookie("oai-did", device_id, domain).await?;
    }

    // Step 1: Establish session on chatgpt.com homepage first for Cloudflare clearance.
    self.navigate("https://chatgpt.com/", 25).await?;
    sleep(std::time::Duration::from_millis(2000)).await;
    let _ = self.evaluate("window.scrollBy(0, 200)", false).await;
    sleep(std::time::Duration::from_millis(800)).await;

    // Step 2: Navigate to dedicated login page (same origin, session preserved).
    self.navigate("https://chatgpt.com/auth/login", 30).await?;
    sleep(std::time::Duration::from_millis(1500)).await;

    // Find and focus the email input in the login form.
    let focus_js = r#"(function(){
      const selectors = 'input[type="email"], input[name="email"], input[id="email"], input[autocomplete*="email"]';
      const nodes = Array.from(document.querySelectorAll(selectors));
      for (const el of nodes) {
        const r = el.getBoundingClientRect();
        if (r.width <= 0 || r.height <= 0) continue;
        el.scrollIntoView({ block: 'center' });
        el.focus();
        el.click();
        try { if (el.select) el.select(); } catch (_) {}
        const proto = window.HTMLInputElement && window.HTMLInputElement.prototype;
        const desc = proto && Object.getOwnPropertyDescriptor(proto, 'value');
        if (desc && desc.set) desc.set.call(el, '');
        else el.value = '';
        el.dispatchEvent(new Event('input', { bubbles: true }));
        return true;
      }
      return false;
    })()"#;
    let mut focused = false;
    for _ in 0..15 {
      sleep(std::time::Duration::from_millis(400)).await;
      if let Ok(res) = self.evaluate(focus_js, false).await {
        if res.get("value").and_then(|v| v.as_bool()) == Some(true) {
          focused = true;
          break;
        }
      }
    }
    if !focused {
      return Err(
        "prepare_existing_account_login: email input not found on auth/login page".into(),
      );
    }

    // Type email character by character (human-like).
    for ch in login_email.chars() {
      self.key_char(ch).await?;
      sleep(std::time::Duration::from_millis(35)).await;
    }
    let _ = self
      .evaluate(
        r#"(function(){
          const el = document.activeElement;
          if (!el) return false;
          el.dispatchEvent(new Event('input', { bubbles: true }));
          el.dispatchEvent(new Event('change', { bubbles: true }));
          return true;
        })()"#,
        false,
      )
      .await;
    sleep(std::time::Duration::from_millis(350)).await;

    // Click "Continue" button.
    let continue_js = r#"(function(){
      const els = Array.from(document.querySelectorAll('button, [role="button"]'));
      for (const el of els) {
        const text = (el.innerText || el.textContent || '').trim().toLowerCase();
        if (text === 'continue') {
          const r = el.getBoundingClientRect();
          if (r.width > 0 && r.height > 0) {
            el.scrollIntoView({ block: 'center' });
            return { x: r.left + r.width/2, y: r.top + r.height/2 };
          }
        }
      }
      // Fallback: form submit button
      const submitBtn = document.querySelector('button[type="submit"], form button');
      if (submitBtn) {
        const r = submitBtn.getBoundingClientRect();
        if (r.width > 0 && r.height > 0) {
          submitBtn.scrollIntoView({ block: 'center' });
          return { x: r.left + r.width/2, y: r.top + r.height/2 };
        }
      }
      return null;
    })()"#;
    let mut continued = false;
    if let Ok(res) = self.evaluate(continue_js, false).await {
      if let (Some(x), Some(y)) = (
        res
          .get("value")
          .and_then(|v| v.get("x"))
          .and_then(|n| n.as_f64()),
        res
          .get("value")
          .and_then(|v| v.get("y"))
          .and_then(|n| n.as_f64()),
      ) {
        self.mouse_click(x, y).await?;
        continued = true;
      }
    }
    if !continued {
      return Err("Could not click Continue on auth/login page".into());
    }
    sleep(std::time::Duration::from_millis(1500)).await;

    // Wait for navigation to auth.openai.com.
    for _ in 0..20 {
      sleep(std::time::Duration::from_millis(500)).await;
      let cur = self.current_url().await.unwrap_or_default();
      if cur.contains("auth.openai.com") {
        return Ok(());
      }
    }

    let cur = self.current_url().await.unwrap_or_default();
    if cur.contains("auth.openai.com") {
      Ok(())
    } else {
      Err(format!(
        "prepare_existing_account_login: did not reach auth.openai.com (url={cur})"
      ))
    }
  }

  /// Wipe cookies + origin storage so the browser looks brand-new (no choose-an-account residue).
  async fn clear_all_site_data(&mut self) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => {
        let _ = cdp.send_cmd("Network.enable", serde_json::json!({})).await;
        let _ = cdp
          .send_cmd("Network.clearBrowserCookies", serde_json::json!({}))
          .await;
        let _ = cdp
          .send_cmd("Network.clearBrowserCache", serde_json::json!({}))
          .await;
        let _ = cdp
          .send_cmd(
            "Storage.clearDataForOrigin",
            serde_json::json!({
              "origin": "https://chatgpt.com",
              "storageTypes": "all",
            }),
          )
          .await;
        let _ = cdp
          .send_cmd(
            "Storage.clearDataForOrigin",
            serde_json::json!({
              "origin": "https://auth.openai.com",
              "storageTypes": "all",
            }),
          )
          .await;
        Ok(())
      }
      Self::Camoufox { page, .. } => {
        let context = page.context();
        if let Err(e) = context.clear_cookies().await {
          return Err(format!("Camoufox clear_cookies failed: {e}"));
        }
        // Best-effort origin storage wipe on a blank page.
        let _: Result<bool, _> = page
          .eval(
            r#"(async () => {
              try { localStorage.clear(); } catch (_) {}
              try { sessionStorage.clear(); } catch (_) {}
              try {
                if (window.caches) {
                  const keys = await caches.keys();
                  await Promise.all(keys.map((k) => caches.delete(k)));
                }
              } catch (_) {}
              try {
                if (window.indexedDB && indexedDB.databases) {
                  const dbs = await indexedDB.databases();
                  await Promise.all((dbs || []).map((d) => d && d.name && indexedDB.deleteDatabase(d.name)));
                }
              } catch (_) {}
              return true;
            })()"#,
          )
          .await;
        Ok(())
      }
    }
  }

  async fn mouse_move(&mut self, x: f64, y: f64) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => cdp.mouse_move(x, y).await,
      Self::Camoufox {
        page, cursor_pos, ..
      } => {
        page
          .mouse
          .r#move(x, y, Some(1))
          .await
          .map_err(|e| format!("Camoufox mouse move failed: {e}"))?;
        *cursor_pos = (x, y);
        Ok(())
      }
    }
  }

  async fn mouse_click(&mut self, x: f64, y: f64) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => cdp.mouse_click(x, y).await,
      Self::Camoufox {
        page, cursor_pos, ..
      } => {
        page
          .mouse
          .click_builder(x, y)
          .click()
          .await
          .map_err(|e| format!("Camoufox mouse click failed: {e}"))?;
        *cursor_pos = (x, y);
        Ok(())
      }
    }
  }

  /// Get current tracked cursor position.
  fn cursor_pos(&self) -> (f64, f64) {
    match self {
      Self::Cdp(cdp) => cdp.cursor_pos,
      Self::Camoufox { cursor_pos, .. } => *cursor_pos,
    }
  }

  /// Humanized move along a curved path then left-click (service-agnostic).
  /// Automatically uses tracked cursor position as starting point.
  async fn human_click(
    &mut self,
    to: (f64, f64),
    profile: &crate::browser_actions::HumanProfile,
  ) -> Result<(), String> {
    let from = self.cursor_pos();
    self.human_click_from(from, to, profile).await
  }

  /// Humanized move along a curved path then left-click with explicit start position.
  async fn human_click_from(
    &mut self,
    from: (f64, f64),
    to: (f64, f64),
    profile: &crate::browser_actions::HumanProfile,
  ) -> Result<(), String> {
    use crate::browser_actions::{jitter_ms, mouse_path, think_delay};

    sleep(think_delay(profile)).await;
    let path = mouse_path(from, to, profile.mouse_steps);
    for (i, (x, y)) in path.iter().enumerate() {
      self.mouse_move(*x, *y).await?;
      if i + 1 < path.len() {
        sleep(jitter_ms(4, 18)).await;
      }
    }
    sleep(jitter_ms(25, 90)).await;
    self.mouse_click(to.0, to.1).await?;
    sleep(jitter_ms(40, 140)).await;
    Ok(())
  }

  async fn key_char(&mut self, ch: char) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => cdp.key_char(ch).await,
      Self::Camoufox { page, .. } => {
        // type() emits keydown/keypress/input/keyup for the character.
        let s = ch.to_string();
        page
          .keyboard
          .r#type(&s, Some(0.0))
          .await
          .map_err(|e| format!("Camoufox type char failed: {e}"))?;
        Ok(())
      }
    }
  }

  async fn key_backspace(&mut self) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => cdp.key_backspace().await,
      Self::Camoufox { page, .. } => {
        page
          .keyboard
          .press("Backspace", Some(20.0))
          .await
          .map_err(|e| format!("Camoufox backspace failed: {e}"))?;
        Ok(())
      }
    }
  }

  /// Focus element + type with Markov delays / occasional typos (via human_typing).
  async fn human_type(
    &mut self,
    selector: &str,
    text: &str,
    profile: &crate::browser_actions::HumanProfile,
  ) -> Result<(), String> {
    use crate::browser_actions::{post_type_delay, think_delay, typing_events, typing_step_delays};
    use crate::human_typing::TypingAction;

    // Focus + clear via JS (still need focus for real key events).
    let focus_js = format!(
      r#"(function(){{
        const el = document.querySelector({sel});
        if (!el) return {{ ok: false, reason: 'not_found' }};
        el.focus();
        el.click();
        try {{
          if (el.select) el.select();
          else if (typeof el.value === 'string') el.value = '';
        }} catch (_) {{}}
        const r = el.getBoundingClientRect();
        return {{ ok: true, x: r.left + r.width/2, y: r.top + r.height/2, w: r.width, h: r.height }};
      }})()"#,
      sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
    );
    let result = self.evaluate(&focus_js, false).await?;
    let value = result
      .get("value")
      .cloned()
      .ok_or_else(|| "human_type: no evaluate value".to_string())?;
    if value["ok"].as_bool() != Some(true) {
      return Err(format!(
        "human_type: {}",
        value["reason"].as_str().unwrap_or("failed")
      ));
    }

    sleep(think_delay(profile)).await;

    let events = typing_events(text, profile.wpm);
    let steps = typing_step_delays(&events);
    for (delay, action) in steps {
      sleep(delay).await;
      match action {
        TypingAction::Char(ch) => self.key_char(ch).await?,
        TypingAction::Backspace => self.key_backspace().await?,
      }
    }
    sleep(post_type_delay(profile)).await;

    // Fire input/change so React/controlled fields sync if needed.
    let fire_js = format!(
      r#"(function(){{
        const el = document.querySelector({sel});
        if (!el) return false;
        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
        return true;
      }})()"#,
      sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
    );
    let _ = self.evaluate(&fire_js, false).await;
    Ok(())
  }
}

#[async_trait::async_trait]
impl ChatGptBrowser for BrowserSession {
  async fn navigate(&mut self, url: &str, timeout_secs: u64) -> Result<(), String> {
    BrowserSession::navigate(self, url, timeout_secs).await
  }

  async fn evaluate(
    &mut self,
    expression: &str,
    await_promise: bool,
  ) -> Result<serde_json::Value, String> {
    BrowserSession::evaluate(self, expression, await_promise).await
  }

  async fn current_url(&mut self) -> Result<String, String> {
    BrowserSession::current_url(self).await
  }

  async fn type_text(&mut self, selector: &str, value: &str) -> Result<(), String> {
    if self
      .human_type(
        selector,
        value,
        &crate::browser_actions::HumanProfile::form_fill(),
      )
      .await
      .is_ok()
    {
      return Ok(());
    }

    let script = format!(
      r#"(function(){{
        const element = document.querySelector({selector});
        if (!element) return false;
        element.focus();
        const prototype = window.HTMLInputElement && window.HTMLInputElement.prototype;
        const descriptor = prototype && Object.getOwnPropertyDescriptor(prototype, 'value');
        if (descriptor && descriptor.set) descriptor.set.call(element, {value});
        else element.value = {value};
        element.dispatchEvent(new Event('input', {{ bubbles: true }}));
        element.dispatchEvent(new Event('change', {{ bubbles: true }}));
        return true;
      }})()"#,
      selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
      value = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
    );
    let result = BrowserSession::evaluate(self, &script, false).await?;
    if result.get("value").and_then(serde_json::Value::as_bool) == Some(true) {
      Ok(())
    } else {
      Err("browser input is unavailable".into())
    }
  }

  async fn click_point(&mut self, x: f64, y: f64) -> Result<(), String> {
    self
      .human_click((x, y), &crate::browser_actions::HumanProfile::careful())
      .await
  }
}

// ---------------------------------------------------------------------------
// CDP target discovery
// ---------------------------------------------------------------------------

async fn fetch_page_targets(port: u16) -> Result<serde_json::Value, String> {
  let url = format!("http://127.0.0.1:{port}/json");
  let resp = reqwest::get(&url)
    .await
    .map_err(|e| format!("Failed to fetch CDP targets: {e}"))?;
  let text = resp
    .text()
    .await
    .map_err(|e| format!("Failed to read CDP targets: {e}"))?;
  serde_json::from_str(&text).map_err(|e| format!("Invalid CDP target JSON: {e}"))
}

async fn get_page_ws_url(port: u16) -> Result<String, String> {
  let targets = fetch_page_targets(port).await?;
  let arr = targets.as_array().ok_or("CDP targets not an array")?;
  for t in arr {
    if t["type"].as_str() == Some("page") {
      if let Some(ws) = t["webSocketDebuggerUrl"].as_str() {
        return Ok(ws.to_string());
      }
    }
  }
  Err("No page target with webSocketDebuggerUrl found".into())
}

/// Attach the automation backend owned by an already-launched worker profile.
/// Chromium is resolved only by PID or the exact effective profile path;
/// Camoufox reuses the active Playwright page held by CamoufoxManager.
pub(crate) async fn attach_browser_session(
  profile: &crate::profile::BrowserProfile,
) -> Result<BrowserSession, String> {
  let profile_path = crate::ephemeral_dirs::get_effective_profile_path(
    profile,
    &crate::profile::ProfileManager::instance().get_profiles_dir(),
  );
  let profile_path_str = profile_path.to_string_lossy().to_string();

  if profile.browser == "camoufox" {
    let mut last_error = String::new();
    for attempt in 0..15 {
      if attempt > 0 {
        sleep(std::time::Duration::from_millis(500)).await;
      }
      match crate::camoufox_manager::CamoufoxManager::instance()
        .get_active_page(&profile_path_str)
        .await
      {
        Ok(page) => {
          return Ok(BrowserSession::Camoufox {
            page,
            cursor_pos: (0.0, 0.0),
          });
        }
        Err(error) => last_error = error.to_string(),
      }
    }
    return Err(format!(
      "Failed to attach Camoufox Playwright page for {profile_path_str}: {last_error}"
    ));
  }

  let manager = crate::chromium_manager::ChromiumManager::instance();
  for attempt in 0..20 {
    if attempt > 0 {
      sleep(std::time::Duration::from_millis(500)).await;
    }
    let cdp_port = if let Some(pid) = profile.process_id {
      manager.get_cdp_port_by_pid(pid).await
    } else {
      None
    }
    .or(manager.get_cdp_port(&profile_path_str).await);

    if let Some(port) = cdp_port {
      let ws_url = get_page_ws_url(port).await?;
      return CdpConnection::connect(&ws_url)
        .await
        .map(BrowserSession::Cdp);
    }
  }

  Err(format!(
    "Failed to get CDP port for browser={} path={} pid={:?}",
    profile.browser, profile_path_str, profile.process_id
  ))
}

// ---------------------------------------------------------------------------
// User info generation
// ---------------------------------------------------------------------------

const FIRST_NAMES: &[&str] = &[
  "James",
  "Robert",
  "John",
  "Michael",
  "David",
  "William",
  "Richard",
  "Joseph",
  "Thomas",
  "Charles",
  "Christopher",
  "Daniel",
  "Matthew",
  "Anthony",
  "Mark",
  "Donald",
  "Steven",
  "Paul",
  "Andrew",
  "Joshua",
  "Kenneth",
  "Kevin",
  "Brian",
  "George",
  "Timothy",
  "Ronald",
  "Edward",
  "Jason",
  "Jeffrey",
  "Ryan",
  "Jacob",
  "Gary",
  "Nicholas",
  "Eric",
  "Jonathan",
  "Stephen",
  "Larry",
  "Justin",
  "Scott",
  "Brandon",
  "Mary",
  "Patricia",
  "Jennifer",
  "Linda",
  "Barbara",
  "Elizabeth",
  "Susan",
  "Jessica",
  "Sarah",
  "Karen",
  "Lisa",
  "Nancy",
  "Betty",
  "Margaret",
  "Sandra",
  "Ashley",
  "Dorothy",
  "Kimberly",
  "Emily",
  "Donna",
  "Michelle",
  "Carol",
  "Amanda",
  "Melissa",
  "Deborah",
  "Stephanie",
  "Rebecca",
  "Sharon",
  "Laura",
  "Cynthia",
  "Kathleen",
  "Amy",
  "Angela",
  "Shirley",
  "Anna",
  "Brenda",
  "Pamela",
  "Emma",
  "Nicole",
  "Helen",
  "Samantha",
  "Katherine",
  "Christine",
  "Debra",
  "Rachel",
  "Carolyn",
  "Janet",
  "Catherine",
  "Maria",
  "Heather",
  "Diane",
  "Ruth",
  "Julie",
  "Olivia",
  "Joyce",
  "Virginia",
  "Victoria",
  "Kelly",
  "Lauren",
  "Christina",
  "Joan",
  "Evelyn",
  "Judith",
  "Megan",
  "Andrea",
  "Cheryl",
  "Hannah",
  "Jacqueline",
  "Martha",
  "Gloria",
  "Teresa",
  "Ann",
  "Sara",
  "Madison",
  "Frances",
  "Kathryn",
  "Janice",
  "Jean",
  "Abigail",
  "Alice",
];

const LAST_NAMES: &[&str] = &[
  "Smith",
  "Johnson",
  "Williams",
  "Brown",
  "Jones",
  "Garcia",
  "Miller",
  "Davis",
  "Rodriguez",
  "Martinez",
  "Hernandez",
  "Lopez",
  "Gonzalez",
  "Wilson",
  "Anderson",
  "Thomas",
  "Taylor",
  "Moore",
  "Jackson",
  "Martin",
  "Lee",
  "Perez",
  "Thompson",
  "White",
  "Harris",
  "Sanchez",
  "Clark",
  "Ramirez",
  "Lewis",
  "Robinson",
  "Walker",
  "Young",
  "Allen",
  "King",
  "Wright",
  "Scott",
  "Torres",
  "Nguyen",
  "Hill",
  "Flores",
  "Green",
  "Adams",
  "Nelson",
  "Baker",
  "Hall",
  "Rivera",
  "Campbell",
  "Mitchell",
  "Carter",
  "Roberts",
];

fn random_name() -> (String, String) {
  let mut rng = rand::rng();
  let first = FIRST_NAMES.choose(&mut rng).unwrap_or(&"James");
  let last = LAST_NAMES.choose(&mut rng).unwrap_or(&"Smith");
  (first.to_string(), last.to_string())
}

fn random_birthday() -> String {
  let mut rng = rand::rng();
  let year = 1996 + (rng.next_u32() % 11);
  let month = 1 + (rng.next_u32() % 12);
  let day = 1 + (rng.next_u32() % 28);
  format!("{year:04}-{month:02}-{day:02}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AboutYouBirthMode {
  Age,
  SingleDate,
  SplitDate,
}

fn resolve_about_you_birth_mode(
  has_age: bool,
  has_single_date: bool,
  has_split_date: bool,
) -> Option<AboutYouBirthMode> {
  if has_age {
    Some(AboutYouBirthMode::Age)
  } else if has_single_date {
    Some(AboutYouBirthMode::SingleDate)
  } else if has_split_date {
    Some(AboutYouBirthMode::SplitDate)
  } else {
    None
  }
}

fn birthdate_parts(birthdate: &str) -> Result<(String, String, String, String), String> {
  let date = NaiveDate::parse_from_str(birthdate, "%Y-%m-%d")
    .map_err(|_| "generated birthdate is invalid".to_string())?;
  let year = date.year().to_string();
  let month = format!("{:02}", date.month());
  let day = format!("{:02}", date.day());
  let age = Utc::now()
    .date_naive()
    .years_since(date)
    .ok_or_else(|| "generated birthdate is in the future".to_string())?
    .to_string();
  Ok((year, month, day, age))
}

#[cfg(test)]
mod about_you_tests {
  use super::{birthdate_parts, resolve_about_you_birth_mode, AboutYouBirthMode};
  use chrono::{Datelike, Utc};

  #[test]
  fn birth_mode_prefers_age_then_single_then_split_date() {
    assert_eq!(
      resolve_about_you_birth_mode(true, true, true),
      Some(AboutYouBirthMode::Age)
    );
    assert_eq!(
      resolve_about_you_birth_mode(false, true, true),
      Some(AboutYouBirthMode::SingleDate)
    );
    assert_eq!(
      resolve_about_you_birth_mode(false, false, true),
      Some(AboutYouBirthMode::SplitDate)
    );
    assert_eq!(resolve_about_you_birth_mode(false, false, false), None);
  }

  #[test]
  fn birthdate_parts_produces_age_for_live_age_form() {
    let (year, month, day, age) = birthdate_parts("2000-01-01").expect("valid birthdate");

    assert_eq!(year, "2000");
    assert_eq!(month, "01");
    assert_eq!(day, "01");
    assert_eq!(age, (Utc::now().year() - 2000).to_string());
  }

  #[test]
  fn birthdate_parts_rejects_invalid_and_future_dates() {
    assert!(birthdate_parts("2000-02-30").is_err());
    assert!(birthdate_parts("2999-01-01").is_err());
  }
}

#[cfg(test)]
mod safe_error_tests {
  //! The error envelope used to collapse every provider failure to a static
  //! `&'static str`, so a 400 "CDK use limit reached" and a DNS NXDOMAIN
  //! looked identical in `cdk_inventory.lastError`. Now the safe string keeps
  //! HTTP status / short message while redacting secrets.

  use super::{safe_email_service_error, safe_provider_detail};
  use crate::email::EmailServiceError;

  #[test]
  fn network_error_keeps_http_status_and_provider_message() {
    let inner = "redeem HTTP 400 Bad Request: {\"message\":\"CDK use limit reached\",\"error\":\"Bad Request\",\"statusCode\":400}";
    let safe = safe_email_service_error(&EmailServiceError::Network(inner.into()));
    assert!(
      safe.contains("provider network request failed"),
      "prefix preserved: {safe}"
    );
    assert!(safe.contains("HTTP 400"), "HTTP status preserved: {safe}");
    assert!(
      safe.contains("CDK use limit reached"),
      "provider message preserved: {safe}"
    );
  }

  #[test]
  fn network_error_strips_emails_and_cdk_strings() {
    let email = "someone@example.com";
    let cdk = "GMAIL-7BE8-AEVK-ACR8-FQ86";
    let inner = format!("redeem HTTP 400 Bad Request: {{\"message\":\"{cdk} used by {email}\"}}");
    let safe = safe_email_service_error(&EmailServiceError::Network(inner));
    assert!(!safe.contains(email), "email must be redacted: {safe}");
    assert!(!safe.contains(cdk), "raw CDK must be redacted: {safe}");
    assert!(safe.contains("[email]"), "email marker present: {safe}");
    assert!(safe.contains("[cdk]"), "cdk marker present: {safe}");
  }

  #[test]
  fn network_error_strips_mail_cards_from_raw_and_encoded_urls() {
    for detail in [
      "GET https://sms.iosmq.xyz/api/v1/order/lookup?code=MAIL-K4L5-EUW5-PHBV-A6KW&poll=true failed",
      "GET https://sms.iosmq.xyz/api/v1/order/lookup?code=MAIL%2DK4L5%2DEUW5%2DPHBV%2DA6KW&poll=true failed",
    ] {
      let safe = safe_email_service_error(&EmailServiceError::Network(detail.into()));
      assert!(!safe.contains("MAIL-K4L5-EUW5-PHBV-A6KW"), "safe={safe}");
      assert!(
        !safe.contains("MAIL%2DK4L5%2DEUW5%2DPHBV%2DA6KW"),
        "safe={safe}"
      );
      assert!(safe.contains("[cdk]"), "safe={safe}");
    }
  }

  #[test]
  fn network_error_redacts_opaque_token_values() {
    let token = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJK";
    let inner = format!("{{\"token\":\"{token}\",\"sessionId\":\"{token}\"}}");
    let safe = safe_provider_detail(&inner);
    assert!(
      !safe.contains(token),
      "opaque token must be redacted: {safe}"
    );
    assert!(
      safe.contains("[redacted]"),
      "redaction marker present: {safe}"
    );
  }

  #[test]
  fn network_error_truncates_giant_payloads() {
    let huge = "x".repeat(2000);
    let safe = safe_provider_detail(&huge);
    assert!(
      safe.chars().count() <= 220,
      "truncated to <=220 chars, got {}: {safe}",
      safe.chars().count()
    );
    assert!(safe.ends_with("..."), "ellipsis sentinel: {safe}");
  }

  #[test]
  fn cdik_invalid_and_timeout_preserve_classified_detail() {
    let invalid =
      safe_email_service_error(&EmailServiceError::CdkInvalid("status: revoked".into()));
    assert!(invalid.contains("provider card is invalid: status: revoked"));
    let timeout =
      safe_email_service_error(&EmailServiceError::Timeout("no code within 150s".into()));
    assert!(timeout.contains("provider request timed out: no code within 150s"));
  }
}

fn random_password() -> String {
  let mut rng = rand::rng();
  let lower: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
  let upper: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
  let digits: &[u8] = b"0123456789";
  let special: &[u8] = b"!@#$%";

  let pick = |rng: &mut rand::rngs::ThreadRng, slice: &[u8]| -> char {
    slice[(rng.next_u32() as usize) % slice.len()] as char
  };

  let mut chars = vec![
    pick(&mut rng, lower),
    pick(&mut rng, upper),
    pick(&mut rng, digits),
    pick(&mut rng, special),
  ];

  let all: &[&[u8]] = &[lower, upper, digits, special];
  for _ in 0..8 {
    let pool = all[(rng.next_u32() as usize) % all.len()];
    chars.push(pick(&mut rng, pool));
  }

  for i in (1..chars.len()).rev() {
    let j = (rng.next_u32() as usize) % (i + 1);
    chars.swap(i, j);
  }

  chars.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Page type detection (from URL)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum PageType {
  CreateAccountPassword,
  EmailOtpVerification,
  AboutYou,
  LoginPassword,
  AddPhone,
  Consent,
  ChatgptHome,
  Callback,
  ExternalUrl,
  ErrorPage,
  Unknown,
}

fn detect_page_type(url: &str) -> PageType {
  let u = url.to_lowercase();
  if u.contains("create-account/password") || u.contains("log-in-or-create-account") {
    // Both the password form and the unified login/create entry should start registration.
    PageType::CreateAccountPassword
  } else if u.contains("email-verification") || u.contains("email-otp") {
    PageType::EmailOtpVerification
  } else if u.contains("about-you") {
    PageType::AboutYou
  } else if u.contains("log-in/password") {
    PageType::LoginPassword
  } else if u.contains("add-phone") {
    PageType::AddPhone
  } else if u.contains("sign-in-with-chatgpt") && u.contains("consent") {
    PageType::Consent
  } else if u.contains("chatgpt.com") && (u.ends_with("chatgpt.com/") || u.ends_with("chatgpt.com"))
  {
    PageType::ChatgptHome
  } else if u.contains("callback") || u.contains("code=") {
    PageType::Callback
  } else if u.contains("/error") || u.contains("api/accounts/authorize") {
    PageType::ErrorPage
  } else if u.starts_with("http") && !u.contains("auth.openai.com") && !u.contains("chatgpt.com") {
    PageType::ExternalUrl
  } else {
    PageType::Unknown
  }
}

/// Strip values that may appear in provider error bodies (emails, tokens,
/// CDK strings, raw JSON values) so the resulting string is safe to persist in
/// `cdk_inventory.lastError` and logs. HTTP status + short `message` field are
/// kept because they never carry secrets and are what the operator needs to
/// tell "CDK use limit reached" (HTTP 400) apart from DNS / timeout / 5xx.
fn safe_provider_detail(raw: &str) -> String {
  // Pair of (compiled, replacement) applied left-to-right.
  let patterns: &[(&str, &str)] = &[
    // Email addresses inside provider payloads.
    (
      r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}",
      "[email]",
    ),
    // GMAIL/MAIL cards in raw or URL-encoded query strings.
    (r"(?i)GMAIL-[A-Z0-9]{4}(?:-[A-Z0-9]{4}){3}", "[cdk]"),
    (
      r"(?i)MAIL(?:-|%2D)[A-Z0-9]{4}(?:(?:-|%2D)[A-Z0-9]{4}){2,3}",
      "[cdk]",
    ),
    // `"token":"..."`, `"sessionId":"..."`, opaque JSON secrets.
    (
      r#"(?i)"(token|sessionId|apiKey|access_token|refresh_token)"\s*:\s*"[^"]+""#,
      r#""$1":"[redacted]""#,
    ),
    // Long opaque base64-ish blobs (>=32 chars).
    (r#"(?i)"[A-Za-z0-9+/=_\-]{32,}""#, "\"[redacted]\""),
  ];
  let mut cleaned = raw.to_string();
  for (pattern, replacement) in patterns {
    if let Ok(re) = regex_lite::Regex::new(pattern) {
      cleaned = re.replace_all(&cleaned, *replacement).to_string();
    }
  }
  // Bound the final string so a giant HTML error page can't flood the record.
  if cleaned.chars().count() > 220 {
    let truncated: String = cleaned.chars().take(217).collect();
    format!("{truncated}...")
  } else {
    cleaned
  }
}

fn safe_email_service_error(error: &EmailServiceError) -> String {
  match error {
    EmailServiceError::CdkInvalid(d) => {
      format!("provider card is invalid: {}", safe_provider_detail(d))
    }
    EmailServiceError::CdkExpired(d) => {
      format!("provider card is expired: {}", safe_provider_detail(d))
    }
    EmailServiceError::Timeout(d) => {
      format!("provider request timed out: {}", safe_provider_detail(d))
    }
    EmailServiceError::Cancelled => "provider polling cancelled".to_string(),
    EmailServiceError::Network(d) => format!(
      "provider network request failed: {}",
      safe_provider_detail(d)
    ),
    EmailServiceError::EmailInvalid(d) => {
      format!("provider email is invalid: {}", safe_provider_detail(d))
    }
    EmailServiceError::Internal(d) => {
      format!("provider operation failed: {}", safe_provider_detail(d))
    }
  }
}

fn safe_sms_service_error(error: &SmsServiceError) -> String {
  match error {
    SmsServiceError::Auth(_) => "provider authentication failed".to_string(),
    SmsServiceError::InsufficientBalance(_) => "provider balance is insufficient".to_string(),
    SmsServiceError::NoNumbersAvailable(_) => "provider has no numbers available".to_string(),
    SmsServiceError::ServiceUnavailable(_) => "provider service is unavailable".to_string(),
    SmsServiceError::LimitExceeded(d) => {
      format!("provider limit was exceeded: {}", safe_provider_detail(d))
    }
    SmsServiceError::Timeout(d) => {
      format!("provider request timed out: {}", safe_provider_detail(d))
    }
    SmsServiceError::Network(d) => format!(
      "provider network request failed: {}",
      safe_provider_detail(d)
    ),
    SmsServiceError::InvalidRequest(_) => "provider rejected the request".to_string(),
    SmsServiceError::Internal(_) => "provider operation failed".to_string(),
  }
}

/// Evaluate ChatGPT account/subscription JSON for free trial / free Plus offer.
/// Primary signal (verified live): accounts/check -> eligible_promo_campaigns.plus
/// with id/title like "plus-1-month-free" / "Try Plus free for 1 month".
fn evaluate_subscription_json(v: &serde_json::Value) -> Option<(bool, String, String)> {
  // Path 1: accounts/check shape with nested accounts map.
  if let Some(accounts) = v.get("accounts").and_then(|a| a.as_object()) {
    for (account_id, account_node) in accounts {
      if let Some(result) = evaluate_account_node(account_id, account_node) {
        return Some(result);
      }
    }
  }

  // Path 2: single account node / entitlement payload.
  if v.get("eligible_promo_campaigns").is_some() || v.get("entitlement").is_some() {
    if let Some(result) = evaluate_account_node("default", v) {
      return Some(result);
    }
  }

  // Path 3: subscriptions payload (when called with account_id).
  if let Some(plan) = v
    .get("plan_type")
    .or_else(|| v.pointer("/subscription/plan_type"))
    .and_then(|x| x.as_str())
  {
    let trial = v.get("trial").cloned();
    let will_renew = v.get("will_renew").and_then(|x| x.as_bool());
    let mut eligible = false;
    let mut reason = format!("subscriptions.plan_type={plan}");
    if let Some(t) = trial {
      if !t.is_null() {
        eligible = true;
        reason.push_str(&format!("; trial={t}"));
      }
    }
    if v.to_string().to_lowercase().contains("free trial") {
      eligible = true;
      reason.push_str("; free_trial_text");
    }
    return Some((
      eligible,
      plan.to_string(),
      format!("{reason}; will_renew={will_renew:?}"),
    ));
  }

  None
}

fn evaluate_account_node(
  _account_id: &str,
  account_node: &serde_json::Value,
) -> Option<(bool, String, String)> {
  let plan = account_node
    .pointer("/account/plan_type")
    .or_else(|| account_node.pointer("/entitlement/subscription_plan"))
    .and_then(|x| x.as_str())
    .unwrap_or("")
    .to_string();

  let promo = account_node.get("eligible_promo_campaigns").cloned();
  let offers = account_node.get("eligible_offers").cloned();
  let entitlement_trial = account_node.pointer("/entitlement/trial").cloned();
  let yearly_new = account_node
    .get("is_eligible_for_yearly_plus_new_user_subscription")
    .and_then(|x| x.as_bool())
    .unwrap_or(false);
  let has_paid = account_node
    .pointer("/account/has_previously_paid_subscription")
    .and_then(|x| x.as_bool())
    .unwrap_or(false);

  let mut eligible = false;
  let mut plan_type = if plan.is_empty() {
    "unknown".into()
  } else {
    plan.clone()
  };
  let mut reasons: Vec<String> = vec![format!("plan={plan}")];

  // PRIMARY: promo campaigns (verified live on a fresh free signup).
  if let Some(promo_obj) = promo.as_ref().and_then(|p| p.as_object()) {
    for (k, campaign) in promo_obj {
      let id = campaign.get("id").and_then(|x| x.as_str()).unwrap_or("");
      let title = campaign
        .pointer("/metadata/title")
        .and_then(|x| x.as_str())
        .unwrap_or("");
      let label = campaign
        .pointer("/metadata/promotion_type_label")
        .and_then(|x| x.as_str())
        .unwrap_or("");
      let summary = campaign
        .pointer("/metadata/summary")
        .and_then(|x| x.as_str())
        .unwrap_or("");
      let pct = campaign
        .pointer("/metadata/discount/percentage")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
      let plan_name = campaign
        .pointer("/metadata/plan_name")
        .and_then(|x| x.as_str())
        .unwrap_or("");

      let blob = format!("{id} {title} {label} {summary} {plan_name}").to_lowercase();
      let looks_free_trial = pct == 100
        || blob.contains("free trial")
        || blob.contains("try plus free")
        || blob.contains("1-month free")
        || blob.contains("plus-1-month-free")
        || (blob.contains("free") && blob.contains("plus"));

      reasons.push(format!(
        "promo[{k}]=id:{id};label:{label};title:{title};pct:{pct};plan:{plan_name}"
      ));

      if looks_free_trial {
        eligible = true;
        if !plan_name.is_empty() {
          plan_type = format!("{plan_name}/trial");
        } else if !id.is_empty() {
          plan_type = id.to_string();
        } else {
          plan_type = "free-trial".into();
        }
      }
    }
  } else {
    reasons.push("promo=none".into());
  }

  // Active trial object on entitlement (already in trial).
  if let Some(trial) = entitlement_trial {
    if !trial.is_null() {
      eligible = true;
      reasons.push(format!("entitlement.trial={trial}"));
      if plan_type == plan || plan_type == "unknown" {
        plan_type = format!("{plan}/active-trial");
      }
    }
  }

  if yearly_new {
    reasons.push("yearly_plus_new_user_eligible=true".into());
  }
  if !has_paid {
    reasons.push("never_paid=true".into());
  }

  // Offers list is not free-trial proof by itself (always lists plus/pro SKUs).
  if let Some(default_offer) = offers
    .as_ref()
    .and_then(|o| o.get("default_offer_id"))
    .and_then(|x| x.as_str())
  {
    reasons.push(format!("default_offer={default_offer}"));
  }

  Some((eligible, plan_type, reasons.join("; ")))
}

fn is_cloudflare_block(url: &str) -> bool {
  let u = url.to_lowercase();
  u.contains("api/accounts/authorize")
    || u.contains("/error")
    || u.contains("just a moment")
    || u.contains("cloudflare")
}

// ---------------------------------------------------------------------------
// Registration Engine
// ---------------------------------------------------------------------------

struct AccountIdentity {
  alias_email: String,
  password: String,
  first_name: String,
  last_name: String,
  birthdate: String,
  device_id: String,
}

impl AccountIdentity {
  fn new(alias_email: String) -> Self {
    let (first_name, last_name) = random_name();
    Self {
      alias_email,
      password: random_password(),
      first_name,
      last_name,
      birthdate: random_birthday(),
      device_id: Uuid::new_v4().to_string(),
    }
  }
}

pub struct RegistrationEngine {
  config: RegistrationConfig,
  cancel_flag: Arc<AtomicBool>,
  task_id: String,
  device_id: String,
  logs: Vec<String>,
  /// Reused worker profile id for this engine instance (one per concurrent CDK slot).
  worker_profile_id: Option<String>,
  /// True when this engine created the worker and must delete it when the CDK finishes.
  owns_worker_profile: bool,
  /// Optional CDK index suffix for concurrent worker profile names.
  worker_slot: u32,
  /// Ephemeral WireGuard conf ids spawned for multi-peer concurrency (batch-owned).
  ephemeral_vpn_ids: Vec<String>,
  /// Slot-local VPN override (from ephemeral pool); takes precedence over config.vpn_id.
  slot_vpn_id: Option<String>,
}

impl RegistrationEngine {
  #[allow(dead_code)]
  pub fn new(config: RegistrationConfig) -> Self {
    Self {
      config,
      cancel_flag: Arc::new(AtomicBool::new(false)),
      task_id: Uuid::new_v4().to_string(),
      device_id: Uuid::new_v4().to_string(),
      logs: Vec::new(),
      worker_profile_id: None,
      owns_worker_profile: false,
      worker_slot: 0,
      ephemeral_vpn_ids: Vec::new(),
      slot_vpn_id: None,
    }
  }

  pub fn with_cancel_flag(config: RegistrationConfig, cancel_flag: Arc<AtomicBool>) -> Self {
    Self {
      config,
      cancel_flag,
      task_id: Uuid::new_v4().to_string(),
      device_id: Uuid::new_v4().to_string(),
      logs: Vec::new(),
      worker_profile_id: None,
      owns_worker_profile: false,
      worker_slot: 0,
      ephemeral_vpn_ids: Vec::new(),
      slot_vpn_id: None,
    }
  }

  /// Fork a per-CDK engine that shares cancel/task/config but owns its own worker + logs.
  /// When an ephemeral peer pool exists, assign `vpn_ids[worker_slot % len]` to this slot.
  fn fork_for_cdk(&self, worker_slot: u32) -> Self {
    let slot_vpn_id = if !self.ephemeral_vpn_ids.is_empty() {
      let idx = (worker_slot as usize) % self.ephemeral_vpn_ids.len();
      Some(self.ephemeral_vpn_ids[idx].clone())
    } else {
      self.slot_vpn_id.clone()
    };
    Self {
      config: self.config.clone(),
      cancel_flag: self.cancel_flag.clone(),
      task_id: self.task_id.clone(),
      device_id: Uuid::new_v4().to_string(),
      logs: Vec::new(),
      worker_profile_id: None,
      owns_worker_profile: false,
      worker_slot,
      ephemeral_vpn_ids: Vec::new(), // only root owns cleanup
      slot_vpn_id,
    }
  }

  /// VPN id for this engine/slot: ephemeral pool assignment, else inventory vpn_id.
  fn worker_vpn_id(&self) -> Option<String> {
    self
      .slot_vpn_id
      .clone()
      .or_else(|| self.config.effective_vpn_id())
  }

  /// Build N ephemeral Nord WireGuard confs from the base inventory conf's private key.
  async fn spawn_vpn_peer_pool(&mut self, pool_size: usize) -> Result<(), String> {
    let base_id = self
      .config
      .effective_vpn_id()
      .ok_or_else(|| "VPN mode requires vpnId to spawn peer pool".to_string())?;
    let base_conf = {
      let storage = crate::vpn::VPN_STORAGE
        .lock()
        .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
      storage
        .load_config(&base_id)
        .map_err(|e| format!("Load base VPN for peer pool: {e}"))?
        .config_data
    };
    let private_key = crate::vpn::extract_wireguard_private_key(&base_conf)?;
    let prefix = format!(
      "auto-reg-ephemeral-{}",
      &self.task_id[..8.min(self.task_id.len())]
    );

    // Resolve preferred Nord country (UI default: "Japan") so peer pool egress
    // matches the OpenAI-friendly location selected in the dialog. Without this,
    // `country_id=None` returns any Nord server and ChatGPT region-blocks VN egress.
    let country_id = if let Some(group) = self
      .config
      .nord_group
      .as_deref()
      .map(str::trim)
      .filter(|s| !s.is_empty())
    {
      // `nord_group` may be a country name ("Japan") or a VPN display name
      // ("Nord · Japan #42"); infer_country_code_from_vpn_name handles both.
      let code = crate::vpn::infer_country_code_from_vpn_name(group)
        .or_else(|| crate::vpn::infer_country_code_from_vpn_name(&format!("Nord · {group}")));
      match code.as_deref() {
        Some(code) => match crate::vpn::list_nord_countries().await {
          Ok(countries) => crate::vpn::resolve_country_id_by_code(&countries, code),
          Err(e) => {
            self.log(&format!(
              "WARN: failed to list Nord countries for group {group:?}: {e}; spawning pool without country filter"
            ));
            None
          }
        },
        None => {
          self.log(&format!(
            "WARN: nord_group {group:?} did not resolve to a known country code; spawning pool without country filter"
          ));
          None
        }
      }
    } else {
      None
    };

    let ids =
      crate::vpn::spawn_ephemeral_nord_peer_pool(&private_key, pool_size, country_id, &prefix)
        .await?;
    self.log(&format!(
      "Spawned ephemeral Nord peer pool: {} conf(s) for concurrency (country_id={:?})",
      ids.len(),
      country_id
    ));
    for (i, id) in ids.iter().enumerate() {
      self.log(&format!("  peer pool[{i}] vpn_id={id}"));
    }
    self.ephemeral_vpn_ids = ids;
    Ok(())
  }

  async fn cleanup_ephemeral_vpn_pool(&mut self) {
    if self.ephemeral_vpn_ids.is_empty() {
      return;
    }
    let ids = std::mem::take(&mut self.ephemeral_vpn_ids);
    for id in ids {
      let _ = crate::vpn_worker_runner::stop_vpn_worker_by_vpn_id(&id).await;
      if let Ok(storage) = crate::vpn::VPN_STORAGE.lock() {
        if let Err(e) = storage.delete_config(&id) {
          self.log(&format!(
            "WARN: failed to delete ephemeral VPN conf {id}: {e}"
          ));
        } else {
          self.log(&format!("Deleted ephemeral VPN conf {id}"));
        }
      }
    }
  }

  pub fn task_id(&self) -> &str {
    &self.task_id
  }

  #[allow(dead_code)]
  pub fn cancel(&self) {
    self.cancel_flag.store(true, Ordering::SeqCst);
  }

  fn is_cancelled(&self) -> bool {
    self.cancel_flag.load(Ordering::SeqCst)
  }

  fn log(&mut self, msg: &str) {
    let ts = Utc::now().format("%H:%M:%S").to_string();
    self.logs.push(format!("[{ts}] {msg}"));
  }

  /// Mid-batch WireGuard peer hop: keep PrivateKey, pick a new Nord peer, rewrite
  /// inventory conf, restart vpn-worker so the next launch gets a new egress IP.
  async fn rotate_wireguard_peer(&mut self, vpn_id: &str) -> Result<(String, String), String> {
    let (conf, name) = {
      let storage = crate::vpn::VPN_STORAGE
        .lock()
        .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
      let cfg = storage
        .load_config(vpn_id)
        .map_err(|e| format!("Load VPN config for rotate: {e}"))?;
      (cfg.config_data, cfg.name)
    };

    let private_key = crate::vpn::extract_wireguard_private_key(&conf)?;
    let avoid_station = crate::vpn::extract_wireguard_peer_endpoint_host(&conf);
    let avoid_pk = crate::vpn::extract_wireguard_peer_public_key(&conf);
    let preferred_code = crate::vpn::infer_country_code_from_vpn_name(&name);
    let country_id = if let Some(code) = preferred_code.as_deref() {
      match crate::vpn::list_nord_countries().await {
        Ok(countries) => crate::vpn::resolve_country_id_by_code(&countries, code),
        Err(_) => None,
      }
    } else {
      None
    };

    let (server, new_conf) = crate::vpn::build_rotated_nord_wireguard_conf(
      &private_key,
      avoid_station.as_deref(),
      avoid_pk.as_deref(),
      country_id,
    )
    .await?;

    {
      let storage = crate::vpn::VPN_STORAGE
        .lock()
        .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
      storage
        .update_config_data(
          vpn_id,
          &new_conf,
          Some(&crate::vpn::default_nord_vpn_name(&server)),
        )
        .map_err(|e| format!("Save rotated VPN config: {e}"))?;
    }

    // Drop old tunnel so the next profile launch starts a worker on the new conf.
    let _ = crate::vpn_worker_runner::stop_vpn_worker_by_vpn_id(vpn_id).await;
    self.human_pause(800, 1200).await;

    match crate::vpn_worker_runner::start_vpn_worker(vpn_id).await {
      Ok(worker) => {
        self.log(&format!(
          "WireGuard worker restarted on {} (port {:?})",
          server.hostname, worker.local_port
        ));
      }
      Err(e) => {
        self.log(&format!(
          "WARN: vpn-worker restart after peer rotate failed (will retry on launch): {e}"
        ));
      }
    }

    Ok((server.hostname, server.station.clone()))
  }

  /// Switch this slot's Nord inventory conf to an allowlisted country after region blocks.
  async fn fallback_nord_location_on_region_block(
    &mut self,
    error_message: &str,
    location_fallbacks: &mut u32,
    tried_locations: &mut Vec<String>,
  ) -> Result<bool, String> {
    if !crate::vpn::is_unsupported_region_error(error_message) {
      return Ok(false);
    }
    if *location_fallbacks >= crate::vpn::MAX_NORD_LOCATION_FALLBACKS {
      return Ok(false);
    }
    let Some(vpn_id) = self.worker_vpn_id() else {
      return Ok(false);
    };

    let current_name = {
      let storage = crate::vpn::VPN_STORAGE
        .lock()
        .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
      storage
        .load_config(&vpn_id)
        .map(|c| c.name)
        .unwrap_or_default()
    };
    let current_code = crate::vpn::infer_country_code_from_vpn_name(&current_name);
    let Some(next_code) =
      crate::vpn::next_fallback_country_code(current_code.as_deref(), tried_locations)
    else {
      self.log("Unsupported region detected; no remaining Nord fallback locations");
      return Ok(false);
    };

    self.log(&format!(
      "Unsupported region detected; switching Nord location {} → {next_code}",
      current_code.as_deref().unwrap_or("?")
    ));
    let server = crate::vpn::retarget_nord_vpn_to_country(&vpn_id, next_code).await?;
    tried_locations.push(next_code.to_string());
    *location_fallbacks += 1;
    self.log(&format!(
      "Nord location fallback #{location_fallbacks}: {} ({}) station={}",
      server.hostname,
      server.country_code.as_deref().unwrap_or(next_code),
      server.station
    ));
    Ok(true)
  }

  #[allow(clippy::too_many_arguments)]
  fn emit(
    &self,
    app_handle: &tauri::AppHandle,
    step: RegistrationStep,
    message: &str,
    cdk_index: u32,
    alias_index: u32,
    total_cdks: u32,
    result: Option<RegistrationResult>,
  ) {
    let payload = RegistrationProgress {
      task_id: self.task_id.clone(),
      cdk_index,
      alias_index,
      total_cdks,
      step,
      message: message.to_string(),
      timestamp: Utc::now(),
      result,
    };
    let _ = app_handle.emit("registration-progress", payload);
  }

  #[allow(dead_code)]
  fn emit_step(&self, app_handle: &tauri::AppHandle, step: RegistrationStep, msg: &str) {
    self.emit(app_handle, step, msg, 0, 0, 0, None);
  }

  // -----------------------------------------------------------------------
  // Main entry point — concurrent CDK workers (1 CDK = 1 task)
  // -----------------------------------------------------------------------

  pub async fn run(
    &mut self,
    app_handle: tauri::AppHandle,
    email_service: &dyn EmailService,
    sms_service: Option<&dyn SmsService>,
  ) -> RegistrationResult {
    let total_cdks = self.config.cdks.len() as u32;
    let accounts_per = self.config.effective_accounts_per_cdk();
    let max_retries = self.config.max_retries.max(1);
    let cdks = self.config.cdks.clone();

    // Retry-cap flow: clamp the per-CDK request to the CDK's remaining capacity
    // so a partial run can be retried without tripping quota validation. CDKs
    // already at `MAX_ACCOUNTS_PER_CDK` used slots are skipped (the user must
    // delete that CDK's stats row to wipe its usage ledger and start fresh).
    let mut per_cdk_target: Vec<u32> = Vec::with_capacity(cdks.len());
    for cdk in &cdks {
      let remaining = cdk_remaining_capacity(cdk);
      if remaining == 0 {
        self.log(&format!(
          "CDK entry is at max capacity ({MAX_ACCOUNTS_PER_CDK}/{MAX_ACCOUNTS_PER_CDK} used+reserved); skipping - free slots only after Delete (full stats+quota reset) or when failed claims release usage"
        ));
        per_cdk_target.push(0);
        continue;
      }
      let target = accounts_per.min(remaining);
      if target < accounts_per {
        self.log(&format!(
          "CDK entry retrying - capping request {accounts_per} -> {target} ({} of {MAX_ACCOUNTS_PER_CDK} slots already used)",
          MAX_ACCOUNTS_PER_CDK - remaining
        ));
      }
      per_cdk_target.push(target);
    }
    if per_cdk_target.iter().all(|&t| t == 0) {
      let msg = format!(
        "All CDKs are at max capacity ({MAX_ACCOUNTS_PER_CDK}/{MAX_ACCOUNTS_PER_CDK} used+reserved); nothing left to schedule — use Top-up only when remaining > 0, or Delete a CDK stats row for a full quota reset"
      );
      self.log(&msg);
      self.emit(
        &app_handle,
        RegistrationStep::Failed,
        &msg,
        0,
        0,
        total_cdks,
        None,
      );
      return self.fail_result(&msg);
    }
    let targets: Vec<(String, u32)> = cdks
      .iter()
      .zip(per_cdk_target.iter())
      .filter(|(_, &t)| t > 0)
      .map(|(c, &t)| (c.clone(), t))
      .collect();
    let mut reserved_iter = match reserve_cdk_slots_per(&targets, &self.task_id) {
      Ok(reservations) => reservations.into_iter(),
      Err(_error) => {
        let msg = "CDK/card quota validation failed".to_string();
        self.log(&msg);
        self.emit(
          &app_handle,
          RegistrationStep::Failed,
          &msg,
          0,
          0,
          total_cdks,
          None,
        );
        return self.fail_result(&msg);
      }
    };
    let mut reservations: Vec<Option<CdkSlotReservation>> = Vec::with_capacity(cdks.len());
    for &target in &per_cdk_target {
      if target > 0 {
        reservations.push(Some(
          reserved_iter
            .next()
            .expect("reservation count must match non-zero target count"),
        ));
      } else {
        reservations.push(None);
      }
    }
    let mut reservations = reservations;
    let mut last_error = String::new();
    let mut nord_connected_by_us = false;

    let cli = self.config.nord_cli_path.clone();
    let group = self.config.nord_group.clone();
    let server = self.config.nord_server_name.clone();
    let rotate_every_n = self.config.rotate_every_n;
    let network_mode = self.config.network_mode;
    let uses_nord_cli = self.config.uses_nord_cli();

    // Concurrency policy:
    // - Nord CLI: always 1 (system-wide).
    // - VPN: user-selected concurrency capped by the detected Nord session budget.
    //   Peer pool size follows effective concurrency; CDKs drain in waves until done.
    let mut concurrency = self.config.concurrency.clamp(1, 8);
    if uses_nord_cli && concurrency > 1 {
      self.log(&format!(
        "Nord CLI mode: forcing concurrency 1 (was {concurrency}) — system-wide IP is not thread-safe"
      ));
      concurrency = 1;
    }

    if network_mode == NetworkMode::Vpn {
      // Auto-detect session budget from the selected Nord/WireGuard inventory config.
      // User only provides Access Token when creating the VPN — no manual session input.
      if let Some(vpn_id) = self.config.effective_vpn_id() {
        if let Ok(storage) = crate::vpn::VPN_STORAGE.lock() {
          if let Ok(cfg) = storage.load_config(&vpn_id) {
            if let Some(ms) = cfg.max_sessions.filter(|n| *n >= 1) {
              self.config.nord_max_sessions = ms.clamp(1, 6);
              self.log(&format!(
                "Nord session budget auto-detected from VPN config: {ms}"
              ));
            } else if cfg.source.as_deref() == Some("nord") {
              // Nord-created conf without stored budget: conservative default already on config.
              self.log(
                "Nord VPN config has no stored session budget; using default nord_max_sessions",
              );
            }
          }
        }
      }
      let requested_concurrency = self.config.concurrency;
      concurrency = self.config.effective_vpn_concurrency();
      self.log(&format!(
        "VPN concurrency selected by operator: requested={requested_concurrency}, session budget={}, effective={concurrency} (CDK count={total_cdks} does not limit concurrency)",
        self.config.nord_max_sessions
      ));

      if let Some(base) = self.config.effective_vpn_id() {
        self.log(&format!(
          "Network mode VPN (WireGuard): base vpn_id={base}; rotate every {rotate_every_n} success(es) per slot"
        ));
      }

      // Pool size = Nord-capped concurrency only (independent of CDK count).
      let pool_size = concurrency.clamp(1, 6) as usize;
      if let Err(e) = self.spawn_vpn_peer_pool(pool_size).await {
        let msg = format!("Failed to spawn ephemeral Nord peer pool: {e}");
        self.log(&msg);
        self.emit(
          &app_handle,
          RegistrationStep::Failed,
          &msg,
          0,
          0,
          total_cdks,
          None,
        );
        return self.fail_result(&msg);
      }
      // If API returned fewer usable peers, shrink concurrency to pool size.
      let pool_len = self.ephemeral_vpn_ids.len().max(1) as u32;
      if concurrency > pool_len {
        self.log(&format!(
          "VPN peer pool size {pool_len} < Nord-capped concurrency {concurrency}; capping concurrency to pool"
        ));
        concurrency = pool_len;
      }
      self.log(&format!(
        "VPN peer pool ready: {pool_len} concurrent Nord session(s); will drain all {total_cdks} CDK(s) in waves"
      ));
    }

    self.log(&format!(
      "Starting batch: {total_cdks} CDK(s), {accounts_per} account(s)/CDK, concurrency={concurrency}"
    ));

    // Nord CLI is backup/legacy only — system-wide connect when mode is Nord.
    if uses_nord_cli {
      self.emit(
        &app_handle,
        RegistrationStep::RotatingIp,
        "Connecting NordVPN CLI (system-wide backup mode)...",
        0,
        0,
        total_cdks,
        None,
      );
      match crate::auto_service::common::nord_cli::connect(
        cli.as_deref(),
        group.as_deref(),
        server.as_deref(),
      ) {
        Ok(()) => {
          nord_connected_by_us = true;
          self.log("NordVPN CLI connected (backup mode)");
          self.human_pause(2500, 3500).await;
          if let Ok(ip) = crate::ip_utils::fetch_public_ip(None).await {
            self.log(&format!("Egress IP after Nord CLI connect: {ip}"));
          }
        }
        Err(e) => {
          let msg = format!("NordVPN CLI connect failed: {e}");
          self.log(&msg);
          self.emit(
            &app_handle,
            RegistrationStep::Failed,
            &msg,
            0,
            0,
            total_cdks,
            None,
          );
          return self.fail_result(&msg);
        }
      }
    }

    // Sequential path (concurrency=1): keep Nord rotate semantics on one engine.
    if concurrency == 1 {
      let mut all_results: Vec<RegistrationResult> = Vec::new();
      let success_count = AtomicU32::new(0);
      for (cdk_idx, cdk) in cdks.iter().enumerate() {
        if self.is_cancelled() {
          self.log("Cancelled by user");
          break;
        }
        let Some(reservation) = reservations[cdk_idx].take() else {
          // Skipped during retry-cap flow — CDK is already at max capacity.
          continue;
        };
        let target = per_cdk_target[cdk_idx];
        let mut slot = self.fork_for_cdk(cdk_idx as u32);
        let (results, err, nord_stop) = slot
          .process_one_cdk(
            &app_handle,
            email_service,
            sms_service,
            cdk,
            reservation,
            cdk_idx as u32,
            total_cdks,
            target,
            max_retries,
            uses_nord_cli,
            rotate_every_n,
            cli.as_deref(),
            group.as_deref(),
            server.as_deref(),
            &success_count,
          )
          .await;
        self.logs.extend(slot.logs);
        all_results.extend(results);
        if let Some(e) = err {
          last_error = e;
        }
        if nord_stop {
          nord_connected_by_us = true;
          break;
        }
      }

      if nord_connected_by_us {
        self.log("NordVPN CLI left connected after auto-reg (no auto-disconnect)");
      }

      let ok = all_results.iter().filter(|r| r.success).count();
      let fail = all_results.iter().filter(|r| !r.success).count();
      let free_no = all_results
        .iter()
        .filter(|r| !r.success && !r.email.is_empty() && !r.free_trial_eligible)
        .count();
      let msg = if ok > 0 || free_no > 0 {
        format!("Done: {ok} free-trial, {free_no} no-trial, {fail} total non-success")
      } else if !last_error.is_empty() {
        last_error.clone()
      } else {
        "No accounts created".into()
      };

      self.emit(
        &app_handle,
        RegistrationStep::Completed,
        &msg,
        0,
        0,
        total_cdks,
        None,
      );

      self.cleanup_ephemeral_vpn_pool().await;

      return RegistrationResult {
        success: ok > 0,
        email: String::new(),
        password: String::new(),
        account_id: format!("batch:{ok}"),
        access_token: String::new(),
        device_id: String::new(),
        error_message: if fail > 0 {
          format!("{fail} non-success")
        } else {
          String::new()
        },
        step_logs: self.logs.clone(),
        created_at: Utc::now(),
        two_fa_enabled: false,
        totp_secret: String::new(),
        free_trial_eligible: false,
        plan_type: String::new(),
        cdk: format!("{total_cdks} CDKs processed"),
        base_email: String::new(),
        phone_number: String::new(),
        status: super::types::AccountInventoryStatus::Available,
        note: String::new(),
        exported_at: None,
        sold_at: None,
        email_provider: Some(self.config.email_provider),
        email_provider_provenance: Some(EmailProviderProvenance::RegistrationConfig),
        registration_outcome_reason: Some(RegistrationOutcomeReason::BatchSummary),
        two_factor_backfill_access_state: Some(TwoFactorBackfillAccessState::Accessible),
        two_factor_backfill_exclusion: None,
        two_factor_backfill_state: None,
        two_factor_backfill_outcome: None,
        two_factor_backfill_operation_id: None,
        record_revision: 1,
      };
    }

    // Parallel path: one concurrent future per CDK, limited by semaphore.
    // Poll on this task via FuturesUnordered (engine futures are !Send — no JoinSet).
    let semaphore = Arc::new(Semaphore::new(concurrency as usize));
    let success_count = Arc::new(AtomicU32::new(0));
    let sms_token = self.config.sms_token.clone();
    let sms_enabled = sms_service.is_some();

    #[allow(clippy::type_complexity)]
    let mut tasks: FuturesUnordered<
      std::pin::Pin<
        Box<
          dyn std::future::Future<
              Output = (u32, Vec<RegistrationResult>, Vec<String>, Option<String>),
            > + '_,
        >,
      >,
    > = FuturesUnordered::new();

    for (cdk_idx, cdk) in cdks.into_iter().enumerate() {
      if self.is_cancelled() {
        break;
      }
      let Some(reservation) = reservations[cdk_idx].take() else {
        // Skipped during retry-cap flow — CDK is already at max capacity.
        continue;
      };
      let target = per_cdk_target[cdk_idx];
      let semaphore = semaphore.clone();
      let mut slot = self.fork_for_cdk(cdk_idx as u32);
      let app = app_handle.clone();
      let success_count = success_count.clone();
      let sms_token = sms_token.clone();
      let email_provider = self.config.email_provider;
      tasks.push(Box::pin(async move {
        let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
        let email = crate::email::build_email_service(email_provider);
        let viotp = if sms_enabled {
          sms_token.map(crate::sms::viotp::ViotpService::new)
        } else {
          None
        };
        let sms_ref: Option<&dyn SmsService> = viotp.as_ref().map(|s| s as &dyn SmsService);
        // Per-slot WireGuard peer rotate is OK (each slot has its own ephemeral vpn_id).
        // Nord CLI remains disabled under concurrency > 1.
        let (results, err, _nord_stop) = slot
          .process_one_cdk(
            &app,
            email.as_ref(),
            sms_ref,
            &cdk,
            reservation,
            cdk_idx as u32,
            total_cdks,
            target,
            max_retries,
            false,          // Nord CLI never under concurrency > 1
            rotate_every_n, // WireGuard per-slot peer hop still allowed
            None,
            None,
            None,
            &success_count,
          )
          .await;
        (cdk_idx as u32, results, slot.logs, err)
      }));
    }

    let mut all_results: Vec<RegistrationResult> = Vec::new();
    while let Some((_idx, results, logs, err)) = tasks.next().await {
      self.logs.extend(logs);
      all_results.extend(results);
      if let Some(e) = err {
        last_error = e;
      }
    }

    self.cleanup_ephemeral_vpn_pool().await;

    if nord_connected_by_us {
      self.log("NordVPN CLI left connected after auto-reg (no auto-disconnect)");
    }

    let ok = all_results.iter().filter(|r| r.success).count();
    let fail = all_results.iter().filter(|r| !r.success).count();
    let free_no = all_results
      .iter()
      .filter(|r| !r.success && !r.email.is_empty() && !r.free_trial_eligible)
      .count();
    let msg = if ok > 0 || free_no > 0 {
      format!("Done: {ok} free-trial, {free_no} no-trial, {fail} total non-success")
    } else if !last_error.is_empty() {
      last_error.clone()
    } else {
      "No accounts created".into()
    };

    self.emit(
      &app_handle,
      RegistrationStep::Completed,
      &msg,
      0,
      0,
      total_cdks,
      None,
    );

    RegistrationResult {
      success: ok > 0,
      email: String::new(),
      password: String::new(),
      account_id: format!("batch:{ok}"),
      access_token: String::new(),
      device_id: String::new(),
      error_message: if fail > 0 {
        format!("{fail} non-success")
      } else {
        String::new()
      },
      step_logs: self.logs.clone(),
      created_at: Utc::now(),
      two_fa_enabled: false,
      totp_secret: String::new(),
      free_trial_eligible: false,
      plan_type: String::new(),
      cdk: format!("{total_cdks} CDKs processed"),
      base_email: String::new(),
      phone_number: String::new(),
      status: super::types::AccountInventoryStatus::Available,
      note: String::new(),
      exported_at: None,
      sold_at: None,
      email_provider: Some(self.config.email_provider),
      email_provider_provenance: Some(EmailProviderProvenance::RegistrationConfig),
      registration_outcome_reason: Some(RegistrationOutcomeReason::BatchSummary),
      two_factor_backfill_access_state: Some(TwoFactorBackfillAccessState::Accessible),
      two_factor_backfill_exclusion: None,
      two_factor_backfill_state: None,
      two_factor_backfill_outcome: None,
      two_factor_backfill_operation_id: None,
      record_revision: 1,
    }
  }

  /// Process a single CDK: redeem → N aliases sequential → update CDK inventory.
  /// Returns (results, last_error, nord_hard_stop).
  #[allow(clippy::too_many_arguments)]
  async fn process_one_cdk(
    &mut self,
    app_handle: &tauri::AppHandle,
    email_service: &dyn EmailService,
    sms_service: Option<&dyn SmsService>,
    cdk: &str,
    mut reservation: CdkSlotReservation,
    cdk_idx: u32,
    total_cdks: u32,
    accounts_per: u32,
    max_retries: u32,
    uses_nord_cli: bool,
    rotate_every_n: u32,
    nord_cli: Option<&str>,
    nord_group: Option<&str>,
    nord_server: Option<&str>,
    success_count: &AtomicU32,
  ) -> (Vec<RegistrationResult>, Option<String>, bool) {
    let mut all_results: Vec<RegistrationResult> = Vec::new();
    let mut last_error: Option<String> = None;
    let mut nord_hard_stop = false;

    let mut cdk_record =
      get_cdk_inventory(cdk).unwrap_or_else(|| CdkInventoryRecord::new(cdk, 0, &self.task_id));
    cdk_record.cdk = cdk.trim().to_ascii_uppercase();
    cdk_record.target_accounts = cdk_record.target_accounts.saturating_add(accounts_per);
    cdk_record.task_id.clone_from(&self.task_id);
    cdk_record.status = "running".into();
    cdk_record.last_error.clear();
    cdk_record.updated_at = Utc::now();
    put_cdk_inventory_record(&cdk_record);

    self.log(&format!("=== CDK {}/{total_cdks} ===", cdk_idx + 1));
    self.emit(
      app_handle,
      RegistrationStep::RedeemingCdk,
      &format!("CDK {}/{}: redeeming...", cdk_idx + 1, total_cdks),
      cdk_idx,
      0,
      total_cdks,
      None,
    );

    let base_email = match email_service.redeem_cdk(cdk) {
      Ok(info) => {
        let email = info.email.clone();
        self.log("CDK redeemed");
        cdk_record.base_email = email.clone();
        put_cdk_inventory_record(&cdk_record);
        email
      }
      Err(e) => {
        let msg = format!("CDK redeem failed: {}", safe_email_service_error(&e));
        self.log(&msg);
        cdk_record.last_error = msg.clone();
        cdk_record.status = "failed".into();
        // Record the *actual* remaining capacity so the dashboard does not
        // mislead the operator with a stale default of 0 when redeem failed
        // before any alias was claimed (e.g. provider HTTP 400 / 5xx).
        cdk_record.remaining = cdk_remaining_capacity(cdk);
        cdk_record.updated_at = Utc::now();
        put_cdk_inventory_record(&cdk_record);
        self.dispose_worker_profile(app_handle).await;
        return (all_results, Some(msg), false);
      }
    };

    for alias_idx in 0..accounts_per {
      if self.is_cancelled() {
        cdk_record.status = "cancelled".into();
        cdk_record.updated_at = Utc::now();
        put_cdk_inventory_record(&cdk_record);
        self.log("Cancelled by user");
        break;
      }

      let mut finished_result: Option<RegistrationResult> = None;
      let mut location_fallbacks: u32 = 0;
      let mut tried_locations: Vec<String> = Vec::new();
      let identity = match email_service.generate_alias(&base_email) {
        Ok(alias) => AccountIdentity::new(alias),
        Err(e) => {
          let msg = format!("Alias: {}", safe_email_service_error(&e));
          last_error = Some(msg.clone());
          self.log(&format!(
            "[CDK {}/{} Alias {}/{}] {msg}",
            cdk_idx + 1,
            total_cdks,
            alias_idx + 1,
            accounts_per
          ));
          cdk_record.record_hard_failure(&msg);
          put_cdk_inventory_record(&cdk_record);
          continue;
        }
      };
      if let Err(error) = reservation.claim_slot() {
        last_error = Some(error.clone());
        self.log(&format!(
          "[CDK {}/{} Alias {}/{}] {error}",
          cdk_idx + 1,
          total_cdks,
          alias_idx + 1,
          accounts_per
        ));
        cdk_record.record_hard_failure(&error);
        put_cdk_inventory_record(&cdk_record);
        break;
      }

      for attempt in 0..max_retries {
        if self.is_cancelled() {
          last_error = Some("Cancelled".into());
          // Roll back the claimed slot so usage ledger does not count a
          // cancelled attempt as a used account.
          if let Err(e) = reservation.release_slot() {
            self.log(&format!(
              "WARN: failed to release CDK slot after cancel: {e}"
            ));
          }
          break;
        }
        if attempt > 0 {
          self.log(&format!(
            "Retry {attempt}/{max_retries} for alias {}/{accounts_per}...",
            alias_idx + 1
          ));
          self.human_pause(1800, 2200).await;
        }

        match self
          .run_once(
            app_handle,
            email_service,
            sms_service,
            cdk,
            &base_email,
            &identity,
            alias_idx,
            cdk_idx,
            total_cdks,
            accounts_per,
          )
          .await
        {
          Ok(result) => {
            if !result.success
              && crate::vpn::is_unsupported_region_error(&result.error_message)
              && location_fallbacks < crate::vpn::MAX_NORD_LOCATION_FALLBACKS
            {
              match self
                .fallback_nord_location_on_region_block(
                  &result.error_message,
                  &mut location_fallbacks,
                  &mut tried_locations,
                )
                .await
              {
                Ok(true) => {
                  last_error = Some(result.error_message.clone());
                  continue;
                }
                Ok(false) => {}
                Err(e) => {
                  self.log(&format!("WARN: Nord location fallback failed: {e}"));
                }
              }
            }

            // Persist every finished attempt before reporting it as durable.
            if let Err(error) = save_registration_result(&result) {
              let safe_error = format!(
                "Account was created remotely, but local credential persistence failed: {error}"
              );
              self.log(&safe_error);
              cdk_record.record_hard_failure(&safe_error);
              put_cdk_inventory_record(&cdk_record);
              self.emit(
                app_handle,
                RegistrationStep::Failed,
                &safe_error,
                cdk_idx,
                alias_idx,
                total_cdks,
                None,
              );
              last_error = Some(safe_error);
              finished_result = Some(result);
              break;
            }
            cdk_record.record_result(&result);
            put_cdk_inventory_record(&cdk_record);
            all_results.push(result.clone());
            finished_result = Some(result.clone());

            let terminal_step = if result.success {
              RegistrationStep::Completed
            } else {
              RegistrationStep::Failed
            };
            let terminal_message = if result.success {
              format!(
                "Account saved. free-trial{}{}",
                if result.plan_type.is_empty() {
                  String::new()
                } else {
                  format!(" [{}]", result.plan_type)
                },
                if result.two_fa_enabled {
                  " (2FA on)"
                } else {
                  " (2FA skipped)"
                }
              )
            } else {
              format!("Account saved as non-success: {}", result.error_message)
            };
            self.emit(
              app_handle,
              terminal_step,
              &terminal_message,
              cdk_idx,
              alias_idx,
              total_cdks,
              Some(result.clone()),
            );

            if result.success {
              let sc = success_count.fetch_add(1, Ordering::SeqCst) + 1;
              if should_rotate(sc, rotate_every_n) {
                if uses_nord_cli {
                  self.emit(
                    app_handle,
                    RegistrationStep::RotatingIp,
                    &format!("Rotating NordVPN CLI IP after {sc} successes..."),
                    cdk_idx,
                    alias_idx,
                    total_cdks,
                    None,
                  );
                  let old_ip = crate::ip_utils::fetch_public_ip(None)
                    .await
                    .unwrap_or_default();
                  match crate::auto_service::common::nord_cli::rotate(
                    nord_cli,
                    nord_group,
                    nord_server,
                  ) {
                    Ok(()) => {
                      let mut new_ip = old_ip.clone();
                      for _ in 0..20 {
                        if self.is_cancelled() {
                          break;
                        }
                        self.human_pause(2500, 3500).await;
                        if let Ok(ip) = crate::ip_utils::fetch_public_ip(None).await {
                          new_ip = ip;
                          if !new_ip.is_empty() && new_ip != old_ip {
                            break;
                          }
                        }
                      }
                      if new_ip == old_ip {
                        self.log(&format!(
                          "WARN: egress IP unchanged after rotate ({old_ip}); continuing"
                        ));
                      } else {
                        self.log(&format!("Egress IP {old_ip} → {new_ip}"));
                      }
                      self.emit(
                        app_handle,
                        RegistrationStep::RotatingIp,
                        &format!("IP {old_ip} → {new_ip}"),
                        cdk_idx,
                        alias_idx,
                        total_cdks,
                        None,
                      );
                    }
                    Err(e) => {
                      let msg = format!("NordVPN CLI rotate failed: {e}");
                      self.log(&msg);
                      self.emit(
                        app_handle,
                        RegistrationStep::Failed,
                        &msg,
                        cdk_idx,
                        alias_idx,
                        total_cdks,
                        None,
                      );
                      nord_hard_stop = true;
                      last_error = Some(msg);
                    }
                  }
                } else if let Some(vpn_id) = self.worker_vpn_id() {
                  self.emit(
                    app_handle,
                    RegistrationStep::RotatingIp,
                    &format!("Rotating WireGuard peer after {sc} successes..."),
                    cdk_idx,
                    alias_idx,
                    total_cdks,
                    None,
                  );
                  match self.rotate_wireguard_peer(&vpn_id).await {
                    Ok((hostname, station)) => {
                      self.log(&format!(
                        "WireGuard peer rotated → {hostname} ({station}) after {sc} successes"
                      ));
                      self.emit(
                        app_handle,
                        RegistrationStep::RotatingIp,
                        &format!("WireGuard peer → {hostname} ({station})"),
                        cdk_idx,
                        alias_idx,
                        total_cdks,
                        None,
                      );
                    }
                    Err(e) => {
                      let msg = format!("WireGuard peer rotate failed: {e}");
                      self.log(&msg);
                      self.emit(
                        app_handle,
                        RegistrationStep::Failed,
                        &msg,
                        cdk_idx,
                        alias_idx,
                        total_cdks,
                        None,
                      );
                      nord_hard_stop = true;
                      last_error = Some(msg);
                    }
                  }
                }
              }
            }
            break;
          }
          Err(e) => {
            last_error = Some(e.clone());
            self.log(&format!("Attempt {attempt} failed: {e}"));
            if crate::vpn::is_unsupported_region_error(&e)
              && location_fallbacks < crate::vpn::MAX_NORD_LOCATION_FALLBACKS
            {
              match self
                .fallback_nord_location_on_region_block(
                  &e,
                  &mut location_fallbacks,
                  &mut tried_locations,
                )
                .await
              {
                Ok(true) => continue,
                Ok(false) => {}
                Err(fe) => {
                  self.log(&format!("WARN: Nord location fallback failed: {fe}"));
                }
              }
            }
            // Browser-kill failure is unrecoverable for this CDK: another
            // alias launch would stack a second browser on the still-running
            // one and freeze the machine. Aborts the CDK immediately.
            if e.contains("browser kill failed") {
              cdk_record.record_hard_failure(&e);
              put_cdk_inventory_record(&cdk_record);
              if let Err(re_err) = reservation.release_slot() {
                self.log(&format!(
                  "WARN: failed to release CDK slot after browser kill failure: {re_err}"
                ));
              }
              self.dispose_worker_profile(app_handle).await;
              return (all_results, Some(e), nord_hard_stop);
            }
          }
        }

        if nord_hard_stop {
          break;
        }
      }

      if finished_result.is_none() {
        let err = last_error
          .clone()
          .unwrap_or_else(|| "unknown failure".into());
        self.log(&format!(
          "Alias {}/{} failed after {max_retries} retries",
          alias_idx + 1,
          accounts_per
        ));
        // Roll back the claimed slot — the account was never created, so the
        // CDK usage ledger must not count this attempt. Lets retry work.
        if let Err(e) = reservation.release_slot() {
          self.log(&format!(
            "WARN: failed to release CDK slot after failure: {e}"
          ));
        }
        cdk_record.record_hard_failure(&err);
        put_cdk_inventory_record(&cdk_record);
      }

      if nord_hard_stop {
        break;
      }
    }

    if self.is_cancelled() && cdk_record.status == "running" {
      cdk_record.status = "cancelled".into();
    } else if cdk_record.status == "running" {
      cdk_record.finalize_status();
    }
    cdk_record.updated_at = Utc::now();
    put_cdk_inventory_record(&cdk_record);

    // Dispose this CDK's worker profile (each concurrent slot owns one).
    self.dispose_worker_profile(app_handle).await;

    (all_results, last_error, nord_hard_stop)
  }

  // -----------------------------------------------------------------------
  // Single account registration with full state machine
  // -----------------------------------------------------------------------

  #[allow(clippy::too_many_arguments)]
  async fn run_once(
    &mut self,
    app_handle: &tauri::AppHandle,
    email_service: &dyn EmailService,
    sms_service: Option<&dyn SmsService>,
    cdk: &str,
    base_email: &str,
    identity: &AccountIdentity,
    alias_idx: u32,
    cdk_idx: u32,
    total_cdks: u32,
    total_aliases: u32,
  ) -> Result<RegistrationResult, String> {
    let prefix = format!(
      "[CDK {}/{} Alias {}/{}]",
      cdk_idx + 1,
      total_cdks,
      alias_idx + 1,
      total_aliases
    );

    // Step 1: Use the alias reserved for this logical account slot.
    self.emit(
      app_handle,
      RegistrationStep::GeneratingAlias,
      &format!("{prefix} Using email alias..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    self.log(&format!("{prefix} Alias: {}", identity.alias_email));

    // Step 2: Use the account identity reserved for this logical slot.
    self.emit(
      app_handle,
      RegistrationStep::GeneratingUserInfo,
      &format!("{prefix} Using reserved user info..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    self.log(&format!(
      "{prefix} Name: {} {}",
      identity.first_name, identity.last_name
    ));

    // Step 3: Launch (or relaunch) the reused worker profile for THIS account.
    // Lifecycle: ensure worker once → launch (new FP + ephemeral dir) → register → kill only.
    self.emit(
      app_handle,
      RegistrationStep::LaunchingBrowser,
      &format!("{prefix} Launching browser worker profile..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );

    self.device_id.clone_from(&identity.device_id);

    let (profile, mut session) = self.launch_and_connect(app_handle).await?;
    self.log(&format!(
      "{prefix} Browser worker launched: {} ({})",
      profile.name, profile.id
    ));

    // Kill browser after each account; keep worker profile metadata for reuse.
    let mut active_profile = profile;
    let registration_result = self
      .run_registration_in_browser(
        app_handle,
        email_service,
        sms_service,
        &mut session,
        &mut active_profile,
        cdk,
        base_email,
        &identity.alias_email,
        &identity.password,
        &identity.first_name,
        &identity.last_name,
        &identity.birthdate,
        prefix.as_str(),
        cdk_idx,
        alias_idx,
        total_cdks,
      )
      .await;

    // Kill the browser before we yield back to the alias loop. If the kill
    // fails, the next alias would launch a second browser on top of the
    // first — accumulating Camoufox processes and freezing the machine.
    // Propagate the error so the outer loop can abort this CDK cleanly.
    if let Err(e) = self.kill_browser_only(app_handle, &active_profile).await {
      self.log(&format!(
        "{prefix} Browser kill failed; aborting CDK to avoid overlapping browsers: {e}"
      ));
      return Err(format!("browser kill failed for alias {alias_idx}: {e}"));
    }
    self.log(&format!(
      "{prefix} Browser closed (worker profile kept for reuse)"
    ));

    registration_result
  }

  /// Core registration steps that run inside an already-launched browser profile.
  #[allow(clippy::too_many_arguments)]
  async fn run_registration_in_browser(
    &mut self,
    app_handle: &tauri::AppHandle,
    email_service: &dyn EmailService,
    sms_service: Option<&dyn SmsService>,
    session: &mut BrowserSession,
    profile: &mut crate::profile::BrowserProfile,
    cdk: &str,
    base_email: &str,
    alias_email: &str,
    password: &str,
    first_name: &str,
    last_name: &str,
    birthdate: &str,
    prefix: &str,
    cdk_idx: u32,
    alias_idx: u32,
    total_cdks: u32,
  ) -> Result<RegistrationResult, String> {
    // Always start from a wiped jar — never inherit previous OpenAI sessions.
    self.log(&format!(
      "{prefix} Clearing browser cookies/cache/storage..."
    ));
    if let Err(e) = session.clear_all_site_data().await {
      self.log(&format!("{prefix} clear_all_site_data warning: {e}"));
    }

    // Seed oai-did cookie
    self.log(&format!("{prefix} Device ID: {}", self.device_id));
    for domain in &[
      "chatgpt.com",
      ".chatgpt.com",
      "auth.openai.com",
      ".auth.openai.com",
    ] {
      let _ = session.set_cookie("oai-did", &self.device_id, domain).await;
    }

    // Step 4: Visit chatgpt.com
    self.emit(
      app_handle,
      RegistrationStep::VisitingHomepage,
      &format!("{prefix} Visiting chatgpt.com..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    session.navigate("https://chatgpt.com/", 30).await?;

    // Humanize: explore the homepage briefly (scroll, idle) before proceeding
    self.human_pause(1200, 2800).await;
    let _ = session.evaluate("window.scrollBy(0, 300)", false).await;
    self.human_pause(800, 1500).await;
    let _ = session.evaluate("window.scrollBy(0, -150)", false).await;
    self.human_pause(600, 1200).await;

    // Step 5: CSRF token
    self.emit(
      app_handle,
      RegistrationStep::GettingCsrfToken,
      &format!("{prefix} Getting CSRF..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    // Step 6: Submit email
    self.emit(
      app_handle,
      RegistrationStep::SubmittingEmail,
      &format!("{prefix} Submitting email..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    // Step 7: Follow authorize
    self.emit(
      app_handle,
      RegistrationStep::FollowingAuthorize,
      &format!("{prefix} Following authorize..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    let mut cur_url = self
      .authorize_with_retry(session, profile, alias_email, app_handle)
      .await?;
    self.log(&format!(
      "{prefix} Authorize → {cur_url} (profile={})",
      profile.id
    ));
    // Give auth.openai.com time to settle cookies after the authorize redirect.
    self.human_pause(1400, 2600).await;
    if let Ok(url) = session.current_url().await {
      cur_url = url;
      self.log(&format!("{prefix} Post-authorize URL: {cur_url}"));
    }

    // Step 8-12: State machine loop
    let mut register_submitted = false;
    let mut account_created = false;
    let mut phone_number_used = String::new();
    let mut seen_states: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

    for _ in 0..12 {
      if self.is_cancelled() {
        if account_created {
          self.log(&format!(
            "{prefix} Cancelled after account creation; preserving credentials"
          ));
          break;
        }
        return Err("Cancelled".into());
      }
      let sig = format!("{:?}", detect_page_type(&cur_url));
      *seen_states.entry(sig.clone()).or_insert(0) += 1;
      if seen_states[&sig] > 2 {
        return Err(format!("State loop: {sig}"));
      }

      let page = detect_page_type(&cur_url);
      self.log(&format!("{prefix} Page: {page:?}"));

      match page {
        PageType::CreateAccountPassword | PageType::Unknown => {
          if register_submitted {
            return Err("Back at password page".into());
          }
          self.emit(
            app_handle,
            RegistrationStep::RegisteringUser,
            &format!("{prefix} Registering..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );

          // UI-first: stay in the authorize session. Hard-navigating away from
          // log-in-or-create-account invalidates OpenAI auth state.
          if let Err(e) = self.advance_auth_ui_to_password(session, alias_email).await {
            self.log(&format!("{prefix} advance_auth_ui: {e}"));
          }
          if let Ok(url) = session.current_url().await {
            cur_url = url;
            self.log(&format!("{prefix} Auth UI URL: {cur_url}"));
          }

          // OpenAI sometimes lands on email-verification without showing a password form
          // (especially after choose-an-account). Prefer UI password; only force
          // API register when session is still on a password-capable step.
          // Never force-register on a stale email-otp step → invalid_auth_step.
          let on_email_otp_surface = cur_url.contains("email-verification")
            || cur_url.contains("email-otp")
            || cur_url.contains("about-you");

          if on_email_otp_surface {
            // Attempt 1: recover password form via "Continue with password".
            self.human_pause(400, 900).await;
            let recovered = self
              .click_by_text(
                session,
                "password",
                "button, a, [role='button'], div[role='button']",
              )
              .await
              .is_ok();
            if recovered {
              self.human_pause(700, 1400).await;
            }
            if self
              .page_has_selector(
                session,
                r#"input[name="new-password"], input[type="password"], input[autocomplete="new-password"]"#,
              )
              .await
            {
              self
                .submit_password_via_ui(session, password)
                .await
                .map_err(|e| format!("password UI after recovery failed: {e}"))?;
              self.log(&format!(
                "{prefix} Password submitted via recovered UI form"
              ));
            } else {
              // Fresh authorize session, then force register once.
              self.log(&format!(
                "{prefix} Password form missing on {cur_url}; refreshing authorize then force-register"
              ));
              self.navigate_to_signup_page(session, alias_email).await?;
              self.human_pause(1200, 2000).await;
              if let Err(e) = self.advance_auth_ui_to_password(session, alias_email).await {
                self.log(&format!("{prefix} advance_auth_ui after refresh: {e}"));
              }
              if self
                .page_has_selector(
                  session,
                  r#"input[name="new-password"], input[type="password"], input[autocomplete="new-password"]"#,
                )
                .await
              {
                self
                  .submit_password_via_ui(session, password)
                  .await
                  .map_err(|e| format!("password UI after authorize refresh failed: {e}"))?;
                self.log(&format!(
                  "{prefix} Password submitted via UI after authorize refresh"
                ));
              } else {
                self
                  .force_api_register(session, alias_email, password)
                  .await?;
                self.log(&format!("{prefix} Force-register request accepted"));
              }
            }
          } else {
            match self.submit_password_via_ui(session, password).await {
              Ok(()) => {
                self.log(&format!("{prefix} Password submitted via UI form"));
              }
              Err(ui_err) => {
                self.log(&format!("{prefix} UI password submit failed: {ui_err}"));
                self
                  .force_api_register(session, alias_email, password)
                  .await
                  .map_err(|api_err| {
                    format!("UI password failed ({ui_err}); API register failed: {api_err}")
                  })?;
                self.log(&format!("{prefix} Register request accepted"));
              }
            }
          }
          register_submitted = true;

          self.emit(
            app_handle,
            RegistrationStep::SendingEmailOtp,
            &format!("{prefix} Requesting OTP..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );
          // Try UI-first OTP send: click "Send code" / "Continue" button on page.
          let mut otp_sent_via_ui = false;
          for label in ["Send code", "Continue", "Send verification", "Next"] {
            if self
              .click_by_text(session, label, "button, [role='button']")
              .await
              .is_ok()
            {
              self.log(&format!("{prefix} OTP send via UI click: '{label}'"));
              otp_sent_via_ui = true;
              break;
            }
          }
          if !otp_sent_via_ui {
            let otp_send = format!(
              "fetch('https://auth.openai.com/api/accounts/email-otp/send', {{ method: 'POST', credentials: 'include', headers: {{ accept: 'application/json', 'content-type': 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{}}) }})",
              did = self.device_id,
            );
            session.fetch_json(&otp_send).await?;
            self.log(&format!("{prefix} OTP send via API accepted"));
          }

          // After password submit, settle on current URL (usually email-verification).
          self.human_pause(1200, 2400).await;
          cur_url = session
            .current_url()
            .await
            .unwrap_or_else(|_| "https://auth.openai.com/email-verification".into());
          if !cur_url.contains("email-verification")
            && !cur_url.contains("about-you")
            && !cur_url.contains("email-otp")
          {
            let _ = session
              .navigate("https://auth.openai.com/email-verification", 20)
              .await;
            cur_url = session
              .current_url()
              .await
              .unwrap_or_else(|_| "https://auth.openai.com/email-verification".into());
          }
          self.log(&format!("{prefix} After password submit URL: {cur_url}"));
          continue;
        }

        PageType::EmailOtpVerification => {
          // Intermediate email-verification screens can appear before password/register.
          // Only poll Gmail once register+send have completed.
          if !register_submitted {
            self.log(&format!(
              "{prefix} Email verification page before register — submitting password first"
            ));
            cur_url = "https://auth.openai.com/create-account/password".to_string();
            continue;
          }

          // Up to 3 OTP cycles: poll → validate; on 401 refresh page + re-send + wait
          // for a *new* code (provider tracks used OTP codes per CDK).
          let mut otp_ok = false;
          let mut last_otp_err = String::new();
          for otp_attempt in 1..=3 {
            self.emit(
              app_handle,
              RegistrationStep::PollingOtp,
              &format!("{prefix} Waiting for OTP (attempt {otp_attempt}/3)..."),
              cdk_idx,
              alias_idx,
              total_cdks,
              None,
            );
            // First attempt can use longer poll; after 401 re-send, shorter wait for new mail.
            let poll_secs = if otp_attempt == 1 { 150 } else { 90 };
            let otp = match email_service.poll_verification_code(cdk, poll_secs) {
              Ok(c) => c,
              Err(e) => {
                last_otp_err = format!("OTP poll: {}", safe_email_service_error(&e));
                self.log(&format!("{prefix} {last_otp_err}"));
                break;
              }
            };
            self.log(&format!("{prefix} OTP received (attempt {otp_attempt}/3)"));
            // Always mark attempted — even before validate — so a failed 401 cannot
            // re-use the same mailbox code on the next poll.
            email_service.mark_verification_code_used(cdk, &otp);

            self.emit(
              app_handle,
              RegistrationStep::VerifyingOtp,
              &format!("{prefix} Verifying OTP..."),
              cdk_idx,
              alias_idx,
              total_cdks,
              None,
            );
            // Ensure we are on the verification page with a live auth session cookie.
            let page_url = session.current_url().await.unwrap_or_default();
            if !page_url.contains("email-verification") && !page_url.contains("email-otp") {
              let _ = session
                .navigate("https://auth.openai.com/email-verification", 20)
                .await;
              self.human_pause(800, 1500).await;
            }

            // Try UI-first OTP validation (type code into form fields).
            // Falls back to direct API call if UI inputs not found or UI fails.
            let mut ui_verified = false;
            match self.try_ui_email_otp(session, &otp).await {
              Ok(Some(continue_url)) => {
                cur_url = continue_url;
                self.log(&format!(
                  "{prefix} OTP verified via UI (attempt {otp_attempt}/3) → {cur_url}"
                ));
                otp_ok = true;
                ui_verified = true;
              }
              Ok(None) => {
                self.log(&format!(
                  "{prefix} UI OTP not available, falling back to API validation"
                ));
              }
              Err(e) => {
                self.log(&format!("{prefix} UI OTP error: {e}, falling back to API"));
              }
            }

            if !ui_verified {
              let verify_js = format!(
              "fetch('https://auth.openai.com/api/accounts/email-otp/validate', {{ method: 'POST', credentials: 'include', headers: {{ 'content-type': 'application/json', accept: 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{ code: '{otp}' }}) }})",
              did = self.device_id,
            );
              let verify = session.fetch_json(&verify_js).await?;
              let vs = verify["_status"].as_u64().unwrap_or(200);
              if vs == 200 && verify.get("error").is_none() {
                cur_url = verify["continue_url"]
                  .as_str()
                  .unwrap_or("https://auth.openai.com/about-you")
                  .to_string();
                self.log(&format!(
                  "{prefix} OTP verified via API (attempt {otp_attempt}/3) → {cur_url}"
                ));
                otp_ok = true;
                break;
              }

              let body = verify
                .get("_body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
              last_otp_err = format!("OTP verification rejected with HTTP {vs}");
              self.log(&format!("{prefix} {last_otp_err}"));

              // 401 / wrong code → stale or not-yet-bound OTP: full page refresh + re-send
              // so OpenAI issues a *new* code; mailbox poll skips used codes.
              let is_stale = vs == 401
                || body.to_ascii_lowercase().contains("wrong")
                || body.to_ascii_lowercase().contains("invalid")
                || body.to_ascii_lowercase().contains("unauthorized")
                || verify
                  .get("error")
                  .map(|e| e.to_string().to_ascii_lowercase())
                  .is_some_and(|s| s.contains("invalid") || s.contains("unauthorized"));

              if !is_stale || otp_attempt == 3 {
                break;
              }

              self.log(&format!(
              "{prefix} OTP rejected (likely stale/unsent binding); refresh page + re-send for new code"
            ));
              // Full page reload to rebind auth session cookies to the OTP challenge.
              let _ = session
                .navigate("https://auth.openai.com/email-verification", 25)
                .await;
              self.human_pause(1200, 2200).await;

              // Prefer UI "Resend email" then API send as backup.
              let mut resent = false;
              for label in ["Resend email", "Resend", "Send code again", "Resend code"] {
                if self
                  .click_by_text(session, label, "button, a, [role='button']")
                  .await
                  .is_ok()
                {
                  self.log(&format!("{prefix} Clicked resend control: {label}"));
                  resent = true;
                  break;
                }
              }
              let otp_send = format!(
              "fetch('https://auth.openai.com/api/accounts/email-otp/send', {{ method: 'POST', credentials: 'include', headers: {{ accept: 'application/json', 'content-type': 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{}}) }})",
              did = self.device_id,
            );
              match session.fetch_json(&otp_send).await {
                Ok(_response) => {
                  self.log(&format!(
                    "{prefix} OTP re-send API accepted (ui_resend={resent})"
                  ));
                }
                Err(e) => {
                  self.log(&format!("{prefix} OTP re-send API failed: {e}"));
                }
              }
              // Give the mailbox time to receive the *new* message before polling.
              self.human_pause(2500, 4500).await;
            } // end if !ui_verified
          }

          if !otp_ok {
            return Err(if last_otp_err.is_empty() {
              "OTP verification failed".into()
            } else {
              last_otp_err
            });
          }
          continue;
        }

        PageType::AboutYou => {
          if account_created {
            return Err("Back at about-you".into());
          }
          self.emit(
            app_handle,
            RegistrationStep::CreatingAccount,
            &format!("{prefix} Creating account..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );

          // About You must be completed through the visible form. Falling back
          // to create_account API skips the same human signals this page asks for.
          self
            .submit_about_you_via_ui(session, first_name, last_name, birthdate)
            .await
            .map_err(|e| format!("About You UI submit failed: {e}"))?;
          account_created = true;
          self.log(&format!("{prefix} Account created via UI form"));
          cur_url = session.current_url().await.unwrap_or_default();
          continue;
        }

        PageType::AddPhone => {
          let Some(sms) = sms_service else {
            return Err(
              "Phone verification required but no SMS provider configured (set smsProvider/smsToken/smsServiceId)"
                .into(),
            );
          };
          let service_id = self.config.sms_service_id.ok_or_else(|| {
            "Phone verification required but smsServiceId is not configured".to_string()
          })?;

          self.emit(
            app_handle,
            RegistrationStep::RequestingSmsOtp,
            &format!("{prefix} Renting SMS number..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );

          let request = NumberRequest {
            service_id,
            network: self.config.sms_network.clone(),
            prefix: None,
            except_prefix: None,
            number: None,
            country: self.config.sms_country.clone(),
          };
          let number_info = sms
            .request_number(&request)
            .map_err(|error| format!("SMS rent number: {}", safe_sms_service_error(&error)))?;
          phone_number_used = number_info.phone_number.clone();
          self.log(&format!(
            "{prefix} SMS number: {} (request_id={})",
            number_info.phone_number, number_info.request_id
          ));

          // Fill phone field + submit (best-effort selectors for OpenAI add-phone UI).
          let phone_selectors = [
            r#"input[type="tel"]"#,
            r#"input[name="phone"]"#,
            r#"input[name="phoneNumber"]"#,
            r#"input[autocomplete="tel"]"#,
            r#"input[inputmode="tel"]"#,
          ];
          let mut filled = false;
          for sel in phone_selectors {
            if self
              .fill_input(session, sel, &number_info.phone_number)
              .await
              .is_ok()
            {
              filled = true;
              break;
            }
          }
          if !filled {
            return Err("Could not find phone input on add-phone page".into());
          }
          self.human_pause(350, 450).await;

          let mut submitted = false;
          for sel in [
            r#"button[type="submit"]"#,
            r#"button[name="intent"]"#,
            r#"form button"#,
          ] {
            if self
              .click_selector(session, sel, "sms phone submit")
              .await
              .is_ok()
            {
              submitted = true;
              break;
            }
          }
          if !submitted {
            let _ = self.click_by_text(session, "Continue", "button").await;
            let _ = self.click_by_text(session, "Send code", "button").await;
            let _ = self.click_by_text(session, "Verify", "button").await;
          }

          self.emit(
            app_handle,
            RegistrationStep::PollingSmsOtp,
            &format!("{prefix} Waiting for SMS OTP..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );
          let otp_info = sms
            .get_otp(&number_info.request_id, 150)
            .map_err(|error| format!("SMS OTP poll: {}", safe_sms_service_error(&error)))?;
          let sms_code = otp_info
            .code
            .filter(|c| !c.is_empty())
            .ok_or_else(|| "SMS OTP completed without code".to_string())?;
          self.log(&format!("{prefix} SMS OTP received"));

          self.emit(
            app_handle,
            RegistrationStep::VerifyingSmsOtp,
            &format!("{prefix} Verifying SMS OTP..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );

          // Prefer OTP input on page; fall back to OpenAI phone-otp validate API.
          let otp_selectors = [
            r#"input[name="code"]"#,
            r#"input[autocomplete="one-time-code"]"#,
            r#"input[inputmode="numeric"]"#,
            r#"input[type="text"]"#,
            r#"input[type="tel"]"#,
          ];
          let mut otp_filled = false;
          for sel in otp_selectors {
            if self.fill_input(session, sel, &sms_code).await.is_ok() {
              otp_filled = true;
              break;
            }
          }
          if otp_filled {
            let mut otp_submitted = false;
            for sel in [r#"button[type="submit"]"#, r#"form button"#] {
              if self
                .click_selector(session, sel, "sms otp submit")
                .await
                .is_ok()
              {
                otp_submitted = true;
                break;
              }
            }
            if !otp_submitted {
              let _ = self.click_by_text(session, "Continue", "button").await;
              let _ = self.click_by_text(session, "Verify", "button").await;
            }
          } else {
            let verify_js = format!(
              "fetch('https://auth.openai.com/api/accounts/phone-otp/validate', {{ method: 'POST', credentials: 'include', headers: {{ 'content-type': 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{ code: '{code}' }}) }})",
              did = self.device_id,
              code = sms_code,
            );
            let verify = session.fetch_json(&verify_js).await?;
            let vs = verify["_status"].as_u64().unwrap_or(200);
            if vs != 200 {
              return Err(format!("SMS OTP verification failed with HTTP {vs}"));
            }
            if let Some(next) = verify["continue_url"].as_str() {
              if !next.is_empty() {
                session.navigate(next, 30).await?;
              }
            }
          }

          self.human_pause(1800, 2200).await;
          cur_url = session.current_url().await.unwrap_or_default();
          continue;
        }
        PageType::ChatgptHome | PageType::Callback | PageType::Consent => {
          self.log(&format!("{prefix} ✅ Flow complete"));
          break;
        }
        PageType::ExternalUrl => {
          session.navigate(&cur_url, 20).await?;
          cur_url = session.current_url().await.unwrap_or_default();
          continue;
        }
        PageType::LoginPassword => {
          return Err("Email already has account".into());
        }
        PageType::ErrorPage => {
          return Err(format!("Error page: {cur_url}"));
        }
      }
    }

    // Extract tokens — soft-fail if account already created (do not lose the account).
    self.emit(
      app_handle,
      RegistrationStep::ExtractingTokens,
      &format!("{prefix} Extracting tokens..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    let (access_token, account_id) = self.extract_access_token_with_retry(session, prefix).await;

    self.log(&format!(
      "{prefix} access token obtained={}",
      !access_token.is_empty()
    ));
    if access_token.is_empty() && !account_created {
      return Err("No access token and account was not created".into());
    }

    // Gate: only keep accounts that have free trial / free Plus offer.
    self.emit(
      app_handle,
      RegistrationStep::CheckingFreeOffer,
      &format!("{prefix} Checking free offer / free trial..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    let (free_trial_eligible, plan_type, offer_detail) = self
      .check_free_trial_offer(session, &access_token, &account_id)
      .await;
    self.log(&format!(
      "{prefix} Free offer check: eligible={free_trial_eligible} plan={plan_type} detail={offer_detail}"
    ));

    // No free trial: still persist the account under CDK stats (not resellable).
    // Return Ok so the alias is not retried; success=false / free_trial_eligible=false.
    if !free_trial_eligible {
      let err = format!("No free trial/free Plus offer (plan={plan_type}; {offer_detail})");
      self.log(&format!("{prefix} {err} — saving as free-trial-no"));
      let result = RegistrationResult {
        success: false,
        email: alias_email.to_string(),
        password: password.to_string(),
        account_id,
        access_token,
        device_id: self.device_id.clone(),
        error_message: err.clone(),
        step_logs: self.logs.clone(),
        created_at: Utc::now(),
        two_fa_enabled: false,
        totp_secret: String::new(),
        free_trial_eligible: false,
        plan_type: plan_type.clone(),
        cdk: cdk.to_string(),
        base_email: base_email.to_string(),
        phone_number: phone_number_used,
        status: super::types::AccountInventoryStatus::Invalid,
        note: "free_trial_no".into(),
        exported_at: None,
        sold_at: None,
        email_provider: Some(self.config.email_provider),
        email_provider_provenance: Some(EmailProviderProvenance::RegistrationConfig),
        registration_outcome_reason: Some(RegistrationOutcomeReason::FreeTrialNo),
        two_factor_backfill_access_state: Some(TwoFactorBackfillAccessState::Accessible),
        two_factor_backfill_exclusion: None,
        two_factor_backfill_state: None,
        two_factor_backfill_outcome: None,
        two_factor_backfill_operation_id: None,
        record_revision: 1,
      };
      return Ok(result);
    }

    // Step: Enable authenticator 2FA (retry only this step; never fail the whole registration).
    self.emit(
      app_handle,
      RegistrationStep::Enabling2Fa,
      &format!("{prefix} Enabling 2FA..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );

    let mut two_fa_enabled = false;
    let mut totp_secret = String::new();
    let mut two_fa_error = String::new();
    const TWO_FA_ATTEMPTS: u32 = 3;

    for attempt in 1..=TWO_FA_ATTEMPTS {
      if self.is_cancelled() {
        two_fa_error = "cancelled after account creation".into();
        self.log(&format!(
          "{prefix} 2FA skipped after cancellation; preserving account"
        ));
        break;
      }
      match self.enable_2fa(session).await {
        Ok(secret) => {
          two_fa_enabled = true;
          totp_secret = secret;
          self.log(&format!(
            "{prefix} 2FA enabled (attempt {attempt}/{TWO_FA_ATTEMPTS})"
          ));
          break;
        }
        Err(e) => {
          two_fa_error = e.clone();
          self.log(&format!(
            "{prefix} 2FA attempt {attempt}/{TWO_FA_ATTEMPTS} failed: {e}"
          ));
          if attempt < TWO_FA_ATTEMPTS {
            // Reset UI surface before retrying the 2FA flow only.
            let _ = session.navigate("https://chatgpt.com/", 15).await;
            self.human_pause(1400, 2600).await;
          }
        }
      }
    }

    if !two_fa_enabled {
      self.log(&format!(
        "{prefix} 2FA soft-failed after {TWO_FA_ATTEMPTS} attempts: {two_fa_error}"
      ));
    }

    let mut error_message = String::new();
    if access_token.is_empty() {
      error_message = "Account created but access token not extracted".into();
    }
    if !two_fa_enabled && !two_fa_error.is_empty() {
      if !error_message.is_empty() {
        error_message.push_str("; ");
      }
      error_message.push_str(&format!("2FA not enabled: {two_fa_error}"));
    }

    let result = RegistrationResult {
      success: true,
      email: alias_email.to_string(),
      password: password.to_string(),
      account_id,
      access_token,
      device_id: self.device_id.clone(),
      error_message,
      step_logs: self.logs.clone(),
      created_at: Utc::now(),
      two_fa_enabled,
      totp_secret,
      free_trial_eligible: true,
      plan_type: plan_type.clone(),
      cdk: cdk.to_string(),
      base_email: base_email.to_string(),
      phone_number: phone_number_used,
      status: super::types::AccountInventoryStatus::Available,
      note: String::new(),
      exported_at: None,
      sold_at: None,
      email_provider: Some(self.config.email_provider),
      email_provider_provenance: Some(EmailProviderProvenance::RegistrationConfig),
      registration_outcome_reason: Some(RegistrationOutcomeReason::Registered),
      two_factor_backfill_access_state: Some(TwoFactorBackfillAccessState::Accessible),
      two_factor_backfill_exclusion: None,
      two_factor_backfill_state: None,
      two_factor_backfill_outcome: None,
      two_factor_backfill_operation_id: None,
      record_revision: 1,
    };

    Ok(result)
  }

  // -----------------------------------------------------------------------
  // Enable ChatGPT authenticator 2FA via UI automation
  // -----------------------------------------------------------------------

  /// Advance OpenAI auth UI from log-in-or-create-account / login toward password.
  async fn advance_auth_ui_to_password(
    &mut self,
    session: &mut BrowserSession,
    email: &str,
  ) -> Result<(), String> {
    let mut url = session.current_url().await.unwrap_or_default();
    self.log(&format!("Auth UI start URL: {url}"));

    if self
      .page_has_selector(
        session,
        r#"input[name="new-password"], input[type="password"]"#,
      )
      .await
    {
      return Ok(());
    }

    // choose-an-account: pick "use a different email / sign up" so we leave the
    // residual session from a previous partial registration on this profile.
    if url.contains("choose-an-account") {
      let mut advanced = false;
      for label in [
        "Use a different email",
        "different email",
        "Sign up",
        "Create account",
        "Log in or sign up",
        "Continue",
      ] {
        if self
          .click_by_text(
            session,
            label,
            "button, a, [role='button'], div[role='button']",
          )
          .await
          .is_ok()
        {
          advanced = true;
          self.human_pause(800, 1200).await;
          break;
        }
      }
      if !advanced {
        // Click the last account card action or a generic "add" control.
        let _ = self
          .click_selector(
            session,
            r#"button, a[href*='email'], [data-testid*='add'], [data-testid*='different']"#,
            "choose-an-account action",
          )
          .await;
        self.human_pause(800, 1200).await;
      }
      url = session.current_url().await.unwrap_or_default();
      self.log(&format!("After choose-an-account action URL: {url}"));
    }

    let email_selectors =
      r#"input#email, input[name="email"], input[type="email"], input[autocomplete*="email"]"#;
    let email_visible = self.page_has_selector(session, email_selectors).await;

    if email_visible {
      // Fill email on this page (when email wasn't already entered via dialog).
      self.fill_input(session, email_selectors, email).await?;
      self.human_pause(350, 450).await;

      let mut clicked = false;
      for sel in [
        r#"button[type="submit"]"#,
        r#"button[name="intent"]"#,
        r#"form button"#,
      ] {
        if self
          .click_selector(session, sel, "auth continue")
          .await
          .is_ok()
        {
          clicked = true;
          break;
        }
      }
      if !clicked {
        let _ = self.click_by_text(session, "Continue", "button").await;
      }
    }

    // After submitting email (or if email was already entered via dialog),
    // wait for the password form to appear. OpenAI may show a method picker
    // ("Continue with password") before the password form.
    for _ in 0..30 {
      self.human_pause(450, 550).await;
      url = session.current_url().await.unwrap_or_default();
      if self
        .page_has_selector(
          session,
          r#"input[name="new-password"], input[type="password"]"#,
        )
        .await
      {
        self.log(&format!("Password field appeared at {url}"));
        return Ok(());
      }
      // Click "password" method option if the method picker is visible.
      if self
        .click_by_text(
          session,
          "password",
          "button, a, [role='button'], div[role='button']",
        )
        .await
        .is_ok()
      {
        self.log(&format!(
          "Clicked 'password' method option at {url}; waiting for password form"
        ));
        self.human_pause(750, 850).await;
        if self
          .page_has_selector(
            session,
            r#"input[name="new-password"], input[type="password"]"#,
          )
          .await
        {
          let url_now = session.current_url().await.unwrap_or_default();
          self.log(&format!("Password field appeared after click at {url_now}"));
          return Ok(());
        }
      }
      // Accept email-verification / about-you as valid outcomes (no password needed).
      if url.contains("email-verification") || url.contains("about-you") {
        self.log(&format!("Auth advanced to {url} without password field"));
        return Ok(());
      }
    }

    url = session.current_url().await.unwrap_or_default();
    if !self
      .page_has_selector(
        session,
        r#"input[name="new-password"], input[type="password"]"#,
      )
      .await
      && !url.contains("password")
    {
      return Err(format!(
        "could not reach password form from auth UI (url={url})"
      ));
    }
    Ok(())
  }

  async fn page_has_selector(&mut self, session: &mut BrowserSession, selector: &str) -> bool {
    let js = format!(
      r#"(function(){{ return !!document.querySelector({sel}); }})()"#,
      sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
    );
    session
      .evaluate(&js, false)
      .await
      .ok()
      .and_then(|v| v["value"].as_bool())
      .unwrap_or(false)
  }

  /// Human-like pause between automation steps (random within range).
  async fn human_pause(&self, min_ms: u64, max_ms: u64) {
    use crate::browser_actions::jitter_ms;
    sleep(jitter_ms(min_ms, max_ms)).await;
  }

  /// Try to validate email OTP via UI form inputs instead of direct API call.
  /// Returns Ok(Some(continue_url)) if UI succeeded, Ok(None) if UI not available,
  /// Err if UI was attempted but failed.
  async fn try_ui_email_otp(
    &mut self,
    session: &mut BrowserSession,
    otp_code: &str,
  ) -> Result<Option<String>, String> {
    // Detect OTP input fields on the page.
    let detect_js = r#"(function(){
      // Pattern 1: Multiple single-digit inputs (common OTP pattern)
      const singleDigitInputs = Array.from(document.querySelectorAll(
        'input[maxlength="1"], input[data-index], input[type="tel"][maxlength="1"]'
      ));
      if (singleDigitInputs.length >= 4) {
        return { found: true, mode: 'multi', count: singleDigitInputs.length,
          selectors: singleDigitInputs.map((el, i) => {
            const sel = el.id ? '#' + el.id :
              el.name ? 'input[name="' + el.name + '"]' :
              'input[data-index="' + i + '"]';
            return sel;
          })
        };
      }
      // Pattern 2: Single code input
      const codeInput = document.querySelector(
        'input[name="code"], input[name="otp"], input[name="emailCode"], ' +
        'input[autocomplete="one-time-code"], input[inputmode="numeric"][maxlength], ' +
        'input[type="tel"][maxlength]'
      );
      if (codeInput) {
        const sel = codeInput.id ? '#' + codeInput.id :
          codeInput.name ? 'input[name="' + codeInput.name + '"]' :
          'input[inputmode="numeric"]';
        return { found: true, mode: 'single', selector: sel };
      }
      return { found: false };
    })()"#;

    let detect_result = match session.evaluate(detect_js, false).await {
      Ok(r) => r.get("value").cloned().unwrap_or_default(),
      Err(_) => return Ok(None),
    };

    if detect_result["found"].as_bool() != Some(true) {
      return Ok(None); // No OTP inputs found — caller falls back to API
    }

    self.log("UI OTP: found input fields, attempting humanized entry");

    let mode = detect_result["mode"].as_str().unwrap_or("single");

    if mode == "multi" {
      // Type each digit into its own input field
      let digits: Vec<char> = otp_code.chars().filter(|c| c.is_ascii_digit()).collect();
      let count = detect_result["count"].as_u64().unwrap_or(6) as usize;

      for (i, &digit) in digits.iter().enumerate().take(count) {
        let selector = detect_result["selectors"]
          .get(i)
          .and_then(|v| v.as_str())
          .unwrap_or("input[maxlength=\"1\"]");
        let digit_str = digit.to_string();

        // Click into the input first (human-like)
        let _ = self.click_selector(session, selector, "otp-digit").await;
        self.human_pause(80, 200).await;

        // Type the digit
        if session
          .human_type(
            selector,
            &digit_str,
            &crate::browser_actions::HumanProfile::form_fill(),
          )
          .await
          .is_err()
        {
          self.log(&format!(
            "UI OTP: failed to type digit {i}, aborting UI path"
          ));
          return Ok(None);
        }
        self.human_pause(100, 300).await;
      }
    } else {
      // Single input field — type the full code
      let selector = detect_result["selector"]
        .as_str()
        .unwrap_or("input[name=\"code\"]");

      let _ = self.click_selector(session, selector, "otp-input").await;
      self.human_pause(100, 250).await;

      if session
        .human_type(
          selector,
          otp_code,
          &crate::browser_actions::HumanProfile::form_fill(),
        )
        .await
        .is_err()
      {
        self.log("UI OTP: failed to type code, aborting UI path");
        return Ok(None);
      }
    }

    // Wait for auto-submit or click submit button
    self.human_pause(1500, 3000).await;

    // Check if page navigated (auto-submit worked)
    let new_url = session.current_url().await.unwrap_or_default();
    if !new_url.contains("email-verification") && !new_url.contains("email-otp") {
      self.log(&format!("UI OTP: auto-submitted → {new_url}"));
      return Ok(Some(new_url));
    }

    // Try clicking submit button
    for label in ["Continue", "Verify", "Submit", "Next", "Confirm"] {
      if self
        .click_by_text(session, label, "button, [role='button']")
        .await
        .is_ok()
      {
        self.human_pause(1500, 3000).await;
        let after_url = session.current_url().await.unwrap_or_default();
        if !after_url.contains("email-verification") && !after_url.contains("email-otp") {
          self.log(&format!("UI OTP: submitted via '{label}' → {after_url}"));
          return Ok(Some(after_url));
        }
        break;
      }
    }

    // UI didn't navigate — fall back to API
    self.log("UI OTP: no navigation detected, falling back to API");
    Ok(None)
  }

  /// Fill and submit the About You form through the visible UI.
  async fn submit_about_you_via_ui(
    &mut self,
    session: &mut BrowserSession,
    first_name: &str,
    last_name: &str,
    birthdate: &str,
  ) -> Result<(), String> {
    let detect_js = r#"(function(){
      const visible = (el) => {
        if (!el) return false;
        const r = el.getBoundingClientRect();
        const style = window.getComputedStyle(el);
        return r.width > 0 && r.height > 0 && style.visibility !== 'hidden' && style.display !== 'none';
      };
      const pick = (selectors) => {
        for (const selector of selectors) {
          const el = Array.from(document.querySelectorAll(selector)).find(visible);
          if (el) return el;
        }
        return null;
      };
      const selectorFor = (el, fallback) => {
        if (!el) return null;
        if (el.id) return '#' + CSS.escape(el.id);
        if (el.name) return el.tagName.toLowerCase() + '[name="' + CSS.escape(el.name) + '"]';
        return fallback;
      };
      const first = pick([
        'input[name="first_name"]', 'input[name="firstName"]', 'input[name="given_name"]',
        'input[autocomplete="given-name"]', 'input[id*="first" i]', 'input[placeholder*="First" i]',
        'input[aria-label*="First" i]'
      ]);
      const last = pick([
        'input[name="last_name"]', 'input[name="lastName"]', 'input[name="family_name"]',
        'input[autocomplete="family-name"]', 'input[id*="last" i]', 'input[placeholder*="Last" i]',
        'input[aria-label*="Last" i]'
      ]);
      const full = pick([
        'input[name="name"]', 'input[name="full_name"]', 'input[name="fullName"]',
        'input[autocomplete="name"]', 'input[id*="name" i]', 'input[placeholder*="Name" i]',
        'input[aria-label*="Name" i]'
      ]);
      const age = pick([
        'input[name="age"]', 'input[autocomplete="age"]', 'input[id*="age" i]',
        'input[placeholder*="Age" i]', 'input[aria-label*="Age" i]',
        'input[type="number"][min][max]'
      ]);
      const birth = pick([
        'input:not([type="hidden"])[name="birthdate"]',
        'input:not([type="hidden"])[name="birthday"]',
        'input:not([type="hidden"])[name="date_of_birth"]',
        'input:not([type="hidden"])[name="birth_date"]',
        'input:not([type="hidden"])[autocomplete="bday"]',
        'input:not([type="hidden"])[id*="birth" i]',
        'input:not([type="hidden"])[placeholder*="Birth" i]',
        'input:not([type="hidden"])[aria-label*="Birth" i]',
        'input[type="date"]'
      ]);
      const month = pick([
        'input[name="birth_month"]', 'input[name="month"]', 'input[autocomplete="bday-month"]',
        'select[name="birth_month"]', 'select[name="month"]', 'input[id*="month" i]', 'select[id*="month" i]'
      ]);
      const day = pick([
        'input[name="birth_day"]', 'input[name="day"]', 'input[autocomplete="bday-day"]',
        'select[name="birth_day"]', 'select[name="day"]', 'input[id*="day" i]', 'select[id*="day" i]'
      ]);
      const year = pick([
        'input[name="birth_year"]', 'input[name="year"]', 'input[autocomplete="bday-year"]',
        'select[name="birth_year"]', 'select[name="year"]', 'input[id*="year" i]', 'select[id*="year" i]'
      ]);
      const birthFormat = birth && birth.type === 'date' ? 'iso'
        : birth && /dd\s*[\/-]\s*mm|day.*month/i.test([birth.placeholder, birth.getAttribute('aria-label')].filter(Boolean).join(' ')) ? 'dmy'
        : birth && /mm\s*[\/-]\s*dd|month.*day/i.test([birth.placeholder, birth.getAttribute('aria-label')].filter(Boolean).join(' ')) ? 'mdy'
        : 'iso';
      return {
        firstSel: selectorFor(first, 'input[placeholder*="First" i]'),
        lastSel: selectorFor(last, 'input[placeholder*="Last" i]'),
        fullSel: selectorFor(full, 'input[autocomplete="name"]'),
        ageSel: selectorFor(age, 'input[name="age"]'),
        birthSel: selectorFor(birth, 'input[type="date"]'),
        birthFormat,
        monthSel: selectorFor(month, 'input[name="month"], select[name="month"]'),
        daySel: selectorFor(day, 'input[name="day"], select[name="day"]'),
        yearSel: selectorFor(year, 'input[name="year"], select[name="year"]')
      };
    })()"#;

    // The About You page is React-rendered: after the OTP succeed step the
    // visit can land on the about-you path before the form fields are painted.
    // Evaluate the detector up to 12 times with 1-2s spacing. React's router
    // sometimes briefly renders email-verification between re-renders — we
    // only bail when the URL has been off about-you for 3 consecutive loops.
    let mut detect: Option<serde_json::Value> = None;
    let mut off_about_you = 0_u32;
    for attempt in 1..=12_u32 {
      // Tiered wait: quick polls early (form renders in 2-4s), slower fallback
      // if the page is genuinely slow. Keeps the same total attempt count for
      // reliability but cuts typical detection time from ~10s to ~2s.
      let (min_ms, max_ms) = if attempt <= 4 {
        (150, 300)
      } else if attempt <= 8 {
        (350, 700)
      } else {
        (700, 1400)
      };
      self.human_pause(min_ms, max_ms).await;
      let cur_url = session.current_url().await.unwrap_or_default();
      if !cur_url.contains("about-you") {
        off_about_you += 1;
        self.log(&format!(
          "UI About You: not on about-you page on attempt {attempt}/12 (url={cur_url})"
        ));
        if off_about_you >= 3 {
          return Err(format!("left about-you before form detection: {cur_url}"));
        }
        continue;
      }
      off_about_you = 0;
      match session.evaluate(detect_js, false).await {
        Ok(res) => {
          if let Some(value) = res.get("value").cloned() {
            let has_name = value
              .get("firstSel")
              .and_then(serde_json::Value::as_str)
              .is_some()
              || value
                .get("fullSel")
                .and_then(serde_json::Value::as_str)
                .is_some();
            let has_birth = value
              .get("ageSel")
              .and_then(serde_json::Value::as_str)
              .is_some()
              || value
                .get("birthSel")
                .and_then(serde_json::Value::as_str)
                .is_some()
              || (value
                .get("monthSel")
                .and_then(serde_json::Value::as_str)
                .is_some()
                && value
                  .get("daySel")
                  .and_then(serde_json::Value::as_str)
                  .is_some()
                && value
                  .get("yearSel")
                  .and_then(serde_json::Value::as_str)
                  .is_some());
            if has_name && has_birth {
              detect = Some(value);
              self.log(&format!(
                "UI About You: form detected on attempt {attempt}/12"
              ));
              break;
            }
          }
          self.log(&format!(
            "UI About You: form fields incomplete on attempt {attempt}/12"
          ));
        }
        Err(e) => self.log(&format!(
          "UI About You: detect evaluate failed on attempt {attempt}: {e}"
        )),
      }
    }
    let detect =
      detect.ok_or_else(|| "About You form fields not detected after 12 retries".to_string())?;

    let first_sel = detect["firstSel"].as_str();
    let last_sel = detect["lastSel"].as_str();
    let full_sel = detect["fullSel"].as_str();
    let age_sel = detect["ageSel"].as_str();
    let birth_sel = detect["birthSel"].as_str();
    let birth_format = detect["birthFormat"].as_str().unwrap_or("iso");
    let month_sel = detect["monthSel"].as_str();
    let day_sel = detect["daySel"].as_str();
    let year_sel = detect["yearSel"].as_str();

    if first_sel.is_none() && full_sel.is_none() {
      snapshot_about_you_page(session, "detect-name-missing").await;
      return Err("name input not found on About You form".into());
    }
    if first_sel.is_some() && last_sel.is_none() {
      snapshot_about_you_page(session, "detect-last-missing").await;
      return Err("last name input not found on About You form".into());
    }
    let birth_mode = resolve_about_you_birth_mode(
      age_sel.is_some(),
      birth_sel.is_some(),
      month_sel.is_some() && day_sel.is_some() && year_sel.is_some(),
    );
    if birth_mode.is_none() {
      snapshot_about_you_page(session, "detect-birth-missing").await;
      return Err("age or birthdate inputs not found on About You form".into());
    }
    let birth_mode = birth_mode.unwrap();
    let (year, month, day, age) = birthdate_parts(birthdate)?;
    let single_birthdate = match birth_format {
      "dmy" => format!("{day}/{month}/{year}"),
      "mdy" => format!("{month}/{day}/{year}"),
      _ => birthdate.to_string(),
    };
    let mut expected_fields = Vec::new();

    self.log("UI About You: found form fields, attempting humanized fill");
    let _ = session.evaluate("window.scrollBy(0, 150)", false).await;
    self.human_pause(600, 1200).await;

    if let (Some(first_sel), Some(last_sel)) = (first_sel, last_sel) {
      self
        .fill_about_you_field(session, first_sel, first_name, "first name")
        .await?;
      expected_fields.push((first_sel.to_string(), first_name.to_string(), "first name"));
      self.human_pause(400, 900).await;

      self
        .fill_about_you_field(session, last_sel, last_name, "last name")
        .await?;
      expected_fields.push((last_sel.to_string(), last_name.to_string(), "last name"));
      self.human_pause(400, 900).await;
    } else if let Some(full_sel) = full_sel {
      let full_name = format!("{first_name} {last_name}");
      self
        .fill_about_you_field(session, full_sel, &full_name, "full name")
        .await?;
      expected_fields.push((full_sel.to_string(), full_name, "full name"));
      self.human_pause(400, 900).await;
    }

    match birth_mode {
      AboutYouBirthMode::Age => {
        let age_sel = age_sel.unwrap_or_default();
        self
          .fill_about_you_field(session, age_sel, &age, "age")
          .await?;
        expected_fields.push((age_sel.to_string(), age.clone(), "age"));
      }
      AboutYouBirthMode::SingleDate => {
        let birth_sel = birth_sel.unwrap_or_default();
        self
          .fill_about_you_field(session, birth_sel, &single_birthdate, "birthdate")
          .await?;
        expected_fields.push((birth_sel.to_string(), single_birthdate.clone(), "birthdate"));
      }
      AboutYouBirthMode::SplitDate => {
        for (selector, value, label) in [
          (month_sel.unwrap_or_default(), month.as_str(), "birth month"),
          (day_sel.unwrap_or_default(), day.as_str(), "birth day"),
          (year_sel.unwrap_or_default(), year.as_str(), "birth year"),
        ] {
          self
            .fill_about_you_field(session, selector, value, label)
            .await?;
          expected_fields.push((selector.to_string(), value.to_string(), label));
          self.human_pause(250, 600).await;
        }
      }
    }

    self.human_pause(400, 700).await;
    for (selector, expected, label) in expected_fields {
      self
        .verify_about_you_field(session, &selector, &expected, label)
        .await?;
    }

    // React-controlled submit buttons re-enable only after their store observes
    // the final input change. Wait up to 4s for the disabled state to clear
    // before attempting a click; otherwise the click is a no-op and the page
    // never advances (observed live: "Finish creating account" aria-disabled
    // true even though all fields were filled).
    let submit_ready_js = r#"(function(){
      const btns = Array.from(document.querySelectorAll('button[type="submit"], button:not([type]), [role="button"]'))
        .filter((el) => {
          const r = el.getBoundingClientRect();
          if (r.width <= 0 || r.height <= 0) return false;
          if (el.disabled) return false;
          if ((el.getAttribute('aria-disabled') || '') === 'true') return false;
          const t = (el.innerText || el.textContent || '').toLowerCase();
          return /finish|continue|create|submit|sign up|next/i.test(t);
        });
      return { ready: btns.length > 0, count: btns.length };
    })()"#;
    let mut submit_enabled = false;
    for _ in 0..25 {
      if let Ok(res) = session.evaluate(submit_ready_js, false).await {
        let ready = res
          .get("value")
          .and_then(|v| v.get("ready"))
          .and_then(serde_json::Value::as_bool)
          == Some(true);
        if ready {
          submit_enabled = true;
          break;
        }
      }
      self.human_pause(100, 150).await;
    }
    let mut via_force = false;
    if !submit_enabled {
      self.log("UI About You: submit still disabled after 4s; forcing form.requestSubmit()");
      eprintln!("UI About You: submit still disabled after 4s; forcing form.requestSubmit()");
      // React's <form onSubmit> handler receives submit events even when the
      // submit button is aria-disabled — the disabled state only affects the
      // button, not the form. requestSubmit() queues a real submit event that
      // goes through the same path as the button click. Form's `noValidate`
      // option is honored; we let React handle server-side validation.
      let force_js = r#"(function(){
        const form = document.querySelector('form');
        if (!form) return { ok: false, reason: 'no_form' };
        // Re-validate React state by dispatching a focus+blur cycle on each
        // visible input before submitting; this often clears stale disabled.
        const visibleInputs = Array.from(document.querySelectorAll('input, select, textarea'))
          .filter((el) => {
            const r = el.getBoundingClientRect();
            return r.width > 0 && r.height > 0;
          });
        visibleInputs.forEach((el) => {
          try { el.focus(); el.blur(); } catch (_) {}
        });
        if (typeof form.requestSubmit === 'function') {
          form.requestSubmit();
          return { ok: true, via: 'requestSubmit' };
        }
        form.dispatchEvent(new SubmitEvent('submit', { bubbles: true, cancelable: true }));
        return { ok: true, via: 'dispatch' };
      })()"#;
      if let Ok(res) = session.evaluate(force_js, false).await {
        let via = res
          .get("value")
          .and_then(|v| v.get("via"))
          .and_then(serde_json::Value::as_str)
          .unwrap_or("?");
        let ok = res
          .get("value")
          .and_then(|v| v.get("ok"))
          .and_then(serde_json::Value::as_bool)
          == Some(true);
        if ok {
          self.log(&format!("UI About You: forced form submit via {via}"));
          eprintln!("UI About You: forced form submit via {via}");
          via_force = true;
        }
      }
    }

    let mut submitted = via_force;
    if !submitted {
      for label in [
        "Continue",
        "Submit",
        "Create account",
        "Finish creating account",
        "Next",
        "Sign up",
      ] {
        if self
          .click_by_text(session, label, "button, [role='button']")
          .await
          .is_ok()
        {
          submitted = true;
          self.log(&format!("UI About You: clicked '{label}'"));
          break;
        }
      }
    }
    if !submitted {
      self
        .click_selector(session, "button[type='submit']", "about-you submit")
        .await
        .map_err(|e| format!("submit button not found: {e}"))?;
    }

    // Wait for page navigation after submit. The form has aria-busy="true"
    // while processing, so we poll both URL and button state for up to 20s.
    // Server-side validation can take 5-15s on auth.openai.com.
    let mut final_url = String::new();
    for attempt in 1..=20 {
      self.human_pause(400, 700).await;
      let cur_url = session.current_url().await.unwrap_or_default();
      if !cur_url.contains("about-you") {
        final_url = cur_url.clone();
        self.log(&format!(
          "UI About You: navigated to {cur_url} after {attempt} poll(s)"
        ));
        break;
      }
      // Still on about-you — check if button is still busy (processing)
      // or if it has re-enabled (validation error).
      let check_js = r#"(function(){
        const btn = document.querySelector('button[type="submit"], button:not([type])');
        if (!btn) return { busy: false, reason: 'no_button' };
        const r = btn.getBoundingClientRect();
        if (r.width <= 0 || r.height <= 0) return { busy: false, reason: 'not_visible' };
        const busy = btn.getAttribute('aria-busy') === 'true' || btn.disabled;
        return { busy, reason: 'checking' };
      })()"#;
      if let Ok(res) = session.evaluate(check_js, false).await {
        let busy = res
          .get("value")
          .and_then(|v| v.get("busy"))
          .and_then(serde_json::Value::as_bool)
          == Some(true);
        if busy && attempt < 20 {
          self.log(&format!(
            "UI About You: form still processing on attempt {attempt}/20"
          ));
          continue;
        }
      }
      if attempt == 20 {
        final_url = cur_url.clone();
        self.log(&format!(
          "UI About You: timeout after 20 polls, still on {cur_url}"
        ));
      }
    }
    if final_url.contains("about-you") {
      // Diagnostics (no secrets): dump the About You DOM state so the operator
      // can see why submit did not advance the page — visible vs disabled
      // submit buttons, the form's current field values, and any inline error
      // text. This is the only evidence we get without a step-log surfacing.
      let snap_js = r#"(function(){
        const form = document.querySelector('form') || document.body;
        const visible = (el) => {
          if (!el) return false;
          const r = el.getBoundingClientRect();
          return r.width > 0 && r.height > 0;
        };
        const inputs = Array.from(document.querySelectorAll('input, select, textarea'))
          .filter(visible)
          .map((el) => ({
            name: el.name || el.id || el.getAttribute('aria-label') || '',
            value: String(el.value || '').length > 30
              ? String(el.value || '').slice(0, 30) + '...'
              : String(el.value || ''),
            kind: el.tagName.toLowerCase() + (el.type ? '[' + el.type + ']' : ''),
            disabled: !!el.disabled,
          }));
        const buttons = Array.from(document.querySelectorAll('button, [role="button"], input[type="submit"]'))
          .filter(visible)
          .map((el) => ({
            text: ((el.innerText || el.textContent || '') + '').trim().slice(0, 40),
            type: el.getAttribute('type') || '',
            disabled: !!el.disabled,
            aria: el.getAttribute('aria-disabled') || '',
          }));
        const active = document.activeElement
          ? (document.activeElement.tagName + '#' + (document.activeElement.id || '')
             + ' name=' + (document.activeElement.getAttribute('name') || ''))
          : 'none';
        const errors = Array.from(document.querySelectorAll('[role="alert"], [class*="error" i], [data-error]'))
          .filter(visible)
          .map((el) => ((el.innerText || el.textContent || '') + '').trim().slice(0, 140))
          .filter((t) => t.length > 0);
        return { inputs, buttons, active, errors };
      })()"#;
      if let Ok(snap) = session.evaluate(snap_js, false).await {
        let value = snap.get("value").cloned().unwrap_or_default();
        let active = value
          .get("active")
          .and_then(serde_json::Value::as_str)
          .unwrap_or("?");
        let inputs = value
          .get("inputs")
          .and_then(|v| v.as_array())
          .map(|arr| serde_json::to_string(arr).unwrap_or_default())
          .unwrap_or_default();
        let buttons = value
          .get("buttons")
          .and_then(|v| v.as_array())
          .map(|arr| serde_json::to_string(arr).unwrap_or_default())
          .unwrap_or_default();
        let errors = value
          .get("errors")
          .and_then(|v| v.as_array())
          .map(|arr| serde_json::to_string(arr).unwrap_or_default())
          .unwrap_or_default();
        let summary = format!(
          "UI About You: still on about-you; active={} inputs={} buttons={} errors={}",
          active, inputs, buttons, errors
        );
        self.log(&summary);
        // Emit to stderr/stdout too because the failing account is not
        // persisted (fail-closed), so the operator needs the snapshot without
        // a separate audit step.
        eprintln!("{summary}");
      } else {
        self.log("UI About You: form snapshot evaluation failed");
        eprintln!("UI About You: form snapshot evaluation failed");
      }
      snapshot_about_you_page(session, "post-submit-still-on-page").await;
      return Err(format!("still on about-you page after submit: {final_url}"));
    }
    self.log(&format!("UI About You: navigated to {final_url}"));
    Ok(())
  }

  async fn force_api_register(
    &mut self,
    session: &mut BrowserSession,
    alias_email: &str,
    password: &str,
  ) -> Result<(), String> {
    let reg_js = format!(
      "fetch('https://auth.openai.com/api/accounts/user/register', {{ method: 'POST', credentials: 'include', headers: {{ 'content-type': 'application/json', accept: 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{ username: '{email}', password: '{pw}' }}) }})",
      did = self.device_id,
      email = alias_email,
      pw = password,
    );
    let reg = session.fetch_json(&reg_js).await?;
    let st = reg["_status"].as_u64().unwrap_or(200);
    if st != 200 || reg.get("error").is_some() {
      return Err(format!("registration request failed with HTTP {st}"));
    }
    Ok(())
  }

  /// Poll `/api/auth/session` with human-like waits. Session cookies often land
  /// a few seconds after ChatGPT home paints (live miss: johnbeasley token empty).
  async fn extract_access_token_with_retry(
    &mut self,
    session: &mut BrowserSession,
    prefix: &str,
  ) -> (String, String) {
    if let Err(e) = session.navigate("https://chatgpt.com/", 45).await {
      self.log(&format!("{prefix} token-extract navigate warn: {e}"));
    }
    self.human_pause(1500, 2800).await;

    for attempt in 1..=5 {
      let mut auth_session = session
        .fetch_json(
          "fetch('https://chatgpt.com/api/auth/session', { credentials: 'include', headers: { accept: 'application/json' } })",
        )
        .await
        .unwrap_or_else(|e| {
          self.log(&format!(
            "{prefix} absolute session fetch failed (attempt {attempt}): {e}"
          ));
          serde_json::json!({})
        });
      if auth_session
        .get("accessToken")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .is_empty()
      {
        auth_session = session
          .fetch_json(
            "fetch('/api/auth/session', { credentials: 'include', headers: { accept: 'application/json' } })",
          )
          .await
          .unwrap_or_else(|e| {
            self.log(&format!(
              "{prefix} relative session fetch failed (attempt {attempt}): {e}"
            ));
            serde_json::json!({})
          });
      }

      let access_token = auth_session["accessToken"]
        .as_str()
        .unwrap_or("")
        .to_string();
      let account_id = auth_session
        .get("account")
        .and_then(|a| a["id"].as_str())
        .or_else(|| auth_session.get("user").and_then(|u| u["id"].as_str()))
        .unwrap_or("")
        .to_string();

      if !access_token.is_empty() {
        if attempt > 1 {
          self.log(&format!(
            "{prefix} accessToken recovered on attempt {attempt}/5"
          ));
        }
        return (access_token, account_id);
      }

      self.log(&format!(
        "{prefix} accessToken empty on attempt {attempt}/5; waiting before retry"
      ));
      // Soft reload home once mid-way — session cookie can finish after SPA boot.
      if attempt == 3 {
        let _ = session.navigate("https://chatgpt.com/", 30).await;
      }
      self
        .human_pause(1200 + attempt as u64 * 400, 2200 + attempt as u64 * 600)
        .await;
    }
    (String::new(), String::new())
  }

  /// Fill password field + submit create-password form (recording path).
  async fn submit_password_via_ui(
    &mut self,
    session: &mut BrowserSession,
    password: &str,
  ) -> Result<(), String> {
    for _ in 0..10 {
      if self
        .page_has_selector(
          session,
          r#"input[name="new-password"], input[type="password"], input[autocomplete="new-password"]"#,
        )
        .await
      {
        break;
      }
      self.human_pause(350, 450).await;
    }

    self
      .fill_input(
        session,
        r#"input[name="new-password"], input[type="password"], input[autocomplete="new-password"], input[placeholder="Password"]"#,
        password,
      )
      .await?;
    self.human_pause(350, 450).await;

    let mut submitted = false;
    if self
      .click_selector(
        session,
        r#"form[action*="password"] button[type="submit"], button[type="submit"]"#,
        "password submit",
      )
      .await
      .is_ok()
    {
      submitted = true;
    }
    if !submitted {
      for label in ["Continue", "Next", "Sign up"] {
        if self.click_by_text(session, label, "button").await.is_ok() {
          submitted = true;
          break;
        }
      }
    }
    if !submitted {
      let js = r#"(function(){
        const el = document.querySelector('input[name="new-password"], input[type="password"]');
        if (!el) return false;
        el.focus();
        const form = el.form || el.closest('form');
        if (form) { form.requestSubmit ? form.requestSubmit() : form.submit(); return true; }
        return false;
      })()"#;
      let res = session.evaluate(js, false).await?;
      if res["value"].as_bool() != Some(true) {
        return Err("could not submit password form".into());
      }
    }

    for _ in 0..30 {
      self.human_pause(450, 550).await;
      let u = session.current_url().await.unwrap_or_default();
      if u.contains("email-verification")
        || u.contains("about-you")
        || u.contains("email-otp")
        || u.contains("chatgpt.com")
      {
        return Ok(());
      }
      if !u.contains("password") && u.contains("auth.openai.com") {
        return Ok(());
      }
    }
    let final_url = session.current_url().await.unwrap_or_default();
    if final_url.contains("password") {
      return Err(format!("still on password page: {final_url}"));
    }
    Ok(())
  }

  /// Drive Settings  /// Probe ChatGPT subscription/checkout endpoints and page content for free trial / free Plus offer.
  /// Returns (eligible, plan_type, detail).
  async fn check_free_trial_offer(
    &mut self,
    session: &mut BrowserSession,
    access_token: &str,
    account_id: &str,
  ) -> (bool, String, String) {
    let mut plan_type = String::new();
    let mut detail_parts: Vec<String> = Vec::new();
    let mut eligible = false;

    if !access_token.is_empty() {
      let auth_header = format!("'Authorization': 'Bearer {access_token}'");
      let account_header = if account_id.is_empty() {
        String::new()
      } else {
        format!(", 'ChatGPT-Account-ID': '{account_id}'")
      };

      // PRIMARY (verified live): accounts/check exposes eligible_promo_campaigns.
      let check_js = format!(
        "fetch('https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27', {{ credentials: 'include', headers: {{ accept: 'application/json', {auth}{acct} }} }})",
        auth = auth_header,
        acct = account_header,
      );
      match session.fetch_json(&check_js).await {
        Ok(check) => {
          detail_parts.push("accounts_check_ok".into());
          if let Some((ok, plan, why)) = evaluate_subscription_json(&check) {
            eligible |= ok;
            if !plan.is_empty() {
              plan_type = plan;
            }
            detail_parts.push(why);
          }
        }
        Err(e) => detail_parts.push(format!("accounts_check_err={e}")),
      }

      // SECONDARY: subscriptions requires account_id query param.
      if !account_id.is_empty() {
        let subs_js = format!(
          "fetch('https://chatgpt.com/backend-api/subscriptions?account_id={account_id}', {{ credentials: 'include', headers: {{ accept: 'application/json', {auth}{acct} }} }})",
          account_id = account_id,
          auth = auth_header,
          acct = account_header,
        );
        match session.fetch_json(&subs_js).await {
          Ok(subs) => {
            detail_parts.push("subscriptions_ok".into());
            if let Some((ok, plan, why)) = evaluate_subscription_json(&subs) {
              eligible |= ok;
              if plan_type.is_empty() && !plan.is_empty() {
                plan_type = plan;
              }
              detail_parts.push(why);
            }
          }
          Err(e) => detail_parts.push(format!("subscriptions_err={e}")),
        }
      }
    } else {
      detail_parts.push("no_access_token".into());
    }

    let _ = session.navigate("https://chatgpt.com/", 20).await;
    self.human_pause(800, 1200).await;
    if let Ok(dom) = session
      .evaluate(
        r#"(function(){
          const t = (document.body && (document.body.innerText || document.body.textContent) || '').toLowerCase();
          return {
            hasFreeTrial: /free trial|try plus free|try it free|start free trial|get plus free|claim offer|free offer|free for \d|0 ?đ|0 ?d|mi[eẽ]n ph[ií]/.test(t),
            hasPlusOffer: /chatgpt plus|upgrade to plus|get plus|try plus/.test(t)
          };
        })()"#,
        false,
      )
      .await
    {
      let value = dom.get("value").cloned().unwrap_or(dom);
      detail_parts.push("dom_probe_ok".into());
      if value["hasFreeTrial"].as_bool() == Some(true) {
        eligible = true;
        if plan_type.is_empty() {
          plan_type = "ui-free-trial".into();
        }
        detail_parts.push("dom_free_trial".into());
      } else if value["hasPlusOffer"].as_bool() == Some(true) {
        detail_parts.push("dom_plus_marketing_only".into());
      }
    }

    if plan_type.is_empty() {
      plan_type = if eligible {
        "free-trial".into()
      } else {
        "none".into()
      };
    }
    (eligible, plan_type, detail_parts.join(" | "))
  }

  /// Enable ChatGPT authenticator 2FA through the shared fail-closed flow.
  async fn enable_2fa(&mut self, session: &mut BrowserSession) -> Result<String, String> {
    let mut adapter = BrowserTwoFactorAdapter::new(session);
    enable_authenticator_two_factor(&mut adapter, |_secret| Ok(()))
      .await
      .map_err(|error| error.to_string())
  }

  async fn fill_about_you_field(
    &mut self,
    session: &mut BrowserSession,
    selector: &str,
    value: &str,
    label: &str,
  ) -> Result<(), String> {
    // About You form inputs are React-controlled. CDP keyboard events
    // (human_type) go to `document.activeElement` which is BODY when Chromium
    // runs in background — React never commits the value. Instead we use the
    // React-compatible JS pattern: native value-setter + InputInput + change
    // + blur, which works regardless of OS-level focus.
    //
    // The canonical sequence React listens to:
    //   1. el.focus()        — React attaches onChange handler
    //   2. native value set  — sets the actual DOM property
    //   3. InputEvent        — React reads e.target.value and commits
    //   4. change + blur     — form-level validation re-runs, button re-enables
    let js = format!(
      r#"(function(){{
    const el = document.querySelector({selector});
    if (!el) return {{ ok: false, reason: 'not_found' }};
    const wanted = {value};
    el.focus();

    if (el.tagName === 'SELECT') {{
      const wantedNumber = /^\d+$/.test(wanted) ? Number(wanted) : null;
      const monthNames = ['january', 'february', 'march', 'april', 'may', 'june',
        'july', 'august', 'september', 'october', 'november', 'december'];
      const option = Array.from(el.options || []).find((candidate) => {{
        const optionValue = String(candidate.value || '').trim();
        const optionText = String(candidate.textContent || '').trim();
        if (optionValue === wanted || optionText === wanted) return true;
        if (wantedNumber !== null &&
            (Number(optionValue) === wantedNumber || Number(optionText) === wantedNumber)) {{
          return true;
        }}
        return {is_month} && wantedNumber !== null &&
          monthNames.indexOf(optionText.toLowerCase()) + 1 === wantedNumber;
      }});
      if (!option) return {{ ok: false, reason: 'option_not_found' }};
      const descriptor = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value');
      if (descriptor && descriptor.set) descriptor.set.call(el, option.value);
      else el.value = option.value;
    }} else {{
      const proto = window.HTMLInputElement && window.HTMLInputElement.prototype;
      const desc = proto && Object.getOwnPropertyDescriptor(proto, 'value');
      // Clear then set so React sees a transition even from a stale prior value.
      if (desc && desc.set) desc.set.call(el, '');
      else el.value = '';
      el.dispatchEvent(new InputEvent('input', {{ bubbles: true, data: '' }}));

      if (desc && desc.set) desc.set.call(el, wanted);
      else el.value = wanted;
    }}
    // React commits the controlled value only when it observes an input event
    // with the current e.target.value. InputEvent with the final data is the
    // most portable contract across React versions.
    el.dispatchEvent(new InputEvent('input', {{ bubbles: true, data: wanted }}));
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
    // blur is critical: React form libraries re-run validation on blur and
    // re-evaluate the submit-button disabled state.
    el.dispatchEvent(new Event('blur', {{ bubbles: true }}));
    return {{ ok: true, value: String(el.value || '') }};
  }})()"#,
      selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
      value = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
      is_month = label == "birth month",
    );
    let result = session.evaluate(&js, false).await?;
    let value_obj = result.get("value").cloned().unwrap_or_default();
    if value_obj.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
      let reason = value_obj
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("failed");
      return Err(format!("fill {label}: {reason}"));
    }

    self
      .verify_about_you_field(session, selector, value, label)
      .await
  }

  async fn verify_about_you_field(
    &mut self,
    session: &mut BrowserSession,
    selector: &str,
    expected: &str,
    label: &str,
  ) -> Result<(), String> {
    let js = format!(
      r#"(function(){{
        const el = document.querySelector({selector});
        if (!el) return {{ ok: false, reason: 'not_found' }};
        const actual = String(el.value || '').trim();
        const expected = String({expected}).trim();
        const sameNumber = /^\d+$/.test(actual) && /^\d+$/.test(expected) &&
          Number(actual) === Number(expected);
        const monthNames = ['january', 'february', 'march', 'april', 'may', 'june',
          'july', 'august', 'september', 'october', 'november', 'december'];
        const sameMonth = {is_month} && /^\d+$/.test(expected) &&
          monthNames.indexOf(actual.toLowerCase()) + 1 === Number(expected);
        return {{ ok: actual === expected || sameNumber || sameMonth, reason: 'value_mismatch' }};
      }})()"#,
      selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
      expected = serde_json::to_string(expected).unwrap_or_else(|_| "\"\"".into()),
      is_month = label == "birth month",
    );
    let result = session.evaluate(&js, false).await?;
    if result
      .get("value")
      .and_then(|value| value.get("ok"))
      .and_then(serde_json::Value::as_bool)
      == Some(true)
    {
      Ok(())
    } else {
      Err(format!("verify {label}: value was not accepted"))
    }
  }

  async fn click_selector(
    &mut self,
    session: &mut BrowserSession,
    selector: &str,
    label: &str,
  ) -> Result<(), String> {
    use crate::browser_actions::{click_point_in_rect, HumanProfile};

    let js = format!(
      r#"(function(){{
        const el = document.querySelector({sel});
        if (!el) return {{ ok: false, reason: 'not_found' }};
        const r = el.getBoundingClientRect();
        if (r.width <= 0 || r.height <= 0) return {{ ok: false, reason: 'not_visible' }};
        el.scrollIntoView({{ block: 'center', inline: 'center' }});
        const r2 = el.getBoundingClientRect();
        return {{
          ok: true,
          x: r2.left,
          y: r2.top,
          w: r2.width,
          h: r2.height
        }};
      }})()"#,
      sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
    );
    let result = session.evaluate(&js, false).await?;
    let value = result
      .get("value")
      .cloned()
      .ok_or_else(|| format!("{label}: no evaluate value"))?;
    if value["ok"].as_bool() != Some(true) {
      return Err(format!(
        "{label}: {}",
        value["reason"].as_str().unwrap_or("not found")
      ));
    }
    let x = value["x"]
      .as_f64()
      .ok_or_else(|| format!("{label}: no x"))?;
    let y = value["y"]
      .as_f64()
      .ok_or_else(|| format!("{label}: no y"))?;
    let w = value["w"].as_f64().unwrap_or(1.0);
    let h = value["h"].as_f64().unwrap_or(1.0);
    let (tx, ty) = click_point_in_rect(x, y, w, h);
    session
      .human_click((tx, ty), &HumanProfile::careful())
      .await
  }

  async fn click_by_text(
    &mut self,
    session: &mut BrowserSession,
    text: &str,
    css_filter: &str,
  ) -> Result<(), String> {
    use crate::browser_actions::{click_point_in_rect, HumanProfile};

    let js = format!(
      r#"(function(){{
        const needle = {text}.toLowerCase();
        const nodes = Array.from(document.querySelectorAll({filter}));
        for (const el of nodes) {{
          const t = (el.innerText || el.textContent || '').trim().toLowerCase();
          if (!t.includes(needle)) continue;
          const r = el.getBoundingClientRect();
          if (r.width <= 0 || r.height <= 0) continue;
          el.scrollIntoView({{ block: 'center', inline: 'center' }});
          const r2 = el.getBoundingClientRect();
          return {{
            ok: true,
            x: r2.left,
            y: r2.top,
            w: r2.width,
            h: r2.height
          }};
        }}
        return {{ ok: false }};
      }})()"#,
      text = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into()),
      filter = serde_json::to_string(css_filter).unwrap_or_else(|_| "\"*\"".into()),
    );
    let result = session.evaluate(&js, false).await?;
    let value = result
      .get("value")
      .cloned()
      .ok_or_else(|| format!("text '{text}': no evaluate value"))?;
    if value["ok"].as_bool() != Some(true) {
      return Err(format!("text '{text}' not found"));
    }
    let x = value["x"]
      .as_f64()
      .ok_or_else(|| format!("text '{text}': no x"))?;
    let y = value["y"]
      .as_f64()
      .ok_or_else(|| format!("text '{text}': no y"))?;
    let w = value["w"].as_f64().unwrap_or(1.0);
    let h = value["h"].as_f64().unwrap_or(1.0);
    let (tx, ty) = click_point_in_rect(x, y, w, h);
    session
      .human_click((tx, ty), &HumanProfile::careful())
      .await
  }

  /// Humanized field fill (Markov keystrokes). Falls back to JS value set only if typing fails.
  async fn fill_input(
    &mut self,
    session: &mut BrowserSession,
    selector: &str,
    value: &str,
  ) -> Result<(), String> {
    use crate::browser_actions::HumanProfile;

    match session
      .human_type(selector, value, &HumanProfile::form_fill())
      .await
    {
      Ok(()) => return Ok(()),
      Err(e) => {
        self.log(&format!("human_type fallback after: {e}"));
      }
    }

    // Fallback: instant JS set (machine-like) — only if human path failed.
    let js = format!(
      r#"(function(){{
        const el = document.querySelector({sel});
        if (!el) return {{ ok: false, reason: 'not_found' }};
        el.focus();
        if (el.tagName === 'SELECT') {{
          const wanted = {val};
          const option = Array.from(el.options || []).find((opt) =>
            opt.value === wanted || opt.textContent.trim() === wanted || opt.textContent.trim().startsWith(wanted)
          );
          if (option) el.value = option.value;
          else el.value = wanted;
          el.dispatchEvent(new Event('input', {{ bubbles: true }}));
          el.dispatchEvent(new Event('change', {{ bubbles: true }}));
          return {{ ok: true, value: el.value }};
        }}
        const proto = window.HTMLInputElement && window.HTMLInputElement.prototype;
        const desc = proto && Object.getOwnPropertyDescriptor(proto, 'value');
        if (desc && desc.set) desc.set.call(el, {val});
        else el.value = {val};
        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
        return {{ ok: true, value: el.value }};
      }})()"#,
      sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
      val = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
    );
    let result = session.evaluate(&js, false).await?;
    let value_json = result
      .get("value")
      .cloned()
      .ok_or_else(|| "fill_input: no value".to_string())?;
    if value_json["ok"].as_bool() != Some(true) {
      return Err(format!(
        "fill_input: {}",
        value_json["reason"].as_str().unwrap_or("failed")
      ));
    }
    Ok(())
  }

  // -----------------------------------------------------------------------
  // Authorize with retry + Cloudflare handling
  // -----------------------------------------------------------------------

  /// First establish session on chatgpt.com homepage (Cloudflare clearance),
  /// then navigate to chatgpt.com/auth/login, fill email in the dedicated
  /// login form, click Continue, and wait for redirect to auth.openai.com.
  /// No API calls — pure UI interaction matching real human behavior.
  async fn navigate_to_signup_page(
    &mut self,
    session: &mut BrowserSession,
    email: &str,
  ) -> Result<String, String> {
    // If already on auth.openai.com, nothing to do.
    let cur = session.current_url().await.unwrap_or_default();
    if cur.contains("auth.openai.com") {
      self.log(&format!("Already on auth.openai.com: {cur}"));
      return Ok(cur);
    }

    // Step 1: Establish session on chatgpt.com homepage first.
    // This ensures Cloudflare cookies / Turnstile clearance is obtained
    // before navigating to auth/login on the same origin.
    if !cur.starts_with("https://chatgpt.com") {
      session.navigate("https://chatgpt.com/", 25).await?;
      self.human_pause(1500, 2800).await;
      // Let Cloudflare challenge auto-resolve if present.
      let _ = session.evaluate("window.scrollBy(0, 200)", false).await;
      self.human_pause(800, 1500).await;
    } else {
      self.log(&format!(
        "Already on chatgpt.com: {cur}, using existing session"
      ));
    }

    // Step 2: Navigate to the dedicated login/signup page (same origin).
    // This page has a stable form with email input and Continue button.
    session
      .navigate("https://chatgpt.com/auth/login", 25)
      .await?;
    self.human_pause(1200, 2200).await;

    // Step 3: Find and fill the email input on the login form.
    let email_selectors = r#"input[type="email"], input[name="email"], input[id="email"], input[autocomplete*="email"], input[placeholder*="email" i], input[aria-label*="email" i]"#;
    let mut filled = false;
    for _ in 0..15 {
      self.human_pause(300, 500).await;
      if self.page_has_selector(session, email_selectors).await {
        self.fill_input(session, email_selectors, email).await?;
        self.human_pause(350, 500).await;
        filled = true;
        break;
      }
    }
    if !filled {
      return Err("Email input not found on chatgpt.com/auth/login".into());
    }
    self.log("Filled email on auth/login page");

    // Step 4: Click the "Continue" button.
    let mut continued = false;
    for sel in [
      r#"button[type="submit"]"#,
      r#"button[name="intent"]"#,
      r#"form button"#,
    ] {
      if self
        .click_selector(session, sel, "login continue")
        .await
        .is_ok()
      {
        continued = true;
        break;
      }
    }
    if !continued {
      let _ = self
        .click_by_text(session, "Continue", "button, [role='button']")
        .await;
    }
    self.log("Clicked Continue on auth/login page");
    self.human_pause(1000, 2000).await;

    // Step 5: Wait for navigation to auth.openai.com.
    for _ in 0..25 {
      self.human_pause(400, 600).await;
      let cur = session.current_url().await.unwrap_or_default();
      if cur.contains("auth.openai.com") {
        self.log(&format!("Landed on auth.openai.com: {cur}"));
        return Ok(cur);
      }
    }

    // Return whatever URL we ended on (caller will retry if wrong).
    let cur = session.current_url().await.unwrap_or_default();
    self.log(&format!("navigate_to_signup_page final URL: {cur}"));
    Ok(cur)
  }

  /// Navigate to auth.openai.com signup page via UI (click "Sign up" on
  /// homepage → fill email in dialog → Continue) with browser-relaunch
  /// retry on failure / Cloudflare block.
  async fn authorize_with_retry(
    &mut self,
    session: &mut BrowserSession,
    profile: &mut crate::profile::BrowserProfile,
    email: &str,
    app_handle: &tauri::AppHandle,
  ) -> Result<String, String> {
    let max_attempts = 3;

    for attempt in 0..max_attempts {
      if attempt > 0 {
        self.log(&format!(
          "Authorize retry {attempt}/{max_attempts}: relaunching same worker with new fingerprint..."
        ));
        // Human-like backoff before kill/relaunch thrash.
        self.human_pause(1500, 3200).await;
        // Kill only — keep the same worker profile metadata for relaunch.
        // Abort the retry chain on kill failure: relaunching would stack a
        // second browser on the still-running one and freeze the machine.
        self.kill_browser_only(app_handle, profile).await?;

        // Relaunch the same worker with a new fingerprint and the same account identity.
        let (new_profile, new_session) = self.launch_and_connect(app_handle).await?;
        *session = new_session;
        *profile = new_profile;

        // Re-seed cookies and re-visit homepage on the relaunched browser.
        for domain in &[
          "chatgpt.com",
          ".chatgpt.com",
          "auth.openai.com",
          ".auth.openai.com",
        ] {
          let _ = session.set_cookie("oai-did", &self.device_id, domain).await;
        }
        session.navigate("https://chatgpt.com/", 20).await?;
        self.human_pause(1200, 2200).await;
        self.log(&format!(
          "Worker relaunched for authorize retry: {} ({})",
          profile.name, profile.id
        ));
      }

      let cur = match self.navigate_to_signup_page(session, email).await {
        Ok(u) => u,
        Err(e) => {
          self.log(&format!(
            "navigate_to_signup_page failed (attempt {attempt}): {e}"
          ));
          continue;
        }
      };

      if is_cloudflare_block(&cur) {
        self.log(&format!("Cloudflare block detected: {cur}"));
        continue;
      }

      return Ok(cur);
    }

    Err("Authorize failed after max retries — signup navigation/Cloudflare".into())
  }

  // -----------------------------------------------------------------------
  // Browser launch + CDP connect + cleanup
  // -----------------------------------------------------------------------

  /// Launch a browser profile and attach the right automation backend.
  /// Chromium uses CDP; Camoufox uses the Playwright page from CamoufoxManager.
  async fn launch_and_connect(
    &mut self,
    app_handle: &tauri::AppHandle,
  ) -> Result<(crate::profile::BrowserProfile, BrowserSession), String> {
    let profile = self.launch_browser(app_handle).await?;
    self.log(&format!(
      "Browser launched: profile={} id={} browser={}",
      profile.name, profile.id, profile.browser
    ));

    let session = attach_browser_session(&profile).await?;
    Ok((profile, session))
  }

  /// Ensure a single reusable worker profile exists for this engine/batch.
  ///
  /// - If `config.profile_id` is set and found, adopt it (do not delete at end).
  /// - Otherwise create one ephemeral worker once and reuse it for every account.
  /// - Each launch regenerates fingerprint + ephemeral data dir via browser_runner.
  async fn ensure_worker_profile(
    &mut self,
    app_handle: &tauri::AppHandle,
  ) -> Result<crate::profile::BrowserProfile, String> {
    use crate::browser::BrowserType;
    use crate::profile::manager::create_browser_profile_with_group;

    // Already have a worker for this engine — reload latest metadata.
    if let Some(id) = self.worker_profile_id.clone() {
      if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
        if let Some(found) = profiles.into_iter().find(|p| p.id.to_string() == id) {
          return Ok(found);
        }
      }
      self.log(&format!(
        "Worker profile {id} missing from store — will recreate"
      ));
      self.worker_profile_id = None;
      self.owns_worker_profile = false;
    }

    // Prefer an existing user-selected profile as the worker (reuse, not template-only).
    if let Some(profile_id) = self.config.profile_id.as_ref() {
      if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
        if let Some(mut found) = profiles
          .into_iter()
          .find(|p| p.id.to_string() == *profile_id)
        {
          // Ensure relaunches renew fingerprint even when reusing a user profile.
          if found.browser.eq_ignore_ascii_case("camoufox") {
            let mut cfg = found.camoufox_config.clone().unwrap_or_default();
            if cfg.randomize_fingerprint_on_launch != Some(true) {
              cfg.randomize_fingerprint_on_launch = Some(true);
              found.camoufox_config = Some(cfg.clone());
              if let Err(e) = crate::profile::ProfileManager::instance()
                .update_camoufox_config(app_handle.clone(), &found.id.to_string(), cfg)
                .await
              {
                self.log(&format!(
                  "Warning: failed to enable Camoufox FP renew on worker: {e}"
                ));
              }
            }
          } else if found.browser.eq_ignore_ascii_case("chromium") {
            let mut cfg = found.chromium_config.clone().unwrap_or_default();
            if cfg.randomize_fingerprint_on_launch != Some(true) {
              cfg.randomize_fingerprint_on_launch = Some(true);
              found.chromium_config = Some(cfg.clone());
              if let Err(e) = crate::profile::ProfileManager::instance()
                .update_chromium_config(app_handle.clone(), &found.id.to_string(), cfg)
                .await
              {
                self.log(&format!(
                  "Warning: failed to enable Chromium FP renew on worker: {e}"
                ));
              }
            }
          }

          // Align network attachment with batch/slot config (vpn preferred, else proxy).
          if let Some(vpn_id) = self.worker_vpn_id() {
            if found.vpn_id.as_deref() != Some(vpn_id.as_str()) {
              match crate::profile::ProfileManager::instance()
                .update_profile_vpn(
                  app_handle.clone(),
                  &found.id.to_string(),
                  Some(vpn_id.clone()),
                )
                .await
              {
                Ok(updated) => {
                  found = updated;
                  self.log(&format!("Worker profile VPN set to {vpn_id}"));
                }
                Err(e) => {
                  self.log(&format!(
                    "Warning: failed to set worker vpn_id={vpn_id}: {e}"
                  ));
                }
              }
            }
          } else if let Some(proxy_id) = self.config.effective_proxy_id() {
            if found.proxy_id.as_deref() != Some(proxy_id.as_str()) {
              match crate::profile::ProfileManager::instance()
                .update_profile_proxy(
                  app_handle.clone(),
                  &found.id.to_string(),
                  Some(proxy_id.clone()),
                )
                .await
              {
                Ok(updated) => {
                  found = updated;
                  self.log(&format!("Worker profile proxy set to {proxy_id}"));
                }
                Err(e) => {
                  self.log(&format!(
                    "Warning: failed to set worker proxy_id={proxy_id}: {e}"
                  ));
                }
              }
            }
          }

          self.log(&format!(
            "Reusing configured profile as worker: {} ({}) browser={} version={}",
            found.name, found.id, found.browser, found.version
          ));
          self.worker_profile_id = Some(found.id.to_string());
          self.owns_worker_profile = false;
          return Ok(found);
        }
      }
      self.log(&format!(
        "Configured profile_id {profile_id} not found — creating auto-reg worker"
      ));
    }

    let browser_str = if self.config.browser_type == "camoufox" {
      "camoufox"
    } else {
      "chromium"
    };
    let mut version = String::new();
    let mut release_type = "stable".to_string();

    // Prefer an installed version from any existing profile of the same browser.
    if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
      if let Some(found) = profiles
        .into_iter()
        .find(|p| p.browser.eq_ignore_ascii_case(browser_str) && !p.version.is_empty())
      {
        version = found.version;
        if !found.release_type.is_empty() {
          release_type = found.release_type;
        }
        self.log(&format!(
          "Using installed {browser_str} version from existing profile: {version}"
        ));
      }
    }
    // Fallback: downloaded browsers registry (critical for chromium — empty version
    // resolves to binaries/fingerprint-chromium/ and fails to find chrome.exe).
    if version.is_empty() {
      let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
      let mut versions = registry.get_downloaded_versions(browser_str);
      versions.sort_by(|a, b| {
        crate::api_client::VersionComponent::parse(b)
          .cmp(&crate::api_client::VersionComponent::parse(a))
      });
      if let Some(v) = versions.into_iter().next() {
        version = v;
        self.log(&format!(
          "Using installed {browser_str} version from registry: {version}"
        ));
      }
    }

    if version.is_empty() && browser_str == "camoufox" {
      version = "v135.0.1-beta.24".into();
      self.log(&format!("Using default Camoufox version: {version}"));
    }
    if version.is_empty() {
      return Err(format!(
        "No downloaded {browser_str} version found. Install the browser in JnmBrowser first."
      ));
    }

    let browser =
      BrowserType::from_str(browser_str).map_err(|e| format!("Invalid browser type: {e}"))?;

    // One stable worker name per task+CDK slot (not per account).
    let profile_name = format!(
      "auto-reg-worker-{}-s{}",
      &self.task_id[..8.min(self.task_id.len())],
      self.worker_slot
    );

    let camoufox_config = if browser_str == "camoufox" {
      Some(crate::camoufox_manager::CamoufoxConfig {
        fingerprint: None,
        randomize_fingerprint_on_launch: Some(true),
        geoip: Some(serde_json::Value::Bool(true)),
        ..Default::default()
      })
    } else {
      None
    };

    // Chromium also renews fingerprint on every launch of this worker.
    let chromium_config = if browser_str == "chromium" {
      Some(crate::chromium_manager::ChromiumConfig {
        fingerprint: None,
        randomize_fingerprint_on_launch: Some(true),
        ..Default::default()
      })
    } else {
      None
    };

    let mut created = create_browser_profile_with_group(
      app_handle.clone(),
      profile_name,
      browser.as_str().to_string(),
      version,
      release_type,
      self.config.effective_proxy_id(),
      self.worker_vpn_id(),
      camoufox_config,
      chromium_config,
      None,
      true, // ephemeral worker: data dir wiped on kill, metadata reused
      None,
      None,
    )
    .await
    .map_err(|e| format!("Create worker profile: {e}"))?;

    // Persist randomize flags so relaunches keep renewing fingerprints.
    if created.browser.eq_ignore_ascii_case("camoufox") {
      let mut cfg = created.camoufox_config.clone().unwrap_or_default();
      cfg.randomize_fingerprint_on_launch = Some(true);
      created.camoufox_config = Some(cfg);
    } else if created.browser.eq_ignore_ascii_case("chromium") {
      let mut cfg = created.chromium_config.clone().unwrap_or_default();
      cfg.randomize_fingerprint_on_launch = Some(true);
      created.chromium_config = Some(cfg);
      if let Err(e) = crate::profile::ProfileManager::instance()
        .update_chromium_config(
          app_handle.clone(),
          &created.id.to_string(),
          created.chromium_config.clone().unwrap_or_default(),
        )
        .await
      {
        self.log(&format!(
          "Warning: failed to persist Chromium randomize flag: {e}"
        ));
      }
    }

    self.worker_profile_id = Some(created.id.to_string());
    self.owns_worker_profile = true;
    self.log(&format!(
      "Created reusable worker profile {} (id={}) browser={} — relaunch renews fingerprint + data",
      created.name, created.id, created.browser
    ));

    Ok(created)
  }

  /// Launch the reused worker profile (creates worker once if needed).
  async fn launch_browser(
    &mut self,
    app_handle: &tauri::AppHandle,
  ) -> Result<crate::profile::BrowserProfile, String> {
    use crate::browser_runner::BrowserRunner;

    let worker = self.ensure_worker_profile(app_handle).await?;
    self.log(&format!(
      "Launching worker {} ({}) — fingerprint renew + fresh ephemeral dir",
      worker.name, worker.id
    ));

    let launched = BrowserRunner::instance()
      .launch_browser(
        app_handle.clone(),
        &worker,
        Some("about:blank".into()),
        None,
      )
      .await
      .map_err(|e| format!("Launch: {e}"))?;

    Ok(launched)
  }

  /// Kill the browser process only. Keep worker profile metadata for reuse.
  /// Ephemeral data dir is removed by BrowserRunner on kill.
  async fn kill_browser_only(
    &mut self,
    app_handle: &tauri::AppHandle,
    profile: &crate::profile::BrowserProfile,
  ) -> Result<(), String> {
    use crate::browser_runner::BrowserRunner;

    match BrowserRunner::instance()
      .kill_browser_process(app_handle.clone(), profile)
      .await
    {
      Ok(()) => {
        self.log(&format!(
          "Browser killed for worker profile {} ({})",
          profile.name, profile.id
        ));
        Ok(())
      }
      Err(e) => {
        let msg = format!(
          "Failed to kill browser for profile {} ({}): {e}",
          profile.name, profile.id
        );
        self.log(&msg);
        Err(msg)
      }
    }
  }

  /// Delete the auto-created worker at batch end. Never delete user-provided profile_id.
  async fn dispose_worker_profile(&mut self, app_handle: &tauri::AppHandle) {
    let Some(id) = self.worker_profile_id.take() else {
      return;
    };
    if !self.owns_worker_profile {
      self.log(&format!(
        "Keeping user-provided worker profile on disk: {id}"
      ));
      return;
    }
    self.owns_worker_profile = false;

    // Best-effort kill if still running. Dispose proceeds regardless — the
    // profile metadata is removed either way, and kill failure is logged
    // by kill_browser_only itself.
    if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
      if let Some(found) = profiles.into_iter().find(|p| p.id.to_string() == id) {
        let _ = self.kill_browser_only(app_handle, &found).await;
      }
    }

    self.human_pause(450, 550).await;

    if let Err(e) = crate::profile::ProfileManager::instance().delete_profile(app_handle, &id) {
      self.log(&format!(
        "Warning: failed to delete worker profile {id}: {e}"
      ));
    } else {
      self.log(&format!("Worker profile deleted: {id}"));
    }
  }

  #[allow(dead_code)]
  fn fail_result(&self, error: &str) -> RegistrationResult {
    RegistrationResult {
      success: false,
      email: String::new(),
      password: String::new(),
      account_id: String::new(),
      access_token: String::new(),
      device_id: String::new(),
      error_message: error.to_string(),
      step_logs: self.logs.clone(),
      created_at: Utc::now(),
      two_fa_enabled: false,
      totp_secret: String::new(),
      free_trial_eligible: false,
      plan_type: String::new(),
      cdk: String::new(),
      base_email: String::new(),
      phone_number: String::new(),
      status: super::types::AccountInventoryStatus::Available,
      note: String::new(),
      exported_at: None,
      sold_at: None,
      email_provider: Some(self.config.email_provider),
      email_provider_provenance: Some(EmailProviderProvenance::RegistrationConfig),
      registration_outcome_reason: Some(RegistrationOutcomeReason::RegistrationFailed),
      two_factor_backfill_access_state: Some(TwoFactorBackfillAccessState::Accessible),
      two_factor_backfill_exclusion: None,
      two_factor_backfill_state: None,
      two_factor_backfill_outcome: None,
      two_factor_backfill_operation_id: None,
      record_revision: 1,
    }
  }
}
