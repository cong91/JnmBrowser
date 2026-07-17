use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use rand::prelude::IndexedRandom;
use rand::Rng;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::Emitter;
use tokio::net::TcpStream;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use super::sentinel::build_sentinel_token;
use super::store::save_registration_result;
use super::totp::{generate_totp_now, normalize_secret};
use super::types::{
  should_rotate, NetworkMode, RegistrationConfig, RegistrationProgress, RegistrationResult,
  RegistrationStep,
};
use crate::email::EmailService;

// ---------------------------------------------------------------------------
// CDP connection wrapper
// ---------------------------------------------------------------------------

type CdpWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct CdpConnection {
  ws: CdpWs,
  next_id: u64,
}

impl CdpConnection {
  async fn connect(ws_url: &str) -> Result<Self, String> {
    let (ws, _) = connect_async(ws_url)
      .await
      .map_err(|e| format!("CDP WebSocket connect failed: {e}"))?;
    Ok(Self { ws, next_id: 1 })
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
    let _ = self.send_cmd("Page.enable", serde_json::json!({})).await;
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
    Ok(())
  }

  async fn mouse_click(&mut self, x: f64, y: f64) -> Result<(), String> {
    // Instant click at point (prefer humanized path via BrowserSession::human_click).
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
}

// ---------------------------------------------------------------------------
// Dual-kernel browser session (Chromium CDP + Camoufox Playwright)
// ---------------------------------------------------------------------------

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
        // Prefer a softer wait than full load — ChatGPT home can keep network
        // busy and exceed Playwright's default 30s load timeout.
        let _ = timeout_secs;
        match page.goto_builder(url).goto().await {
          Ok(_) => Ok(()),
          Err(e) => {
            // If we already landed on a related origin, treat timeout as soft success.
            let current = page.url().unwrap_or_default();
            if current.starts_with(url)
              || (url.contains("chatgpt.com") && current.contains("chatgpt.com"))
              || (url.contains("auth.openai.com") && current.contains("auth.openai.com"))
            {
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
        // Prefer document.cookie (works cross-domain cookie jar via page origin).
        let js = format!(
          "document.cookie = '{name}={value}; domain={domain}; path=/; SameSite=None; Secure'"
        );
        let _: serde_json::Value = page
          .eval(&js)
          .await
          .map_err(|e| format!("Camoufox set_cookie failed: {e}"))?;
        Ok(())
      }
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
      Self::Camoufox { page, .. } => {
        page
          .mouse
          .r#move(x, y, Some(1))
          .await
          .map_err(|e| format!("Camoufox mouse move failed: {e}"))?;
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

  /// Humanized move along a curved path then left-click (service-agnostic).
  async fn human_click(
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
  "Mary",
  "Jennifer",
  "Linda",
  "Elizabeth",
  "Susan",
  "Jessica",
  "Sarah",
  "Emily",
  "Emma",
  "Olivia",
  "Sophia",
  "Liam",
  "Noah",
  "Oliver",
  "Ethan",
];

const LAST_NAMES: &[&str] = &[
  "Smith", "Johnson", "Williams", "Brown", "Jones", "Garcia", "Miller", "Davis", "Wilson",
  "Anderson", "Thomas", "Taylor", "Moore", "Martin",
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

fn compact_json(v: &serde_json::Value) -> String {
  let s = v.to_string();
  if s.len() <= 400 {
    s
  } else {
    format!("{}…", &s[..400])
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
  account_id: &str,
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
  let mut reasons: Vec<String> = vec![format!("account_id={account_id}"), format!("plan={plan}")];

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

pub struct RegistrationEngine {
  config: RegistrationConfig,
  cancel_flag: Arc<AtomicBool>,
  task_id: String,
  device_id: String,
  logs: Vec<String>,
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
    }
  }

  pub fn with_cancel_flag(config: RegistrationConfig, cancel_flag: Arc<AtomicBool>) -> Self {
    Self {
      config,
      cancel_flag,
      task_id: Uuid::new_v4().to_string(),
      device_id: Uuid::new_v4().to_string(),
      logs: Vec::new(),
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
  // Main entry point — iterates CDK list
  // -----------------------------------------------------------------------

  pub async fn run(
    &mut self,
    app_handle: tauri::AppHandle,
    email_service: &dyn EmailService,
  ) -> RegistrationResult {
    let total_cdks = self.config.cdks.len() as u32;
    let accounts_per = self.config.accounts_per_cdk.max(1);
    let max_retries = self.config.max_retries.max(1);
    let cdks = self.config.cdks.clone(); // Clone to avoid borrow conflicts
    let mut all_results: Vec<RegistrationResult> = Vec::new();
    let mut last_error = String::new();
    let mut success_count: u32 = 0;
    let mut nord_connected_by_us = false;

    let cli = self.config.nord_cli_path.clone();
    let group = self.config.nord_group.clone();
    let server = self.config.nord_server_name.clone();
    let rotate_every_n = self.config.rotate_every_n;
    let network_mode = self.config.network_mode;

    // Nord mode: connect once at start so all accounts share a fresh egress.
    if network_mode == NetworkMode::Nord {
      self.emit(
        &app_handle,
        RegistrationStep::RotatingIp,
        "Connecting NordVPN...",
        0,
        0,
        total_cdks,
        None,
      );
      match super::nord_cli::connect(cli.as_deref(), group.as_deref(), server.as_deref()) {
        Ok(()) => {
          nord_connected_by_us = true;
          self.log("NordVPN connected");
          sleep(std::time::Duration::from_secs(3)).await;
          if let Ok(ip) = crate::ip_utils::fetch_public_ip(None).await {
            self.log(&format!("Egress IP after Nord connect: {ip}"));
          }
        }
        Err(e) => {
          let msg = format!("NordVPN connect failed: {e}");
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

    for (cdk_idx, cdk) in cdks.iter().enumerate() {
      if self.is_cancelled() {
        self.log("Cancelled by user");
        break;
      }

      self.log(&format!("=== CDK {}/{total_cdks}: {cdk} ===", cdk_idx + 1));

      // Redeem CDK once to get base email
      self.emit(
        &app_handle,
        RegistrationStep::RedeemingCdk,
        &format!("CDK {}/{}: redeeming...", cdk_idx + 1, total_cdks),
        cdk_idx as u32,
        0,
        total_cdks,
        None,
      );

      let base_email = match email_service.redeem_cdk(cdk) {
        Ok(info) => {
          let email = info.email.clone();
          self.log(&format!("CDK → {email}"));
          email
        }
        Err(e) => {
          self.log(&format!("CDK redeem failed: {e}"));
          last_error = format!("CDK {cdk}: {e}");
          continue; // Skip this CDK, try next
        }
      };

      // Create N accounts per CDK via aliases
      for alias_idx in 0..accounts_per {
        if self.is_cancelled() {
          break;
        }

        let mut succeeded = false;

        for attempt in 0..max_retries {
          if attempt > 0 {
            self.log(&format!(
              "Retry {attempt}/{max_retries} for alias {}/{accounts_per}...",
              alias_idx + 1
            ));
            sleep(std::time::Duration::from_secs(2)).await;
          }

          match self
            .run_once(
              &app_handle,
              email_service,
              cdk,
              &base_email,
              alias_idx,
              cdk_idx as u32,
              total_cdks,
              accounts_per,
            )
            .await
          {
            Ok(result) => {
              save_registration_result(&result);
              all_results.push(result);
              succeeded = true;
              success_count += 1;

              // Nord: rotate egress after every N free-trial successes (browser already cleaned).
              if network_mode == NetworkMode::Nord && should_rotate(success_count, rotate_every_n) {
                self.emit(
                  &app_handle,
                  RegistrationStep::RotatingIp,
                  &format!("Rotating NordVPN IP after {success_count} successes..."),
                  cdk_idx as u32,
                  alias_idx,
                  total_cdks,
                  None,
                );
                let old_ip = crate::ip_utils::fetch_public_ip(None)
                  .await
                  .unwrap_or_default();
                match super::nord_cli::rotate(cli.as_deref(), group.as_deref(), server.as_deref()) {
                  Ok(()) => {
                    nord_connected_by_us = true;
                    let mut new_ip = old_ip.clone();
                    for _ in 0..20 {
                      if self.is_cancelled() {
                        self.log("Cancelled during Nord IP verify");
                        break;
                      }
                      sleep(std::time::Duration::from_secs(3)).await;
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
                      &app_handle,
                      RegistrationStep::RotatingIp,
                      &format!("IP {old_ip} → {new_ip}"),
                      cdk_idx as u32,
                      alias_idx,
                      total_cdks,
                      None,
                    );
                  }
                  Err(e) => {
                    // Hard-stop batch if rotate fails. Do NOT disconnect Nord —
                    // leave VPN up for the user after auto-reg ends (success or fail).
                    let msg = format!("NordVPN rotate failed: {e}");
                    self.log(&msg);
                    self.emit(
                      &app_handle,
                      RegistrationStep::Failed,
                      &msg,
                      cdk_idx as u32,
                      alias_idx,
                      total_cdks,
                      None,
                    );
                    let ok = all_results.iter().filter(|r| r.success).count();
                    return RegistrationResult {
                      success: ok > 0,
                      email: String::new(),
                      password: String::new(),
                      account_id: format!("batch:{ok}"),
                      access_token: String::new(),
                      device_id: String::new(),
                      error_message: msg,
                      step_logs: self.logs.clone(),
                      created_at: Utc::now(),
                      two_fa_enabled: false,
                      totp_secret: String::new(),
                      free_trial_eligible: false,
                      plan_type: String::new(),
                      cdk: format!("{total_cdks} CDKs processed"),
                      base_email: String::new(),
                      status: super::types::AccountInventoryStatus::Available,
                      note: String::new(),
                      exported_at: None,
                      sold_at: None,
                    };
                  }
                }
              }
              break;
            }
            Err(e) => {
              last_error = e;
              self.log(&format!("Attempt {attempt} failed: {last_error}"));
            }
          }
        }

        if !succeeded {
          self.log(&format!(
            "Alias {}/{} failed after {max_retries} retries",
            alias_idx + 1,
            accounts_per
          ));
        }
      }
    }

    // Keep NordVPN connected after the batch finishes (user manages disconnect).
    if nord_connected_by_us {
      self.log("NordVPN left connected after auto-reg (no auto-disconnect)");
    }

    // Return summary result
    let ok = all_results.iter().filter(|r| r.success).count();
    let fail = all_results.iter().filter(|r| !r.success).count();
    let msg = if ok > 0 {
      format!("Done: {ok} accounts created, {fail} failed")
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
        format!("{fail} failures")
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
      status: super::types::AccountInventoryStatus::Available,
      note: String::new(),
      exported_at: None,
      sold_at: None,
    }
  }

  // -----------------------------------------------------------------------
  // Single account registration with full state machine
  // -----------------------------------------------------------------------

  #[allow(clippy::too_many_arguments)]
  async fn run_once(
    &mut self,
    app_handle: &tauri::AppHandle,
    email_service: &dyn EmailService,
    cdk: &str,
    base_email: &str,
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

    // Step 1: Generate alias from base email
    self.emit(
      app_handle,
      RegistrationStep::GeneratingAlias,
      &format!("{prefix} Generating alias..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    let alias_email = email_service
      .generate_alias(base_email)
      .map_err(|e| format!("Alias: {e}"))?;
    self.log(&format!("{prefix} Alias: {alias_email}"));

    // Step 2: Generate user info
    self.emit(
      app_handle,
      RegistrationStep::GeneratingUserInfo,
      &format!("{prefix} Generating user info..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    let password = random_password();
    let (first_name, last_name) = random_name();
    let birthdate = random_birthday();
    self.log(&format!("{prefix} Name: {first_name} {last_name}"));

    // Step 3: Launch a fresh browser profile for THIS account only.
    // Lifecycle: create ephemeral profile → register → kill browser → delete profile.
    self.emit(
      app_handle,
      RegistrationStep::LaunchingBrowser,
      &format!("{prefix} Launching fresh browser profile..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );

    // Fresh device id per account so cookies/fingerprint are isolated.
    self.device_id = Uuid::new_v4().to_string();

    let (profile, mut session) = self.launch_and_connect(app_handle).await?;
    self.log(&format!(
      "{prefix} Browser profile launched: {} ({})",
      profile.name, profile.id
    ));

    // Always clean up the active profile when this account finishes (success or failure).
    let mut active_profile = profile;
    let registration_result = self
      .run_registration_in_browser(
        app_handle,
        email_service,
        &mut session,
        &mut active_profile,
        cdk,
        base_email,
        &alias_email,
        &password,
        &first_name,
        &last_name,
        &birthdate,
        prefix.as_str(),
        cdk_idx,
        alias_idx,
        total_cdks,
      )
      .await;

    self.cleanup_browser(app_handle, &active_profile).await;
    self.log(&format!("{prefix} Browser closed and profile cleaned up"));

    registration_result
  }

  /// Core registration steps that run inside an already-launched browser profile.
  #[allow(clippy::too_many_arguments)]
  async fn run_registration_in_browser(
    &mut self,
    app_handle: &tauri::AppHandle,
    email_service: &dyn EmailService,
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
    let csrf_json = session
      .fetch_json("fetch('/api/auth/csrf', { headers: { accept: 'application/json', referer: 'https://chatgpt.com/' } })")
      .await?;
    let csrf_token = csrf_json["csrfToken"].as_str().ok_or("No csrfToken")?;

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
    let session_log_id = Uuid::new_v4().to_string();
    let signin_js = format!(
      "fetch('/api/auth/signin/openai?prompt=login&ext-oai-did={did}&auth_session_logging_id={sid}&screen_hint=login_or_signup&login_hint={email}', {{ method: 'POST', headers: {{ 'content-type': 'application/x-www-form-urlencoded', referer: 'https://chatgpt.com/' }}, body: new URLSearchParams({{ callbackUrl: '/', csrfToken: '{token}', json: 'true' }}) }})",
      did = self.device_id, sid = session_log_id, email = alias_email, token = csrf_token,
    );
    let signin = session.fetch_json(&signin_js).await?;
    let auth_url = signin["url"].as_str().ok_or("No authorize URL")?;

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
      .authorize_with_retry(session, profile, auth_url, app_handle)
      .await?;
    self.log(&format!(
      "{prefix} Authorize → {cur_url} (profile={})",
      profile.id
    ));
    // Give auth.openai.com time to settle cookies after the authorize redirect.
    sleep(std::time::Duration::from_secs(2)).await;
    if let Ok(url) = session.current_url().await {
      cur_url = url;
      self.log(&format!("{prefix} Post-authorize URL: {cur_url}"));
    }

    // Step 8-12: State machine loop
    let mut register_submitted = false;
    let mut account_created = false;
    let mut seen_states: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

    for _ in 0..12 {
      if self.is_cancelled() {
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

          // OpenAI sometimes jumps to email-verification without a password form
          // (especially after choose-an-account). Never mark register_submitted
          // until the password is actually set — otherwise create_account → 400.
          let skipped_password_form = cur_url.contains("email-verification")
            || cur_url.contains("email-otp")
            || cur_url.contains("about-you");

          if skipped_password_form {
            self.log(&format!(
              "{prefix} Password form skipped by UI ({cur_url}); forcing API register"
            ));
            let reg_js = format!(
              "fetch('https://auth.openai.com/api/accounts/user/register', {{ method: 'POST', credentials: 'include', headers: {{ 'content-type': 'application/json', accept: 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{ username: '{email}', password: '{pw}' }}) }})",
              did = self.device_id,
              email = alias_email,
              pw = password,
            );
            let reg = session.fetch_json(&reg_js).await?;
            self.log(&format!("{prefix} Force-register response: {reg}"));
            let st = reg["_status"].as_u64().unwrap_or(200);
            if st != 200 || reg.get("error").is_some() {
              return Err(format!("Force API register failed: {reg}"));
            }
          } else {
            match self.submit_password_via_ui(session, password).await {
              Ok(()) => {
                self.log(&format!("{prefix} Password submitted via UI form"));
              }
              Err(ui_err) => {
                self.log(&format!("{prefix} UI password submit failed: {ui_err}"));
                let reg_js = format!(
                "fetch('https://auth.openai.com/api/accounts/user/register', {{ method: 'POST', credentials: 'include', headers: {{ 'content-type': 'application/json', accept: 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{ username: '{email}', password: '{pw}' }}) }})",
                did = self.device_id, email = alias_email, pw = password,
              );
                let reg = session.fetch_json(&reg_js).await?;
                self.log(&format!("{prefix} Register response: {reg}"));
                let st = reg["_status"].as_u64().unwrap_or(200);
                if st != 200 || reg.get("error").is_some() {
                  return Err(format!(
                    "UI password failed ({ui_err}); API register failed: {reg}"
                  ));
                }
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
          let otp_send = format!(
            "fetch('https://auth.openai.com/api/accounts/email-otp/send', {{ method: 'POST', credentials: 'include', headers: {{ accept: 'application/json', 'content-type': 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{}}) }})",
            did = self.device_id,
          );
          let otp_send_resp = session.fetch_json(&otp_send).await?;
          self.log(&format!("{prefix} OTP send response: {otp_send_resp}"));
          // After password submit, settle on current URL (usually email-verification).
          sleep(std::time::Duration::from_secs(2)).await;
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
          self.emit(
            app_handle,
            RegistrationStep::PollingOtp,
            &format!("{prefix} Waiting for OTP..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );
          let otp = email_service
            .poll_verification_code(cdk, 150)
            .map_err(|e| format!("OTP: {e}"))?;
          self.log(&format!("{prefix} OTP: {otp}"));

          self.emit(
            app_handle,
            RegistrationStep::VerifyingOtp,
            &format!("{prefix} Verifying OTP..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );
          let verify_js = format!(
            "fetch('https://auth.openai.com/api/accounts/email-otp/validate', {{ method: 'POST', credentials: 'include', headers: {{ 'content-type': 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{ code: '{otp}' }}) }})",
            did = self.device_id,
          );
          let verify = session.fetch_json(&verify_js).await?;
          let vs = verify["_status"].as_u64().unwrap_or(200);
          if vs != 200 {
            let body = verify["_body"].as_str().unwrap_or("");
            if body.contains("wrong") || body.contains("401") {
              continue;
            }
            return Err(format!("OTP verify HTTP {vs}: {body}"));
          }
          cur_url = verify["continue_url"]
            .as_str()
            .unwrap_or("https://auth.openai.com/about-you")
            .to_string();
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

          let sentinel = build_sentinel_token(&self.device_id, "authorize_continue").await;
          let sentinel_header = sentinel
            .as_ref()
            .map(|s| format!("'openai-sentinel-token': '{s}', "))
            .unwrap_or_default();

          let create_js = format!(
            "fetch('https://auth.openai.com/api/accounts/create_account', {{ method: 'POST', credentials: 'include', headers: {{ 'content-type': 'application/json', {sentinel}'oai-device-id': '{did}' }}, body: JSON.stringify({{ name: '{first} {last}', birthdate: '{birth}' }}) }})",
            sentinel = sentinel_header, did = self.device_id, first = first_name, last = last_name, birth = birthdate,
          );
          let create = session.fetch_json(&create_js).await?;
          self.log(&format!("{prefix} create_account response: {create}"));
          let cs = create["_status"].as_u64().unwrap_or(200);
          if cs != 200 {
            let body = create["_body"].as_str().unwrap_or("");
            return Err(format!("Create account HTTP {cs}: {create} {body}"));
          }
          account_created = true;
          self.log(&format!("{prefix} Account created"));

          if let Some(next) = create["continue_url"].as_str() {
            if !next.is_empty() {
              session.navigate(next, 30).await?;
            }
          }
          cur_url = session.current_url().await.unwrap_or_default();
          continue;
        }

        PageType::AddPhone => {
          self.log(&format!("{prefix} Phone required — marking pending"));
          break;
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
    if let Err(e) = session.navigate("https://chatgpt.com/", 45).await {
      self.log(&format!("{prefix} token-extract navigate warn: {e}"));
    }
    sleep(std::time::Duration::from_secs(2)).await;

    let mut auth_session = session
      .fetch_json("fetch('https://chatgpt.com/api/auth/session', { credentials: 'include', headers: { accept: 'application/json' } })")
      .await
      .unwrap_or_else(|e| {
        self.log(&format!("{prefix} absolute session fetch failed: {e}"));
        serde_json::json!({})
      });
    if auth_session
      .get("accessToken")
      .and_then(|v| v.as_str())
      .unwrap_or("")
      .is_empty()
    {
      auth_session = session
        .fetch_json("fetch('/api/auth/session', { credentials: 'include', headers: { accept: 'application/json' } })")
        .await
        .unwrap_or_else(|e| {
          self.log(&format!("{prefix} relative session fetch failed: {e}"));
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
      .unwrap_or("")
      .to_string();

    self.log(&format!(
      "{prefix} accessToken obtained={}, account={account_id}",
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
    if !free_trial_eligible {
      return Err(format!(
        "Skipped account without free trial/free Plus offer (plan={plan_type}; {offer_detail})"
      ));
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
        return Err("Cancelled".into());
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
            sleep(std::time::Duration::from_secs(2)).await;
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
      status: super::types::AccountInventoryStatus::Available,
      note: String::new(),
      exported_at: None,
      sold_at: None,
    };

    self.emit(
      app_handle,
      RegistrationStep::Completed,
      &format!(
        "{prefix} Done! free-trial{}{}",
        if plan_type.is_empty() {
          String::new()
        } else {
          format!(" [{plan_type}]")
        },
        if two_fa_enabled {
          " (2FA on)"
        } else {
          " (2FA skipped)"
        }
      ),
      cdk_idx,
      alias_idx,
      total_cdks,
      Some(result.clone()),
    );
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
          sleep(std::time::Duration::from_secs(1)).await;
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
        sleep(std::time::Duration::from_secs(1)).await;
      }
      url = session.current_url().await.unwrap_or_default();
      self.log(&format!("After choose-an-account action URL: {url}"));
    }

    if self
      .page_has_selector(
        session,
        r#"input#email, input[name="email"], input[type="email"], input[autocomplete*="email"]"#,
      )
      .await
    {
      self
        .fill_input(
          session,
          r#"input#email, input[name="email"], input[type="email"], input[autocomplete*="email"]"#,
          email,
        )
        .await?;
      sleep(std::time::Duration::from_millis(400)).await;

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

      for _ in 0..30 {
        sleep(std::time::Duration::from_millis(500)).await;
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
        if url.contains("email-verification") || url.contains("about-you") {
          self.log(&format!("Auth advanced to {url} without password field"));
          return Ok(());
        }
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
      sleep(std::time::Duration::from_millis(400)).await;
    }

    self
      .fill_input(
        session,
        r#"input[name="new-password"], input[type="password"], input[autocomplete="new-password"], input[placeholder="Password"]"#,
        password,
      )
      .await?;
    sleep(std::time::Duration::from_millis(400)).await;

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
      sleep(std::time::Duration::from_millis(500)).await;
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
          detail_parts.push(format!("accounts_check={}", compact_json(&check)));
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
            detail_parts.push(format!("subscriptions={}", compact_json(&subs)));
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
    sleep(std::time::Duration::from_secs(1)).await;
    if let Ok(dom) = session
      .evaluate(
        r#"(function(){
          const t = (document.body && (document.body.innerText || document.body.textContent) || '').toLowerCase();
          return {
            hasFreeTrial: /free trial|try plus free|try it free|start free trial|get plus free|claim offer|free offer|free for \d|0 ?đ|0 ?d|mi[eẽ]n ph[ií]/.test(t),
            hasPlusOffer: /chatgpt plus|upgrade to plus|get plus|try plus/.test(t),
            snippet: t.slice(0, 500)
          };
        })()"#,
        false,
      )
      .await
    {
      let value = dom.get("value").cloned().unwrap_or(dom);
      detail_parts.push(format!("dom={}", compact_json(&value)));
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

  /// Drive Settings → Security → Authenticator app using stable locators from recordings.
  async fn enable_2fa(&mut self, session: &mut BrowserSession) -> Result<String, String> {
    // Ensure we are on ChatGPT home with a live session.
    session.navigate("https://chatgpt.com/", 20).await?;
    sleep(std::time::Duration::from_secs(2)).await;

    // Open profile menu (bottom-left account area → settings menu item).
    // Prefer data-testid from enable2FA / register_2 recordings.
    if self
      .click_selector(
        session,
        r#"[data-testid="profile-button"], button[data-testid="profile-button"], nav button[aria-haspopup="menu"]"#,
        "profile menu button",
      )
      .await
      .is_err()
    {
      // Fallback: click near bottom-left chrome where account chip lives.
      self.click_xy(session, 65.0, 640.0).await?;
    }
    sleep(std::time::Duration::from_millis(800)).await;

    self
      .click_selector(
        session,
        r#"[data-testid="settings-menu-item"]"#,
        "settings menu item",
      )
      .await?;
    sleep(std::time::Duration::from_secs(1)).await;

    // Security tab — avoid brittle radix ids; match text content.
    self
      .click_by_text(session, "Security", "button, [role='tab'], div[role='tab']")
      .await?;
    sleep(std::time::Duration::from_secs(1)).await;

    // Toggle authenticator app on.
    self
      .click_selector(
        session,
        r#"[data-testid="mfa-authenticator-toggle"]"#,
        "mfa authenticator toggle",
      )
      .await?;
    sleep(std::time::Duration::from_secs(2)).await;

    // Recording step: click the underlined "show code / can't scan QR" link so the
    // base32 secret becomes visible. Both enable2FA.json and register_2.json hit
    // `button.interactive-label-accent.underline` here.
    if self
      .click_selector(
        session,
        r#"button.interactive-label-accent.underline, button.underline.interactive-label-accent"#,
        "reveal secret link",
      )
      .await
      .is_err()
    {
      // Text fallbacks if ChatGPT renames the control.
      let mut revealed = false;
      for label in [
        "Can't scan",
        "can't scan",
        "enter the code",
        "Enter the code",
        "show code",
        "Show code",
      ] {
        if self
          .click_by_text(session, label, "button, a, [role='button']")
          .await
          .is_ok()
        {
          revealed = true;
          break;
        }
      }
      if !revealed {
        self.log("2FA reveal-secret link not found; trying to scrape secret anyway");
      }
    }
    sleep(std::time::Duration::from_secs(1)).await;

    // Recording step: click "Copy code" (aria-label / role button|Copy code).
    // We still scrape from DOM afterwards — clipboard is not required for automation.
    if self
      .click_selector(
        session,
        r#"button[aria-label="Copy code"]"#,
        "copy code button",
      )
      .await
      .is_err()
    {
      let _ = self
        .click_by_text(session, "Copy code", "button, [role='button']")
        .await;
    }
    sleep(std::time::Duration::from_millis(500)).await;

    // Capture the base32 secret that was just revealed / copied.
    let secret_raw = self.scrape_totp_secret(session).await?;
    let secret = normalize_secret(&secret_raw)?;
    self.log(&format!("2FA secret captured (len={})", secret.len()));

    // Same as manual flow: secret → local TOTP generator → 6-digit code.
    let code = generate_totp_now(&secret)?;
    self.log(&format!("Generated TOTP code from secret: {code}"));

    // Recording step: paste code into #totp_otp (we set value via React setter).
    self
      .fill_input(
        session,
        r#"#totp_otp, input[name="totp_otp"], input[placeholder*="6-digit"], input[placeholder*="Enter your 6-digit"]"#,
        &code,
      )
      .await?;
    sleep(std::time::Duration::from_millis(500)).await;

    // Confirm button (recording clicks the primary btn under the dialog).
    let mut confirmed = false;
    for label in ["Continue", "Confirm", "Done", "Verify"] {
      if self.click_by_text(session, label, "button").await.is_ok() {
        confirmed = true;
        break;
      }
    }
    if !confirmed {
      self
        .click_selector(
          session,
          r#"button.btn.relative, button[type="submit"]"#,
          "2FA confirm button",
        )
        .await?;
    }
    sleep(std::time::Duration::from_secs(2)).await;

    // Best-effort verification: toggle should now be on, or OTP field gone.
    let verified = session
      .evaluate(
        r#"(function(){
          const t = document.querySelector('[data-testid="mfa-authenticator-toggle"]');
          if (t && (t.getAttribute('aria-checked') === 'true' || t.getAttribute('data-state') === 'checked')) return true;
          const otp = document.querySelector('#totp_otp, input[name="totp_otp"]');
          return !otp;
        })()"#,
        false,
      )
      .await
      .ok()
      .and_then(|v| v["value"].as_bool())
      .unwrap_or(true);

    if !verified {
      return Err("2FA confirm did not clear setup UI".into());
    }

    Ok(secret)
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
    // Approximate previous cursor near viewport origin-ish of element.
    let from = (x.max(8.0) - 24.0, y.max(8.0) - 18.0);
    session
      .human_click(from, (tx, ty), &HumanProfile::careful())
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
    let from = (x.max(8.0) - 24.0, y.max(8.0) - 18.0);
    session
      .human_click(from, (tx, ty), &HumanProfile::careful())
      .await
  }

  async fn click_xy(&mut self, session: &mut BrowserSession, x: f64, y: f64) -> Result<(), String> {
    use crate::browser_actions::HumanProfile;
    let from = ((x - 40.0).max(4.0), (y - 28.0).max(4.0));
    session
      .human_click(from, (x, y), &HumanProfile::careful())
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

  async fn scrape_totp_secret(&mut self, session: &mut BrowserSession) -> Result<String, String> {
    // After "reveal secret" + "Copy code", the base32 secret is visible in the dialog.
    // Prefer nearby text around the Copy code button, then broader DOM / otpauth fallback.
    let js = r#"(function(){
      const clean = (t) => (t || '').replace(/[\s\-]/g, '').toUpperCase();
      const isSecret = (t) => /^[A-Z2-7]{16,64}$/.test(t);
      const candidates = [];

      const push = (raw, score) => {
        const c = clean(raw);
        if (!isSecret(c)) return;
        candidates.push({ secret: c, score: score + c.length });
      };

      // 1) Text near the "Copy code" control (recording target).
      const copyBtn = document.querySelector('button[aria-label="Copy code"]')
        || Array.from(document.querySelectorAll('button')).find(b =>
             /copy code/i.test((b.innerText || b.textContent || b.getAttribute('aria-label') || ''))
           );
      if (copyBtn) {
        let root = copyBtn.parentElement;
        for (let i = 0; i < 5 && root; i++, root = root.parentElement) {
          push(root.innerText || root.textContent || '', 100 - i * 5);
          for (const child of root.querySelectorAll('code, pre, span, div, p')) {
            const t = (child.innerText || child.textContent || '').trim();
            if (t && t.length <= 80) push(t, 120 - i * 5);
          }
        }
      }

      // 2) Explicit secret-ish nodes.
      for (const el of document.querySelectorAll(
        'code, pre, [data-testid*="secret"], [class*="secret"], [class*="mono"], span, div, p'
      )) {
        const t = (el.innerText || el.textContent || '').trim();
        if (!t || t.length < 16 || t.length > 80) continue;
        // Single-token secrets score higher than multi-line blobs.
        const score = (t.split(/\s+/).length === 1) ? 80 : 40;
        push(t, score);
      }

      // 3) otpauth:// URL embedded in page/QR payload.
      const html = document.documentElement.innerHTML;
      const m = html.match(/secret=([A-Z2-7]{16,64})/i);
      if (m && m[1]) push(m[1], 200);

      if (!candidates.length) return { ok: false };
      candidates.sort((a, b) => b.score - a.score);
      return { ok: true, secret: candidates[0].secret };
    })()"#;
    let result = session.evaluate(js, false).await?;
    let value = result
      .get("value")
      .cloned()
      .ok_or_else(|| "scrape secret: no value".to_string())?;
    if value["ok"].as_bool() != Some(true) {
      return Err("could not scrape TOTP secret from dialog after reveal/copy".into());
    }
    value["secret"]
      .as_str()
      .map(|s| s.to_string())
      .ok_or_else(|| "scrape secret: empty".into())
  }

  // -----------------------------------------------------------------------
  // Authorize with retry + Cloudflare handling
  // -----------------------------------------------------------------------

  async fn authorize_with_retry(
    &mut self,
    session: &mut BrowserSession,
    profile: &mut crate::profile::BrowserProfile,
    auth_url: &str,
    app_handle: &tauri::AppHandle,
  ) -> Result<String, String> {
    let max_attempts = 3;

    for attempt in 0..max_attempts {
      if attempt > 0 {
        self.log(&format!(
          "Authorize retry {attempt}/{max_attempts}: closing old browser and opening a fresh profile..."
        ));
        // Kill old browser first so we never stack multiple browser processes.
        self.cleanup_browser(app_handle, profile).await;

        // Fresh device + fresh browser profile for Cloudflare bypass.
        self.device_id = Uuid::new_v4().to_string();
        let (new_profile, new_session) = self.launch_and_connect(app_handle).await?;
        *session = new_session;
        *profile = new_profile;

        // Re-seed cookies and re-visit homepage on the new browser.
        for domain in &[
          "chatgpt.com",
          ".chatgpt.com",
          "auth.openai.com",
          ".auth.openai.com",
        ] {
          let _ = session.set_cookie("oai-did", &self.device_id, domain).await;
        }
        session.navigate("https://chatgpt.com/", 20).await?;
        self.log(&format!(
          "Fresh browser profile ready: {} ({})",
          profile.name, profile.id
        ));
      }

      session.navigate(auth_url, 30).await?;
      let cur = session.current_url().await?;

      if is_cloudflare_block(&cur) {
        self.log(&format!("Cloudflare block detected: {cur}"));
        continue;
      }

      return Ok(cur);
    }

    Err("Authorize failed after max retries — persistent Cloudflare block".into())
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

    let profile_path = crate::ephemeral_dirs::get_effective_profile_path(
      &profile,
      &crate::profile::ProfileManager::instance().get_profiles_dir(),
    );
    let profile_path_str = profile_path.to_string_lossy().to_string();

    if profile.browser == "camoufox" {
      // Camoufox has no CDP port — attach Playwright page from the manager.
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
              profile,
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

    // Chromium path: wait for CDP port then open debugger websocket.
    let cdp_port = self
      .wait_for_cdp_port(&profile.browser, &profile_path_str)
      .await?;
    self.log(&format!("CDP port ready: {cdp_port}"));
    let ws_url = get_page_ws_url(cdp_port).await?;
    let cdp = CdpConnection::connect(&ws_url).await?;
    Ok((profile, BrowserSession::Cdp(cdp)))
  }

  async fn wait_for_cdp_port(&self, browser: &str, profile_path: &str) -> Result<u16, String> {
    for attempt in 0..15 {
      if attempt > 0 {
        sleep(std::time::Duration::from_millis(500)).await;
      }
      let port = crate::chromium_manager::ChromiumManager::instance()
        .get_cdp_port(profile_path)
        .await;
      if let Some(p) = port {
        return Ok(p);
      }
    }
    Err(format!(
      "Failed to get CDP port for browser={browser} path={profile_path}"
    ))
  }

  /// Always launch a **brand-new ephemeral** profile per account.
  ///
  /// `config.profile_id` is only a template: we copy browser/version/proxy defaults,
  /// never reuse its cookie jar / choose-an-account history.
  async fn launch_browser(
    &mut self,
    app_handle: &tauri::AppHandle,
  ) -> Result<crate::profile::BrowserProfile, String> {
    use crate::browser::BrowserType;
    use crate::browser_runner::BrowserRunner;
    use crate::profile::manager::create_browser_profile_with_group;

    let browser_str = if self.config.browser_type == "camoufox" {
      "camoufox"
    } else {
      "chromium"
    };
    let mut version = String::new();
    let mut release_type = "stable".to_string();

    // Optional template profile: version only — never reuse profile data dir / cookies.
    if let Some(profile_id) = self.config.profile_id.as_ref() {
      if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
        if let Some(found) = profiles
          .into_iter()
          .find(|p| p.id.to_string() == *profile_id)
        {
          self.log(&format!(
            "Template profile {} ({}) browser={} version={} — spawning fresh ephemeral clone",
            found.name, found.id, found.browser, found.version
          ));
          if !found.version.is_empty() {
            version = found.version.clone();
          }
          if !found.release_type.is_empty() {
            release_type = found.release_type.clone();
          }
        }
      }
    }

    // Prefer an installed Camoufox version when template omitted version.
    if version.is_empty() && browser_str == "camoufox" {
      if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
        if let Some(v) = profiles
          .into_iter()
          .find(|p| p.browser.eq_ignore_ascii_case("camoufox") && !p.version.is_empty())
          .map(|p| p.version)
        {
          version = v;
          self.log(&format!(
            "Using installed Camoufox version from existing profile: {version}"
          ));
        }
      }
      if version.is_empty() {
        version = "v135.0.1-beta.24".into();
        self.log(&format!("Using default Camoufox version: {version}"));
      }
    }

    let browser =
      BrowserType::from_str(browser_str).map_err(|e| format!("Invalid browser type: {e}"))?;

    // Unique profile name per account so concurrent/sequential runs never collide.
    let short_id = Uuid::new_v4().to_string();
    let profile_name = format!(
      "auto-reg-{}-{}",
      &self.task_id[..8.min(self.task_id.len())],
      &short_id[..8]
    );

    // Camoufox: request a new fingerprint at create time (no cached FP).
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

    let mut created = create_browser_profile_with_group(
      app_handle.clone(),
      profile_name,
      browser.as_str().to_string(),
      version,
      release_type,
      self.config.effective_proxy_id(),
      None,
      camoufox_config,
      None,
      None,
      true, // always ephemeral for auto-reg
      None,
      None,
    )
    .await
    .map_err(|e| format!("Create profile: {e}"))?;

    if created.browser.eq_ignore_ascii_case("camoufox") {
      let mut cfg = created.camoufox_config.clone().unwrap_or_default();
      // Ensure launch path regenerates if create-time FP is reused later.
      cfg.randomize_fingerprint_on_launch = Some(true);
      created.camoufox_config = Some(cfg);
      self.log(&format!(
        "Fresh ephemeral Camoufox profile {} (id={}) — no cookie cache, new fingerprint",
        created.name, created.id
      ));
    } else {
      self.log(&format!(
        "Fresh ephemeral Chromium profile {} (id={}) — no cookie cache",
        created.name, created.id
      ));
    }

    let launched = BrowserRunner::instance()
      .launch_browser(
        app_handle.clone(),
        &created,
        Some("about:blank".into()),
        None,
      )
      .await
      .map_err(|e| format!("Launch: {e}"))?;

    Ok(launched)
  }

  /// Kill the browser process. Delete only ephemeral auto-created profiles.
  async fn cleanup_browser(
    &mut self,
    app_handle: &tauri::AppHandle,
    profile: &crate::profile::BrowserProfile,
  ) {
    use crate::browser_runner::BrowserRunner;

    // 1) Kill browser process first
    if let Err(e) = BrowserRunner::instance()
      .kill_browser_process(app_handle.clone(), profile)
      .await
    {
      self.log(&format!(
        "Warning: failed to kill browser for profile {}: {e}",
        profile.id
      ));
    } else {
      self.log(&format!("Browser killed for profile {}", profile.id));
    }

    // Auto-reg always spawns ephemeral profiles (template profile_id is never the
    // launched id). Delete after each account so no cookie residue remains.
    if !profile.ephemeral {
      self.log(&format!(
        "Keeping non-ephemeral profile on disk: {} ({})",
        profile.name, profile.id
      ));
      return;
    }

    // Give OS a moment to release file locks before deleting profile data.
    sleep(std::time::Duration::from_millis(500)).await;

    if let Err(e) =
      crate::profile::ProfileManager::instance().delete_profile(app_handle, &profile.id.to_string())
    {
      self.log(&format!(
        "Warning: failed to delete profile {}: {e}",
        profile.id
      ));
    } else {
      self.log(&format!("Ephemeral profile deleted: {}", profile.id));
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
      status: super::types::AccountInventoryStatus::Available,
      note: String::new(),
      exported_at: None,
      sold_at: None,
    }
  }
}
