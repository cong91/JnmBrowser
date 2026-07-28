use async_trait::async_trait;
use serde_json::Value;
use std::fmt;
use std::sync::atomic::AtomicBool;
use thiserror::Error;

use crate::auto_service::common::totp::generate_totp_now;
use crate::auto_service::openai::browser::{
  click_first_text, click_password_method, click_trusted_submit, fill_visible_input, ChatGptBrowser,
};
use crate::email::{EmailService, EmailServiceError};

const EMAIL_INPUT_SELECTOR: &str = r#"input[type="email"], input[name="email"], input[name="username"], input[autocomplete="email"], input[autocomplete="username"]"#;
const PASSWORD_INPUT_SELECTOR: &str = r#"input[name="new-password"], input[type="password"], input[name="password"], input[name="current-password"], input[autocomplete="current-password"]"#;
const EMAIL_OTP_INPUT_SELECTOR: &str = r#"input[name="code"], input[name="otp"], input[name="emailCode"], input[autocomplete="one-time-code"], input[inputmode="numeric"]"#;
const TOTP_INPUT_SELECTOR: &str = r#"input[name="code"], input[name="totp"], input[autocomplete="one-time-code"], input[inputmode="numeric"]"#;
const SUBMIT_SELECTOR: &str =
  r#"button[type="submit"], button[data-continue], button[name="intent"]"#;
const EMAIL_VERIFICATION_URL: &str = "https://auth.openai.com/email-verification";

// --- Timing constants (ms) — tune for speed vs reliability ------------------
/// Wait after activating password login before typing credentials.
const PASSWORD_ACTIVATE_MS: u64 = 1200;
/// Pause after typing password to let blocked/deactivated messages appear.
const PASSWORD_SETTLE_MS: u64 = 500;
/// Wait after submitting OTP before re-inspecting state.
const OTP_SUBMIT_MS: u64 = 600;
/// Pause after refresh-email-otp before re-inspect.
const OTP_REFRESH_MS: u64 = 800;
/// Wait after API resend-OTP succeeds.
const RESEND_API_MS: u64 = 1500;
/// Wait after UI resend-OTP succeeds.
const RESEND_UI_MS: u64 = 1500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthState {
  EmailEntry,
  Password,
  EmailOtp,
  AuthenticatorTotp,
  LoggedIn,
  Cloudflare,
  RateLimited,
  Locked,
  WrongCredentials,
  Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthSignals {
  pub url: String,
  pub http_status: Option<u16>,
  pub body_text: String,
  pub has_email_input: bool,
  pub has_password_input: bool,
  pub has_email_otp_input: bool,
  pub has_authenticator_totp_input: bool,
  pub logged_in_marker: bool,
  pub has_password_method_button: bool,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuthError {
  #[error("wrong account credentials")]
  WrongCredentials,
  #[error("account is locked")]
  Locked,
  #[error("authentication is rate limited")]
  RateLimited,
  #[error("Cloudflare challenge blocks authentication")]
  Cloudflare,
  #[error("email OTP polling timed out")]
  EmailOtpTimeout,
  #[error("email OTP attempts were exhausted")]
  EmailOtpAttemptsExhausted,
  #[error("email OTP was rejected")]
  EmailOtpRejected,
  #[error("email OTP polling was cancelled")]
  Cancelled,
  #[error("authentication state is unsupported: {0}")]
  Unsupported(String),
  #[error("authentication state is unknown: {0}")]
  Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailOtpSubmission {
  Accepted,
  WrongCode,
  Rejected,
}

#[derive(Debug, Clone, Copy)]
pub struct AuthPolicy {
  pub max_state_transitions: usize,
  pub max_email_otp_attempts: usize,
  pub initial_email_otp_poll_secs: u64,
  pub retry_email_otp_poll_secs: u64,
  pub max_unknown_observations: usize,
  pub unknown_retry_delay_ms: u64,
}

impl Default for AuthPolicy {
  fn default() -> Self {
    Self {
      max_state_transitions: 24,
      max_email_otp_attempts: 3,
      initial_email_otp_poll_secs: 150,
      retry_email_otp_poll_secs: 90,
      max_unknown_observations: 3,
      unknown_retry_delay_ms: 1_000,
    }
  }
}

#[derive(Clone, Copy)]
pub struct ExistingAccountCredentials<'a> {
  pub email: &'a str,
  pub password: &'a str,
  pub raw_cdk: &'a str,
  pub totp_secret: Option<&'a str>,
}

impl fmt::Debug for ExistingAccountCredentials<'_> {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("ExistingAccountCredentials")
      .field("email", &self.email)
      .field("password", &"[REDACTED]")
      .field("raw_cdk", &"[REDACTED]")
      .field("totp_secret", &self.totp_secret.map(|_| "[REDACTED]"))
      .finish()
  }
}

pub fn classify_auth_state(signals: &AuthSignals) -> AuthState {
  let url = signals.url.to_ascii_lowercase();
  let body = signals.body_text.to_ascii_lowercase();

  if signals.http_status == Some(429)
    || body.contains("too many requests")
    || body.contains("rate limit")
  {
    return AuthState::RateLimited;
  }
  if body.contains("verify you are human")
    || body.contains("performing security verification")
    || body.contains("just a moment")
    || body.contains("cf-turnstile")
    || body.contains("challenges.cloudflare.com")
  {
    return AuthState::Cloudflare;
  }
  if body.contains("account has been locked")
    || body.contains("account temporarily locked")
    || body.contains("account is locked")
    || body.contains("account suspended")
    || body.contains("account has been deactivated")
    || body.contains("account deactivated")
    || body.contains("account disabled")
    || body.contains("account has been disabled")
    || body.contains("authentication error")
  {
    return AuthState::Locked;
  }
  if body.contains("incorrect email or password")
    || body.contains("invalid email or password")
    || body.contains("wrong password")
  {
    return AuthState::WrongCredentials;
  }
  if signals.logged_in_marker
    || url::Url::parse(&signals.url).ok().is_some_and(|parsed| {
      parsed.host_str() == Some("chatgpt.com") && matches!(parsed.path(), "" | "/")
    })
  {
    return AuthState::LoggedIn;
  }
  // OpenAI's email-verification page ("Check your inbox") sometimes shows
  // a "Continue with password" button rather than OTP inputs — the account
  // has not yet entered its password. Classify as Password FIRST so the
  // submit_password adapter clicks the method button and progresses.
  // Must be checked before EmailOtp because the body text "Check your inbox"
  // matches the emailOtp heuristic even when no real OTP input is visible.
  if (url.contains("email-otp") || url.contains("email-verification"))
    && signals.has_password_method_button
  {
    return AuthState::Password;
  }
  if signals.has_email_otp_input {
    return AuthState::EmailOtp;
  }
  // Fallback: URL-based email verification (no OTP input, no button).
  if url.contains("email-otp") || url.contains("email-verification") {
    return AuthState::EmailOtp;
  }
  if url.contains("mfa")
    || url.contains("totp")
    || url.contains("2fa")
    || url.contains("multi-factor")
    || signals.has_authenticator_totp_input
  {
    return AuthState::AuthenticatorTotp;
  }
  if signals.has_password_input {
    return AuthState::Password;
  }
  if signals.has_email_input {
    return AuthState::EmailEntry;
  }
  AuthState::Unknown
}

fn safe_unknown_surface_detail(signals: &AuthSignals) -> String {
  let (host, path) = url::Url::parse(&signals.url)
    .ok()
    .map(|url| {
      (
        url.host_str().unwrap_or("unknown").to_string(),
        url.path().to_string(),
      )
    })
    .unwrap_or_else(|| ("unknown".into(), "unknown".into()));
  format!(
    "host={host} path={path} email={} password={} email_otp={} totp={} logged_in={}",
    signals.has_email_input,
    signals.has_password_input,
    signals.has_email_otp_input,
    signals.has_authenticator_totp_input,
    signals.logged_in_marker,
  )
}

#[async_trait]
pub trait ExistingAccountAuthAdapter {
  async fn observe(&mut self) -> Result<AuthSignals, AuthError>;
  async fn submit_email(&mut self, email: &str) -> Result<(), AuthError>;
  async fn submit_password(&mut self, password: &str) -> Result<(), AuthError>;
  async fn submit_email_otp(&mut self, code: &str) -> Result<EmailOtpSubmission, AuthError>;
  async fn refresh_email_verification(&mut self) -> Result<(), AuthError>;
  async fn resend_email_otp(&mut self) -> Result<(), AuthError>;
  async fn submit_authenticator_totp(&mut self, code: &str) -> Result<(), AuthError>;
}

pub async fn authenticate_existing_account<A: ExistingAccountAuthAdapter + Send + ?Sized>(
  adapter: &mut A,
  credentials: ExistingAccountCredentials<'_>,
  email_service: &dyn EmailService,
  policy: AuthPolicy,
) -> Result<(), AuthError> {
  let cancel_flag = AtomicBool::new(false);
  authenticate_existing_account_with_cancel(
    adapter,
    credentials,
    email_service,
    policy,
    &cancel_flag,
  )
  .await
}

pub async fn authenticate_existing_account_with_cancel<
  A: ExistingAccountAuthAdapter + Send + ?Sized,
>(
  adapter: &mut A,
  credentials: ExistingAccountCredentials<'_>,
  email_service: &dyn EmailService,
  policy: AuthPolicy,
  cancel_flag: &AtomicBool,
) -> Result<(), AuthError> {
  if credentials.email.trim().is_empty() || credentials.password.is_empty() {
    return Err(AuthError::WrongCredentials);
  }
  if credentials.raw_cdk.trim().is_empty() {
    return Err(AuthError::Unsupported(
      "email OTP provider card is unavailable".into(),
    ));
  }

  let mut unknown_observations = 0;
  for _ in 0..policy.max_state_transitions {
    if cancel_flag.load(std::sync::atomic::Ordering::SeqCst) {
      return Err(AuthError::Cancelled);
    }
    let signals = adapter.observe().await?;
    let state = classify_auth_state(&signals);
    if state != AuthState::Unknown {
      unknown_observations = 0;
    }
    match state {
      AuthState::EmailEntry => adapter.submit_email(credentials.email).await?,
      AuthState::Password => adapter.submit_password(credentials.password).await?,
      AuthState::EmailOtp => {
        complete_email_otp(
          adapter,
          email_service,
          credentials.raw_cdk,
          policy,
          cancel_flag,
        )
        .await?;
      }
      AuthState::AuthenticatorTotp => {
        let secret = credentials
          .totp_secret
          .filter(|secret| !secret.trim().is_empty())
          .ok_or_else(|| {
            AuthError::Unsupported("remote authenticator challenge has no local secret".into())
          })?;
        let code = generate_totp_now(secret)
          .map_err(|_| AuthError::Unsupported("local TOTP secret is invalid".into()))?;
        adapter.submit_authenticator_totp(&code).await?;
      }
      AuthState::LoggedIn => return Ok(()),
      AuthState::Cloudflare => return Err(AuthError::Cloudflare),
      AuthState::RateLimited => return Err(AuthError::RateLimited),
      AuthState::Locked => return Err(AuthError::Locked),
      AuthState::WrongCredentials => return Err(AuthError::WrongCredentials),
      AuthState::Unknown => {
        unknown_observations += 1;
        if unknown_observations >= policy.max_unknown_observations.max(1) {
          return Err(AuthError::Unknown(safe_unknown_surface_detail(&signals)));
        }
        tokio::time::sleep(std::time::Duration::from_millis(
          policy.unknown_retry_delay_ms,
        ))
        .await;
      }
    }
  }

  Err(AuthError::Unknown(
    "authentication exceeded the state transition limit".into(),
  ))
}

async fn complete_email_otp<A: ExistingAccountAuthAdapter + Send + ?Sized>(
  adapter: &mut A,
  email_service: &dyn EmailService,
  raw_cdk: &str,
  policy: AuthPolicy,
  cancel_flag: &AtomicBool,
) -> Result<(), AuthError> {
  for attempt in 0..policy.max_email_otp_attempts {
    if cancel_flag.load(std::sync::atomic::Ordering::SeqCst) {
      return Err(AuthError::Cancelled);
    }
    let timeout_secs = if attempt == 0 {
      policy.initial_email_otp_poll_secs
    } else {
      policy.retry_email_otp_poll_secs
    };
    let code = email_service
      .poll_verification_code_with_cancel(raw_cdk, timeout_secs, cancel_flag)
      .map_err(map_email_poll_error)?;
    let submission = adapter.submit_email_otp(&code).await?;
    email_service.mark_verification_code_used(raw_cdk, &code);

    match submission {
      EmailOtpSubmission::Accepted => return Ok(()),
      EmailOtpSubmission::WrongCode if attempt + 1 < policy.max_email_otp_attempts => {
        adapter.refresh_email_verification().await?;
        adapter.resend_email_otp().await?;
      }
      EmailOtpSubmission::WrongCode | EmailOtpSubmission::Rejected => {
        return Err(if attempt + 1 == policy.max_email_otp_attempts {
          AuthError::EmailOtpAttemptsExhausted
        } else {
          AuthError::EmailOtpRejected
        });
      }
    }
  }
  Err(AuthError::EmailOtpAttemptsExhausted)
}

fn map_email_poll_error(error: EmailServiceError) -> AuthError {
  match error {
    EmailServiceError::Cancelled => AuthError::Cancelled,
    EmailServiceError::Timeout(_) => AuthError::EmailOtpTimeout,
    EmailServiceError::CdkInvalid(_) | EmailServiceError::CdkExpired(_) => {
      AuthError::Unsupported("email OTP provider card is unavailable".into())
    }
    EmailServiceError::Network(_)
    | EmailServiceError::EmailInvalid(_)
    | EmailServiceError::Internal(_) => {
      AuthError::Unknown("email OTP provider operation failed".into())
    }
  }
}

pub(crate) struct BrowserAuthAdapter<'a, B> {
  browser: &'a mut B,
  device_id: &'a str,
}

impl<'a, B: ChatGptBrowser + Send> BrowserAuthAdapter<'a, B> {
  pub(crate) fn new(browser: &'a mut B, device_id: &'a str) -> Self {
    Self { browser, device_id }
  }
}

#[async_trait]
impl<B: ChatGptBrowser + Send> ExistingAccountAuthAdapter for BrowserAuthAdapter<'_, B> {
  async fn observe(&mut self) -> Result<AuthSignals, AuthError> {
    let url = self
      .browser
      .current_url()
      .await
      .map_err(|_| AuthError::Unknown("could not read browser URL".into()))?;
    let result = self
      .browser
      .evaluate(
        r#"(function(){
          const body = ((document.body && (document.body.innerText || document.body.textContent)) || '').slice(0, 6000);
          const has = (selector) => !!document.querySelector(selector);
          const emailOtp = has('input[name="otp"], input[name="emailCode"]')
            || (has('input[autocomplete="one-time-code"], input[inputmode="numeric"]')
              && /email|inbox|verification code/i.test(body));
          const authenticator = has('input[name="totp"]')
            || (has('input[autocomplete="one-time-code"], input[inputmode="numeric"]')
              && /authenticator|two-factor|2fa|mfa/i.test(body));
          const pwdBtn = Array.from(document.querySelectorAll('button, a, [role="button"]'))
            .some((el) => {
              const t = (el.innerText || el.textContent || '').toLowerCase();
              return /password/i.test(t) && !/forgot|reset/i.test(t);
            });
          return {
            bodyText: body,
            hasEmailInput: has('input[type="email"], input[name="email"], input[name="username"], input[autocomplete="email"], input[autocomplete="username"]'),
            hasPasswordInput: has('input[type="password"], input[name="password"], input[name="current-password"]'),
            hasEmailOtpInput: emailOtp,
            hasAuthenticatorTotpInput: authenticator,
            loggedInMarker: has('[data-testid="accounts-profile-button"], [data-testid="profile-button"]'),
            hasPasswordMethodButton: pwdBtn
          };
        })()"#,
        false,
      )
      .await
      .map_err(|_| AuthError::Unknown("could not inspect authentication page".into()))?;
    let value = result.get("value").cloned().unwrap_or_default();
    let body_text = value
      .get("bodyText")
      .and_then(Value::as_str)
      .unwrap_or_default()
      .to_string();
    let lower = body_text.to_ascii_lowercase();
    let http_status = if lower.contains("429") || lower.contains("too many requests") {
      Some(429)
    } else {
      None
    };
    Ok(AuthSignals {
      url,
      http_status,
      body_text,
      has_email_input: value.get("hasEmailInput").and_then(Value::as_bool) == Some(true),
      has_password_input: value.get("hasPasswordInput").and_then(Value::as_bool) == Some(true),
      has_email_otp_input: value.get("hasEmailOtpInput").and_then(Value::as_bool) == Some(true),
      has_authenticator_totp_input: value
        .get("hasAuthenticatorTotpInput")
        .and_then(Value::as_bool)
        == Some(true),
      logged_in_marker: value.get("loggedInMarker").and_then(Value::as_bool) == Some(true),
      has_password_method_button: value
        .get("hasPasswordMethodButton")
        .and_then(Value::as_bool)
        == Some(true),
    })
  }

  async fn submit_email(&mut self, email: &str) -> Result<(), AuthError> {
    fill_visible_input(self.browser, EMAIL_INPUT_SELECTOR, email, "email")
      .await
      .map_err(|_| AuthError::Unknown("could not fill email input".into()))?;
    click_trusted_submit(self.browser, SUBMIT_SELECTOR, "authentication")
      .await
      .map_err(|_| AuthError::Unknown("authentication submit control not found".into()))
  }

  async fn submit_password(&mut self, password: &str) -> Result<(), AuthError> {
    // The email-verification page ("Check your inbox") may have hidden
    // password inputs that match PASSWORD_INPUT_SELECTOR but is not the
    // actual login form. Always check the URL first: if on email-verification
    // without a visible password form, click the "Continue with password"
    // button to navigate to the real login page.
    let url = self
      .browser
      .current_url()
      .await
      .unwrap_or_default()
      .to_ascii_lowercase();
    let is_email_verif = url.contains("email-verification") || url.contains("email-otp");
    if is_email_verif {
      click_password_method(self.browser)
        .await
        .map_err(|_| AuthError::Unknown("could not activate password method".into()))?;
      tokio::time::sleep(std::time::Duration::from_millis(PASSWORD_ACTIVATE_MS)).await;
      return Ok(());
    }
    fill_visible_input(self.browser, PASSWORD_INPUT_SELECTOR, password, "password")
      .await
      .map_err(|_| AuthError::Unknown("could not fill password input".into()))?;
    // After typing password, let the page settle then check for blocking
    // before trying to submit — OpenAI may show a "deactivated" message.
    tokio::time::sleep(std::time::Duration::from_millis(PASSWORD_SETTLE_MS)).await;
    let body = self
      .browser
      .evaluate("(document.body?.innerText||'').slice(0,2000)", false)
      .await
      .map(|r| {
        r.get("value")
          .and_then(Value::as_str)
          .unwrap_or("")
          .to_ascii_lowercase()
      })
      .unwrap_or_default();
    if body.contains("account has been deactivated")
      || body.contains("account deactivated")
      || body.contains("account suspended")
      || body.contains("account has been locked")
      || body.contains("account locked")
      || body.contains("authentication error")
    {
      return Err(AuthError::Locked);
    }
    click_trusted_submit(self.browser, SUBMIT_SELECTOR, "authentication")
      .await
      .map_err(|_| AuthError::Unknown("authentication submit control not found".into()))
  }

  async fn submit_email_otp(&mut self, code: &str) -> Result<EmailOtpSubmission, AuthError> {
    if fill_visible_input(self.browser, EMAIL_OTP_INPUT_SELECTOR, code, "email OTP")
      .await
      .is_ok()
    {
      click_trusted_submit(self.browser, SUBMIT_SELECTOR, "authentication")
        .await
        .map_err(|_| AuthError::Unknown("authentication submit control not found".into()))?;
      tokio::time::sleep(std::time::Duration::from_millis(OTP_SUBMIT_MS)).await;
      let signals = self.observe().await?;
      if !matches!(classify_auth_state(&signals), AuthState::EmailOtp) {
        return Ok(EmailOtpSubmission::Accepted);
      }
      let lower = signals.body_text.to_ascii_lowercase();
      if lower.contains("wrong_email_otp_code")
        || lower.contains("incorrect code")
        || lower.contains("invalid code")
      {
        return Ok(EmailOtpSubmission::WrongCode);
      }
    }

    let code_json = serde_json::to_string(code).unwrap_or_else(|_| "\"\"".into());
    let device_json = serde_json::to_string(self.device_id).unwrap_or_else(|_| "\"\"".into());
    let script = format!(
      r#"(async () => {{
        const response = await fetch('https://auth.openai.com/api/accounts/email-otp/validate', {{
          method: 'POST', credentials: 'include',
          headers: {{ 'content-type': 'application/json', accept: 'application/json', 'oai-device-id': {device} }},
          body: JSON.stringify({{ code: {code} }})
        }});
        const responseText = await response.text();
        let errorCode = '';
        let errorMessage = '';
        try {{
          const json = JSON.parse(responseText);
          errorCode = String(json?.error?.code || json?.error || json?.code || '');
          errorMessage = String(json?.error?.message || json?.message || json?.detail || '');
        }} catch (_) {{}}
        return {{ status: response.status, errorCode, errorMessage }};
      }})()"#,
      device = device_json,
      code = code_json,
    );
    let result = self
      .browser
      .evaluate(&script, true)
      .await
      .map_err(|_| AuthError::Unknown("email OTP validation request failed".into()))?;
    let value = result.get("value").cloned().unwrap_or_default();
    let status = value.get("status").and_then(Value::as_u64).unwrap_or(0);
    let error_code = value
      .get("errorCode")
      .and_then(Value::as_str)
      .unwrap_or_default()
      .to_ascii_lowercase();
    let error_message = value
      .get("errorMessage")
      .and_then(Value::as_str)
      .unwrap_or_default();
    classify_otp_api_response(status, &error_code, error_message)
  }

  async fn refresh_email_verification(&mut self) -> Result<(), AuthError> {
    self
      .browser
      .navigate(EMAIL_VERIFICATION_URL, 25)
      .await
      .map_err(|_| AuthError::Unknown("could not refresh email verification".into()))?;
    tokio::time::sleep(std::time::Duration::from_millis(OTP_REFRESH_MS)).await;
    Ok(())
  }

  async fn resend_email_otp(&mut self) -> Result<(), AuthError> {
    if matches!(
      click_first_text(
        self.browser,
        &["resend email", "resend", "send code again", "resend code"],
        "button, a, [role='button']",
      )
      .await,
      Ok(true)
    ) {
      tokio::time::sleep(std::time::Duration::from_millis(RESEND_UI_MS)).await;
      return Ok(());
    }

    let device_json = serde_json::to_string(self.device_id).unwrap_or_else(|_| "\"\"".into());
    let script = format!(
      r#"(async () => {{
        const response = await fetch('https://auth.openai.com/api/accounts/email-otp/send', {{
          method: 'POST', credentials: 'include',
          headers: {{ accept: 'application/json', 'content-type': 'application/json', 'oai-device-id': {device} }},
          body: JSON.stringify({{}})
        }});
        return {{ status: response.status }};
      }})()"#,
      device = device_json,
    );
    let api_status = self
      .browser
      .evaluate(&script, true)
      .await
      .ok()
      .and_then(|result| result.get("value").cloned())
      .and_then(|value| value.get("status").and_then(Value::as_u64));
    if api_status.is_some_and(|status| (200..300).contains(&status)) {
      tokio::time::sleep(std::time::Duration::from_millis(RESEND_API_MS)).await;
      Ok(())
    } else {
      Err(AuthError::EmailOtpRejected)
    }
  }

  async fn submit_authenticator_totp(&mut self, code: &str) -> Result<(), AuthError> {
    fill_visible_input(
      self.browser,
      TOTP_INPUT_SELECTOR,
      code,
      "authenticator code",
    )
    .await
    .map_err(|_| AuthError::Unknown("could not fill authenticator code input".into()))?;
    click_trusted_submit(self.browser, SUBMIT_SELECTOR, "authentication")
      .await
      .map_err(|_| AuthError::Unknown("authentication submit control not found".into()))
  }
}

fn classify_otp_api_response(
  status: u64,
  error_code: &str,
  error_message: &str,
) -> Result<EmailOtpSubmission, AuthError> {
  let safe_details = format!("{error_code} {error_message}").to_ascii_lowercase();
  if status == 429
    || safe_details.contains("rate limit")
    || safe_details.contains("too many requests")
  {
    return Err(AuthError::RateLimited);
  }
  if status == 423
    || safe_details.contains("account_locked")
    || safe_details.contains("account is locked")
  {
    return Err(AuthError::Locked);
  }
  if status == 403
    && (safe_details.contains("cloudflare")
      || safe_details.contains("challenge")
      || safe_details.contains("verify you are human"))
  {
    return Err(AuthError::Cloudflare);
  }
  if status == 200 && error_code.is_empty() {
    Ok(EmailOtpSubmission::Accepted)
  } else if status == 401
    || safe_details.contains("wrong_email_otp_code")
    || safe_details.contains("incorrect code")
    || safe_details.contains("invalid code")
  {
    Ok(EmailOtpSubmission::WrongCode)
  } else {
    Ok(EmailOtpSubmission::Rejected)
  }
}

#[cfg(test)]
mod tests {
  use std::collections::VecDeque;
  use std::sync::Mutex;

  use async_trait::async_trait;

  use super::*;
  use crate::email::{EmailInfo, EmailService, EmailServiceError};

  struct FakeEmailService {
    polls: Mutex<VecDeque<Result<String, EmailServiceError>>>,
    used: Mutex<Vec<String>>,
  }

  impl FakeEmailService {
    fn new(polls: Vec<Result<&str, EmailServiceError>>) -> Self {
      Self {
        polls: Mutex::new(
          polls
            .into_iter()
            .map(|result| result.map(str::to_string))
            .collect(),
        ),
        used: Mutex::new(Vec::new()),
      }
    }
  }

  impl EmailService for FakeEmailService {
    fn redeem_cdk(&self, _cdk: &str) -> Result<EmailInfo, EmailServiceError> {
      panic!("existing-account auth must not redeem CDK quota")
    }

    fn generate_alias(&self, _base_email: &str) -> Result<String, EmailServiceError> {
      panic!("existing-account auth must not generate registration aliases")
    }

    fn poll_verification_code(
      &self,
      _cdk: &str,
      _timeout_secs: u64,
    ) -> Result<String, EmailServiceError> {
      self
        .polls
        .lock()
        .unwrap()
        .pop_front()
        .expect("test should provide an OTP poll result")
    }

    fn mark_verification_code_used(&self, _cdk: &str, code: &str) {
      self.used.lock().unwrap().push(code.to_string());
    }

    fn check_health(&self) -> bool {
      true
    }
  }

  #[derive(Default)]
  struct FakeBrowser {
    ui_resend_available: bool,
    submit_available: bool,
    api_resend_status: Option<u64>,
    api_validate_status: Option<u64>,
    api_validate_error_code: String,
    api_validate_error_message: String,
    type_text_fails: bool,
    type_text_failures_remaining: usize,
    type_text_attempts: usize,
    dom_text_accepts: bool,
    dom_text_failures_remaining: usize,
    typed_value_accepted: bool,
    evaluated: Vec<String>,
  }

  #[async_trait]
  impl ChatGptBrowser for FakeBrowser {
    async fn navigate(&mut self, _url: &str, _timeout_secs: u64) -> Result<(), String> {
      Ok(())
    }

    async fn evaluate(&mut self, expression: &str, _await_promise: bool) -> Result<Value, String> {
      self.evaluated.push(expression.to_string());
      if expression.contains("const labels") {
        return Ok(serde_json::json!({
          "value": {
            "found": self.ui_resend_available,
            "x": 10.0,
            "y": 20.0,
          }
        }));
      }
      if expression.contains("verified: verifyDocument(document, 0)") {
        return Ok(serde_json::json!({
          "value": {
            "found": true,
            "verified": self.typed_value_accepted,
          }
        }));
      }
      if expression.contains("HTMLTextAreaElement") && expression.contains("setNativeValue") {
        if self.dom_text_failures_remaining > 0 {
          self.dom_text_failures_remaining -= 1;
          return Ok(serde_json::json!({
            "value": {
              "ok": false,
              "verified": false,
              "reason": "not_found",
            }
          }));
        }
        return Ok(serde_json::json!({
          "value": {
            "ok": self.dom_text_accepts,
            "verified": self.dom_text_accepts,
            "reason": "value_not_accepted",
          }
        }));
      }
      if expression.contains("function isReady(el)") {
        return Ok(serde_json::json!({
          "value": {
            "ok": self.submit_available,
            "x": 10.0,
            "y": 20.0,
          }
        }));
      }
      if expression.contains("/api/accounts/email-otp/send") {
        return Ok(serde_json::json!({
          "value": { "status": self.api_resend_status.unwrap_or(500) }
        }));
      }
      if expression.contains("/api/accounts/email-otp/validate") {
        return Ok(serde_json::json!({
          "value": {
            "status": self.api_validate_status.unwrap_or(500),
            "errorCode": self.api_validate_error_code,
            "errorMessage": self.api_validate_error_message,
          }
        }));
      }
      Err("unexpected browser evaluation".into())
    }

    async fn current_url(&mut self) -> Result<String, String> {
      Ok("https://auth.openai.com/email-verification".into())
    }

    async fn type_text(&mut self, _selector: &str, _value: &str) -> Result<(), String> {
      self.type_text_attempts += 1;
      if self.type_text_fails {
        return Err("test input failure".into());
      }
      if self.type_text_failures_remaining > 0 {
        self.type_text_failures_remaining -= 1;
        return Err("test stale input".into());
      }
      Ok(())
    }

    async fn click_point(&mut self, _x: f64, _y: f64) -> Result<(), String> {
      Ok(())
    }
  }

  fn evaluate_count(browser: &FakeBrowser, needle: &str) -> usize {
    browser
      .evaluated
      .iter()
      .filter(|expression| expression.contains(needle))
      .count()
  }

  #[tokio::test]
  async fn production_resend_uses_ui_once_without_api_evaluation() {
    let mut browser = FakeBrowser {
      ui_resend_available: true,
      ..FakeBrowser::default()
    };
    let mut adapter = BrowserAuthAdapter::new(&mut browser, "device-id");

    adapter.resend_email_otp().await.unwrap();

    assert_eq!(evaluate_count(&browser, "/api/accounts/email-otp/send"), 0);
  }

  #[tokio::test]
  async fn production_resend_uses_api_only_when_ui_control_is_unavailable() {
    let mut browser = FakeBrowser {
      api_resend_status: Some(204),
      ..FakeBrowser::default()
    };
    let mut adapter = BrowserAuthAdapter::new(&mut browser, "device-id");

    adapter.resend_email_otp().await.unwrap();

    assert_eq!(evaluate_count(&browser, "/api/accounts/email-otp/send"), 1);
  }

  #[tokio::test]
  async fn production_email_entry_retries_after_stale_dom_replacement() {
    let mut browser = FakeBrowser {
      type_text_failures_remaining: 1,
      typed_value_accepted: true,
      submit_available: true,
      ..FakeBrowser::default()
    };
    let mut adapter = BrowserAuthAdapter::new(&mut browser, "device-id");

    adapter.submit_email("person@example.com").await.unwrap();

    assert_eq!(browser.type_text_attempts, 2);
    assert_eq!(evaluate_count(&browser, "HTMLTextAreaElement"), 1);
  }

  #[tokio::test]
  async fn production_email_entry_uses_dom_fallback_after_persistent_type_failure() {
    let mut browser = FakeBrowser {
      type_text_fails: true,
      dom_text_accepts: true,
      submit_available: true,
      ..FakeBrowser::default()
    };
    let mut adapter = BrowserAuthAdapter::new(&mut browser, "device-id");

    adapter.submit_email("person@example.com").await.unwrap();

    assert_eq!(browser.type_text_attempts, 1);
    assert_eq!(evaluate_count(&browser, "HTMLTextAreaElement"), 1);
  }

  #[tokio::test]
  async fn production_email_entry_waits_for_delayed_dom_mount() {
    let mut browser = FakeBrowser {
      type_text_fails: true,
      dom_text_accepts: true,
      dom_text_failures_remaining: 2,
      submit_available: true,
      ..FakeBrowser::default()
    };
    let mut adapter = BrowserAuthAdapter::new(&mut browser, "device-id");

    adapter.submit_email("person@example.com").await.unwrap();

    assert_eq!(browser.type_text_attempts, 3);
    assert_eq!(evaluate_count(&browser, "HTMLTextAreaElement"), 3);
  }

  #[tokio::test]
  async fn production_email_entry_fails_closed_after_persistent_dom_replacement() {
    let mut browser = FakeBrowser {
      type_text_fails: true,
      dom_text_accepts: false,
      ..FakeBrowser::default()
    };
    let mut adapter = BrowserAuthAdapter::new(&mut browser, "device-id");

    let error = adapter
      .submit_email("person@example.com")
      .await
      .unwrap_err();

    assert!(matches!(error, AuthError::Unknown(_)));
    assert_eq!(browser.type_text_attempts, 10);
    assert_eq!(evaluate_count(&browser, "HTMLTextAreaElement"), 10);
  }

  #[tokio::test]
  async fn production_otp_api_maps_429_to_rate_limited() {
    let mut browser = FakeBrowser {
      api_validate_status: Some(429),
      type_text_fails: true,
      ..FakeBrowser::default()
    };
    let mut adapter = BrowserAuthAdapter::new(&mut browser, "device-id");

    let error = adapter.submit_email_otp("123456").await.unwrap_err();

    assert_eq!(error, AuthError::RateLimited);
  }

  #[tokio::test]
  async fn cancellation_stops_before_email_provider_poll() {
    let mut email_otp = signals("https://auth.openai.com/email-verification");
    email_otp.has_email_otp_input = true;
    let mut adapter = FakeAuthAdapter {
      observations: VecDeque::from([email_otp]),
      ..FakeAuthAdapter::default()
    };
    let email = FakeEmailService::new(Vec::new());
    let cancel_flag = AtomicBool::new(true);

    let error = authenticate_existing_account_with_cancel(
      &mut adapter,
      credentials(None),
      &email,
      AuthPolicy::default(),
      &cancel_flag,
    )
    .await
    .unwrap_err();

    assert_eq!(error, AuthError::Cancelled);
    assert!(adapter.events.is_empty());
  }

  #[test]
  fn existing_account_credentials_debug_is_redacted() {
    let credentials = credentials(Some("JBSWY3DPEHPK3PXP"));
    let formatted = format!("{credentials:?}");

    assert!(formatted.contains("person@example.com"));
    assert!(formatted.contains("[REDACTED]"));
    assert!(!formatted.contains("password-value"));
    assert!(!formatted.contains("provider-card-value"));
    assert!(!formatted.contains("JBSWY3DPEHPK3PXP"));
  }

  #[derive(Default)]
  struct FakeAuthAdapter {
    observations: VecDeque<AuthSignals>,
    otp_results: VecDeque<EmailOtpSubmission>,
    otp_error: Option<AuthError>,
    events: Vec<String>,
    submitted_otps: Vec<String>,
    submitted_totps: Vec<String>,
    registration_calls: usize,
    quota_calls: usize,
    oauth_calls: usize,
    sub2api_calls: usize,
  }

  #[async_trait]
  impl ExistingAccountAuthAdapter for FakeAuthAdapter {
    async fn observe(&mut self) -> Result<AuthSignals, AuthError> {
      self.events.push("observe".into());
      self
        .observations
        .pop_front()
        .ok_or_else(|| AuthError::Unknown("test observation queue exhausted".into()))
    }

    async fn submit_email(&mut self, _email: &str) -> Result<(), AuthError> {
      self.events.push("submit_email".into());
      Ok(())
    }

    async fn submit_password(&mut self, _password: &str) -> Result<(), AuthError> {
      self.events.push("submit_password".into());
      Ok(())
    }

    async fn submit_email_otp(&mut self, code: &str) -> Result<EmailOtpSubmission, AuthError> {
      self.events.push("submit_email_otp".into());
      self.submitted_otps.push(code.to_string());
      if let Some(error) = self.otp_error.clone() {
        return Err(error);
      }
      Ok(
        self
          .otp_results
          .pop_front()
          .unwrap_or(EmailOtpSubmission::Accepted),
      )
    }

    async fn refresh_email_verification(&mut self) -> Result<(), AuthError> {
      self.events.push("refresh_email_verification".into());
      Ok(())
    }

    async fn resend_email_otp(&mut self) -> Result<(), AuthError> {
      self.events.push("resend_email_otp".into());
      Ok(())
    }

    async fn submit_authenticator_totp(&mut self, code: &str) -> Result<(), AuthError> {
      self.events.push("submit_authenticator_totp".into());
      self.submitted_totps.push(code.to_string());
      Ok(())
    }
  }

  fn signals(url: &str) -> AuthSignals {
    AuthSignals {
      url: url.into(),
      ..AuthSignals::default()
    }
  }

  fn credentials<'a>(totp_secret: Option<&'a str>) -> ExistingAccountCredentials<'a> {
    ExistingAccountCredentials {
      email: "person@example.com",
      password: "password-value",
      raw_cdk: "provider-card-value",
      totp_secret,
    }
  }

  fn assert_no_forbidden_collaborators(adapter: &FakeAuthAdapter) {
    assert_eq!(adapter.registration_calls, 0);
    assert_eq!(adapter.quota_calls, 0);
    assert_eq!(adapter.oauth_calls, 0);
    assert_eq!(adapter.sub2api_calls, 0);
  }

  #[test]
  fn classifier_distinguishes_email_otp_from_email_entry_and_terminal_states() {
    let mut email_entry = signals("https://auth.openai.com/log-in");
    email_entry.has_email_input = true;
    assert_eq!(classify_auth_state(&email_entry), AuthState::EmailEntry);

    let mut email_otp = signals("https://auth.openai.com/email-otp");
    email_otp.has_email_input = true;
    email_otp.has_email_otp_input = true;
    assert_eq!(classify_auth_state(&email_otp), AuthState::EmailOtp);

    let url_only_email_otp = signals("https://auth.openai.com/email-verification?state=redacted");
    assert_eq!(
      classify_auth_state(&url_only_email_otp),
      AuthState::EmailOtp
    );

    assert_eq!(
      classify_auth_state(&signals("https://chatgpt.com/")),
      AuthState::LoggedIn
    );

    let mut cloudflare = signals("https://auth.openai.com/");
    cloudflare.body_text = "Performing security verification. Verify you are human".into();
    assert_eq!(classify_auth_state(&cloudflare), AuthState::Cloudflare);

    let mut rate_limited = signals("https://auth.openai.com/");
    rate_limited.http_status = Some(429);
    assert_eq!(classify_auth_state(&rate_limited), AuthState::RateLimited);

    let mut locked = signals("https://auth.openai.com/");
    locked.body_text = "Your account has been locked after too many attempts".into();
    assert_eq!(classify_auth_state(&locked), AuthState::Locked);
  }

  #[test]
  fn classifier_requires_mounted_inputs_for_login_and_password_urls() {
    assert_eq!(
      classify_auth_state(&signals("https://auth.openai.com/log-in")),
      AuthState::Unknown
    );
    assert_eq!(
      classify_auth_state(&signals("https://auth.openai.com/log-in/password")),
      AuthState::Unknown
    );

    let mut email = signals("https://auth.openai.com/log-in");
    email.has_email_input = true;
    assert_eq!(classify_auth_state(&email), AuthState::EmailEntry);

    let mut password = signals("https://auth.openai.com/log-in/password");
    password.has_password_input = true;
    assert_eq!(classify_auth_state(&password), AuthState::Password);
  }

  #[tokio::test]
  async fn password_only_authentication_reaches_logged_in() {
    let mut password = signals("https://auth.openai.com/log-in/password");
    password.has_password_input = true;
    let mut adapter = FakeAuthAdapter {
      observations: VecDeque::from([password, signals("https://chatgpt.com/")]),
      ..FakeAuthAdapter::default()
    };
    let email = FakeEmailService::new(vec![]);

    authenticate_existing_account(
      &mut adapter,
      credentials(None),
      &email,
      AuthPolicy::default(),
    )
    .await
    .unwrap();

    assert_eq!(adapter.events, ["observe", "submit_password", "observe"]);
    assert_no_forbidden_collaborators(&adapter);
  }

  #[tokio::test]
  async fn email_otp_authentication_marks_attempted_code_and_succeeds() {
    let mut email_otp = signals("https://auth.openai.com/email-otp");
    email_otp.has_email_otp_input = true;
    let mut adapter = FakeAuthAdapter {
      observations: VecDeque::from([email_otp, signals("https://chatgpt.com/")]),
      ..FakeAuthAdapter::default()
    };
    let email = FakeEmailService::new(vec![Ok("111222")]);

    authenticate_existing_account(
      &mut adapter,
      credentials(None),
      &email,
      AuthPolicy::default(),
    )
    .await
    .unwrap();

    assert_eq!(adapter.submitted_otps, ["111222"]);
    assert_eq!(*email.used.lock().unwrap(), ["111222"]);
    assert_no_forbidden_collaborators(&adapter);
  }

  #[tokio::test]
  async fn indeterminate_email_otp_submission_does_not_consume_code() {
    let mut email_otp = signals("https://auth.openai.com/email-otp");
    email_otp.has_email_otp_input = true;
    let mut adapter = FakeAuthAdapter {
      observations: VecDeque::from([email_otp]),
      otp_error: Some(AuthError::Cloudflare),
      ..FakeAuthAdapter::default()
    };
    let email = FakeEmailService::new(vec![Ok("111222")]);

    let error = authenticate_existing_account(
      &mut adapter,
      credentials(None),
      &email,
      AuthPolicy::default(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, AuthError::Cloudflare);
    assert_eq!(adapter.submitted_otps, ["111222"]);
    assert!(email.used.lock().unwrap().is_empty());
  }

  #[tokio::test]
  async fn wrong_email_otp_refreshes_resends_and_uses_new_code() {
    let mut email_otp = signals("https://auth.openai.com/email-otp");
    email_otp.has_email_otp_input = true;
    let mut adapter = FakeAuthAdapter {
      observations: VecDeque::from([email_otp, signals("https://chatgpt.com/")]),
      otp_results: VecDeque::from([EmailOtpSubmission::WrongCode, EmailOtpSubmission::Accepted]),
      ..FakeAuthAdapter::default()
    };
    let email = FakeEmailService::new(vec![Ok("111222"), Ok("333444")]);

    authenticate_existing_account(
      &mut adapter,
      credentials(None),
      &email,
      AuthPolicy::default(),
    )
    .await
    .unwrap();

    assert_eq!(adapter.submitted_otps, ["111222", "333444"]);
    assert_eq!(*email.used.lock().unwrap(), ["111222", "333444"]);
    assert_eq!(
      adapter.events,
      [
        "observe",
        "submit_email_otp",
        "refresh_email_verification",
        "resend_email_otp",
        "submit_email_otp",
        "observe"
      ]
    );
    assert_no_forbidden_collaborators(&adapter);
  }

  #[tokio::test]
  async fn exhausted_wrong_email_otp_marks_every_code_and_returns_rejection() {
    let mut email_otp = signals("https://auth.openai.com/email-otp");
    email_otp.has_email_otp_input = true;
    let mut adapter = FakeAuthAdapter {
      observations: VecDeque::from([email_otp]),
      otp_results: VecDeque::from([
        EmailOtpSubmission::WrongCode,
        EmailOtpSubmission::WrongCode,
        EmailOtpSubmission::WrongCode,
      ]),
      ..FakeAuthAdapter::default()
    };
    let email = FakeEmailService::new(vec![Ok("111222"), Ok("333444"), Ok("555666")]);

    let error = authenticate_existing_account(
      &mut adapter,
      credentials(None),
      &email,
      AuthPolicy::default(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, AuthError::EmailOtpAttemptsExhausted);
    assert_eq!(adapter.submitted_otps, ["111222", "333444", "555666"]);
    assert_eq!(*email.used.lock().unwrap(), ["111222", "333444", "555666"]);
    assert_eq!(
      adapter.events,
      [
        "observe",
        "submit_email_otp",
        "refresh_email_verification",
        "resend_email_otp",
        "submit_email_otp",
        "refresh_email_verification",
        "resend_email_otp",
        "submit_email_otp",
      ]
    );
    assert_no_forbidden_collaborators(&adapter);
  }

  #[tokio::test]
  async fn email_otp_timeout_is_typed_and_never_submits() {
    let mut email_otp = signals("https://auth.openai.com/email-otp");
    email_otp.has_email_otp_input = true;
    let mut adapter = FakeAuthAdapter {
      observations: VecDeque::from([email_otp]),
      ..FakeAuthAdapter::default()
    };
    let email = FakeEmailService::new(vec![Err(EmailServiceError::Timeout("mail wait".into()))]);

    let error = authenticate_existing_account(
      &mut adapter,
      credentials(None),
      &email,
      AuthPolicy::default(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, AuthError::EmailOtpTimeout);
    assert!(adapter.submitted_otps.is_empty());
    assert_no_forbidden_collaborators(&adapter);
  }

  #[tokio::test]
  async fn terminal_authentication_states_stop_with_typed_errors() {
    let mut wrong_password = signals("https://auth.openai.com/log-in/password");
    wrong_password.body_text = "Incorrect email or password".into();
    let mut rate_limited = signals("https://auth.openai.com/log-in");
    rate_limited.http_status = Some(429);
    let mut locked = signals("https://auth.openai.com/log-in");
    locked.body_text = "Account temporarily locked".into();
    let mut cloudflare = signals("https://auth.openai.com/log-in");
    cloudflare.body_text = "Performing security verification. Verify you are human".into();

    for (signal, expected) in [
      (wrong_password, AuthError::WrongCredentials),
      (rate_limited, AuthError::RateLimited),
      (locked, AuthError::Locked),
      (cloudflare, AuthError::Cloudflare),
    ] {
      let mut adapter = FakeAuthAdapter {
        observations: VecDeque::from([signal]),
        ..FakeAuthAdapter::default()
      };
      let email = FakeEmailService::new(vec![]);
      let error = authenticate_existing_account(
        &mut adapter,
        credentials(None),
        &email,
        AuthPolicy::default(),
      )
      .await
      .unwrap_err();
      assert_eq!(error, expected);
      assert_eq!(adapter.events, ["observe"]);
      assert_no_forbidden_collaborators(&adapter);
    }
  }

  #[test]
  fn unknown_surface_diagnostic_excludes_query_and_body_text() {
    let mut unknown =
      signals("https://auth.openai.com/interstitial?email=person%40example.com&token=secret-value");
    unknown.body_text = "password-value provider-card-value secret-value".into();

    let detail = safe_unknown_surface_detail(&unknown);

    assert!(detail.contains("host=auth.openai.com path=/interstitial"));
    assert!(!detail.contains("person@example.com"));
    assert!(!detail.contains("password-value"));
    assert!(!detail.contains("provider-card-value"));
    assert!(!detail.contains("secret-value"));
  }

  #[tokio::test]
  async fn transient_unknown_surface_is_reobserved_before_login_continues() {
    let mut email_entry = signals("https://auth.openai.com/log-in");
    email_entry.has_email_input = true;
    let mut adapter = FakeAuthAdapter {
      observations: VecDeque::from([
        signals("about:blank"),
        email_entry,
        signals("https://chatgpt.com/"),
      ]),
      ..FakeAuthAdapter::default()
    };
    let email = FakeEmailService::new(vec![]);
    let policy = AuthPolicy {
      unknown_retry_delay_ms: 0,
      ..AuthPolicy::default()
    };

    authenticate_existing_account(&mut adapter, credentials(None), &email, policy)
      .await
      .unwrap();

    assert_eq!(
      adapter.events,
      ["observe", "observe", "submit_email", "observe"]
    );
    assert_no_forbidden_collaborators(&adapter);
  }

  #[tokio::test]
  async fn persistent_unknown_surface_still_fails_closed() {
    let mut adapter = FakeAuthAdapter {
      observations: VecDeque::from([
        signals("about:blank"),
        signals("about:blank"),
        signals("about:blank"),
      ]),
      ..FakeAuthAdapter::default()
    };
    let email = FakeEmailService::new(vec![]);
    let policy = AuthPolicy {
      unknown_retry_delay_ms: 0,
      ..AuthPolicy::default()
    };

    let error = authenticate_existing_account(&mut adapter, credentials(None), &email, policy)
      .await
      .unwrap_err();

    assert!(matches!(error, AuthError::Unknown(_)));
    assert_eq!(adapter.events, ["observe", "observe", "observe"]);
    assert_no_forbidden_collaborators(&adapter);
  }

  #[tokio::test]
  async fn existing_remote_totp_challenge_uses_optional_secret_without_logging_values() {
    let mut totp = signals("https://auth.openai.com/mfa/totp");
    totp.has_authenticator_totp_input = true;
    let mut adapter = FakeAuthAdapter {
      observations: VecDeque::from([totp, signals("https://chatgpt.com/")]),
      ..FakeAuthAdapter::default()
    };
    let email = FakeEmailService::new(vec![]);

    authenticate_existing_account(
      &mut adapter,
      credentials(Some("JBSWY3DPEHPK3PXP")),
      &email,
      AuthPolicy::default(),
    )
    .await
    .unwrap();

    assert_eq!(adapter.submitted_totps.len(), 1);
    assert!(adapter.submitted_totps[0]
      .chars()
      .all(|ch| ch.is_ascii_digit()));
    assert!(adapter.events.iter().all(|event| {
      !event.contains("JBSWY3DPEHPK3PXP") && !event.contains(&adapter.submitted_totps[0])
    }));
    assert_no_forbidden_collaborators(&adapter);
  }
}
