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
  LoginConfig, LoginCredential, LoginProgress, LoginResult, LoginResultStatus, LoginStep,
};
use super::{oauth, pkce};
use crate::sms::{NumberRequest, SmsService};

type CdpWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

const OAUTH_CALLBACK_HOST: &str = "127.0.0.1";
const OAUTH_CALLBACK_PORT: u16 = 1455;
const OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// Short-lived local HTTP listener for OpenAI OAuth redirect.
///
/// OpenAI redirects to `http://localhost:1455/auth/callback?code=...&state=...`.
/// Without something bound on :1455, Chromium lands on chrome-error and the code is lost.
struct OAuthCallbackListener {
  rx: oneshot::Receiver<Result<(String, String), String>>,
  shutdown: Option<oneshot::Sender<()>>,
}

impl OAuthCallbackListener {
  async fn start() -> Result<Self, String> {
    let addr = format!("{OAUTH_CALLBACK_HOST}:{OAUTH_CALLBACK_PORT}");
    let listener = TcpListener::bind(&addr)
      .await
      .map_err(|e| format!("Failed to bind OAuth callback listener on {addr}: {e}"))?;
    let (tx, rx) = oneshot::channel::<Result<(String, String), String>>();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
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
      if let Some(sender) = tx.take() {
        let _ = sender.send(Err(
          "OAuth callback listener shut down before receiving code".into(),
        ));
      }
    });

    Ok(Self {
      rx,
      shutdown: Some(shutdown_tx),
    })
  }

  async fn wait_for_code(
    mut self,
    timeout: std::time::Duration,
  ) -> Result<(String, String), String> {
    // Drop of Self shuts down the accept loop after we return.
    tokio::time::timeout(timeout, &mut self.rx)
      .await
      .map_err(|_| "Timeout waiting for OAuth callback on localhost:1455".to_string())?
      .map_err(|_| "OAuth callback listener closed unexpectedly".to_string())?
  }
}

impl Drop for OAuthCallbackListener {
  fn drop(&mut self) {
    if let Some(tx) = self.shutdown.take() {
      let _ = tx.send(());
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
        match page.goto_builder(url).goto().await {
          Ok(_) => Ok(()),
          Err(e) => {
            let current = page.url().unwrap_or_default();
            if current.starts_with(url)
              || (url.contains("auth.openai.com") && current.contains("auth.openai.com"))
              || (url.contains("chatgpt.com") && current.contains("chatgpt.com"))
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
#[derive(Debug, PartialEq)]
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

fn detect_login_page_type(url: &str) -> LoginPageType {
  let u = url.to_lowercase();
  // Callback first: after consent, Chrome may briefly show localhost or chrome-error with code=.
  if u.contains("localhost")
    || u.contains("127.0.0.1")
    || (u.contains("callback") && u.contains("code="))
    || (u.contains("code=") && u.contains("state="))
    || u.contains("chrome-error://")
  {
    return LoginPageType::Callback;
  }

  // OpenAI uses both "login" and hyphenated "log-in" paths.
  let is_password = u.contains("password")
    || u.contains("log-in/password")
    || u.contains("login/password")
    || u.contains("create-account/password");
  // "authorize" alone is the OAuth start URL — only treat as email entry when it looks like login.
  let is_email_entry = u.contains("identifier")
    || u.contains("email-otp")
    || u.contains("/log-in")
    || u.contains("/login")
    || u.contains("log-in-or-create")
    || (u.contains("oauth/authorize") && !u.contains("consent"));
  if is_password {
    LoginPageType::LoginPassword
  } else if u.contains("mfa")
    || u.contains("totp")
    || u.contains("2fa")
    || u.contains("multi-factor")
    || (u.contains("challenge") && !u.contains("authorize") && !u.contains("consent"))
  {
    LoginPageType::TwoFactor
  } else if u.contains("phone-verification")
    || u.contains("verify-phone")
    || u.contains("phone/verify")
    || u.contains("add-phone/verify")
  {
    // After number submit OpenAI moves to /phone-verification (OTP only).
    // Must NOT re-enter AddPhone or we rent a second SMS number.
    LoginPageType::PhoneOtp
  } else if u.contains("add-phone") || u.contains("phone/add") {
    LoginPageType::AddPhone
  } else if u.contains("consent")
    || u.contains("sign-in-with-chatgpt")
    || u.contains("sign-in-with-openai")
    || u.contains("oauth/consent")
    || u.contains("workspace/select")
    || u.contains("organization/select")
  {
    // OAuth consent / org-select / "Continue" gate after successful auth.
    // Accounts that already verified phone often land here directly after 2FA — skip SMS.
    LoginPageType::Consent
  } else if u.contains("chatgpt.com")
    && (u.ends_with("chatgpt.com/") || u.ends_with("chatgpt.com") || u.contains("chatgpt.com/?"))
  {
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

      for attempt in 0..max_retries {
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
            result.step_logs = self.account_logs(log_start);
            save_login_result(&result);
            self.emit(
              &app_handle,
              if result.success {
                LoginStep::Completed
              } else {
                LoginStep::Failed
              },
              &format!(
                "[{}/{}] {}",
                idx + 1,
                total,
                if result.success {
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
            break;
          }
          Err(e) => {
            last_error = e.clone();
            self.log(&format!("Attempt {attempt} failed: {e}"));
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
    let callback_listener = OAuthCallbackListener::start().await?;
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
        drop(callback_listener);
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
        callback_listener,
      )
      .await;

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
    callback_listener: OAuthCallbackListener,
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

    let mut phone_number_used = String::new();
    // Pending SMS rent for the AddPhone → PhoneOtp two-step flow.
    let mut pending_sms_request_id: Option<String> = None;
    let mut phone_otp_submitted = false;

    // Step 4-8: Login flow state machine.
    // Returning accounts (phone already verified) skip AddPhone/PhoneOtp and land on Consent.
    for step_i in 0..20 {
      if self.is_cancelled() {
        return Err("Cancelled".into());
      }

      let mut page = detect_login_page_type(&cur_url);
      // URL can lag SPA transitions (or be chrome-error without path). Probe DOM.
      if matches!(
        page,
        LoginPageType::Unknown | LoginPageType::LoginEmail | LoginPageType::AddPhone
      ) {
        if let Ok(dom_page) = self.probe_page_type_from_dom(cdp).await {
          if dom_page != LoginPageType::Unknown {
            // Prefer PhoneOtp over AddPhone when DOM shows OTP code field only.
            if !(page == LoginPageType::PhoneOtp && dom_page == LoginPageType::AddPhone) {
              self.log(&format!(
                "{prefix} DOM page override: {page:?} -> {dom_page:?} (url={cur_url})"
              ));
              page = dom_page;
            }
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
            // Already rented for this attempt — wait for navigation to OTP page.
            self.log(&format!(
              "{prefix} Phone number already submitted; waiting for phone-verification page..."
            ));
            sleep(std::time::Duration::from_secs(1)).await;
            cur_url = cdp.current_url().await.unwrap_or_default();
            continue;
          }

          let Some(sms) = sms_service else {
            return Err("Phone verification required but no SMS provider configured".into());
          };
          let service_id = self.config.sms_service_id.ok_or_else(|| {
            "Phone verification required but smsServiceId not configured".to_string()
          })?;

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
          let number_info = sms
            .request_number(&request)
            .map_err(|e| format!("SMS rent number: {e}"))?;

          if self.used_phones.contains(&number_info.phone_number) {
            return Err(format!(
              "Phone number {} already used for another account",
              number_info.phone_number
            ));
          }
          phone_number_used = number_info.phone_number.clone();
          pending_sms_request_id = Some(number_info.request_id.clone());

          self.log(&format!(
            "{prefix} SMS number rented (request_id={})",
            number_info.request_id
          ));

          self
            .fill_phone_and_submit(cdp, &number_info.phone_number)
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
          let request_id = pending_sms_request_id.clone().ok_or_else(|| {
            "Phone OTP page shown but no SMS request is pending (unexpected navigation)".to_string()
          })?;

          self.emit(
            app_handle,
            LoginStep::PollingSmsOtp,
            &format!("{prefix} Waiting for SMS OTP..."),
            idx,
            total,
            None,
          );
          let otp_info = sms
            .get_otp(&request_id, 150)
            .map_err(|e| format!("SMS OTP poll: {e}"))?;
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
          let mut left_consent = false;
          for attempt in 0..3 {
            self.log(&format!("{prefix} Consent click attempt {}", attempt + 1));
            self.click_consent_button(cdp).await?;
            for _ in 0..12 {
              sleep(std::time::Duration::from_millis(500)).await;
              cur_url = cdp.current_url().await.unwrap_or_default();
              let after = detect_login_page_type(&cur_url);
              if matches!(after, LoginPageType::Callback | LoginPageType::ChatgptHome) {
                self.log(&format!("{prefix} Left consent -> {after:?} url={cur_url}"));
                left_consent = true;
                break;
              }
              // Still on consent — keep polling this attempt briefly.
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
    // Never use form.submit() — OpenAI auth SPA expects a real button click /
    // requestSubmit, otherwise the route can 400 with "Invalid content type".
    let js = format!(
      r#"(function(){{
        const selectors = {sels};
        function visible(el) {{
          try {{
            const r = el.getBoundingClientRect();
            const s = getComputedStyle(el);
            return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none';
          }} catch (_) {{ return true; }}
        }}
        const buttons = Array.from(document.querySelectorAll(selectors)).filter(visible);
        for (const btn of buttons) {{
          btn.scrollIntoView({{ block: 'center' }});
          btn.click();
          return {{ ok: true, how: 'selector', text: (btn.innerText||'').slice(0,40) }};
        }}
        // Prefer Continue/Next text buttons.
        const texts = ['continue', 'next', 'log in', 'sign in', 'submit', 'verify'];
        const all = Array.from(document.querySelectorAll('button,[role="button"]')).filter(visible);
        for (const btn of all) {{
          const t = (btn.innerText || btn.textContent || '').toLowerCase().trim();
          if (texts.some((x) => t === x || t.includes(x))) {{
            btn.click();
            return {{ ok: true, how: 'text', text: t.slice(0,40) }};
          }}
        }}
        // requestSubmit only (not HTMLFormElement.submit which bypasses React handlers).
        const forms = Array.from(document.querySelectorAll('form'));
        for (const form of forms) {{
          if (typeof form.requestSubmit === 'function') {{
            try {{ form.requestSubmit(); return {{ ok: true, how: 'requestSubmit' }}; }} catch (_) {{}}
          }}
        }}
        return {{ ok: false }};
      }})()"#,
      sels = serde_json::to_string(selectors).unwrap_or_else(|_| "\"\"".into()),
    );
    let result = cdp.evaluate(&js, false).await?;
    let value = result.get("value").cloned().unwrap_or_default();
    if value.get("ok").and_then(|v| v.as_bool()) != Some(true) {
      return Err(format!("submit {field}: no clickable control"));
    }
    Ok(())
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
      "{prefix} Auth route error detected, clicking Try again…"
    ));
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
  async fn select_vietnam_country_code(
    &mut self,
    cdp: &mut BrowserSession,
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
      if v.get("already").and_then(|b| b.as_bool()) == Some(true) {
        self.log(&format!(
          "Phone country already Vietnam: {}",
          v.get("text").and_then(|t| t.as_str()).unwrap_or("+84")
        ));
        return Ok(true);
      }
      self.log(&format!(
        "Phone country currently: {}",
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
    sleep(std::time::Duration::from_millis(400)).await;

    // Wait for listbox, then type to filter (React Aria listbox often supports typeahead).
    for _ in 0..10 {
      let has = cdp
        .evaluate(
          r#"(function(){ return !!document.querySelector('[role="listbox"]'); })()"#,
          false,
        )
        .await
        .ok()
        .and_then(|r| r.get("value").and_then(|v| v.as_bool()))
        .unwrap_or(false);
      if has {
        break;
      }
      sleep(std::time::Duration::from_millis(200)).await;
    }

    // Typeahead "Vietnam" into focused listbox via real key events, then Enter.
    // React Aria virtualized Select often focuses the matching option on typeahead.
    let _ = cdp
      .evaluate(
        r#"(function(){
          const lb = document.querySelector('[role="listbox"]');
          if (lb) { lb.focus(); return true; }
          return false;
        })()"#,
        false,
      )
      .await;
    for ch in "Vietnam".chars() {
      cdp.key_char(ch).await?;
      sleep(std::time::Duration::from_millis(40)).await;
    }
    sleep(std::time::Duration::from_millis(350)).await;
    // Enter to commit focused/typeahead option if React Aria already highlighted VN.
    let _ = cdp.press_enter().await;
    sleep(std::time::Duration::from_millis(400)).await;

    // If typeahead+Enter already selected Vietnam, skip click path.
    if let Ok(res) = cdp.evaluate(check, false).await {
      let v = res.get("value").cloned().unwrap_or_default();
      if v.get("already").and_then(|b| b.as_bool()) == Some(true) {
        self.log(&format!(
          "Selected phone country via typeahead+Enter: {}",
          v.get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("Vietnam +84")
        ));
        return Ok(true);
      }
    }

    // Pick VN option — list is virtualized, so scroll until data-key=VN mounts.
    // DOM shape (live): listbox > presentation(height~9320) > absolute rows of options.
    let pick_js = r#"(function(){
      function clickOption(opt){
        try {
          opt.scrollIntoView({ block: 'center' });
          opt.focus();
          opt.click();
          // React Aria sometimes needs pointer sequence
          opt.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }));
          opt.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
          opt.dispatchEvent(new MouseEvent('mouseup', { bubbles: true }));
          opt.dispatchEvent(new MouseEvent('click', { bubbles: true }));
          return true;
        } catch(_) { return false; }
      }

      function findVn(){
        return document.querySelector('[role="option"][data-key="VN"]')
          || Array.from(document.querySelectorAll('[role="option"]')).find((el) => {
            const t = (el.innerText || el.textContent || '');
            const key = el.getAttribute('data-key') || '';
            return key === 'VN' || (/vietnam/i.test(t) && /(\+84|\+\(84\)|\(84\))/i.test(t));
          }) || null;
      }

      const listbox = document.querySelector('[role="listbox"]');
      if (!listbox) return { picked: false, reason: 'no_listbox' };

      // Direct hit if already mounted (typeahead often mounts VN near top).
      let opt = findVn();
      if (opt && clickOption(opt)) {
        return { picked: true, how: 'direct', text: (opt.innerText||'').replace(/\s+/g,' ').trim().slice(0,80) };
      }

      // Virtualized list: find the tall presentation spacer (~233 * 40px) and scroll it/parent.
      const presentations = Array.from(listbox.querySelectorAll('[role="presentation"]'));
      const spacer = presentations.find((el) => {
        const h = parseFloat(el.style.height || '0') || el.getBoundingClientRect().height || 0;
        return h > 1000;
      }) || presentations[0] || listbox;
      // Scroll container is often the listbox itself (overflow auto), not the spacer.
      const scrollers = [listbox, listbox.parentElement, spacer].filter(Boolean);
      const itemH = 40;
      const total = 233;
      // VN is near the end alphabetically (~ position ~220). Scan end first, then full range.
      const order = [];
      for (let i = 210; i < total; i++) order.push(i);
      for (let i = 0; i < 210; i += 2) order.push(i);
      for (const i of order) {
        const top = i * itemH;
        for (const s of scrollers) {
          try { s.scrollTop = top; } catch(_) {}
        }
        // Also set spacer transform-style virtualizers that use absolute top offsets.
        opt = findVn();
        if (opt && clickOption(opt)) {
          return { picked: true, how: 'scroll_'+i, text: (opt.innerText||'').replace(/\s+/g,' ').trim().slice(0,80) };
        }
      }
      // Dump currently mounted options for debug.
      const mounted = Array.from(document.querySelectorAll('[role="option"]')).slice(0, 16).map((el) => ({
        key: el.getAttribute('data-key') || '',
        text: (el.innerText||'').replace(/\s+/g,' ').trim().slice(0,60),
        selected: el.getAttribute('aria-selected') || '',
        pos: el.getAttribute('aria-posinset') || ''
      }));
      return { picked: false, reason: 'vn_not_mounted', mounted };
    })()"#;
    let mut picked = false;
    for attempt in 0..6 {
      if attempt > 0 {
        sleep(std::time::Duration::from_millis(300)).await;
      }
      let pick_res = cdp.evaluate(pick_js, false).await?;
      let v = pick_res.get("value").cloned().unwrap_or_default();
      if v.get("picked").and_then(|b| b.as_bool()) == Some(true) {
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
      if attempt == 5 {
        self.log(&format!(
          "VN option not found in listbox: {}",
          v.to_string().chars().take(400).collect::<String>()
        ));
      }
    }
    if !picked {
      return Err(
        "Failed to select Vietnam (+84) in React Aria country listbox (data-key=VN)".into(),
      );
    }

    sleep(std::time::Duration::from_millis(500)).await;

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

  /// Fill phone number and submit.
  async fn fill_phone_and_submit(
    &mut self,
    cdp: &mut BrowserSession,
    phone: &str,
  ) -> Result<(), String> {
    // REQUIRED: select Vietnam (+84) on React Aria Select first.
    // Default UI is "United States (+1)" — typing VN digits without this fails validation.
    self.select_vietnam_country_code(cdp).await?;

    let national = Self::normalize_phone_for_openai(phone, self.config.sms_country.as_deref());
    if national.is_empty() {
      return Err("Phone number empty after normalize".into());
    }
    self.log(&format!(
      "Filling phone national digits len={} (raw_len={})",
      national.len(),
      phone.len()
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

    // Guard: reject if country still shows +1 / United States.
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
        return Err(format!(
          "Refusing to submit phone: country still not VN (+84). Shows: {}",
          v.get("country").and_then(|c| c.as_str()).unwrap_or("?")
        ));
      }
      self.log(&format!(
        "Pre-submit phone guard ok: country={} phone={}",
        v.get("country").and_then(|c| c.as_str()).unwrap_or("?"),
        v.get("phone").and_then(|c| c.as_str()).unwrap_or("?")
      ));
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

  /// Probe DOM when URL classification is ambiguous (SPA / post-OTP consent jump).
  async fn probe_page_type_from_dom(
    &mut self,
    cdp: &mut BrowserSession,
  ) -> Result<LoginPageType, String> {
    let js = r#"(function(){
      const href = (location.href || '').toLowerCase();
      if (href.includes('localhost') || href.includes('127.0.0.1') || (href.includes('code=') && href.includes('state='))) {
        return 'callback';
      }
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

    // 1) Real mouse click via CDP
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
    callback_listener: OAuthCallbackListener,
  ) -> Result<(String, String), String> {
    // Already at a callback URL in the browser (rare; chrome-error loses query).
    if let Some((code, state)) = Sub2ApiClient::parse_callback_url(initial_url) {
      drop(callback_listener);
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

        if let Some(proxy_id) = self.config.proxy_id.as_ref() {
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

    let mut created = create_browser_profile_with_group(
      app_handle.clone(),
      profile_name,
      browser.as_str().to_string(),
      version,
      release_type,
      self.config.proxy_id.clone(),
      None,
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
    let cdp_port = self
      .wait_for_cdp_port(&launched.browser, &profile_path_str)
      .await?;
    self.log(&format!("CDP port ready: {cdp_port}"));
    let ws_url = get_page_ws_url(cdp_port).await?;
    let cdp = CdpConnection::connect(&ws_url).await?;
    Ok((launched, BrowserSession::Cdp(cdp)))
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
