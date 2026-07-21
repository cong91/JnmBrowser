use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use tauri::Emitter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use super::store::save_login_result;
use super::sub2api::Sub2ApiClient;
use super::types::{
  should_rotate, LoginConfig, LoginCredential, LoginNetworkMode, LoginProgress, LoginResult,
  LoginResultStatus, LoginStep,
};
use super::{oauth, pkce};
use crate::sms::{NumberRequest, SmsService};

type CdpWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

const OAUTH_CALLBACK_HOST: &str = "127.0.0.1";
const OAUTH_CALLBACK_PORT: u16 = 1455;
const OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// Max seconds to wait for one Viotp SMS before rotating to a new number.
const SMS_OTP_TIMEOUT_SECS: u64 = 90;
/// How many different Viotp numbers to try within a single login_once attempt.
const MAX_SMS_NUMBER_ATTEMPTS: u32 = 3;
/// Soft-wait budget for Cloudflare Turnstile / "Just a moment" challenges.
const CLOUDFLARE_SOFT_WAIT_SECS: u64 = 20;
/// Extra relaunches dedicated to Cloudflare recovery (on top of normal retries).
const MAX_CLOUDFLARE_RECOVERIES: u32 = 2;

/// Short-lived local HTTP listener for OpenAI OAuth redirect.
///
/// OpenAI redirects to `http://localhost:1455/auth/callback?code=...&state=...`.
/// Without something bound on :1455, Chromium lands on chrome-error and the code is lost.
struct OAuthCallbackListener {
  rx: oneshot::Receiver<Result<(String, String), String>>,
  shutdown: Option<oneshot::Sender<()>>,
  task: Option<tokio::task::JoinHandle<()>>,
}

impl OAuthCallbackListener {
  async fn start() -> Result<Self, String> {
    let addr = format!("{OAUTH_CALLBACK_HOST}:{OAUTH_CALLBACK_PORT}");
    Self::start_on_addr(&addr).await
  }

  async fn start_on_addr(addr: &str) -> Result<Self, String> {
    // Windows keeps sockets in TIME_WAIT after close. Retries of the same login
    // attempt can hit os error 10048 unless we wait for the previous accept-loop
    // task to drop its TcpListener.
    let mut last_err = String::new();
    let mut listener = None;
    for attempt in 0..20 {
      if attempt > 0 {
        sleep(std::time::Duration::from_millis(150)).await;
      }
      match TcpListener::bind(&addr).await {
        Ok(l) => {
          listener = Some(l);
          break;
        }
        Err(e) => {
          last_err = e.to_string();
        }
      }
    }
    let listener = listener
      .ok_or_else(|| format!("Failed to bind OAuth callback listener on {addr}: {last_err}"))?;
    let (tx, rx) = oneshot::channel::<Result<(String, String), String>>();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    let task = tokio::spawn(async move {
      let mut tx = Some(tx);
      loop {
        tokio::select! {
          _ = &mut shutdown_rx => break,
          accept = listener.accept() => {
            match accept {
              Ok((mut socket, _)) => {
                if let Some(result) = handle_oauth_callback_connection(&mut socket).await {
                  if let Some(sender) = tx.take() {
                    let _ = sender.send(Ok(result));
                  }
                  // Keep accepting briefly so Chrome can finish loading success HTML,
                  // but only the first valid code is returned.
                }
              }
              Err(_) => break,
            }
          }
        }
      }
      // Drop the listener before the task completes so shutdown guarantees re-bind safety.
      drop(listener);
      if let Some(sender) = tx.take() {
        let _ = sender.send(Err(
          "OAuth callback listener shut down before receiving code".into(),
        ));
      }
    });

    Ok(Self {
      rx,
      shutdown: Some(shutdown_tx),
      task: Some(task),
    })
  }

  async fn wait_for_code(
    &mut self,
    timeout: std::time::Duration,
  ) -> Result<(String, String), String> {
    tokio::time::timeout(timeout, &mut self.rx)
      .await
      .map_err(|_| "Timeout waiting for OAuth callback on localhost:1455".to_string())?
      .map_err(|_| "OAuth callback listener closed unexpectedly".to_string())?
  }

  /// Signal shutdown and wait until the accept-loop task has released :1455.
  async fn shutdown(&mut self) {
    if let Some(tx) = self.shutdown.take() {
      let _ = tx.send(());
    }
    if let Some(mut task) = self.task.take() {
      if tokio::time::timeout(std::time::Duration::from_secs(2), &mut task)
        .await
        .is_err()
      {
        task.abort();
        let _ = task.await;
      }
    }
  }
}

impl Drop for OAuthCallbackListener {
  fn drop(&mut self) {
    if let Some(tx) = self.shutdown.take() {
      let _ = tx.send(());
    }
    if let Some(task) = self.task.take() {
      task.abort();
    }
  }
}

async fn handle_oauth_callback_connection(socket: &mut TcpStream) -> Option<(String, String)> {
  let mut buf = vec![0u8; 8192];
  let n = match tokio::time::timeout(std::time::Duration::from_secs(5), socket.read(&mut buf)).await
  {
    Ok(Ok(n)) if n > 0 => n,
    _ => return None,
  };
  let req = String::from_utf8_lossy(&buf[..n]);
  let first_line = req.lines().next().unwrap_or("");
  // GET /auth/callback?code=...&state=... HTTP/1.1
  let path = first_line.split_whitespace().nth(1).unwrap_or("");
  let full_url = if path.starts_with("http") {
    path.to_string()
  } else {
    format!("http://localhost:{OAUTH_CALLBACK_PORT}{path}")
  };

  let parsed = Sub2ApiClient::parse_callback_url(&full_url);
  let (status_line, body) = if parsed.is_some() {
    (
      "HTTP/1.1 200 OK",
      "<!doctype html><html><head><meta charset=\"utf-8\"><title>Login complete</title></head><body style=\"font-family:system-ui;padding:2rem\"><h1>Authorization complete</h1><p>You can close this window and return to JnmBrowser.</p></body></html>",
    )
  } else {
    (
      "HTTP/1.1 400 Bad Request",
      "<!doctype html><html><body><h1>Missing OAuth code</h1></body></html>",
    )
  };
  let response = format!(
    "{status_line}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
    body.len()
  );
  let _ = socket.write_all(response.as_bytes()).await;
  let _ = socket.shutdown().await;
  parsed
}

fn submit_control_probe_js(selectors: &str) -> String {
  format!(
    r#"(function(){{
      const selectors = {sels};
      function ready(el) {{
        try {{
          const r = el.getBoundingClientRect();
          const s = getComputedStyle(el);
          const reactReady = Object.keys(el).some((key) => key.startsWith('__reactProps$'));
          const enabled = !el.disabled && el.getAttribute('aria-disabled') !== 'true';
          return reactReady && enabled && r.width > 0 && r.height > 0
            && s.visibility !== 'hidden' && s.display !== 'none';
        }} catch (_) {{ return false; }}
      }}
      function point(el, how) {{
        el.scrollIntoView({{ block: 'center', inline: 'nearest' }});
        const r = el.getBoundingClientRect();
        return {{
          ok: true,
          how,
          x: r.left + r.width / 2,
          y: r.top + r.height / 2,
          text: (el.innerText || el.textContent || '').slice(0, 40)
        }};
      }}
      const button = Array.from(document.querySelectorAll(selectors)).find(ready);
      if (button) return point(button, 'selector');

      const texts = ['continue', 'next', 'log in', 'sign in', 'submit', 'verify'];
      const textButton = Array.from(document.querySelectorAll('button,[role="button"]'))
        .filter(ready)
        .find((el) => {{
          const text = (el.innerText || el.textContent || '').toLowerCase().trim();
          return texts.some((candidate) => text === candidate || text.includes(candidate));
        }});
      if (textButton) return point(textButton, 'text');
      return {{ ok: false }};
    }})()"#,
    sels = serde_json::to_string(selectors).unwrap_or_else(|_| "\"\"".into()),
  )
}

/// CDP WebSocket connection.
struct CdpConnection {
  ws: CdpWs,
  next_id: u64,
}

impl CdpConnection {
  async fn connect(ws_url: &str) -> Result<Self, String> {
    let (ws, _) = connect_async(ws_url)
      .await
      .map_err(|e| format!("CDP WebSocket connect failed: {e}"))?;
    let mut conn = Self { ws, next_id: 1 };
    // Automation must keep working when the OS window is minimized/unfocused.
    conn.prepare_for_background_automation().await?;
    Ok(conn)
  }

  /// Enable focus emulation and restore a usable window even if minimized.
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

    // If the OS window is minimized, CDP input/layout can break. Normalize state.
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
          let _ = self
            .send_cmd(
              "Browser.setWindowBounds",
              serde_json::json!({
                "windowId": window_id,
                "bounds": {
                  "windowState": "normal",
                  "width": 1280,
                  "height": 900,
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

  async fn current_url(&mut self) -> Result<String, String> {
    let result = self.evaluate("window.location.href", false).await?;
    result["value"]
      .as_str()
      .map(|s| s.to_string())
      .ok_or_else(|| "Failed to get current URL".into())
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

  #[allow(dead_code)]
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

  #[allow(dead_code)]
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
    Ok(())
  }

  #[allow(dead_code)]
  async fn mouse_click(&mut self, x: f64, y: f64) -> Result<(), String> {
    let _ = self.mouse_move(x, y).await;
    sleep(std::time::Duration::from_millis(25)).await;
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
    sleep(std::time::Duration::from_millis(35)).await;
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

  async fn press_enter(&mut self) -> Result<(), String> {
    self
      .send_cmd(
        "Input.dispatchKeyEvent",
        serde_json::json!({
          "type": "keyDown",
          "key": "Enter",
          "code": "Enter",
          "windowsVirtualKeyCode": 13,
          "nativeVirtualKeyCode": 13,
        }),
      )
      .await?;
    self
      .send_cmd(
        "Input.dispatchKeyEvent",
        serde_json::json!({
          "type": "keyUp",
          "key": "Enter",
          "code": "Enter",
          "windowsVirtualKeyCode": 13,
          "nativeVirtualKeyCode": 13,
        }),
      )
      .await?;
    Ok(())
  }
}

/// Dual-kernel browser session (Chromium CDP + Camoufox Playwright).
/// Mirrors openai::register so Camoufox auto-login works the same as Chromium.
#[allow(clippy::large_enum_variant)]
enum BrowserSession {
  Cdp(CdpConnection),
  Camoufox {
    page: playwright::api::Page,
    #[allow(dead_code)]
    profile_path: String,
  },
}

impl BrowserSession {
  async fn navigate(&mut self, url: &str, timeout_secs: u64) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => cdp.navigate(url, timeout_secs).await,
      Self::Camoufox { page, .. } => {
        let _ = timeout_secs;
        // Keep page usable when the OS window is minimized/unfocused.
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
            let current = page.url().unwrap_or_default();
            if current.starts_with(url)
              || (url.contains("auth.openai.com") && current.contains("auth.openai.com"))
              || (url.contains("chatgpt.com") && current.contains("chatgpt.com"))
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

  /// Returns a CDP-shaped Runtime.evaluate result object: `{ "value": ... }` / result envelope.
  async fn evaluate(
    &mut self,
    expression: &str,
    await_promise: bool,
  ) -> Result<serde_json::Value, String> {
    match self {
      Self::Cdp(cdp) => cdp.evaluate(expression, await_promise).await,
      Self::Camoufox { page, .. } => {
        let _ = await_promise;
        let value: serde_json::Value = page
          .eval(expression)
          .await
          .map_err(|e| format!("Camoufox evaluate failed: {e}"))?;
        // Match CDP shape used throughout login engine: result["value"].
        Ok(serde_json::json!({ "value": value, "type": "object" }))
      }
    }
  }

  async fn current_url(&mut self) -> Result<String, String> {
    match self {
      Self::Cdp(cdp) => cdp.current_url().await,
      Self::Camoufox { page, .. } => page
        .url()
        .map_err(|e| format!("Camoufox current_url failed: {e}")),
    }
  }

  async fn key_char(&mut self, ch: char) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => cdp.key_char(ch).await,
      Self::Camoufox { page, .. } => {
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

  async fn mouse_click(&mut self, x: f64, y: f64) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => cdp.mouse_click(x, y).await,
      Self::Camoufox { page, .. } => {
        page
          .mouse
          .click_builder(x, y)
          .click()
          .await
          .map_err(|e| format!("Camoufox mouse click failed: {e}"))?;
        Ok(())
      }
    }
  }

  /// Playwright selector click (Camoufox). Falls back to coordinate/DOM click on CDP.
  /// Uses a short timeout so missing optional controls don't burn 30s each.
  async fn selector_click(&mut self, selector: &str) -> Result<(), String> {
    match self {
      Self::Camoufox { page, .. } => {
        page
          .click_builder(selector)
          .timeout(2500.0)
          .click()
          .await
          .map_err(|e| format!("Camoufox selector click failed ({selector}): {e}"))?;
        Ok(())
      }
      Self::Cdp(cdp) => {
        // CDP path: resolve center via evaluate then Input.dispatchMouseEvent.
        let js = format!(
          r#"(function(){{
            const el = document.querySelector({sel});
            if (!el) return null;
            try {{ el.scrollIntoView({{ block: 'center', inline: 'nearest' }}); }} catch(_) {{}}
            const r = el.getBoundingClientRect();
            if (!(r.width > 0 && r.height > 0)) return null;
            return {{ x: r.left + r.width/2, y: r.top + r.height/2 }};
          }})()"#,
          sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into())
        );
        let res = cdp.evaluate(&js, false).await?;
        let x = res
          .get("value")
          .and_then(|v| v.get("x"))
          .and_then(|n| n.as_f64())
          .ok_or_else(|| format!("CDP selector not found/visible: {selector}"))?;
        let y = res
          .get("value")
          .and_then(|v| v.get("y"))
          .and_then(|n| n.as_f64())
          .ok_or_else(|| format!("CDP selector not found/visible: {selector}"))?;
        cdp.mouse_click(x, y).await
      }
    }
  }

  async fn press_enter(&mut self) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => cdp.press_enter().await,
      Self::Camoufox { page, .. } => {
        page
          .keyboard
          .press("Enter", Some(20.0))
          .await
          .map_err(|e| format!("Camoufox Enter failed: {e}"))?;
        Ok(())
      }
    }
  }
}

/// Login page type detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginPageType {
  LoginEmail,
  LoginPassword,
  TwoFactor,
  /// Enter phone number (country select + tel input).
  AddPhone,
  /// Enter SMS OTP after phone number was submitted.
  PhoneOtp,
  /// OAuth "Continue" gate. Returning accounts (phone already verified) land here after 2FA.
  Consent,
  Callback,
  ChatgptHome,
  Unknown,
}

fn is_oauth_callback_url(url: &str) -> bool {
  if url.to_ascii_lowercase().starts_with("chrome-error://") {
    return true;
  }
  let Some(parsed) = url::Url::parse(url).ok() else {
    return false;
  };
  let host = parsed.host_str().unwrap_or_default();
  if !matches!(host, "localhost" | "127.0.0.1") {
    return false;
  }
  let path_is_callback = parsed.path().contains("/auth/callback");
  let has_code = parsed.query_pairs().any(|(key, _)| key == "code");
  let has_state = parsed.query_pairs().any(|(key, _)| key == "state");
  path_is_callback || (has_code && has_state)
}

fn extract_unsupported_region_error(body: &str) -> Option<String> {
  let body = body.trim();
  if body.is_empty() || !crate::vpn::is_unsupported_region_error(body) {
    return None;
  }

  let json = serde_json::from_str::<serde_json::Value>(body)
    .ok()
    .map(|_| body)
    .or_else(|| {
      let start = body.find('{')?;
      let end = body.rfind('}')?;
      let candidate = body.get(start..=end)?.trim();
      serde_json::from_str::<serde_json::Value>(candidate)
        .ok()
        .map(|_| candidate)
    })?;
  Some(json.to_string())
}

/// Detect Cloudflare managed challenge / Turnstile interstitial from URL/title/body text.
fn is_cloudflare_challenge_signal(text: &str) -> bool {
  let lower = text.to_ascii_lowercase();
  if lower.contains("verify you are human")
    || lower.contains("performing security verification")
    || lower.contains("just a moment")
    || lower.contains("cf-turnstile")
    || lower.contains("challenges.cloudflare.com")
  {
    return true;
  }
  // Generic Cloudflare challenge wording; avoid bare "cloudflare" alone (privacy footer).
  if lower.contains("cloudflare")
    && (lower.contains("security verification")
      || lower.contains("checking your browser")
      || lower.contains("attention required")
      || lower.contains("enable javascript and cookies"))
  {
    return true;
  }
  false
}

fn is_cloudflare_challenge_error(message: &str) -> bool {
  let lower = message.to_ascii_lowercase();
  lower.contains("cloudflare")
    && (lower.contains("challenge")
      || lower.contains("turnstile")
      || lower.contains("verify you are human")
      || lower.contains("security verification"))
}

fn cloudflare_challenge_error_message() -> String {
  "Cloudflare Turnstile challenge (persistent after soft-wait)".into()
}

fn resolve_dom_page_override(current: LoginPageType, dom: LoginPageType) -> LoginPageType {
  if matches!(dom, LoginPageType::Unknown) {
    return current;
  }
  if matches!(dom, LoginPageType::Callback)
    && !matches!(current, LoginPageType::Unknown | LoginPageType::Callback)
  {
    return current;
  }
  dom
}

fn detect_login_page_type(url: &str) -> LoginPageType {
  let parsed = url::Url::parse(url).ok();
  let host = parsed
    .as_ref()
    .and_then(url::Url::host_str)
    .unwrap_or_default()
    .to_lowercase();
  let path = parsed
    .as_ref()
    .map(|url| url.path().to_lowercase())
    .unwrap_or_else(|| url.to_lowercase());
  // Callback first: after consent, Chrome may briefly show localhost or chrome-error.
  if is_oauth_callback_url(url) {
    return LoginPageType::Callback;
  }

  // OpenAI uses both "login" and hyphenated "log-in" paths.
  let is_password = path.contains("password")
    || path.contains("log-in/password")
    || path.contains("login/password")
    || path.contains("create-account/password");
  // "authorize" alone is the OAuth start URL — only treat as email entry when it looks like login.
  let is_email_entry = path.contains("identifier")
    || path.contains("email-otp")
    || path.contains("/log-in")
    || path.contains("/login")
    || path.contains("log-in-or-create")
    || (path.contains("oauth/authorize") && !path.contains("consent"));
  if is_password {
    LoginPageType::LoginPassword
  } else if path.contains("mfa")
    || path.contains("totp")
    || path.contains("2fa")
    || path.contains("multi-factor")
    || (path.contains("challenge") && !path.contains("authorize") && !path.contains("consent"))
  {
    LoginPageType::TwoFactor
  } else if path.contains("phone-verification")
    || path.contains("verify-phone")
    || path.contains("phone/verify")
    || path.contains("add-phone/verify")
  {
    // After number submit OpenAI moves to /phone-verification (OTP only).
    // Must NOT re-enter AddPhone or we rent a second SMS number.
    LoginPageType::PhoneOtp
  } else if path.contains("add-phone") || path.contains("phone/add") {
    LoginPageType::AddPhone
  } else if path.contains("consent")
    || path.contains("sign-in-with-chatgpt")
    || path.contains("sign-in-with-openai")
    || path.contains("oauth/consent")
    || path.contains("workspace/select")
    || path.contains("organization/select")
  {
    // OAuth consent / org-select / "Continue" gate after successful auth.
    // Accounts that already verified phone often land here directly after 2FA — skip SMS.
    LoginPageType::Consent
  } else if host == "chatgpt.com" && (path.is_empty() || path == "/") {
    LoginPageType::ChatgptHome
  } else if is_email_entry && !is_password {
    LoginPageType::LoginEmail
  } else {
    LoginPageType::Unknown
  }
}

/// Auto-login engine for existing OpenAI accounts.
pub struct LoginEngine {
  config: LoginConfig,
  cancel_flag: Arc<AtomicBool>,
  task_id: String,
  logs: Vec<String>,
  used_phones: HashSet<String>,
  /// Reused worker profile for this engine (one slot for sequential batch).
  worker_profile_id: Option<String>,
  /// True when we created the worker and must delete it at batch end.
  owns_worker_profile: bool,
}

impl LoginEngine {
  pub fn with_cancel_flag(config: LoginConfig, cancel_flag: Arc<AtomicBool>) -> Self {
    Self {
      config,
      cancel_flag,
      task_id: Uuid::new_v4().to_string(),
      logs: Vec::new(),
      used_phones: HashSet::new(),
      worker_profile_id: None,
      owns_worker_profile: false,
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

  #[allow(clippy::too_many_arguments)]
  fn emit(
    &self,
    app_handle: &tauri::AppHandle,
    step: LoginStep,
    message: &str,
    credential_index: u32,
    total_credentials: u32,
    result: Option<LoginResult>,
  ) {
    let payload = LoginProgress {
      task_id: self.task_id.clone(),
      credential_index,
      total_credentials,
      step,
      message: message.to_string(),
      timestamp: Utc::now(),
      result,
    };
    let _ = app_handle.emit("login-progress", payload);
  }

  #[allow(dead_code)]
  fn fail_result(&self, error: &str) -> LoginResult {
    LoginResult {
      success: false,
      email: String::new(),
      account_id: String::new(),
      access_token: String::new(),
      refresh_token: String::new(),
      sub2api_account_id: None,
      error_message: error.to_string(),
      push_error: String::new(),
      step_logs: self.logs.clone(),
      created_at: Utc::now(),
      phone_number: String::new(),
      status: LoginResultStatus::Available,
      note: String::new(),
      exported_at: None,
      password: String::new(),
      totp_secret: String::new(),
    }
  }

  fn account_logs(&self, start: usize) -> Vec<String> {
    self.logs.get(start..).unwrap_or(&[]).to_vec()
  }

  /// Main entry: iterate credentials, login each, push to sub2api.
  pub async fn run(
    &mut self,
    app_handle: tauri::AppHandle,
    sms_service: Option<&dyn SmsService>,
  ) -> Vec<LoginResult> {
    let total = self.config.credentials.len() as u32;
    let mut results = Vec::new();
    let mut success_count: u32 = 0;
    let rotate_every_n = self.config.rotate_every_n;

    if let Some(vpn_id) = self.config.effective_vpn_id() {
      self.log(&format!(
        "VPN mode: inventory conf vpn_id={vpn_id}; rotate every {rotate_every_n} success(es)"
      ));
    }

    let sub2api = if self.config.push_to_sub2api {
      Some(Sub2ApiClient::new(
        &self.config.sub2api_url,
        &self.config.sub2api_api_key,
      ))
    } else {
      None
    };

    for (idx, credential) in self.config.credentials.clone().iter().enumerate() {
      if self.is_cancelled() {
        self.log("Cancelled by user");
        break;
      }

      let log_start = self.logs.len();
      self.log(&format!(
        "=== Login {}/{}: {} ===",
        idx + 1,
        total,
        credential.email
      ));

      let max_retries = self.config.max_retries.max(1);
      let mut succeeded = false;
      let mut last_error = String::new();
      let mut location_fallbacks: u32 = 0;
      let mut tried_locations: Vec<String> = Vec::new();

      let mut attempt = 0;
      let mut cloudflare_recoveries: u32 = 0;
      while attempt < max_retries {
        if attempt > 0 {
          self.log(&format!("Retry {attempt}/{max_retries}..."));
          sleep(std::time::Duration::from_secs(2)).await;
        }

        match self
          .login_once(
            &app_handle,
            sms_service,
            sub2api.as_ref(),
            credential,
            idx as u32,
            total,
          )
          .await
        {
          Ok(mut result) => {
            let login_ok = result.success;
            if !login_ok
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
                  // Switched location — retry without persisting this blocked attempt as final.
                  last_error = result.error_message.clone();
                  continue;
                }
                Ok(false) => {}
                Err(e) => {
                  self.log(&format!("WARN: Nord location fallback failed: {e}"));
                }
              }
            }

            if !login_ok
              && is_cloudflare_challenge_error(&result.error_message)
              && cloudflare_recoveries < MAX_CLOUDFLARE_RECOVERIES
            {
              cloudflare_recoveries += 1;
              last_error = result.error_message.clone();
              self.log(&format!(
                "Cloudflare challenge recovery {cloudflare_recoveries}/{MAX_CLOUDFLARE_RECOVERIES}: relaunch same worker with new fingerprint..."
              ));
              if let Some(vpn_id) = self.config.effective_vpn_id() {
                if let Err(e) = self.rotate_wireguard_peer(&vpn_id).await {
                  self.log(&format!(
                    "WARN: WireGuard peer rotate after Cloudflare failed: {e}"
                  ));
                }
              }
              continue;
            }

            result.step_logs = self.account_logs(log_start);
            save_login_result(&result);
            self.emit(
              &app_handle,
              if login_ok {
                LoginStep::Completed
              } else {
                LoginStep::Failed
              },
              &format!(
                "[{}/{}] {}",
                idx + 1,
                total,
                if login_ok {
                  "Login succeeded"
                } else {
                  result.error_message.as_str()
                }
              ),
              idx as u32,
              total,
              Some(result.clone()),
            );
            results.push(result);
            succeeded = true;
            if login_ok {
              success_count += 1;
              // After each success (default rotate_every_n=1): hop WireGuard peer so
              // the next account gets a fresh egress IP.
              if should_rotate(success_count, rotate_every_n) {
                if let Some(vpn_id) = self.config.effective_vpn_id() {
                  self.log(&format!(
                    "Rotating WireGuard peer after {success_count} success(es) (vpn_id={vpn_id})..."
                  ));
                  match self.rotate_wireguard_peer(&vpn_id).await {
                    Ok((host, station)) => {
                      self.log(&format!(
                        "WireGuard peer rotated -> host={host} station={station}"
                      ));
                    }
                    Err(e) => {
                      self.log(&format!("WARN: WireGuard peer rotate failed: {e}"));
                    }
                  }
                }
              }
            }
            break;
          }
          Err(e) => {
            last_error = e.clone();
            self.log(&format!("Attempt {attempt} failed: {e}"));
            if crate::vpn::is_unsupported_region_error(&e) {
              if location_fallbacks >= crate::vpn::MAX_NORD_LOCATION_FALLBACKS {
                self.log("Unsupported region persists after exhausting Nord fallback locations");
                break;
              }
              match self
                .fallback_nord_location_on_region_block(
                  &e,
                  &mut location_fallbacks,
                  &mut tried_locations,
                )
                .await
              {
                Ok(true) => continue,
                Ok(false) => {
                  self.log("Unsupported region detected; Nord location fallback unavailable");
                  break;
                }
                Err(fe) => {
                  self.log(&format!("WARN: Nord location fallback failed: {fe}"));
                  break;
                }
              }
            }
            if is_cloudflare_challenge_error(&e)
              && cloudflare_recoveries < MAX_CLOUDFLARE_RECOVERIES
            {
              cloudflare_recoveries += 1;
              self.log(&format!(
                "Cloudflare challenge recovery {cloudflare_recoveries}/{MAX_CLOUDFLARE_RECOVERIES}: relaunch same worker with new fingerprint..."
              ));
              if let Some(vpn_id) = self.config.effective_vpn_id() {
                if let Err(re) = self.rotate_wireguard_peer(&vpn_id).await {
                  self.log(&format!(
                    "WARN: WireGuard peer rotate after Cloudflare failed: {re}"
                  ));
                }
              }
              // Recovery relaunch consumes a normal attempt slot (same as register retries).
              attempt += 1;
              continue;
            }
            attempt += 1;
          }
        }
      }

      if !succeeded {
        let result = LoginResult {
          success: false,
          email: credential.email.clone(),
          account_id: String::new(),
          access_token: String::new(),
          refresh_token: String::new(),
          sub2api_account_id: None,
          error_message: if last_error.is_empty() {
            format!("Failed after {max_retries} retries")
          } else {
            format!("Failed after {max_retries} retries: {last_error}")
          },
          push_error: String::new(),
          step_logs: self.account_logs(log_start),
          created_at: Utc::now(),
          phone_number: String::new(),
          status: LoginResultStatus::Invalid,
          note: String::new(),
          exported_at: None,
          password: credential.password.clone(),
          totp_secret: credential.totp_secret.clone(),
        };
        save_login_result(&result);
        self.emit(
          &app_handle,
          LoginStep::Failed,
          &format!("[{}/{}] {}", idx + 1, total, result.error_message),
          idx as u32,
          total,
          Some(result.clone()),
        );
        results.push(result);
      }
    }

    // Batch end: delete only auto-created worker profile (never user profiles).
    self.dispose_worker_profile(&app_handle).await;

    let ok = results.iter().filter(|r| r.success).count();
    let fail = results.iter().filter(|r| !r.success).count();
    let msg = format!("Done: {ok} logged in, {fail} failed");

    self.emit(&app_handle, LoginStep::Completed, &msg, 0, total, None);

    results
  }

  /// Single account login flow.
  #[allow(clippy::too_many_arguments)]
  async fn login_once(
    &mut self,
    app_handle: &tauri::AppHandle,
    sms_service: Option<&dyn SmsService>,
    sub2api: Option<&Sub2ApiClient>,
    credential: &LoginCredential,
    idx: u32,
    total: u32,
  ) -> Result<LoginResult, String> {
    let prefix = format!("[{}/{}]", idx + 1, total);

    // Step 1: Generate PKCE locally
    self.emit(
      app_handle,
      LoginStep::GeneratingAuthUrl,
      &format!("{prefix} Generating auth URL (local PKCE)..."),
      idx,
      total,
      None,
    );

    let state = pkce::generate_state();
    let code_verifier = pkce::generate_code_verifier();
    let code_challenge = pkce::generate_code_challenge(&code_verifier);
    let redirect_uri = OAUTH_REDIRECT_URI;
    let client_id = "app_EMoamEEZ73f0CkXaXp7hrann";

    let auth_url = pkce::build_authorization_url(&state, &code_challenge, redirect_uri, client_id);
    self.log(&format!(
      "{prefix} Auth URL ready (PKCE, client=codex, redirect=localhost:1455)"
    ));

    // Start local callback listener BEFORE browser navigates to auth.
    // OpenAI redirects to localhost:1455; without a listener Chromium shows chrome-error.
    let mut callback_listener = OAuthCallbackListener::start().await?;
    self.log(&format!(
      "{prefix} OAuth callback listener bound on {OAUTH_CALLBACK_HOST}:{OAUTH_CALLBACK_PORT}"
    ));

    // Step 2: Launch browser
    self.emit(
      app_handle,
      LoginStep::LaunchingBrowser,
      &format!("{prefix} Launching browser..."),
      idx,
      total,
      None,
    );

    let (profile, mut cdp) = match self.launch_browser(app_handle).await {
      Ok(v) => v,
      Err(e) => {
        // Await socket release so the next retry can re-bind :1455 on Windows.
        callback_listener.shutdown().await;
        return Err(e);
      }
    };
    self.log(&format!("{prefix} Browser launched: {}", profile.name));

    let result = self
      .run_login_in_browser(
        app_handle,
        sms_service,
        sub2api,
        &mut cdp,
        credential,
        &auth_url,
        &code_verifier,
        &state,
        prefix.as_str(),
        idx,
        total,
        &mut callback_listener,
      )
      .await;

    callback_listener.shutdown().await;
    // Kill browser only — keep worker profile metadata for the rest of the batch.
    self.kill_browser_only(app_handle, &profile).await;
    self.log(&format!("{prefix} Browser closed (worker retained)"));

    result
  }

  /// Core login steps inside browser.
  #[allow(clippy::too_many_arguments)]
  async fn run_login_in_browser(
    &mut self,
    app_handle: &tauri::AppHandle,
    sms_service: Option<&dyn SmsService>,
    sub2api: Option<&Sub2ApiClient>,
    cdp: &mut BrowserSession,
    credential: &LoginCredential,
    auth_url: &str,
    code_verifier: &str,
    expected_state: &str,
    prefix: &str,
    idx: u32,
    total: u32,
    callback_listener: &mut OAuthCallbackListener,
  ) -> Result<LoginResult, String> {
    // Step 3: Navigate to auth URL
    self.emit(
      app_handle,
      LoginStep::NavigatingToAuth,
      &format!("{prefix} Navigating to auth page..."),
      idx,
      total,
      None,
    );
    cdp.navigate(auth_url, 30).await?;
    sleep(std::time::Duration::from_secs(2)).await;

    let mut cur_url = cdp.current_url().await.unwrap_or_default();
    self.log(&format!("{prefix} Auth page URL: {cur_url}"));
    if let Some(error) = self.detect_unsupported_region_error_from_dom(cdp).await {
      return Err(error);
    }
    self
      .wait_out_cloudflare_challenge_if_any(cdp, prefix)
      .await?;
    cur_url = cdp.current_url().await.unwrap_or_default();

    let mut phone_number_used = String::new();
    // Pending SMS rent for the AddPhone → PhoneOtp two-step flow.
    let mut pending_sms_request_id: Option<String> = None;
    let mut phone_otp_submitted = false;
    // How many Viotp numbers we have tried for this login_once (OTP timeout → new number).
    let mut sms_number_attempts: u32 = 0;
    // Loops spent waiting for AddPhone → PhoneOtp after a submit (detect stuck form).
    let mut add_phone_wait_loops: u32 = 0;

    // Step 4-8: Login flow state machine.
    // Returning accounts (phone already verified) skip AddPhone/PhoneOtp and land on Consent.
    // Extra budget when SMS numbers need rotation after OTP timeout.
    for step_i in 0..30 {
      if self.is_cancelled() {
        return Err("Cancelled".into());
      }
      if let Some(error) = self.detect_unsupported_region_error_from_dom(cdp).await {
        return Err(error);
      }
      // Mid-flow Turnstile can appear on password/2FA transitions.
      if self.detect_cloudflare_challenge_from_dom(cdp).await {
        self
          .wait_out_cloudflare_challenge_if_any(cdp, prefix)
          .await?;
        cur_url = cdp.current_url().await.unwrap_or_default();
        continue;
      }

      let mut page = detect_login_page_type(&cur_url);
      // URL can lag SPA transitions (or be chrome-error without path). Probe DOM.
      if matches!(
        page,
        LoginPageType::Unknown | LoginPageType::LoginEmail | LoginPageType::AddPhone
      ) {
        if let Ok(dom_page) = self.probe_page_type_from_dom(cdp).await {
          let resolved_page = resolve_dom_page_override(page, dom_page);
          if resolved_page != page {
            self.log(&format!(
              "{prefix} DOM page override: {page:?} -> {resolved_page:?} (url={cur_url})"
            ));
            page = resolved_page;
          }
        }
      }
      self.log(&format!("{prefix} Page[{step_i}]: {page:?} url={cur_url}"));

      match page {
        LoginPageType::LoginEmail => {
          self.emit(
            app_handle,
            LoginStep::EnteringEmail,
            &format!("{prefix} Entering email..."),
            idx,
            total,
            None,
          );
          self.fill_and_submit_email(cdp, &credential.email).await?;
          // OpenAI often animates to password step; wait for URL/DOM settle.
          sleep(std::time::Duration::from_secs(3)).await;
          self.recover_auth_route_error_if_any(cdp, prefix).await?;
          cur_url = cdp.current_url().await.unwrap_or_default();
        }

        LoginPageType::LoginPassword => {
          self.recover_auth_route_error_if_any(cdp, prefix).await?;
          self.emit(
            app_handle,
            LoginStep::EnteringPassword,
            &format!("{prefix} Entering password..."),
            idx,
            total,
            None,
          );
          self
            .fill_and_submit_password(cdp, &credential.password)
            .await?;
          sleep(std::time::Duration::from_secs(3)).await;
          self.recover_auth_route_error_if_any(cdp, prefix).await?;
          cur_url = cdp.current_url().await.unwrap_or_default();
        }

        LoginPageType::TwoFactor => {
          if credential.totp_secret.is_empty() {
            return Err("2FA required but no TOTP secret provided".into());
          }
          self.recover_auth_route_error_if_any(cdp, prefix).await?;
          // Method chooser: click Google Authenticator / similar if shown.
          match self.select_totp_mfa_method(cdp, prefix).await {
            Ok(true) => {
              sleep(std::time::Duration::from_secs(2)).await;
              cur_url = cdp.current_url().await.unwrap_or_default();
              continue;
            }
            Ok(false) => {}
            Err(e) => return Err(e),
          }
          self.emit(
            app_handle,
            LoginStep::Entering2FA,
            &format!("{prefix} Entering 2FA code..."),
            idx,
            total,
            None,
          );
          let totp_code =
            crate::auto_service::common::totp::generate_totp_now(&credential.totp_secret)
              .map_err(|e| format!("TOTP generation failed: {e}"))?;
          self.log(&format!("{prefix} TOTP code generated"));
          self.fill_and_submit_2fa(cdp, &totp_code).await?;
          sleep(std::time::Duration::from_secs(2)).await;
          cur_url = cdp.current_url().await.unwrap_or_default();
        }

        LoginPageType::AddPhone => {
          // Only rent + enter number. OTP is handled on PhoneOtp page.
          // Returning accounts that already verified phone never hit this branch.
          if pending_sms_request_id.is_some() {
            // Already rented for this attempt — wait briefly for OTP page.
            // If OpenAI stays on add-phone (validation / silent reject), clear and re-rent.
            add_phone_wait_loops += 1;
            self.log(&format!(
              "{prefix} Phone number already submitted; waiting for phone-verification page... ({add_phone_wait_loops})"
            ));
            if add_phone_wait_loops <= 6 {
              sleep(std::time::Duration::from_secs(1)).await;
              cur_url = cdp.current_url().await.unwrap_or_default();
              continue;
            }
            self.log(&format!(
              "{prefix} Still on AddPhone after submit; discarding number and renting a new one"
            ));
            if !phone_number_used.is_empty() {
              self.used_phones.insert(phone_number_used.clone());
            }
            // Fall through: rent block below overwrites pending request + phone.
            phone_number_used.clear();
          }

          let Some(sms) = sms_service else {
            return Err("Phone verification required but no SMS provider configured".into());
          };
          let service_id = self.config.sms_service_id.ok_or_else(|| {
            "Phone verification required but smsServiceId not configured".to_string()
          })?;

          if sms_number_attempts >= MAX_SMS_NUMBER_ATTEMPTS {
            return Err(format!(
              "Phone verification failed after {MAX_SMS_NUMBER_ATTEMPTS} SMS numbers without OTP"
            ));
          }

          self.emit(
            app_handle,
            LoginStep::RequestingSmsOtp,
            &format!("{prefix} Renting SMS number..."),
            idx,
            total,
            None,
          );

          let request = NumberRequest {
            service_id,
            network: self.config.sms_network.clone(),
            prefix: None,
            except_prefix: None,
            number: None,
            country: self.config.sms_country.clone().or(Some("vn".into())),
          };
          // Skip numbers already used this batch / this account (dead OTP).
          let mut number_info = None;
          for rent_try in 0..5 {
            let candidate = sms
              .request_number(&request)
              .map_err(|e| format!("SMS rent number: {e}"))?;
            if self.used_phones.contains(&candidate.phone_number) {
              self.log(&format!(
                "{prefix} Skipping already-used phone {} (rent try {})",
                candidate.phone_number,
                rent_try + 1
              ));
              continue;
            }
            number_info = Some(candidate);
            break;
          }
          let number_info = number_info.ok_or_else(|| {
            "Could not rent a fresh SMS number (all candidates already used)".to_string()
          })?;

          phone_number_used = number_info.phone_number.clone();
          pending_sms_request_id = Some(number_info.request_id.clone());
          sms_number_attempts += 1;
          add_phone_wait_loops = 0;

          self.log(&format!(
            "{prefix} SMS number rented attempt={sms_number_attempts}/{MAX_SMS_NUMBER_ATTEMPTS} (request_id={}, phone={})",
            number_info.request_id, number_info.phone_number
          ));

          // First number: normal country select. After OTP timeout / re-rent: force
          // reselect Vietnam + clear leftover digits (SPA often leaves +1 active).
          let force_country = sms_number_attempts > 1;
          self
            .fill_phone_and_submit_inner(cdp, &number_info.phone_number, force_country)
            .await?;
          // Wait for OpenAI to move to /phone-verification.
          for _ in 0..10 {
            sleep(std::time::Duration::from_millis(500)).await;
            cur_url = cdp.current_url().await.unwrap_or_default();
            if matches!(detect_login_page_type(&cur_url), LoginPageType::PhoneOtp) {
              break;
            }
          }
          self.log(&format!("{prefix} After phone submit, URL: {cur_url}"));
        }

        LoginPageType::PhoneOtp => {
          if phone_otp_submitted {
            // Already filled OTP; wait for navigation to consent/callback.
            sleep(std::time::Duration::from_secs(1)).await;
            cur_url = cdp.current_url().await.unwrap_or_default();
            continue;
          }

          let Some(sms) = sms_service else {
            return Err("Phone OTP page shown but no SMS provider configured".into());
          };
          let Some(request_id) = pending_sms_request_id.clone() else {
            // Landed on OTP without a live Viotp session (after timeout rotation failed
            // to leave this page). Force back to AddPhone and rent again.
            self.log(&format!(
              "{prefix} PhoneOtp without pending SMS request — returning to AddPhone"
            ));
            let _ = self.return_to_add_phone(cdp, prefix).await;
            cur_url = cdp.current_url().await.unwrap_or_default();
            continue;
          };

          self.emit(
            app_handle,
            LoginStep::PollingSmsOtp,
            &format!(
              "{prefix} Waiting for SMS OTP (number {sms_number_attempts}/{MAX_SMS_NUMBER_ATTEMPTS}, {SMS_OTP_TIMEOUT_SECS}s)..."
            ),
            idx,
            total,
            None,
          );
          // On timeout/no SMS: do NOT fail the whole login_once. Blacklist the number,
          // return to AddPhone, and rent a different Viotp number within this attempt.
          let otp_info = match sms.get_otp(&request_id, SMS_OTP_TIMEOUT_SECS) {
            Ok(info) => info,
            Err(e) => {
              let err = e.to_string();
              self.log(&format!(
                "{prefix} SMS OTP poll failed for request {request_id}: {err}"
              ));
              if !phone_number_used.is_empty() {
                self.used_phones.insert(phone_number_used.clone());
              }
              pending_sms_request_id = None;
              phone_otp_submitted = false;
              add_phone_wait_loops = 0;

              if sms_number_attempts >= MAX_SMS_NUMBER_ATTEMPTS {
                return Err(format!(
                  "SMS OTP failed after {MAX_SMS_NUMBER_ATTEMPTS} numbers: {err}"
                ));
              }

              self.emit(
                app_handle,
                LoginStep::RequestingSmsOtp,
                &format!(
                  "{prefix} No SMS received — switching to a new phone number ({sms_number_attempts}/{MAX_SMS_NUMBER_ATTEMPTS})..."
                ),
                idx,
                total,
                None,
              );
              if let Err(nav_err) = self.return_to_add_phone(cdp, prefix).await {
                self.log(&format!(
                  "{prefix} return_to_add_phone warning: {nav_err} (will reclassify page)"
                ));
              }
              phone_number_used.clear();
              cur_url = cdp.current_url().await.unwrap_or_default();
              continue;
            }
          };
          let sms_code = otp_info
            .code
            .filter(|c| !c.is_empty())
            .ok_or_else(|| "SMS OTP completed without code".to_string())?;
          self.log(&format!(
            "{prefix} SMS OTP received (len={})",
            sms_code.len()
          ));

          self.emit(
            app_handle,
            LoginStep::VerifyingSmsOtp,
            &format!("{prefix} Verifying SMS OTP..."),
            idx,
            total,
            None,
          );
          self.fill_otp_and_submit(cdp, &sms_code).await?;
          phone_otp_submitted = true;
          if !phone_number_used.is_empty() {
            self.used_phones.insert(phone_number_used.clone());
          }
          // After OTP, expect Consent (Continue) or direct callback.
          for _ in 0..12 {
            sleep(std::time::Duration::from_millis(500)).await;
            cur_url = cdp.current_url().await.unwrap_or_default();
            let after = detect_login_page_type(&cur_url);
            self.log(&format!(
              "{prefix} After phone OTP poll: {after:?} url={cur_url}"
            ));
            if matches!(
              after,
              LoginPageType::Consent | LoginPageType::Callback | LoginPageType::ChatgptHome
            ) {
              break;
            }
          }
        }

        LoginPageType::Consent => {
          // OpenAI OAuth consent page - click "Continue" then expect localhost:1455 callback.
          // Returning users (phone already verified) often hit this right after 2FA — no SMS.
          self.emit(
            app_handle,
            LoginStep::ConfirmingAuthorization,
            &format!("{prefix} Confirming Codex authorization..."),
            idx,
            total,
            None,
          );
          self.log(&format!(
            "{prefix} Consent page detected, clicking Continue... url={cur_url}"
          ));
          // A few robust click strategies, then stop re-looping forever on the same page.
          // Camoufox often lands on Remix "Try again" after Continue — recover and re-click.
          let mut left_consent = false;
          for attempt in 0..5 {
            // If previous Continue produced an auth route error, recover first.
            if let Err(e) = self.recover_auth_route_error_if_any(cdp, prefix).await {
              self.log(&format!("{prefix} Consent recover warning: {e}"));
            }
            cur_url = cdp.current_url().await.unwrap_or_default();
            // If recover navigated away from consent entirely, re-classify next loop.
            if !matches!(detect_login_page_type(&cur_url), LoginPageType::Consent) {
              if let Ok(dom_page) = self.probe_page_type_from_dom(cdp).await {
                if dom_page != LoginPageType::Consent && dom_page != LoginPageType::Unknown {
                  self.log(&format!(
                    "{prefix} Left consent via recover/DOM -> {dom_page:?} url={cur_url}"
                  ));
                  if matches!(
                    dom_page,
                    LoginPageType::Callback | LoginPageType::ChatgptHome
                  ) {
                    left_consent = true;
                  }
                  break;
                }
              }
            }

            self.log(&format!("{prefix} Consent click attempt {}", attempt + 1));
            self.click_consent_button(cdp).await?;
            for _ in 0..14 {
              sleep(std::time::Duration::from_millis(500)).await;
              cur_url = cdp.current_url().await.unwrap_or_default();
              let after = detect_login_page_type(&cur_url);
              if matches!(after, LoginPageType::Callback | LoginPageType::ChatgptHome) {
                self.log(&format!("{prefix} Left consent -> {after:?} url={cur_url}"));
                left_consent = true;
                break;
              }
              // Mid-poll: if Remix error with Try again appears, recover immediately.
              if self.detect_auth_route_error(cdp).await.is_some() {
                self.log(&format!(
                  "{prefix} Auth route error after consent click; recovering…"
                ));
                let _ = self.recover_auth_route_error_if_any(cdp, prefix).await;
                cur_url = cdp.current_url().await.unwrap_or_default();
                break;
              }
            }
            if left_consent {
              break;
            }
            self.log(&format!(
              "{prefix} Still on consent after attempt {}: {cur_url}",
              attempt + 1
            ));
          }
          if !left_consent {
            // Break out of the state machine so extract_callback can wait on the listener
            // in case a late redirect arrives, instead of clicking forever.
            self.log(&format!(
              "{prefix} Consent did not navigate away; waiting on callback listener. url={cur_url}"
            ));
            break;
          }
        }

        LoginPageType::Callback | LoginPageType::ChatgptHome => {
          self.log(&format!("{prefix} Login flow reached end: {cur_url}"));
          break;
        }

        LoginPageType::Unknown => {
          // Wait and recheck URL + DOM (consent can look "unknown" mid-transition).
          sleep(std::time::Duration::from_secs(1)).await;
          cur_url = cdp.current_url().await.unwrap_or_default();
        }
      }
    }

    // Step 9: Extract callback code and state (prefer local :1455 listener)
    self.emit(
      app_handle,
      LoginStep::ExtractingCallback,
      &format!("{prefix} Extracting callback..."),
      idx,
      total,
      None,
    );

    let (code, callback_state) = self
      .extract_callback(cdp, &cur_url, callback_listener)
      .await?;
    if callback_state != expected_state {
      return Err(format!(
        "OAuth state mismatch (expected len={}, got len={})",
        expected_state.len(),
        callback_state.len()
      ));
    }
    self.log(&format!("{prefix} Callback code extracted (state ok)"));

    // Step 10: Exchange code locally (not via sub2api)
    self.emit(
      app_handle,
      LoginStep::ExchangingCode,
      &format!("{prefix} Exchanging code locally..."),
      idx,
      total,
      None,
    );

    let token_response = oauth::exchange_code(&code, code_verifier).await?;
    self.log(&format!(
      "{prefix} Token exchange successful: access_token length={}",
      token_response.access_token.len()
    ));

    let access_token = token_response.access_token;
    let refresh_token = token_response.refresh_token;

    // Step 11: Import to Sub2API if configured
    let mut push_error = String::new();
    let sub2api_account_id = if let Some(api) = sub2api {
      if self.config.push_to_sub2api {
        self.emit(
          app_handle,
          LoginStep::CreatingAccount,
          &format!("{prefix} Importing to Sub2API..."),
          idx,
          total,
          None,
        );

        match api
          .import_oauth_tokens(
            &access_token,
            &refresh_token,
            &credential.email,
            self.config.sub2api_proxy_id,
            self.config.sub2api_group_ids.clone(),
            Some(&credential.email),
          )
          .await
        {
          Ok(result) => {
            self.log(&format!(
              "{prefix} Sub2API import successful: created={}, updated={}",
              result.created, result.updated
            ));
            result
              .items
              .first()
              .and_then(|item| item.get("account_id").and_then(|id| id.as_i64()))
          }
          Err(e) => {
            push_error = e.clone();
            self.log(&format!("{prefix} Sub2API import failed: {e}"));
            None
          }
        }
      } else {
        None
      }
    } else {
      None
    };

    // Extract account_id from token (optional)
    let account_id = self.extract_account_id_from_token(&access_token);

    Ok(LoginResult {
      success: true,
      email: credential.email.clone(),
      account_id,
      access_token,
      refresh_token,
      sub2api_account_id,
      error_message: String::new(),
      push_error,
      // Filled by caller with per-account slice of logs.
      step_logs: Vec::new(),
      created_at: Utc::now(),
      phone_number: phone_number_used,
      status: LoginResultStatus::Available,
      note: String::new(),
      exported_at: None,
      password: credential.password.clone(),
      totp_secret: credential.totp_secret.clone(),
    })
  }

  /// Fill a visible input using the native HTMLInputElement value setter (React-safe).
  /// Walks open shadow roots + same-origin iframes. Polls ~12s for SPA mount.
  async fn fill_visible_input(
    &self,
    cdp: &mut BrowserSession,
    selectors: &str,
    value: &str,
    field: &str,
  ) -> Result<(), String> {
    let js = format!(
      r#"(function(){{
        const selectors = {sels};
        const val = {val};

        function isVisible(el) {{
          try {{
            const r = el.getBoundingClientRect();
            const style = window.getComputedStyle(el);
            return r.width > 0 && r.height > 0 && style.visibility !== 'hidden' && style.display !== 'none' && style.opacity !== '0';
          }} catch (_) {{ return true; }}
        }}

        function collectRoots(root, out, depth) {{
          if (!root || depth > 8) return;
          out.push(root);
          try {{
            const all = root.querySelectorAll ? root.querySelectorAll('*') : [];
            for (const el of all) {{
              if (el.shadowRoot) collectRoots(el.shadowRoot, out, depth + 1);
            }}
          }} catch (_) {{}}
        }}

        function collectDocs() {{
          const docs = [];
          const roots = [];
          collectRoots(document, roots, 0);
          docs.push({{ doc: document, roots, frame: 'top' }});
          try {{
            const iframes = document.querySelectorAll('iframe');
            for (let i = 0; i < iframes.length; i++) {{
              try {{
                const idoc = iframes[i].contentDocument;
                if (!idoc) continue;
                const iroots = [];
                collectRoots(idoc, iroots, 0);
                docs.push({{ doc: idoc, roots: iroots, frame: 'iframe:' + i + ':' + (iframes[i].src || '') }});
              }} catch (_) {{}}
            }}
          }} catch (_) {{}}
          return docs;
        }}

        function setNativeValue(el, v) {{
          el.focus();
          try {{ el.click(); }} catch (_) {{}}
          try {{ if (el.select) el.select(); }} catch (_) {{}}
          const proto = window.HTMLInputElement && window.HTMLInputElement.prototype;
          const desc = proto && Object.getOwnPropertyDescriptor(proto, 'value');
          if (desc && desc.set) desc.set.call(el, v);
          else el.value = v;
          try {{
            el.dispatchEvent(new InputEvent('input', {{ bubbles: true, composed: true, data: v, inputType: 'insertText' }}));
          }} catch (_) {{
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
          }}
          el.dispatchEvent(new Event('change', {{ bubbles: true }}));
        }}

        const docs = collectDocs();
        for (const entry of docs) {{
          for (const root of entry.roots) {{
            let nodes = [];
            try {{ nodes = Array.from(root.querySelectorAll(selectors)); }} catch (_) {{ continue; }}
            const visible = nodes.filter(isVisible);
            const list = visible.length ? visible : nodes;
            for (const el of list) {{
              try {{ el.scrollIntoView({{ block: 'center', inline: 'nearest' }}); }} catch (_) {{}}
              setNativeValue(el, val);
              return {{
                ok: true,
                value: el.value || '',
                tag: el.tagName,
                type: el.getAttribute('type') || '',
                name: el.getAttribute('name') || '',
                id: el.id || '',
                frame: entry.frame
              }};
            }}
          }}
        }}

        // Debug inventory across docs/shadows.
        const dump = [];
        for (const entry of docs) {{
          for (const root of entry.roots) {{
            let nodes = [];
            try {{ nodes = Array.from(root.querySelectorAll('input,textarea,button,[contenteditable="true"]')); }} catch (_) {{ continue; }}
            for (const el of nodes.slice(0, 20)) {{
              dump.push({{
                frame: entry.frame,
                tag: el.tagName,
                type: el.getAttribute('type') || '',
                name: el.getAttribute('name') || '',
                id: el.id || '',
                role: el.getAttribute('role') || '',
                autocomplete: el.getAttribute('autocomplete') || '',
                placeholder: el.getAttribute('placeholder') || '',
                aria: el.getAttribute('aria-label') || '',
                text: (el.innerText || el.textContent || '').slice(0, 40),
                visible: isVisible(el)
              }});
            }}
          }}
        }}
        const bodyText = (document.body && (document.body.innerText || '') || '').replace(/\\s+/g, ' ').slice(0, 240);
        return {{
          ok: false,
          reason: 'not_found',
          dump,
          url: location.href,
          title: document.title || '',
          ready: document.readyState,
          iframeCount: document.querySelectorAll('iframe').length,
          bodyText
        }};
      }})()"#,
      sels = serde_json::to_string(selectors).unwrap_or_else(|_| "\"\"".into()),
      val = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
    );

    let mut last_err = format!("fill {field}: not_found");
    for attempt in 0..24 {
      if attempt > 0 {
        sleep(std::time::Duration::from_millis(500)).await;
      }
      let result = cdp.evaluate(&js, false).await?;
      let value_json = result
        .get("value")
        .cloned()
        .ok_or_else(|| format!("fill {field}: no evaluate value"))?;
      if value_json["ok"].as_bool() == Some(true) {
        let got = value_json["value"].as_str().unwrap_or("");
        if got == value {
          return Ok(());
        }
        // Phone UIs often inject country code (+84...) after set.
        let got_digits: String = got.chars().filter(|c| c.is_ascii_digit()).collect();
        let exp_digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
        if !exp_digits.is_empty() && got_digits.ends_with(&exp_digits) {
          return Ok(());
        }
        last_err = format!(
          "fill {field}: value mismatch (expected_len={}, got_len={})",
          value.len(),
          got.len()
        );
        // For phone, one successful set with rewritten value is enough.
        if field == "phone" && got_digits.contains(&exp_digits) {
          return Ok(());
        }
        continue;
      }
      let dump = value_json
        .get("dump")
        .map(|d| d.to_string())
        .unwrap_or_default();
      let url = value_json.get("url").and_then(|u| u.as_str()).unwrap_or("");
      let body = value_json
        .get("bodyText")
        .and_then(|u| u.as_str())
        .unwrap_or("");
      let ready = value_json
        .get("ready")
        .and_then(|u| u.as_str())
        .unwrap_or("");
      let iframes = value_json
        .get("iframeCount")
        .and_then(|u| u.as_u64())
        .unwrap_or(0);
      last_err = format!(
        "fill {field}: {} url={url} ready={ready} iframes={iframes} body={body:?} dump={dump}",
        value_json["reason"].as_str().unwrap_or("failed")
      );
    }
    Err(last_err)
  }

  async fn click_submit(
    &self,
    cdp: &mut BrowserSession,
    selectors: &str,
    field: &str,
  ) -> Result<(), String> {
    let js = submit_control_probe_js(selectors);
    for attempt in 0..20 {
      if attempt > 0 {
        sleep(std::time::Duration::from_millis(250)).await;
      }
      let result = cdp.evaluate(&js, false).await?;
      let value = result.get("value").cloned().unwrap_or_default();
      if value.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        continue;
      }
      let x = value
        .get("x")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| format!("submit {field}: control has no x coordinate"))?;
      let y = value
        .get("y")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| format!("submit {field}: control has no y coordinate"))?;
      return cdp.mouse_click(x, y).await;
    }
    Err(format!(
      "submit {field}: no hydrated clickable control after 5s"
    ))
  }

  /// Detect OpenAI auth SPA hard error pages so we fail fast with a clear message.
  async fn detect_auth_route_error(&self, cdp: &mut BrowserSession) -> Option<String> {
    let js = r#"(function(){
      const body = (document.body && document.body.innerText || '').replace(/\s+/g, ' ').trim();
      if (/Oops, an error occurred/i.test(body) || /Route Error/i.test(body) || /Invalid content type/i.test(body)) {
        return body.slice(0, 280);
      }
      return null;
    })()"#;
    cdp.evaluate(js, false).await.ok().and_then(|r| {
      r.get("value")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    })
  }

  /// On MFA method chooser pages, select authenticator/TOTP.
  /// Returns Ok(true) if a method was clicked (caller should re-detect page).
  async fn select_totp_mfa_method(
    &mut self,
    cdp: &mut BrowserSession,
    prefix: &str,
  ) -> Result<bool, String> {
    let js = r#"(function(){
      // If OTP input already present, no method selection needed.
      const codeInput = document.querySelector(
        'input[name="code"], input[autocomplete="one-time-code"], input[inputmode="numeric"], input[type="tel"], input[type="text"]'
      );
      if (codeInput) {
        const r = codeInput.getBoundingClientRect();
        if (r.width > 0 && r.height > 0) return { action: 'input_ready' };
      }

      const body = (document.body && document.body.innerText || '').replace(/\s+/g, ' ');
      const hasChooser = /Select a method to verify your identity/i.test(body)
        || /Google Authenticator or similar/i.test(body);
      if (!hasChooser) return { action: 'none' };

      // Authenticator unavailable on chooser.
      if (/Google Authenticator or similar[\s\S]{0,120}temporarily unavailable/i.test(body)
          || (/temporarily unavailable/i.test(body) && /Google Authenticator/i.test(body))) {
        // Prefer Email fallback if listed.
        const emailNodes = Array.from(document.querySelectorAll('button,a,[role="button"]'));
        for (const el of emailNodes) {
          const t = (el.innerText || el.textContent || '').toLowerCase().replace(/\s+/g, ' ').trim();
          if (t === 'email' || t.startsWith('email')) {
            el.click();
            return { action: 'clicked_email_fallback', text: t.slice(0, 80) };
          }
        }
        return { action: 'totp_unavailable', body: body.slice(0, 240) };
      }

      // Only click compact method cards / buttons (not page-wide containers).
      const needles = ['google authenticator', 'authenticator', 'authentication app'];
      const nodes = Array.from(document.querySelectorAll('button,a,[role="button"]'));
      for (const el of nodes) {
        const t = (el.innerText || el.textContent || '').toLowerCase().replace(/\s+/g, ' ').trim();
        if (!t || t.length < 4 || t.length > 80) continue;
        if (t.includes('unavailable')) continue;
        if (!needles.some((n) => t.includes(n))) continue;
        try {
          el.scrollIntoView({ block: 'center' });
          el.click();
          return { action: 'clicked', text: t.slice(0, 80) };
        } catch (_) {}
      }
      return { action: 'chooser_no_click', body: body.slice(0, 240) };
    })()"#;
    let res = cdp.evaluate(js, false).await?;
    let value = res.get("value").cloned().unwrap_or_default();
    let action = value
      .get("action")
      .and_then(|a| a.as_str())
      .unwrap_or("none");
    match action {
      "none" | "input_ready" => Ok(false),
      "clicked" | "clicked_email_fallback" => {
        let text = value
          .get("text")
          .and_then(|t| t.as_str())
          .unwrap_or("authenticator");
        self.log(&format!("{prefix} MFA method selected: {text}"));
        Ok(true)
      }
      "totp_unavailable" => {
        let body = value
          .get("body")
          .and_then(|b| b.as_str())
          .unwrap_or("authenticator unavailable");
        Err(format!(
          "OpenAI MFA: Google Authenticator temporarily unavailable. Body: {body}"
        ))
      }
      other => {
        let body = value.get("body").and_then(|b| b.as_str()).unwrap_or("");
        Err(format!(
          "OpenAI MFA method chooser present but could not select TOTP ({other}). Body: {body}"
        ))
      }
    }
  }

  /// If OpenAI shows the Remix route error, click "Try again" once.
  async fn recover_auth_route_error_if_any(
    &mut self,
    cdp: &mut BrowserSession,
    prefix: &str,
  ) -> Result<(), String> {
    let Some(err) = self.detect_auth_route_error(cdp).await else {
      return Ok(());
    };
    self.log(&format!(
      "{prefix} Auth route error detected, clicking Try again… ({err})"
    ));

    // Prefer trusted selector/pointer click — bare DOM .click() is flaky on Camoufox.
    let mut clicked = cdp
      .selector_click(r#"button:has-text("Try again")"#)
      .await
      .is_ok();
    if !clicked {
      // Coordinate/DOM fallback for Chromium or selector engines without :has-text.
      let locate = r#"(function(){
        function visible(el){
          try {
            const r = el.getBoundingClientRect();
            const s = getComputedStyle(el);
            return r.width>0 && r.height>0 && s.visibility!=='hidden' && s.display!=='none';
          } catch(_) { return false; }
        }
        const buttons = Array.from(document.querySelectorAll('button,a,[role="button"]'));
        for (const b of buttons) {
          if (!visible(b)) continue;
          const t = (b.innerText || b.textContent || '').toLowerCase().trim();
          if (t === 'try again' || t.includes('try again') || t.includes('retry')) {
            const r = b.getBoundingClientRect();
            return { found: true, x: r.left + r.width/2, y: r.top + r.height/2, text: t.slice(0,40) };
          }
        }
        return { found: false };
      })()"#;
      if let Ok(res) = cdp.evaluate(locate, false).await {
        let v = res.get("value").cloned().unwrap_or_default();
        if v.get("found").and_then(|b| b.as_bool()) == Some(true) {
          let x = v.get("x").and_then(|n| n.as_f64()).unwrap_or(0.0);
          let y = v.get("y").and_then(|n| n.as_f64()).unwrap_or(0.0);
          if x > 0.0 && y > 0.0 {
            clicked = cdp.mouse_click(x, y).await.is_ok();
          }
        }
      }
    }
    if !clicked {
      let js = r#"(function(){
        const buttons = Array.from(document.querySelectorAll('button,a,[role="button"]'));
        for (const b of buttons) {
          const t = (b.innerText || b.textContent || '').toLowerCase().trim();
          if (t === 'try again' || t.includes('try again') || t.includes('retry')) {
            b.click();
            return true;
          }
        }
        return false;
      })()"#;
      let res = cdp.evaluate(js, false).await?;
      if res.get("value").and_then(|v| v.as_bool()) != Some(true) {
        return Err(format!("OpenAI auth route error (no Try again): {err}"));
      }
    }
    sleep(std::time::Duration::from_secs(3)).await;
    if let Some(err2) = self.detect_auth_route_error(cdp).await {
      return Err(format!("OpenAI auth route error after Try again: {err2}"));
    }
    Ok(())
  }

  /// Fill email input and submit.
  async fn fill_and_submit_email(
    &mut self,
    cdp: &mut BrowserSession,
    email: &str,
  ) -> Result<(), String> {
    let sels = r#"input[type="email"], input[name="email"], input[id="email"], input[autocomplete="email"], input[autocomplete*="email"], input[inputmode="email"]"#;
    self.type_into_focused(cdp, sels, email, "email").await?;
    sleep(std::time::Duration::from_millis(350)).await;
    self
      .click_submit(
        cdp,
        r#"button[type="submit"], button[data-continue="email"], button[name="intent"]"#,
        "email",
      )
      .await
  }

  /// Focus a field then type with real CDP key events (more reliable on OpenAI SPA).
  async fn type_into_focused(
    &mut self,
    cdp: &mut BrowserSession,
    selectors: &str,
    value: &str,
    field: &str,
  ) -> Result<(), String> {
    // Focus via JS first.
    let focus_js = format!(
      r#"(function(){{
        const selectors = {sels};
        const nodes = Array.from(document.querySelectorAll(selectors));
        for (const el of nodes) {{
          const r = el.getBoundingClientRect();
          if (r.width <= 0 || r.height <= 0) continue;
          el.scrollIntoView({{ block: 'center' }});
          el.focus();
          el.click();
          try {{ if (el.select) el.select(); }} catch (_) {{}}
          // Clear existing value via native setter.
          const proto = window.HTMLInputElement && window.HTMLInputElement.prototype;
          const desc = proto && Object.getOwnPropertyDescriptor(proto, 'value');
          if (desc && desc.set) desc.set.call(el, '');
          else el.value = '';
          el.dispatchEvent(new Event('input', {{ bubbles: true }}));
          return true;
        }}
        return false;
      }})()"#,
      sels = serde_json::to_string(selectors).unwrap_or_else(|_| "\"\"".into()),
    );
    let mut focused = false;
    for attempt in 0..20 {
      if attempt > 0 {
        sleep(std::time::Duration::from_millis(400)).await;
      }
      let res = cdp.evaluate(&focus_js, false).await?;
      if res.get("value").and_then(|v| v.as_bool()) == Some(true) {
        focused = true;
        break;
      }
    }
    if !focused {
      // Fall back to JS fill path for diagnostics.
      return self.fill_visible_input(cdp, selectors, value, field).await;
    }
    sleep(std::time::Duration::from_millis(150)).await;
    for ch in value.chars() {
      cdp.key_char(ch).await?;
      sleep(std::time::Duration::from_millis(35)).await;
    }
    // Fire change after typing.
    let fire = r#"(function(){
      const el = document.activeElement;
      if (!el) return false;
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      return true;
    })()"#;
    let _ = cdp.evaluate(fire, false).await;
    Ok(())
  }

  /// Fill password input and submit.
  async fn fill_and_submit_password(
    &mut self,
    cdp: &mut BrowserSession,
    password: &str,
  ) -> Result<(), String> {
    let sels = r#"input[type="password"], input[name="password"], input[name="current-password"], input[autocomplete="current-password"], input[autocomplete="password"], input[id*="password" i], input[placeholder*="password" i], input[aria-label*="password" i]"#;
    self
      .type_into_focused(cdp, sels, password, "password")
      .await?;
    sleep(std::time::Duration::from_millis(400)).await;
    self
      .click_submit(
        cdp,
        r#"button[type="submit"], button[data-continue="password"], button[name="intent"], button[value="password"]"#,
        "password",
      )
      .await
  }

  /// Fill 2FA code and submit.
  async fn fill_and_submit_2fa(
    &mut self,
    cdp: &mut BrowserSession,
    code: &str,
  ) -> Result<(), String> {
    let sels = r#"input[name="code"], input[id*="code"], input[autocomplete="one-time-code"], input[inputmode="numeric"], input[type="tel"], input[type="text"]"#;
    self.type_into_focused(cdp, sels, code, "2fa").await?;
    sleep(std::time::Duration::from_millis(300)).await;
    self
      .click_submit(
        cdp,
        r#"button[type="submit"], button[data-continue="mfa"], button[name="intent"]"#,
        "2fa",
      )
      .await
  }

  /// Normalize phone for OpenAI add-phone UI (country often preselected as +84).
  fn normalize_phone_for_openai(phone: &str, country: Option<&str>) -> String {
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    let country = country.unwrap_or("vn").to_ascii_lowercase();
    if country == "vn" || country == "la" {
      // Strip country code / leading 0 so we only type national subscriber number.
      let national = if digits.starts_with("84") && digits.len() >= 11 {
        digits[2..].to_string()
      } else if digits.starts_with('0') && digits.len() >= 10 {
        digits[1..].to_string()
      } else {
        digits
      };
      return national;
    }
    digits
  }

  /// Select Vietnam (+84) on OpenAI add-phone React Aria Select BEFORE typing number.
  ///
  /// Real control (from live DOM):
  /// - Trigger: `button[aria-haspopup="listbox"]` with `.react-aria-SelectValue`
  ///   e.g. text "United States (+1)"
  /// - Popup: `[role="listbox"]` virtualized (~233 options, only ~20 mounted)
  /// - Option: `[role="option"][data-key="VN"]` / text "Vietnam +(84)"
  async fn select_vietnam_country_code_inner(
    &mut self,
    cdp: &mut BrowserSession,
    force: bool,
  ) -> Result<bool, String> {
    // Already Vietnam?
    let check = r#"(function(){
      const trigger = document.querySelector('button[aria-haspopup="listbox"] .react-aria-SelectValue')
        || document.querySelector('button[aria-haspopup="listbox"]');
      const t = (trigger && (trigger.innerText || trigger.textContent) || '').replace(/\s+/g,' ').trim();
      if (/\+84\b/.test(t) || /vietnam/i.test(t)) return { already: true, text: t.slice(0,80) };
      return { already: false, text: t.slice(0,80) };
    })()"#;
    if let Ok(res) = cdp.evaluate(check, false).await {
      let v = res.get("value").cloned().unwrap_or_default();
      if !force && v.get("already").and_then(|b| b.as_bool()) == Some(true) {
        self.log(&format!(
          "Phone country already Vietnam: {}",
          v.get("text").and_then(|t| t.as_str()).unwrap_or("+84")
        ));
        return Ok(true);
      }
      self.log(&format!(
        "Phone country currently{}: {}",
        if force { " (force reselect)" } else { "" },
        v.get("text")
          .and_then(|t| t.as_str())
          .unwrap_or("unknown (likely United States +1)")
      ));
    }

    // Open React Aria Select trigger (NOT random buttons).
    let open_js = r#"(function(){
      function visible(el){
        try {
          const r = el.getBoundingClientRect();
          const s = getComputedStyle(el);
          return r.width>0 && r.height>0 && s.visibility!=='hidden' && s.display!=='none';
        } catch(_) { return true; }
      }
      // Prefer the SelectValue trigger used by OpenAI phone country control.
      let trigger = document.querySelector('button[aria-haspopup="listbox"]');
      // If multiple, pick one near a tel input / PhoneInput.
      const triggers = Array.from(document.querySelectorAll('button[aria-haspopup="listbox"]')).filter(visible);
      if (triggers.length > 1) {
        const tel = document.querySelector('input[type="tel"], input[autocomplete="tel"], input[inputmode="tel"]');
        if (tel) {
          const tr = tel.getBoundingClientRect();
          triggers.sort((a,b) => {
            const ar = a.getBoundingClientRect(), br = b.getBoundingClientRect();
            return Math.abs(ar.top-tr.top) - Math.abs(br.top-tr.top);
          });
        }
        trigger = triggers[0];
      }
      if (!trigger) return { opened: false, reason: 'no_listbox_trigger' };
      const before = (trigger.innerText || trigger.textContent || '').replace(/\s+/g,' ').trim();
      trigger.scrollIntoView({ block: 'center' });
      trigger.focus();
      trigger.click();
      // Also dispatch pointer events for React Aria.
      try {
        trigger.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }));
        trigger.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
        trigger.dispatchEvent(new MouseEvent('mouseup', { bubbles: true }));
        trigger.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      } catch(_) {}
      return {
        opened: true,
        expanded: trigger.getAttribute('aria-expanded') || '',
        before: before.slice(0,80)
      };
    })()"#;
    let open_res = cdp.evaluate(open_js, false).await?;
    let opened = open_res
      .get("value")
      .and_then(|v| v.get("opened"))
      .and_then(|v| v.as_bool())
      .unwrap_or(false);
    if !opened {
      return Err(
        "Phone country React Aria Select trigger not found (button[aria-haspopup=listbox])".into(),
      );
    }
    self.log(&format!(
      "Opened country Select (was: {})",
      open_res
        .get("value")
        .and_then(|v| v.get("before"))
        .and_then(|t| t.as_str())
        .unwrap_or("?")
    ));
    sleep(std::time::Duration::from_millis(500)).await;

    // Wait until the virtualized listbox actually mounts options.
    // Camoufox often needs longer than Chromium before rows appear.
    let mut options_ready = false;
    for _ in 0..20 {
      let state = cdp
        .evaluate(
          r#"(function(){
            const lb = document.querySelector('[role="listbox"]');
            const opts = Array.from(document.querySelectorAll('[role="option"]'));
            return {
              hasListbox: !!lb,
              optionCount: opts.length,
              sample: opts.slice(0, 5).map((el) => ({
                key: el.getAttribute('data-key') || '',
                text: (el.innerText||'').replace(/\s+/g,' ').trim().slice(0,40)
              }))
            };
          })()"#,
          false,
        )
        .await
        .ok()
        .and_then(|r| r.get("value").cloned())
        .unwrap_or_default();
      let count = state
        .get("optionCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
      if state.get("hasListbox").and_then(|v| v.as_bool()) == Some(true) && count > 0 {
        options_ready = true;
        self.log(&format!(
          "Country listbox ready: {count} options mounted, sample={}",
          state
            .get("sample")
            .map(|s| s.to_string())
            .unwrap_or_default()
            .chars()
            .take(180)
            .collect::<String>()
        ));
        break;
      }
      sleep(std::time::Duration::from_millis(200)).await;
    }
    if !options_ready {
      // Re-click trigger once if first open left an empty shell.
      let _ = cdp.evaluate(open_js, false).await;
      sleep(std::time::Duration::from_millis(600)).await;
    }

    // Prefer real pointer click on VN once mounted. Typeahead on Camoufox can
    // collapse the virtualizer (mounted=[]) so only type after options exist,
    // and never Enter until data-key=VN is visible.
    let pick_js = r#"(function(){
      function findVn(){
        return document.querySelector('[role="option"][data-key="VN"]')
          || Array.from(document.querySelectorAll('[role="option"]')).find((el) => {
            const t = (el.innerText || el.textContent || '');
            const key = el.getAttribute('data-key') || '';
            return key === 'VN' || (/vietnam/i.test(t) && /(\+84|\+\(84\)|\(84\))/i.test(t));
          }) || null;
      }

      function optionRect(opt){
        try {
          const r = opt.getBoundingClientRect();
          if (!(r.width > 0 && r.height > 0)) return null;
          return { x: r.left + r.width/2, y: r.top + r.height/2, w: r.width, h: r.height };
        } catch(_) {
          return null;
        }
      }

      function foundPayload(how, opt){
        try { opt.scrollIntoView({ block: 'center', inline: 'nearest' }); } catch(_) {}
        // Rect MUST be measured after scroll — pre-scroll coords miss on Camoufox.
        const rect = optionRect(opt);
        try {
          opt.setAttribute('aria-selected', 'true');
          opt.focus({ preventScroll: true });
        } catch(_) {}
        return {
          found: true,
          how,
          text: (opt.innerText||'').replace(/\s+/g,' ').trim().slice(0,80),
          rect
        };
      }

      const listbox = document.querySelector('[role="listbox"]');
      if (!listbox) return { found: false, reason: 'no_listbox', mounted: [] };

      let opt = findVn();
      if (opt) return foundPayload('mounted', opt);

      // Virtualized list: scroll tall spacer / listbox until VN mounts.
      const presentations = Array.from(listbox.querySelectorAll('[role="presentation"]'));
      const spacer = presentations.find((el) => {
        const h = parseFloat(el.style.height || '0') || el.getBoundingClientRect().height || 0;
        return h > 1000;
      }) || presentations[0] || listbox;
      const scrollers = [listbox, listbox.parentElement, spacer].filter(Boolean);
      const itemH = 40;
      const total = 250;
      // VN is near the end alphabetically — scan end first, then full range.
      const order = [];
      for (let i = 200; i < total; i++) order.push(i);
      for (let i = 0; i < 200; i += 2) order.push(i);
      for (const i of order) {
        const top = i * itemH;
        for (const s of scrollers) {
          try {
            s.scrollTop = top;
            s.dispatchEvent(new Event('scroll', { bubbles: true }));
          } catch(_) {}
        }
        try {
          listbox.dispatchEvent(new WheelEvent('wheel', {
            bubbles: true, deltaY: itemH * 3, cancelable: true
          }));
        } catch(_) {}
        opt = findVn();
        if (opt) return foundPayload('scroll_'+i, opt);
      }

      const mounted = Array.from(document.querySelectorAll('[role="option"]')).slice(0, 20).map((el) => ({
        key: el.getAttribute('data-key') || '',
        text: (el.innerText||'').replace(/\s+/g,' ').trim().slice(0,60),
        selected: el.getAttribute('aria-selected') || '',
        pos: el.getAttribute('aria-posinset') || ''
      }));
      return {
        found: false,
        reason: mounted.length ? 'vn_not_in_range' : 'vn_not_mounted',
        mounted,
        scrollTop: listbox.scrollTop || 0,
        scrollHeight: listbox.scrollHeight || 0
      };
    })()"#;

    let mut picked = false;
    for attempt in 0..8 {
      if attempt > 0 {
        sleep(std::time::Duration::from_millis(250)).await;
      }

      // Attempts 0-1: pure scroll/virtualizer without typeahead.
      // Attempts 2-3: typeahead "Vietnam" only while options are mounted.
      // Attempts 4+: reopen select and try again.
      if attempt == 2 || attempt == 5 {
        let focused = cdp
          .evaluate(
            r#"(function(){
              const lb = document.querySelector('[role="listbox"]');
              const count = document.querySelectorAll('[role="option"]').length;
              if (!lb || count === 0) return { ok: false, count: count||0 };
              lb.focus();
              return { ok: true, count };
            })()"#,
            false,
          )
          .await
          .ok()
          .and_then(|r| r.get("value").cloned())
          .unwrap_or_default();
        if focused.get("ok").and_then(|v| v.as_bool()) == Some(true) {
          self.log(&format!(
            "Country typeahead attempt {} (options={})",
            attempt,
            focused.get("count").and_then(|v| v.as_u64()).unwrap_or(0)
          ));
          for ch in "Vietnam".chars() {
            let _ = cdp.key_char(ch).await;
            sleep(std::time::Duration::from_millis(60)).await;
          }
          sleep(std::time::Duration::from_millis(400)).await;
        }
      } else if attempt == 4 || attempt == 7 {
        // Escape any broken typeahead filter, reopen clean list.
        let _ = cdp
          .evaluate(
            r#"(function(){
              document.activeElement && document.activeElement.blur && document.activeElement.blur();
              document.body && document.body.click();
              return true;
            })()"#,
            false,
          )
          .await;
        sleep(std::time::Duration::from_millis(200)).await;
        let _ = cdp.evaluate(open_js, false).await;
        sleep(std::time::Duration::from_millis(500)).await;
      } else if attempt > 0 {
        // Keyboard navigation helps virtualizers that ignore scrollTop assignment.
        for _ in 0..8 {
          let _ = cdp
            .evaluate(
              r#"(function(){
                const lb = document.querySelector('[role="listbox"]') || document.activeElement;
                if (!lb) return false;
                lb.dispatchEvent(new KeyboardEvent('keydown', { key: 'PageDown', code: 'PageDown', keyCode: 34, which: 34, bubbles: true }));
                lb.dispatchEvent(new KeyboardEvent('keyup', { key: 'PageDown', code: 'PageDown', keyCode: 34, which: 34, bubbles: true }));
                return true;
              })()"#,
              false,
            )
            .await;
          sleep(std::time::Duration::from_millis(40)).await;
        }
      }

      let pick_res = cdp.evaluate(pick_js, false).await?;
      let v = pick_res.get("value").cloned().unwrap_or_default();
      if v.get("found").and_then(|b| b.as_bool()) == Some(true) {
        // Prefer Playwright selector click (Camoufox). React Aria needs a real
        // trusted pointer sequence — synthetic DOM events alone often leave US selected.
        let mut clicked = false;
        match cdp
          .selector_click(r#"[role="option"][data-key="VN"]"#)
          .await
        {
          Ok(()) => {
            clicked = true;
            self.log("Clicked VN option via Playwright/CDP selector [data-key=VN]");
          }
          Err(e) => {
            self.log(&format!(
              "Selector click [data-key=VN] failed ({e}); trying text fallback"
            ));
            // Text fallback for builds without data-key.
            if cdp
              .selector_click(r#"[role="option"]:has-text("Vietnam")"#)
              .await
              .is_ok()
            {
              clicked = true;
              self.log("Clicked VN option via text selector");
            }
          }
        }

        // Coordinate click as secondary path (remeasured after scroll).
        if !clicked {
          let rect_js = r#"(function(){
            const opt = document.querySelector('[role="option"][data-key="VN"]')
              || Array.from(document.querySelectorAll('[role="option"]')).find((el) => {
                const t = (el.innerText || el.textContent || '');
                return /vietnam/i.test(t) && /(\+84|\+\(84\)|\(84\))/i.test(t);
              });
            if (!opt) return null;
            try { opt.scrollIntoView({ block: 'center', inline: 'nearest' }); } catch(_) {}
            const r = opt.getBoundingClientRect();
            if (!(r.width > 0 && r.height > 0)) return null;
            return { x: r.left + r.width/2, y: r.top + r.height/2, w: r.width, h: r.height };
          })()"#;
          if let Ok(rect_res) = cdp.evaluate(rect_js, false).await {
            if let Some(rect) = rect_res.get("value") {
              let x = rect.get("x").and_then(|n| n.as_f64()).unwrap_or(0.0);
              let y = rect.get("y").and_then(|n| n.as_f64()).unwrap_or(0.0);
              if x > 1.0 && y > 1.0 {
                self.log(&format!("Clicking VN option at ({x:.1},{y:.1})"));
                if cdp.mouse_click(x, y).await.is_ok() {
                  clicked = true;
                }
              }
            }
          }
        }

        // DOM activation as last resort.
        let dom_click = cdp
          .evaluate(
            r#"(function(){
              const opt = document.querySelector('[role="option"][data-key="VN"]')
                || Array.from(document.querySelectorAll('[role="option"]')).find((el) => {
                  const t = (el.innerText || el.textContent || '');
                  return /vietnam/i.test(t) && /(\+84|\+\(84\)|\(84\))/i.test(t);
                });
              if (!opt) return { ok: false, reason: 'missing' };
              try { opt.scrollIntoView({ block: 'center' }); } catch(_) {}
              try { opt.focus({ preventScroll: true }); } catch(_) {}
              const fire = (type, Ctor, init) => {
                try { opt.dispatchEvent(new Ctor(type, Object.assign({ bubbles: true, cancelable: true, view: window }, init||{}))); } catch(_) {}
              };
              fire('pointerdown', PointerEvent, { pointerId: 1, pointerType: 'mouse', isPrimary: true, button: 0, buttons: 1 });
              fire('mousedown', MouseEvent, { button: 0, buttons: 1 });
              fire('pointerup', PointerEvent, { pointerId: 1, pointerType: 'mouse', isPrimary: true, button: 0, buttons: 0 });
              fire('mouseup', MouseEvent, { button: 0, buttons: 0 });
              fire('click', MouseEvent, { button: 0, buttons: 0 });
              try { opt.click(); } catch(_) {}
              try {
                opt.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', code: 'Enter', keyCode: 13, which: 13, bubbles: true }));
                opt.dispatchEvent(new KeyboardEvent('keyup', { key: 'Enter', code: 'Enter', keyCode: 13, which: 13, bubbles: true }));
              } catch(_) {}
              return {
                ok: true,
                selected: opt.getAttribute('aria-selected') || '',
                text: (opt.innerText||'').replace(/\s+/g,' ').trim().slice(0,60)
              };
            })()"#,
            false,
          )
          .await;
        if let Ok(res) = dom_click {
          self.log(&format!(
            "DOM VN activate: {}",
            res
              .get("value")
              .map(|v| v.to_string())
              .unwrap_or_default()
              .chars()
              .take(160)
              .collect::<String>()
          ));
        }
        if !clicked {
          self.log("Trusted click path failed; relied on DOM activate");
        }
        sleep(std::time::Duration::from_millis(350)).await;
        // Enter only after VN is mounted/focused — commits selection on React Aria.
        let _ = cdp.press_enter().await;
        sleep(std::time::Duration::from_millis(400)).await;

        // Early verify — if still US, keep looping attempts rather than fail once.
        if let Ok(res) = cdp.evaluate(check, false).await {
          let early = res.get("value").cloned().unwrap_or_default();
          if early.get("already").and_then(|b| b.as_bool()) == Some(true) {
            picked = true;
            self.log(&format!(
              "Selected phone country via {}: {}",
              v.get("how").and_then(|h| h.as_str()).unwrap_or("?"),
              early
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("Vietnam +84")
            ));
            break;
          }
          self.log(&format!(
            "VN click attempt {} did not stick (still: {}); retrying",
            attempt,
            early.get("text").and_then(|t| t.as_str()).unwrap_or("?")
          ));
          // Re-open list for next attempt.
          let _ = cdp.evaluate(open_js, false).await;
          sleep(std::time::Duration::from_millis(400)).await;
          continue;
        }

        picked = true;
        self.log(&format!(
          "Selected phone country via {}: {}",
          v.get("how").and_then(|h| h.as_str()).unwrap_or("?"),
          v.get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("Vietnam +84")
        ));
        break;
      }
      if attempt == 7 {
        self.log(&format!(
          "VN option not found in listbox: {}",
          v.to_string().chars().take(500).collect::<String>()
        ));
      }
    }
    if !picked {
      return Err(
        "Failed to select Vietnam (+84) in React Aria country listbox (data-key=VN)".into(),
      );
    }

    sleep(std::time::Duration::from_millis(300)).await;

    // Verify trigger now shows +84 / Vietnam.
    let verify = r#"(function(){
      const trigger = document.querySelector('button[aria-haspopup="listbox"] .react-aria-SelectValue')
        || document.querySelector('button[aria-haspopup="listbox"]');
      const t = (trigger && (trigger.innerText || trigger.textContent) || '').replace(/\s+/g,' ').trim();
      const ok = /\+84\b/.test(t) || /vietnam/i.test(t);
      return { ok, text: t.slice(0,80) };
    })()"#;
    let ver = cdp.evaluate(verify, false).await?;
    let ok = ver
      .get("value")
      .and_then(|v| v.get("ok"))
      .and_then(|b| b.as_bool())
      .unwrap_or(false);
    let text = ver
      .get("value")
      .and_then(|v| v.get("text"))
      .and_then(|t| t.as_str())
      .unwrap_or("")
      .to_string();
    if !ok {
      return Err(format!(
        "Country Select still not Vietnam after pick (shows: {text})"
      ));
    }
    self.log(&format!("Phone country confirmed: {text}"));
    Ok(true)
  }

  /// After SMS timeout on phone-verification, return to add-phone so a new
  /// Viotp number can be entered. Prefer in-page "change / different number"
  /// links, then browser history, then hard-navigate to add-phone.
  async fn return_to_add_phone(
    &mut self,
    cdp: &mut BrowserSession,
    prefix: &str,
  ) -> Result<(), String> {
    // 1) Click "change number" / "use a different number" if OpenAI shows it.
    let click_change = r#"(function(){
      function visible(el){
        try {
          const r = el.getBoundingClientRect();
          const st = getComputedStyle(el);
          return r.width > 0 && r.height > 0 && st.visibility !== 'hidden' && st.display !== 'none';
        } catch(_) { return false; }
      }
      const re = /(change|different|another|other|edit|update|use a different|try another).*(number|phone)|s[ốo]\s*kh[áa]c|đ[ổo]i\s*s[ốo]/i;
      const nodes = Array.from(document.querySelectorAll('a,button,[role="button"],[role="link"]'));
      for (const el of nodes) {
        if (!visible(el)) continue;
        const t = (el.innerText || el.textContent || el.getAttribute('aria-label') || '').replace(/\s+/g,' ').trim();
        if (!t || t.length > 80) continue;
        if (re.test(t)) {
          try { el.click(); return { ok:true, how:'click', text:t.slice(0,60) }; } catch(e) {}
        }
      }
      return { ok:false };
    })()"#;
    if let Ok(res) = cdp.evaluate(click_change, false).await {
      if res
        .pointer("/value/ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
      {
        let text = res
          .pointer("/value/text")
          .and_then(|v| v.as_str())
          .unwrap_or("?");
        self.log(&format!("{prefix} Clicked change-number control: {text}"));
        sleep(std::time::Duration::from_secs(2)).await;
      }
    }

    // 2) history.back() once or twice (OTP → add-phone) if still not on add-phone.
    for i in 0..2 {
      let url = cdp.current_url().await.unwrap_or_default();
      let on_add = matches!(detect_login_page_type(&url), LoginPageType::AddPhone)
        || self
          .probe_page_type_from_dom(cdp)
          .await
          .ok()
          .is_some_and(|p| p == LoginPageType::AddPhone);
      if on_add {
        break;
      }
      let _ = cdp.evaluate("history.back(); 'ok'", false).await;
      sleep(std::time::Duration::from_secs(1)).await;
      let url = cdp.current_url().await.unwrap_or_default();
      self.log(&format!("{prefix} history.back() #{i} -> {url}"));
    }

    // 3) Hard navigate to add-phone when still not there (most reliable after OTP).
    let url = cdp.current_url().await.unwrap_or_default();
    let on_add = matches!(detect_login_page_type(&url), LoginPageType::AddPhone)
      || self
        .probe_page_type_from_dom(cdp)
        .await
        .ok()
        .is_some_and(|p| p == LoginPageType::AddPhone);
    if !on_add {
      self.log(&format!(
        "{prefix} Navigating hard to add-phone after SMS timeout"
      ));
      cdp
        .navigate("https://auth.openai.com/add-phone", 20)
        .await?;
      sleep(std::time::Duration::from_secs(2)).await;
    }

    // 4) Wait until country Select + tel input are mounted (fresh form).
    self.wait_for_add_phone_form(cdp, prefix).await?;
    Ok(())
  }

  /// Poll until add-phone country Select + tel input are present.
  async fn wait_for_add_phone_form(
    &mut self,
    cdp: &mut BrowserSession,
    prefix: &str,
  ) -> Result<(), String> {
    for attempt in 0..20 {
      let ready = cdp
        .evaluate(
          r#"(function(){
            const trigger = document.querySelector('button[aria-haspopup="listbox"]');
            const tel = document.querySelector('input[type="tel"], input[autocomplete="tel"], input[inputmode="tel"], input[name="phone"], input[name="phoneNumber"]');
            const country = (trigger && (trigger.innerText || trigger.textContent) || '').replace(/\s+/g,' ').trim().slice(0,80);
            return {
              ok: !!(trigger && tel),
              country,
              hasTel: !!tel,
              hasCountry: !!trigger,
              href: (location.href || '').slice(0,120)
            };
          })()"#,
          false,
        )
        .await
        .ok()
        .and_then(|r| r.get("value").cloned())
        .unwrap_or_default();
      if ready.get("ok").and_then(|b| b.as_bool()) == Some(true) {
        self.log(&format!(
          "{prefix} Add-phone form ready (country='{}' href={})",
          ready.get("country").and_then(|c| c.as_str()).unwrap_or("?"),
          ready.get("href").and_then(|h| h.as_str()).unwrap_or("?")
        ));
        return Ok(());
      }
      if attempt == 0 || attempt % 5 == 4 {
        self.log(&format!(
          "{prefix} Waiting add-phone form… hasCountry={} hasTel={} href={}",
          ready
            .get("hasCountry")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
          ready
            .get("hasTel")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
          ready.get("href").and_then(|h| h.as_str()).unwrap_or("?")
        ));
      }
      sleep(std::time::Duration::from_millis(250)).await;
    }
    Err("Add-phone form (country select + tel) not ready after navigation".into())
  }

  /// Clear residual digits from a previous phone attempt.
  async fn clear_phone_input(&mut self, cdp: &mut BrowserSession) -> Result<(), String> {
    let js = r#"(function(){
      const sels = [
        'input[type="tel"]',
        'input[name="phone"]',
        'input[name="phoneNumber"]',
        'input[autocomplete="tel"]',
        'input[inputmode="tel"]',
        'input[inputmode="numeric"]'
      ];
      let el = null;
      for (const s of sels) {
        const c = document.querySelector(s);
        if (c && c.offsetParent !== null) { el = c; break; }
      }
      if (!el) return { ok:false, reason:'no_tel' };
      try { el.focus(); } catch(_) {}
      const proto = window.HTMLInputElement && window.HTMLInputElement.prototype;
      const desc = proto && Object.getOwnPropertyDescriptor(proto, 'value');
      if (desc && desc.set) desc.set.call(el, '');
      else el.value = '';
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      return { ok:true, beforeLen: (el.defaultValue||'').length, after: (el.value||'').length };
    })()"#;
    match cdp.evaluate(js, false).await {
      Ok(res) => {
        self.log(&format!(
          "Cleared phone input: {}",
          res.get("value").map(|v| v.to_string()).unwrap_or_default()
        ));
        Ok(())
      }
      Err(e) => {
        self.log(&format!("Clear phone input warning: {e}"));
        Ok(())
      }
    }
  }

  /// Fill phone number and submit.
  ///
  /// `force_country_reselect`: after SMS timeout we always re-pick Vietnam even
  /// if the Select already shows +84 (stale SPA state after change-number).
  async fn fill_phone_and_submit_inner(
    &mut self,
    cdp: &mut BrowserSession,
    phone: &str,
    force_country_reselect: bool,
  ) -> Result<(), String> {
    // Wait for form controls — critical after return_to_add_phone.
    if let Err(e) = self.wait_for_add_phone_form(cdp, "phone").await {
      self.log(&format!("Add-phone form wait: {e}"));
    }

    // Clear leftover number from previous Viotp attempt before re-typing.
    if force_country_reselect {
      self.clear_phone_input(cdp).await?;
      sleep(std::time::Duration::from_millis(200)).await;
    }

    // REQUIRED: select Vietnam (+84) on React Aria Select first.
    // Default UI is "United States (+1)" — typing VN digits without this fails validation.
    // After OTP timeout re-entry, always force reselect so we never type into +1 by accident.
    self
      .select_vietnam_country_code_inner(cdp, force_country_reselect)
      .await?;

    let national = Self::normalize_phone_for_openai(phone, self.config.sms_country.as_deref());
    if national.is_empty() {
      return Err("Phone number empty after normalize".into());
    }
    self.log(&format!(
      "Filling phone national digits len={} (raw_len={}) force_country={}",
      national.len(),
      phone.len(),
      force_country_reselect
    ));

    let sels = r#"input[type="tel"], input[name="phone"], input[name="phoneNumber"], input[autocomplete="tel"], input[inputmode="tel"], input[inputmode="numeric"]"#;
    // Soft-fill: OpenAI often rewrites value to +84XXXXXXXXX — accept if national digits are present.
    match self.type_into_focused(cdp, sels, &national, "phone").await {
      Ok(()) => {}
      Err(e) if e.contains("value mismatch") => {
        self.log(&format!("phone fill soft-accept after format rewrite: {e}"));
      }
      Err(e) => return Err(e),
    }
    sleep(std::time::Duration::from_millis(500)).await;

    // Guard: reject if country still shows +1 / United States. Retry force-select once.
    let guard = r#"(function(){
      const trigger = document.querySelector('button[aria-haspopup="listbox"] .react-aria-SelectValue')
        || document.querySelector('button[aria-haspopup="listbox"]');
      const t = (trigger && (trigger.innerText || trigger.textContent) || '').replace(/\s+/g,' ').trim();
      const phone = document.querySelector('input[type="tel"], input[autocomplete="tel"]');
      const pv = phone ? (phone.value || '') : '';
      return { country: t.slice(0,80), phone: pv.slice(0,40), ok: /\+84\b/.test(t) || /vietnam/i.test(t) };
    })()"#;
    if let Ok(res) = cdp.evaluate(guard, false).await {
      let v = res.get("value").cloned().unwrap_or_default();
      if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
        self.log(&format!(
          "Phone country guard failed (shows {}); force reselect + retype",
          v.get("country").and_then(|c| c.as_str()).unwrap_or("?")
        ));
        self.clear_phone_input(cdp).await?;
        self.select_vietnam_country_code_inner(cdp, true).await?;
        match self.type_into_focused(cdp, sels, &national, "phone").await {
          Ok(()) => {}
          Err(e) if e.contains("value mismatch") => {
            self.log(&format!("phone fill soft-accept after country retry: {e}"));
          }
          Err(e) => return Err(e),
        }
        sleep(std::time::Duration::from_millis(400)).await;
        let res2 = cdp.evaluate(guard, false).await?;
        let v2 = res2.get("value").cloned().unwrap_or_default();
        if v2.get("ok").and_then(|b| b.as_bool()) != Some(true) {
          return Err(format!(
            "Refusing to submit phone: country still not VN (+84). Shows: {}",
            v2.get("country").and_then(|c| c.as_str()).unwrap_or("?")
          ));
        }
        self.log(&format!(
          "Pre-submit phone guard ok after retry: country={} phone={}",
          v2.get("country").and_then(|c| c.as_str()).unwrap_or("?"),
          v2.get("phone").and_then(|c| c.as_str()).unwrap_or("?")
        ));
      } else {
        self.log(&format!(
          "Pre-submit phone guard ok: country={} phone={}",
          v.get("country").and_then(|c| c.as_str()).unwrap_or("?"),
          v.get("phone").and_then(|c| c.as_str()).unwrap_or("?")
        ));
      }
    }

    self
      .click_submit(
        cdp,
        r#"button[type="submit"], button[name="intent"]"#,
        "phone",
      )
      .await
  }

  /// Fill OTP code and submit.
  async fn fill_otp_and_submit(&self, cdp: &mut BrowserSession, code: &str) -> Result<(), String> {
    self
      .fill_visible_input(
        cdp,
        r#"input[name="code"], input[autocomplete="one-time-code"], input[inputmode="numeric"]"#,
        code,
        "sms_otp",
      )
      .await?;
    sleep(std::time::Duration::from_millis(300)).await;
    self
      .click_submit(
        cdp,
        r#"button[type="submit"], button[name="intent"]"#,
        "sms_otp",
      )
      .await
  }

  async fn detect_unsupported_region_error_from_dom(
    &mut self,
    cdp: &mut BrowserSession,
  ) -> Option<String> {
    let result = cdp
      .evaluate(
        r#"(document.body && (document.body.innerText || document.body.textContent) || '').slice(0, 4000)"#,
        false,
      )
      .await
      .ok()?;
    let body = result.get("value").and_then(|v| v.as_str())?;
    extract_unsupported_region_error(body)
  }

  async fn detect_cloudflare_challenge_from_dom(&mut self, cdp: &mut BrowserSession) -> bool {
    let result = cdp
      .evaluate(
        r#"(function(){
          const title = document.title || '';
          const body = (document.body && (document.body.innerText || document.body.textContent) || '').slice(0, 4000);
          const hasTurnstile = !!document.querySelector(
            'iframe[src*="challenges.cloudflare.com"], .cf-turnstile, #cf-turnstile, input[name="cf-turnstile-response"]'
          );
          return {
            title,
            body,
            hasTurnstile,
            href: location.href || ''
          };
        })()"#,
        false,
      )
      .await;
    let Ok(res) = result else {
      return false;
    };
    let value = res.get("value").cloned().unwrap_or_default();
    let title = value.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let body = value.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let href = value.get("href").and_then(|v| v.as_str()).unwrap_or("");
    let has_turnstile = value
      .get("hasTurnstile")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);
    has_turnstile
      || is_cloudflare_challenge_signal(title)
      || is_cloudflare_challenge_signal(body)
      || is_cloudflare_challenge_signal(href)
  }

  /// Soft-wait for Cloudflare managed challenge / Turnstile to auto-clear.
  /// Returns Ok when challenge disappears; Err when still present after budget.
  async fn wait_out_cloudflare_challenge_if_any(
    &mut self,
    cdp: &mut BrowserSession,
    prefix: &str,
  ) -> Result<(), String> {
    if !self.detect_cloudflare_challenge_from_dom(cdp).await {
      return Ok(());
    }
    self.log(&format!(
      "{prefix} Cloudflare challenge detected; soft-waiting up to {CLOUDFLARE_SOFT_WAIT_SECS}s..."
    ));
    let deadline =
      tokio::time::Instant::now() + std::time::Duration::from_secs(CLOUDFLARE_SOFT_WAIT_SECS);
    while tokio::time::Instant::now() < deadline {
      if self.is_cancelled() {
        return Err("Cancelled during Cloudflare challenge wait".into());
      }
      sleep(std::time::Duration::from_secs(2)).await;
      if !self.detect_cloudflare_challenge_from_dom(cdp).await {
        self.log(&format!(
          "{prefix} Cloudflare challenge cleared after soft-wait"
        ));
        return Ok(());
      }
    }
    Err(cloudflare_challenge_error_message())
  }

  /// Probe DOM when URL classification is ambiguous (SPA / post-OTP consent jump).
  async fn probe_page_type_from_dom(
    &mut self,
    cdp: &mut BrowserSession,
  ) -> Result<LoginPageType, String> {
    let js = r#"(function(){
      const url = new URL(location.href);
      const localHost = url.hostname === 'localhost' || url.hostname === '127.0.0.1';
      const hasCallbackParams = url.searchParams.has('code') && url.searchParams.has('state');
      if (localHost && (url.pathname.includes('/auth/callback') || hasCallbackParams)) {
        return 'callback';
      }
      const href = url.href.toLowerCase();
      const body = (document.body && (document.body.innerText || document.body.textContent) || '').slice(0, 4000).toLowerCase();
      const hasTel = !!document.querySelector('input[type="tel"], input[autocomplete="tel"]');
      const hasCountry = !!document.querySelector('button[aria-haspopup="listbox"]');
      if (hasTel && hasCountry) return 'phone';
      const hasPwd = !!document.querySelector('input[type="password"], input[name="password"], input[autocomplete="current-password"]');
      if (hasPwd) return 'password';
      const hasCode = !!document.querySelector('input[autocomplete="one-time-code"], input[name="code"], input[inputmode="numeric"]');
      // Phone SMS OTP page (after add-phone submit): code field + phone wording, not authenticator.
      if (hasCode && /phone|sms|text message|verification code/i.test(body)
          && !/authenticator|google authenticator|two-factor|2fa|mfa/i.test(body)) {
        return 'phone_otp';
      }
      const hasTotp = hasCode
        && /authenticator|verification code|enter code|two-factor|2fa|mfa/i.test(body);
      if (hasTotp) return 'totp';
      const hasEmail = !!document.querySelector('input[type="email"], input[name="email"], input[name="username"], input[autocomplete="username"]');
      if (hasEmail) return 'email';
      // Consent / authorize gate: Continue button, no credential fields.
      // Returning users (phone already verified) land here after password/2FA — skip SMS.
      const btns = Array.from(document.querySelectorAll('button, [role="button"]'))
        .filter((b) => b.offsetParent !== null)
        .map((b) => (b.textContent || '').trim().toLowerCase());
      const consentish = btns.some((t) =>
        t === 'continue' || t.includes('continue') || t.includes('authorize') || t.includes('allow') || t.includes('accept')
        || t.includes('continuar') || t.includes('続行') || t.includes('bestätigen')
      );
      const hasConsentAttr = !!document.querySelector('button[data-continue="consent"]');
      if ((hasConsentAttr || consentish) && !hasEmail && !hasPwd && !hasTel) return 'consent';
      if (href.includes('consent') || href.includes('sign-in-with')) return 'consent';
      return 'unknown';
    })()"#;
    let res = cdp.evaluate(js, false).await?;
    let kind = res
      .get("value")
      .and_then(|v| v.as_str())
      .unwrap_or("unknown");
    Ok(match kind {
      "callback" => LoginPageType::Callback,
      "phone" => LoginPageType::AddPhone,
      "phone_otp" => LoginPageType::PhoneOtp,
      "password" => LoginPageType::LoginPassword,
      "totp" => LoginPageType::TwoFactor,
      "email" => LoginPageType::LoginEmail,
      "consent" => LoginPageType::Consent,
      _ => LoginPageType::Unknown,
    })
  }

  /// Click consent/authorization button on OpenAI consent page.
  /// Prefer real CDP mouse click at button center — React Aria / SPA often ignores bare .click().
  async fn click_consent_button(&mut self, cdp: &mut BrowserSession) -> Result<(), String> {
    // Dump visible controls for diagnostics (first attempt / stuck pages).
    let dump_js = r#"(function(){
      function visible(el){
        try {
          const r = el.getBoundingClientRect();
          const s = getComputedStyle(el);
          return r.width>0 && r.height>0 && s.visibility!=='hidden' && s.display!=='none';
        } catch(_) { return false; }
      }
      const nodes = Array.from(document.querySelectorAll('button,[role="button"],input[type="submit"],a'));
      return nodes.filter(visible).slice(0, 12).map((el) => ({
        tag: el.tagName.toLowerCase(),
        type: el.getAttribute('type') || '',
        text: (el.innerText || el.textContent || '').replace(/\s+/g,' ').trim().slice(0,60),
        disabled: !!(el.disabled || el.getAttribute('aria-disabled') === 'true'),
        dataContinue: el.getAttribute('data-continue') || '',
        name: el.getAttribute('name') || '',
        href: (el.getAttribute('href') || '').slice(0,80),
      }));
    })()"#;
    if let Ok(dump) = cdp.evaluate(dump_js, false).await {
      let v = dump.get("value").cloned().unwrap_or_default();
      self.log(&format!(
        "Consent page controls: {}",
        v.to_string().chars().take(500).collect::<String>()
      ));
    }

    // Tick any visible checkbox / switch that may gate Continue.
    let _ = cdp
      .evaluate(
        r#"(function(){
          const boxes = Array.from(document.querySelectorAll('input[type="checkbox"], [role="checkbox"]'));
          let n = 0;
          for (const el of boxes) {
            try {
              const checked = el.checked === true || el.getAttribute('aria-checked') === 'true';
              if (!checked) { el.click(); n++; }
            } catch(_) {}
          }
          return n;
        })()"#,
        false,
      )
      .await;

    // Locate best Continue target and return center coords for CDP mouse click.
    let locate_js = r#"(function(){
      function visible(el){
        try {
          const r = el.getBoundingClientRect();
          const s = getComputedStyle(el);
          return r.width>0 && r.height>0 && s.visibility!=='hidden' && s.display!=='none' && s.pointerEvents!=='none';
        } catch(_) { return false; }
      }
      function center(el){
        const r = el.getBoundingClientRect();
        return { x: r.left + r.width/2, y: r.top + r.height/2, w: r.width, h: r.height };
      }
      function disabled(el){
        return !!(el.disabled || el.getAttribute('aria-disabled') === 'true' || el.classList.contains('disabled'));
      }
      const patterns = [
        'continue', 'continuar', 'continua', '続行', 'weiter',
        'authorize', 'autorisieren', 'autoriser', '承認',
        'confirm', 'confirmar', 'bestätigen', 'confirmer',
        'allow', 'permit', 'zulassen', 'accept', 'aceptar', 'aceitar', 'accepter'
      ];
      const candidates = [];
      // Highest priority: data-continue=consent
      for (const el of document.querySelectorAll('button[data-continue="consent"], [data-continue="consent"]')) {
        if (visible(el) && !disabled(el)) candidates.push({ el, score: 100, how: 'data-continue' });
      }
      // Exact / includes Continue text
      for (const el of document.querySelectorAll('button,[role="button"],input[type="submit"],a')) {
        if (!visible(el) || disabled(el)) continue;
        const t = (el.innerText || el.textContent || el.value || '').replace(/\s+/g,' ').trim().toLowerCase();
        if (!t) continue;
        // Skip cancel/back/deny
        if (/cancel|back|deny|reject|not now|skip|logout|sign out/.test(t)) continue;
        let score = 0;
        if (t === 'continue') score = 90;
        else if (patterns.some((p) => t === p)) score = 80;
        else if (patterns.some((p) => t.includes(p))) score = 60;
        else if (el.getAttribute('type') === 'submit') score = 30;
        if (score > 0) candidates.push({ el, score, how: 'text:'+t.slice(0,40) });
      }
      candidates.sort((a,b) => b.score - a.score);
      if (!candidates.length) return { found: false };
      const best = candidates[0];
      const c = center(best.el);
      try { best.el.scrollIntoView({ block: 'center', inline: 'center' }); } catch(_) {}
      // Focus for keyboard fallback
      try { best.el.focus(); } catch(_) {}
      return {
        found: true,
        how: best.how,
        score: best.score,
        disabled: disabled(best.el),
        text: (best.el.innerText || best.el.textContent || '').replace(/\s+/g,' ').trim().slice(0,60),
        x: c.x, y: c.y, w: c.w, h: c.h
      };
    })()"#;

    let locate = cdp.evaluate(locate_js, false).await?;
    let v = locate.get("value").cloned().unwrap_or_default();
    if v.get("found").and_then(|b| b.as_bool()) != Some(true) {
      self.log("Warning: consent Continue control not found");
      return Ok(());
    }
    let x = v.get("x").and_then(|n| n.as_f64()).unwrap_or(0.0);
    let y = v.get("y").and_then(|n| n.as_f64()).unwrap_or(0.0);
    self.log(&format!(
      "Consent target how={} text={} at ({x:.0},{y:.0})",
      v.get("how").and_then(|h| h.as_str()).unwrap_or("?"),
      v.get("text").and_then(|t| t.as_str()).unwrap_or("?")
    ));

    // 0) Playwright/CDP selector click first (most reliable on Camoufox).
    let mut selector_ok = false;
    for sel in [
      r#"button[data-continue="consent"]"#,
      r#"button[type="submit"]"#,
      r#"button:has-text("Continue")"#,
    ] {
      match cdp.selector_click(sel).await {
        Ok(()) => {
          self.log(&format!("Consent selector click ok: {sel}"));
          selector_ok = true;
          break;
        }
        Err(e) => {
          self.log(&format!("Consent selector click miss ({sel}): {e}"));
        }
      }
    }
    if selector_ok {
      // Avoid multi-click storms that can trigger Remix route errors.
      sleep(std::time::Duration::from_millis(800)).await;
      return Ok(());
    }

    // 1) Real mouse click via CDP / Playwright coordinates
    if x > 0.0 && y > 0.0 {
      if let Err(e) = cdp.mouse_click(x, y).await {
        self.log(&format!("Consent CDP mouse_click failed: {e}"));
      } else {
        self.log("Consent CDP mouse_click dispatched");
      }
      sleep(std::time::Duration::from_millis(400)).await;
    }

    // 2) Also fire DOM click + pointer sequence + requestSubmit as fallback
    let fire_js = r#"(function(){
      function visible(el){
        try {
          const r = el.getBoundingClientRect();
          const s = getComputedStyle(el);
          return r.width>0 && r.height>0 && s.visibility!=='hidden' && s.display!=='none';
        } catch(_) { return false; }
      }
      const patterns = ['continue','authorize','allow','accept','confirm','continuar','続行'];
      let target =
        document.querySelector('button[data-continue="consent"]') ||
        Array.from(document.querySelectorAll('button,[role="button"],input[type="submit"]'))
          .find((el) => {
            if (!visible(el)) return false;
            const t = (el.innerText || el.textContent || el.value || '').toLowerCase().trim();
            return patterns.some((p) => t === p || t.includes(p));
          });
      if (!target) return { ok: false };
      try {
        target.focus();
        target.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }));
        target.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, cancelable: true }));
        target.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, cancelable: true }));
        target.dispatchEvent(new MouseEvent('mouseup', { bubbles: true, cancelable: true }));
        target.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
        target.click();
      } catch(e) { return { ok: false, err: String(e) }; }
      // If inside form, requestSubmit with this button
      try {
        const form = target.closest('form');
        if (form && typeof form.requestSubmit === 'function') form.requestSubmit(target);
      } catch(_) {}
      return { ok: true, text: (target.innerText||'').replace(/\s+/g,' ').trim().slice(0,40) };
    })()"#;
    if let Ok(res) = cdp.evaluate(fire_js, false).await {
      let vv = res.get("value").cloned().unwrap_or_default();
      self.log(&format!(
        "Consent DOM fire: {}",
        vv.to_string().chars().take(200).collect::<String>()
      ));
    }

    // 3) Enter key on focused control
    let _ = cdp.press_enter().await;

    Ok(())
  }

  /// Extract code and state from OAuth redirect.
  ///
  /// Prefer the local HTTP listener on :1455 (reliable). Fall back to polling
  /// CDP `current_url` in case the browser still exposes the callback URL.
  async fn extract_callback(
    &mut self,
    cdp: &mut BrowserSession,
    initial_url: &str,
    callback_listener: &mut OAuthCallbackListener,
  ) -> Result<(String, String), String> {
    async {
      // Already at a callback URL in the browser (rare; chrome-error loses query).
      if let Some((code, state)) = Sub2ApiClient::parse_callback_url(initial_url) {
        return Ok((code, state));
      }

      self.log("Waiting for OAuth callback on localhost:1455 (listener primary)...");

      let timeout = std::time::Duration::from_secs(90);
      let listener_wait = callback_listener.wait_for_code(timeout);
      tokio::pin!(listener_wait);

      let deadline = tokio::time::Instant::now() + timeout;
      loop {
        if self.is_cancelled() {
          return Err("Cancelled during callback wait".into());
        }

        tokio::select! {
          biased;
          result = &mut listener_wait => {
            match result {
              Ok((code, state)) => {
                self.log("OAuth callback received via local HTTP listener");
                return Ok((code, state));
              }
              Err(e) => {
                self.log(&format!(
                  "OAuth listener ended ({e}); continuing with CDP URL poll if any time remains"
                ));
                break;
              }
            }
          }
          _ = sleep(std::time::Duration::from_millis(500)) => {
            let url = cdp.current_url().await.unwrap_or_default();
            if let Some((code, state)) = Sub2ApiClient::parse_callback_url(&url) {
              self.log("OAuth callback parsed from browser URL (CDP fallback)");
              return Ok((code, state));
            }
            if url.contains("chatgpt.com") && !url.contains("auth.openai.com") {
              return Err(
                "Login completed without OAuth callback - tokens not extractable via this flow"
                  .into(),
              );
            }
            if tokio::time::Instant::now() >= deadline {
              return Err("Timeout waiting for OAuth callback on localhost:1455".into());
            }
          }
        }
      }

      // Listener failed early — pure CDP poll for remaining time.
      while tokio::time::Instant::now() < deadline {
        if self.is_cancelled() {
          return Err("Cancelled during callback wait".into());
        }
        sleep(std::time::Duration::from_secs(1)).await;
        let url = cdp.current_url().await.unwrap_or_default();
        if let Some((code, state)) = Sub2ApiClient::parse_callback_url(&url) {
          self.log("OAuth callback parsed from browser URL (CDP fallback)");
          return Ok((code, state));
        }
        if url.contains("chatgpt.com") && !url.contains("auth.openai.com") {
          return Err(
            "Login completed without OAuth callback - tokens not extractable via this flow".into(),
          );
        }
      }

      Err("Timeout waiting for OAuth callback on localhost:1455".into())
    }
    .await
  }

  /// Extract account_id from JWT access_token.
  fn extract_account_id_from_token(&self, access_token: &str) -> String {
    if access_token.is_empty() {
      return String::new();
    }

    // JWT format: header.payload.signature
    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() != 3 {
      return String::new();
    }

    // Decode base64url payload
    let payload = parts[1];
    let decoded =
      match base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload) {
        Ok(d) => d,
        Err(_) => return String::new(),
      };

    // Parse JSON and extract account_id
    match serde_json::from_slice::<serde_json::Value>(&decoded) {
      Ok(json) => json
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(|id| id.as_str())
        .unwrap_or("")
        .to_string(),
      Err(_) => String::new(),
    }
  }

  /// Extract session tokens from browser.
  #[allow(dead_code)]
  async fn extract_session_tokens(&self, cdp: &mut BrowserSession) -> (String, String) {
    // Navigate to chatgpt.com and get session
    if cdp.navigate("https://chatgpt.com/", 30).await.is_err() {
      return (String::new(), String::new());
    }
    sleep(std::time::Duration::from_secs(2)).await;

    let session_js = r#"fetch('/api/auth/session', { credentials: 'include', headers: { accept: 'application/json' } })"#;
    match cdp.evaluate(session_js, true).await {
      Ok(result) => {
        let value = result.get("value").cloned().unwrap_or_default();
        let access_token = value["accessToken"].as_str().unwrap_or("").to_string();
        let account_id = value
          .get("account")
          .and_then(|a| a["id"].as_str())
          .unwrap_or("")
          .to_string();
        (access_token, account_id)
      }
      Err(_) => (String::new(), String::new()),
    }
  }

  /// Attach proxy or inventory VPN to the worker profile (mutually exclusive).
  async fn apply_network_to_worker(
    &mut self,
    app_handle: &tauri::AppHandle,
    mut found: crate::profile::BrowserProfile,
  ) -> Result<crate::profile::BrowserProfile, String> {
    match self.config.network_mode {
      LoginNetworkMode::Proxy => {
        if let Some(proxy_id) = self.config.proxy_id.as_ref() {
          if found.proxy_id.as_deref() != Some(proxy_id.as_str()) || found.vpn_id.is_some() {
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
      }
      LoginNetworkMode::Vpn => {
        if let Some(vpn_id) = self.config.effective_vpn_id() {
          if found.vpn_id.as_deref() != Some(vpn_id.as_str()) || found.proxy_id.is_some() {
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
        }
      }
      LoginNetworkMode::None | LoginNetworkMode::Nord => {
        // Clear residual network bindings so direct/Nord-CLI runs don't inherit old VPN/proxy.
        if found.proxy_id.is_some() {
          if let Ok(updated) = crate::profile::ProfileManager::instance()
            .update_profile_proxy(app_handle.clone(), &found.id.to_string(), None)
            .await
          {
            found = updated;
          }
        }
        if found.vpn_id.is_some() {
          if let Ok(updated) = crate::profile::ProfileManager::instance()
            .update_profile_vpn(app_handle.clone(), &found.id.to_string(), None)
            .await
          {
            found = updated;
          }
        }
      }
    }
    Ok(found)
  }

  /// Mid-batch WireGuard peer hop (same approach as auto-reg): keep PrivateKey,
  /// pick a new Nord peer, rewrite inventory conf, restart vpn-worker.
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
    // Prefer staying in the current inferred country after a successful login hop.
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

    let _ = crate::vpn_worker_runner::stop_vpn_worker_by_vpn_id(vpn_id).await;
    sleep(std::time::Duration::from_secs(1)).await;

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

  /// Switch Nord inventory conf to an allowlisted country after OpenAI region blocks.
  /// Returns Ok(true) when a location switch was applied.
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
    let Some(vpn_id) = self.config.effective_vpn_id() else {
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

  /// Ensure one reusable worker profile for the whole batch.
  /// Creates only when missing; reuses already-created auto-login workers of the same browser.
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

    let browser_str = if self.config.browser_type.eq_ignore_ascii_case("camoufox") {
      "camoufox"
    } else {
      "chromium"
    };

    // Prefer the stable auto-login worker for this browser (cross-batch reuse).
    // Exact name first, then any auto-login-worker-* of the same browser.
    let stable_name = format!("auto-login-worker-{browser_str}");
    if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
      if let Some(mut found) = profiles.into_iter().find(|p| {
        p.browser.eq_ignore_ascii_case(browser_str)
          && (p.name.eq_ignore_ascii_case(&stable_name) || p.name.starts_with("auto-login-worker-"))
      }) {
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

        // Attach proxy or VPN to the reused worker (mutually exclusive).
        found = self.apply_network_to_worker(app_handle, found).await?;

        self.log(&format!(
          "Reusing existing auto-login worker: {} ({}) browser={} version={}",
          found.name, found.id, found.browser, found.version
        ));
        // Existing worker was created earlier — keep it after this batch too.
        self.worker_profile_id = Some(found.id.to_string());
        self.owns_worker_profile = false;
        return Ok(found);
      }
    }

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

    // One stable worker name per browser type (reused across batches).
    let profile_name = stable_name;

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
    let chromium_config = if browser_str == "chromium" {
      Some(crate::chromium_manager::ChromiumConfig {
        fingerprint: None,
        randomize_fingerprint_on_launch: Some(true),
        ..Default::default()
      })
    } else {
      None
    };

    let (create_proxy_id, create_vpn_id) = match self.config.network_mode {
      LoginNetworkMode::Proxy => (self.config.proxy_id.clone(), None),
      LoginNetworkMode::Vpn => (None, self.config.effective_vpn_id()),
      _ => (None, None),
    };

    let mut created = create_browser_profile_with_group(
      app_handle.clone(),
      profile_name,
      browser.as_str().to_string(),
      version,
      release_type,
      create_proxy_id,
      create_vpn_id,
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
      created.chromium_config = Some(cfg.clone());
      if let Err(e) = crate::profile::ProfileManager::instance()
        .update_chromium_config(app_handle.clone(), &created.id.to_string(), cfg)
        .await
      {
        self.log(&format!(
          "Warning: failed to persist Chromium randomize flag: {e}"
        ));
      }
    }

    // Keep the worker after batch so later auto-login runs reuse it (no storage spam).
    self.worker_profile_id = Some(created.id.to_string());
    self.owns_worker_profile = false;
    self.log(&format!(
      "Created reusable worker profile {} (id={}) browser={} — relaunch renews fingerprint + data",
      created.name, created.id, created.browser
    ));

    Ok(created)
  }

  /// Launch the reused worker profile and attach CDP / Playwright.
  async fn launch_browser(
    &mut self,
    app_handle: &tauri::AppHandle,
  ) -> Result<(crate::profile::BrowserProfile, BrowserSession), String> {
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

    sleep(std::time::Duration::from_secs(2)).await;

    let browser_str = launched.browser.as_str();
    let profile_path = crate::ephemeral_dirs::get_effective_profile_path(
      &launched,
      &crate::profile::ProfileManager::instance().get_profiles_dir(),
    );
    let profile_path_str = profile_path.to_string_lossy().to_string();

    // Camoufox: Playwright page from manager (no CDP port).
    if browser_str.eq_ignore_ascii_case("camoufox") {
      let mut last_err = String::new();
      for attempt in 0..15 {
        if attempt > 0 {
          sleep(std::time::Duration::from_millis(500)).await;
        }
        match crate::camoufox_manager::CamoufoxManager::instance()
          .get_active_page(&profile_path_str)
          .await
        {
          Ok(page) => {
            self.log(&format!(
              "Camoufox Playwright page attached (attempt {})",
              attempt + 1
            ));
            return Ok((
              launched,
              BrowserSession::Camoufox {
                page,
                profile_path: profile_path_str,
              },
            ));
          }
          Err(e) => last_err = e.to_string(),
        }
      }
      return Err(format!(
        "Failed to attach Camoufox Playwright page for {profile_path_str}: {last_err}"
      ));
    }

    // Chromium: wait for CDP port then open debugger websocket.
    // Prefer PID lookup: ephemeral path can race with status checks.
    let cdp_port = self
      .wait_for_cdp_port(&launched.browser, &profile_path_str, launched.process_id)
      .await?;
    self.log(&format!("CDP port ready: {cdp_port}"));
    let ws_url = get_page_ws_url(cdp_port).await?;
    let cdp = CdpConnection::connect(&ws_url).await?;
    Ok((launched, BrowserSession::Cdp(cdp)))
  }

  async fn wait_for_cdp_port(
    &self,
    browser: &str,
    profile_path: &str,
    process_id: Option<u32>,
  ) -> Result<u16, String> {
    let mgr = crate::chromium_manager::ChromiumManager::instance();
    for attempt in 0..20 {
      if attempt > 0 {
        sleep(std::time::Duration::from_millis(500)).await;
      }
      if let Some(pid) = process_id {
        if let Some(p) = mgr.get_cdp_port_by_pid(pid).await {
          return Ok(p);
        }
      }
      if let Some(p) = mgr.get_cdp_port(profile_path).await {
        return Ok(p);
      }
      // Last resort for single-worker auto-login: only one Chromium instance.
      if let Some(p) = mgr.get_single_cdp_port().await {
        log::info!(
          "CDP port resolved via single-instance fallback: {p} (path lookup missed {profile_path})"
        );
        return Ok(p);
      }
    }
    Err(format!(
      "Failed to get CDP port for browser={browser} path={profile_path} pid={process_id:?}"
    ))
  }

  /// Kill the browser process only. Keep worker profile metadata for reuse.
  /// Ephemeral data dir is removed by BrowserRunner on kill.
  async fn kill_browser_only(
    &mut self,
    app_handle: &tauri::AppHandle,
    profile: &crate::profile::BrowserProfile,
  ) {
    use crate::browser_runner::BrowserRunner;

    if let Err(e) = BrowserRunner::instance()
      .kill_browser_process(app_handle.clone(), profile)
      .await
    {
      self.log(&format!(
        "Warning: failed to kill browser for profile {}: {e}",
        profile.id
      ));
    } else {
      self.log(&format!(
        "Browser killed for worker profile {} ({})",
        profile.name, profile.id
      ));
    }
  }

  /// Delete the auto-created worker only when this engine owns it.
  /// Cross-batch workers are kept on disk for reuse (`owns_worker_profile = false`).
  async fn dispose_worker_profile(&mut self, app_handle: &tauri::AppHandle) {
    let Some(id) = self.worker_profile_id.take() else {
      return;
    };
    if !self.owns_worker_profile {
      self.log(&format!(
        "Keeping reusable auto-login worker profile on disk: {id}"
      ));
      return;
    }
    self.owns_worker_profile = false;

    if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
      if let Some(found) = profiles.into_iter().find(|p| p.id.to_string() == id) {
        self.kill_browser_only(app_handle, &found).await;
      }
    }

    sleep(std::time::Duration::from_millis(500)).await;

    if let Err(e) = crate::profile::ProfileManager::instance().delete_profile(app_handle, &id) {
      self.log(&format!(
        "Warning: failed to delete worker profile {id}: {e}"
      ));
    } else {
      self.log(&format!("Worker profile deleted: {id}"));
    }
  }
}

/// Get WebSocket URL for CDP page target.
async fn get_page_ws_url(port: u16) -> Result<String, String> {
  let url = format!("http://127.0.0.1:{port}/json");
  let resp = reqwest::get(&url)
    .await
    .map_err(|e| format!("Failed to fetch CDP targets: {e}"))?;
  let text = resp
    .text()
    .await
    .map_err(|e| format!("Failed to read CDP targets: {e}"))?;
  let targets: serde_json::Value =
    serde_json::from_str(&text).map_err(|e| format!("Invalid CDP target JSON: {e}"))?;

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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn submit_probe_never_submits_from_javascript() {
    let js = submit_control_probe_js(r#"button[type="submit"]"#);

    assert!(!js.contains(".click()"));
    assert!(!js.contains("requestSubmit"));
    assert!(!js.contains(".submit()"));
    assert!(js.contains("__reactProps$"));
    assert!(js.contains("aria-disabled"));
  }

  #[test]
  fn oauth_authorize_url_with_localhost_redirect_is_login_email() {
    let url = concat!(
      "https://auth.openai.com/oauth/authorize?client_id=codex",
      "&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
      "&response_type=code&state=test-state"
    );

    assert_eq!(detect_login_page_type(url), LoginPageType::LoginEmail);
  }

  #[test]
  fn oauth_authorize_url_with_localhost_redirect_is_not_a_callback() {
    let url = concat!(
      "https://auth.openai.com/oauth/authorize?response_type=code",
      "&client_id=app_EMoamEEZ73f0CkXaXp7hrann",
      "&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
      "&scope=openid%20profile%20email%20offline_access",
      "&state=test-state&code_challenge=test&code_challenge_method=S256"
    );

    assert!(!is_oauth_callback_url(url));
    assert_eq!(detect_login_page_type(url), LoginPageType::LoginEmail);
  }

  #[test]
  fn html_json_region_error_is_preserved_for_vpn_fallback() {
    let json = r#"{"error":{"code":"unsupported_country_region_territory","message":"Country, region, or territory not supported","param":null,"type":"request_forbidden"}}"#;
    let body = format!("Pretty-print\n{json}");

    let error = extract_unsupported_region_error(&body).expect("region error should be detected");
    assert_eq!(error, json);
  }

  #[test]
  fn detects_cloudflare_turnstile_challenge_signals() {
    assert!(is_cloudflare_challenge_signal(
      "auth.openai.com\nPerforming security verification\nVerify you are human\nCLOUDFLARE"
    ));
    assert!(is_cloudflare_challenge_signal("Just a moment..."));
    assert!(is_cloudflare_challenge_signal(
      r#"iframe src="https://challenges.cloudflare.com/cdn-cgi/challenge-platform""#
    ));
    assert!(!is_cloudflare_challenge_signal(
      "Privacy Policy powered by Cloudflare CDN footer only"
    ));
    assert!(is_cloudflare_challenge_error(
      &cloudflare_challenge_error_message()
    ));
  }

  #[test]
  fn login_page_detection_uses_url_host_and_path() {
    let cases = [
      (
        "http://localhost:1455/auth/callback?code=test&state=test",
        LoginPageType::Callback,
      ),
      (
        "https://auth.openai.com/log-in/password?state=test",
        LoginPageType::LoginPassword,
      ),
      (
        "https://auth.openai.com/mfa/totp?state=test",
        LoginPageType::TwoFactor,
      ),
      (
        "https://auth.openai.com/oauth/consent?state=test",
        LoginPageType::Consent,
      ),
      (
        "https://chatgpt.com/?model=auto",
        LoginPageType::ChatgptHome,
      ),
    ];

    for (url, expected) in cases {
      assert_eq!(detect_login_page_type(url), expected, "url={url}");
    }
  }

  #[test]
  fn oauth_authorize_dom_callback_hint_does_not_override_login_email() {
    assert_eq!(
      resolve_dom_page_override(LoginPageType::LoginEmail, LoginPageType::Callback),
      LoginPageType::LoginEmail
    );
    assert_eq!(
      resolve_dom_page_override(LoginPageType::Unknown, LoginPageType::Callback),
      LoginPageType::Callback
    );
  }

  fn free_callback_test_addr() -> String {
    let listener = std::net::TcpListener::bind((OAUTH_CALLBACK_HOST, 0))
      .expect("test callback port should bind");
    let addr = listener
      .local_addr()
      .expect("test callback address should resolve");
    drop(listener);
    addr.to_string()
  }

  #[tokio::test(flavor = "current_thread")]
  #[serial_test::serial]
  async fn callback_shutdown_releases_port_before_retry() {
    let addr = free_callback_test_addr();
    let mut listener = OAuthCallbackListener::start_on_addr(&addr)
      .await
      .expect("first callback listener should bind");

    let error = listener
      .wait_for_code(std::time::Duration::from_millis(1))
      .await
      .expect_err("callback wait should time out");
    assert_eq!(
      error,
      "Timeout waiting for OAuth callback on localhost:1455"
    );
    listener.shutdown().await;

    let rebound = TcpListener::bind(&addr)
      .await
      .expect("callback listener must release the port before retry");
    drop(rebound);
  }

  #[tokio::test(flavor = "current_thread")]
  #[serial_test::serial]
  async fn callback_shutdown_aborts_stalled_connection_before_retry() {
    let addr = free_callback_test_addr();
    let mut listener = OAuthCallbackListener::start_on_addr(&addr)
      .await
      .expect("callback listener should bind");
    let stalled_client = TcpStream::connect(&addr)
      .await
      .expect("stalled client should connect");
    sleep(std::time::Duration::from_millis(10)).await;

    tokio::time::timeout(std::time::Duration::from_secs(3), listener.shutdown())
      .await
      .expect("callback shutdown should not wait for the stalled request");

    let rebound = TcpListener::bind(&addr)
      .await
      .expect("callback listener must release the port before retry");
    drop(rebound);
    drop(stalled_client);
  }
}
