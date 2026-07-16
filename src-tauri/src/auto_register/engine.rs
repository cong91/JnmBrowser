use chrono::Utc;
use rand::prelude::IndexedRandom;
use rand::Rng;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::Emitter;
use tokio::time::sleep;
use uuid::Uuid;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream, MaybeTlsStream};
use tokio::net::TcpStream;

use super::sentinel::build_sentinel_token;
use super::store::save_registration_result;
use super::types::{RegistrationConfig, RegistrationProgress, RegistrationResult, RegistrationStep};
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
      expr.replace(
        "})",
        &format!(", headers: {{ {extra_headers} }} }})"),
      )
    };
    self.evaluate(&with_headers, true).await.and_then(|r| {
      r.get("value")
        .cloned()
        .ok_or_else(|| "evaluate returned no value".into())
    })
  }

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
  "James", "Robert", "John", "Michael", "David", "William", "Richard",
  "Mary", "Jennifer", "Linda", "Elizabeth", "Susan", "Jessica", "Sarah",
  "Emily", "Emma", "Olivia", "Sophia", "Liam", "Noah", "Oliver", "Ethan",
];

const LAST_NAMES: &[&str] = &[
  "Smith", "Johnson", "Williams", "Brown", "Jones", "Garcia", "Miller",
  "Davis", "Wilson", "Anderson", "Thomas", "Taylor", "Moore", "Martin",
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
  if u.contains("create-account/password") {
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
  } else if u.contains("chatgpt.com") && (u.ends_with("chatgpt.com/") || u.ends_with("chatgpt.com")) {
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
        cdk_idx as u32, 0, total_cdks, None,
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
            self.log(&format!("Retry {attempt}/{max_retries} for alias {}/{accounts_per}...", alias_idx + 1));
            sleep(std::time::Duration::from_secs(2)).await;
          }

          match self
            .run_once(&app_handle, email_service, cdk, &base_email, alias_idx, cdk_idx as u32, total_cdks, accounts_per)
            .await
          {
            Ok(result) => {
              save_registration_result(&result);
              all_results.push(result);
              succeeded = true;
              break;
            }
            Err(e) => {
              last_error = e;
              self.log(&format!("Attempt {attempt} failed: {last_error}"));
            }
          }
        }

        if !succeeded {
          self.log(&format!("Alias {}/{} failed after {max_retries} retries", alias_idx + 1, accounts_per));
        }
      }
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
      0, 0, total_cdks,
      None,
    );

    RegistrationResult {
      success: ok > 0,
      email: String::new(),
      password: String::new(),
      account_id: format!("batch:{ok}"),
      access_token: String::new(),
      device_id: String::new(),
      error_message: if fail > 0 { format!("{fail} failures") } else { String::new() },
      step_logs: self.logs.clone(),
      created_at: Utc::now(),
      two_fa_enabled: false,
      cdk: format!("{total_cdks} CDKs processed"),
      base_email: String::new(),
    }
  }

  // -----------------------------------------------------------------------
  // Single account registration with full state machine
  // -----------------------------------------------------------------------

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
    let prefix = format!("[CDK {}/{} Alias {}/{}]", cdk_idx + 1, total_cdks, alias_idx + 1, total_aliases);

    // Step 1: Generate alias from base email
    self.emit(
      app_handle, RegistrationStep::GeneratingAlias,
      &format!("{prefix} Generating alias..."),
      cdk_idx, alias_idx, total_cdks, None,
    );
    let alias_email = email_service
      .generate_alias(base_email)
      .map_err(|e| format!("Alias: {e}"))?;
    self.log(&format!("{prefix} Alias: {alias_email}"));

    // Step 2: Generate user info
    self.emit(
      app_handle, RegistrationStep::GeneratingUserInfo,
      &format!("{prefix} Generating user info..."),
      cdk_idx, alias_idx, total_cdks, None,
    );
    let password = random_password();
    let (first_name, last_name) = random_name();
    let birthdate = random_birthday();
    self.log(&format!("{prefix} Name: {first_name} {last_name}"));

    // Step 3: Launch a fresh browser profile for THIS account only.
    // Lifecycle: create ephemeral profile → register → kill browser → delete profile.
    self.emit(
      app_handle, RegistrationStep::LaunchingBrowser,
      &format!("{prefix} Launching fresh browser profile..."),
      cdk_idx, alias_idx, total_cdks, None,
    );

    // Fresh device id per account so cookies/fingerprint are isolated.
    self.device_id = Uuid::new_v4().to_string();

    let (profile, mut cdp) = self.launch_and_connect(app_handle).await?;
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
        &mut cdp,
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
  async fn run_registration_in_browser(
    &mut self,
    app_handle: &tauri::AppHandle,
    email_service: &dyn EmailService,
    cdp: &mut CdpConnection,
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
    // Seed oai-did cookie
    self.log(&format!("{prefix} Device ID: {}", self.device_id));
    for domain in &["chatgpt.com", ".chatgpt.com", "auth.openai.com", ".auth.openai.com"] {
      let _ = cdp.set_cookie("oai-did", &self.device_id, domain).await;
    }

    // Step 4: Visit chatgpt.com
    self.emit(
      app_handle, RegistrationStep::VisitingHomepage,
      &format!("{prefix} Visiting chatgpt.com..."),
      cdk_idx, alias_idx, total_cdks, None,
    );
    cdp.navigate("https://chatgpt.com/", 30).await?;

    // Step 5: CSRF token
    self.emit(
      app_handle, RegistrationStep::GettingCsrfToken,
      &format!("{prefix} Getting CSRF..."),
      cdk_idx, alias_idx, total_cdks, None,
    );
    let csrf_json = cdp
      .fetch_json("fetch('/api/auth/csrf', { headers: { accept: 'application/json', referer: 'https://chatgpt.com/' } })")
      .await?;
    let csrf_token = csrf_json["csrfToken"].as_str().ok_or("No csrfToken")?;

    // Step 6: Submit email
    self.emit(
      app_handle, RegistrationStep::SubmittingEmail,
      &format!("{prefix} Submitting email..."),
      cdk_idx, alias_idx, total_cdks, None,
    );
    let session_log_id = Uuid::new_v4().to_string();
    let signin_js = format!(
      "fetch('/api/auth/signin/openai?prompt=login&ext-oai-did={did}&auth_session_logging_id={sid}&screen_hint=login_or_signup&login_hint={email}', {{ method: 'POST', headers: {{ 'content-type': 'application/x-www-form-urlencoded', referer: 'https://chatgpt.com/' }}, body: new URLSearchParams({{ callbackUrl: '/', csrfToken: '{token}', json: 'true' }}) }})",
      did = self.device_id, sid = session_log_id, email = alias_email, token = csrf_token,
    );
    let signin = cdp.fetch_json(&signin_js).await?;
    let auth_url = signin["url"].as_str().ok_or("No authorize URL")?;

    // Step 7: Follow authorize
    self.emit(
      app_handle, RegistrationStep::FollowingAuthorize,
      &format!("{prefix} Following authorize..."),
      cdk_idx, alias_idx, total_cdks, None,
    );
    let mut cur_url = self
      .authorize_with_retry(cdp, profile, auth_url, app_handle)
      .await?;
    self.log(&format!(
      "{prefix} Authorize → {cur_url} (profile={})",
      profile.id
    ));

    // Step 8-12: State machine loop
    let mut register_submitted = false;
    let mut account_created = false;
    let mut seen_states: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

    for _ in 0..12 {
      if self.is_cancelled() { return Err("Cancelled".into()); }
      let sig = format!("{:?}", detect_page_type(&cur_url));
      *seen_states.entry(sig.clone()).or_insert(0) += 1;
      if seen_states[&sig] > 2 { return Err(format!("State loop: {sig}")); }

      let page = detect_page_type(&cur_url);
      self.log(&format!("{prefix} Page: {page:?}"));

      match page {
        PageType::CreateAccountPassword | PageType::Unknown => {
          if register_submitted { return Err("Back at password page".into()); }
          self.emit(app_handle, RegistrationStep::RegisteringUser, &format!("{prefix} Registering..."), cdk_idx, alias_idx, total_cdks, None);

          let reg_js = format!(
            "fetch('https://auth.openai.com/api/accounts/user/register', {{ method: 'POST', headers: {{ 'content-type': 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{ username: '{email}', password: '{pw}' }}) }})",
            did = self.device_id, email = alias_email, pw = password,
          );
          let reg = cdp.fetch_json(&reg_js).await?;
          let st = reg["_status"].as_u64().unwrap_or(200);
          if st != 200 { return Err(format!("Register HTTP {st}")); }
          register_submitted = true;

          self.emit(app_handle, RegistrationStep::SendingEmailOtp, &format!("{prefix} Requesting OTP..."), cdk_idx, alias_idx, total_cdks, None);
          let otp_send = format!(
            "fetch('https://auth.openai.com/api/accounts/email-otp/send', {{ headers: {{ accept: 'application/json', 'oai-device-id': '{did}' }} }})",
            did = self.device_id,
          );
          let _ = cdp.fetch_json(&otp_send).await;
          cur_url = "https://auth.openai.com/email-verification".to_string();
          continue;
        }

        PageType::EmailOtpVerification => {
          self.emit(app_handle, RegistrationStep::PollingOtp, &format!("{prefix} Waiting for OTP..."), cdk_idx, alias_idx, total_cdks, None);
          let otp = email_service.poll_verification_code(cdk, 90).map_err(|e| format!("OTP: {e}"))?;
          self.log(&format!("{prefix} OTP: {otp}"));

          self.emit(app_handle, RegistrationStep::VerifyingOtp, &format!("{prefix} Verifying OTP..."), cdk_idx, alias_idx, total_cdks, None);
          let verify_js = format!(
            "fetch('https://auth.openai.com/api/accounts/email-otp/validate', {{ method: 'POST', headers: {{ 'content-type': 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{ code: '{otp}' }}) }})",
            did = self.device_id,
          );
          let verify = cdp.fetch_json(&verify_js).await?;
          let vs = verify["_status"].as_u64().unwrap_or(200);
          if vs != 200 {
            let body = verify["_body"].as_str().unwrap_or("");
            if body.contains("wrong") || body.contains("401") { continue; }
            return Err(format!("OTP verify HTTP {vs}: {body}"));
          }
          cur_url = verify["continue_url"].as_str().unwrap_or("https://auth.openai.com/about-you").to_string();
          continue;
        }

        PageType::AboutYou => {
          if account_created { return Err("Back at about-you".into()); }
          self.emit(app_handle, RegistrationStep::CreatingAccount, &format!("{prefix} Creating account..."), cdk_idx, alias_idx, total_cdks, None);

          let sentinel = build_sentinel_token(&self.device_id, "authorize_continue").await;
          let sentinel_header = sentinel.as_ref().map(|s| format!("'openai-sentinel-token': '{s}', ")).unwrap_or_default();

          let create_js = format!(
            "fetch('https://auth.openai.com/api/accounts/create_account', {{ method: 'POST', headers: {{ 'content-type': 'application/json', {sentinel}'oai-device-id': '{did}' }}, body: JSON.stringify({{ name: '{first} {last}', birthdate: '{birth}' }}) }})",
            sentinel = sentinel_header, did = self.device_id, first = first_name, last = last_name, birth = birthdate,
          );
          let create = cdp.fetch_json(&create_js).await?;
          let cs = create["_status"].as_u64().unwrap_or(200);
          if cs != 200 { return Err(format!("Create account HTTP {cs}")); }
          account_created = true;
          self.log(&format!("{prefix} Account created"));

          if let Some(next) = create["continue_url"].as_str() {
            if !next.is_empty() { cdp.navigate(next, 30).await?; }
          }
          cur_url = cdp.current_url().await.unwrap_or_default();
          continue;
        }

        PageType::AddPhone => { self.log(&format!("{prefix} Phone required — marking pending")); break; }
        PageType::ChatgptHome | PageType::Callback | PageType::Consent => { self.log(&format!("{prefix} ✅ Flow complete")); break; }
        PageType::ExternalUrl => { cdp.navigate(&cur_url, 20).await?; cur_url = cdp.current_url().await.unwrap_or_default(); continue; }
        PageType::LoginPassword => { return Err("Email already has account".into()); }
        PageType::ErrorPage => { return Err(format!("Error page: {cur_url}")); }
      }
    }

    // Extract tokens
    self.emit(app_handle, RegistrationStep::ExtractingTokens, &format!("{prefix} Extracting tokens..."), cdk_idx, alias_idx, total_cdks, None);
    cdp.navigate("https://chatgpt.com/", 15).await?;
    sleep(std::time::Duration::from_secs(2)).await;

    let session = cdp.fetch_json("fetch('/api/auth/session', { headers: { accept: 'application/json' } })").await?;
    let access_token = session["accessToken"].as_str().unwrap_or("").to_string();
    let account_id = session.get("account").and_then(|a| a["id"].as_str()).unwrap_or("").to_string();

    self.log(&format!("{prefix} accessToken obtained, account={account_id}"));

    self.emit(app_handle, RegistrationStep::Enabling2Fa, &format!("{prefix} 2FA (deferred)..."), cdk_idx, alias_idx, total_cdks, None);

    let result = RegistrationResult {
      success: true,
      email: alias_email.to_string(),
      password: password.to_string(),
      account_id,
      access_token,
      device_id: self.device_id.clone(),
      error_message: String::new(),
      step_logs: self.logs.clone(),
      created_at: Utc::now(),
      two_fa_enabled: false,
      cdk: cdk.to_string(),
      base_email: base_email.to_string(),
    };

    self.emit(app_handle, RegistrationStep::Completed, &format!("{prefix} Done!"), cdk_idx, alias_idx, total_cdks, Some(result.clone()));
    Ok(result)
  }

  // -----------------------------------------------------------------------
  // Authorize with retry + Cloudflare handling
  // -----------------------------------------------------------------------

  async fn authorize_with_retry(
    &mut self,
    cdp: &mut CdpConnection,
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
        let (new_profile, new_cdp) = self.launch_and_connect(app_handle).await?;
        *cdp = new_cdp;
        *profile = new_profile;

        // Re-seed cookies and re-visit homepage on the new browser.
        for domain in &["chatgpt.com", ".chatgpt.com", "auth.openai.com", ".auth.openai.com"] {
          let _ = cdp.set_cookie("oai-did", &self.device_id, domain).await;
        }
        cdp.navigate("https://chatgpt.com/", 20).await?;
        self.log(&format!(
          "Fresh browser profile ready: {} ({})",
          profile.name, profile.id
        ));
      }

      cdp.navigate(auth_url, 30).await?;
      let cur = cdp.current_url().await?;

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

  /// Launch a NEW ephemeral browser profile and connect CDP.
  /// Always creates a fresh profile — never reuses an existing one.
  async fn launch_and_connect(
    &mut self,
    app_handle: &tauri::AppHandle,
  ) -> Result<(crate::profile::BrowserProfile, CdpConnection), String> {
    let profile = self.launch_browser(app_handle).await?;
    self.log(&format!(
      "Browser launched: profile={} id={}",
      profile.name, profile.id
    ));
    sleep(std::time::Duration::from_secs(2)).await;

    let profile_path = crate::ephemeral_dirs::get_effective_profile_path(
      &profile,
      &crate::profile::ProfileManager::instance().get_profiles_dir(),
    );
    let cdp_port = crate::chromium_manager::ChromiumManager::instance()
      .get_cdp_port(&profile_path.to_string_lossy())
      .await
      .ok_or_else(|| "Failed to get CDP port".to_string())?;

    self.log(&format!("CDP port ready: {cdp_port}"));
    let ws_url = get_page_ws_url(cdp_port).await?;
    let cdp = CdpConnection::connect(&ws_url).await?;
    Ok((profile, cdp))
  }

  /// Create + launch a brand-new ephemeral browser profile for one registration.
  async fn launch_browser(
    &self,
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
    let browser = BrowserType::from_str(browser_str)
      .map_err(|e| format!("Invalid browser type: {e}"))?;

    // Unique profile name per account so concurrent/sequential runs never collide.
    let short_id = Uuid::new_v4().to_string();
    let profile_name = format!(
      "auto-reg-{}-{}",
      &self.task_id[..8.min(self.task_id.len())],
      &short_id[..8]
    );

    let profile = create_browser_profile_with_group(
      app_handle.clone(),
      profile_name,
      browser.as_str().to_string(),
      String::new(),
      "stable".into(),
      self.config.proxy_id.clone(),
      None,
      None,
      None,
      None,
      true, // always ephemeral
      None,
      None,
    )
    .await
    .map_err(|e| format!("Create profile: {e}"))?;

    let launched = BrowserRunner::instance()
      .launch_browser(
        app_handle.clone(),
        &profile,
        Some("about:blank".into()),
        None,
      )
      .await
      .map_err(|e| format!("Launch: {e}"))?;

    Ok(launched)
  }

  /// Kill the browser process and delete the ephemeral profile.
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

    // Give OS a moment to release file locks before deleting profile data.
    sleep(std::time::Duration::from_millis(500)).await;

    // 2) Delete the ephemeral profile so it doesn't clutter the UI.
    if let Err(e) = crate::profile::ProfileManager::instance()
      .delete_profile(app_handle, &profile.id.to_string())
    {
      self.log(&format!(
        "Warning: failed to delete profile {}: {e}",
        profile.id
      ));
    } else {
      self.log(&format!("Profile deleted: {}", profile.id));
    }
  }

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
      cdk: String::new(),
      base_email: String::new(),
    }
  }
}
