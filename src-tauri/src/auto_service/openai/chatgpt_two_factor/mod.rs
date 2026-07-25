use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

use super::chatgpt_auth::{click_first_text, click_first_visible, ChatGptBrowser};
use crate::auto_service::common::totp::{generate_totp_now, normalize_secret};

const CHATGPT_HOME: &str = "https://chatgpt.com/";
const PROFILE_MENU_SELECTOR: &str = r#"[data-testid="accounts-profile-button"], [data-testid="profile-button"], [aria-label*="open profile menu" i]"#;
const SETTINGS_MENU_SELECTOR: &str = r#"[data-testid="settings-menu-item"]"#;
const SETTINGS_MENU_FALLBACK_SELECTOR: &str = r#"[role="menuitem"], button, a, div[role="button"]"#;
const SECURITY_TAB_SELECTOR: &str = r#"[data-testid="security-tab"]"#;
const SECURITY_TAB_FALLBACK_SELECTOR: &str =
  r#"[data-testid="security-tab"], button, [role="tab"], div[role="tab"]"#;
const MFA_TOGGLE_SELECTOR: &str = r#"[data-testid="mfa-authenticator-toggle"]"#;
const REVEAL_SECRET_SELECTOR: &str =
  r#"button.interactive-label-accent.underline, button.underline.interactive-label-accent"#;
const COPY_SECRET_SELECTOR: &str = r#"button[aria-label="Copy code"]"#;
const SETUP_TOTP_INPUT_SELECTOR: &str = r#"#totp_otp, input[name="totp_otp"], input[name="code"], input[name="totp"], input[autocomplete="one-time-code"], input[inputmode="numeric"], input[inputmode="decimal"], input[placeholder*="6-digit"], input[placeholder*="Enter your 6-digit"], input[type="tel"][maxlength]:not([maxlength="1"]), dialog input:not([type="hidden"]):not([type="submit"])[maxlength]:not([maxlength="1"]), [role="dialog"] input:not([type="hidden"]):not([type="submit"])[maxlength]:not([maxlength="1"])"#;
const SETUP_CONFIRM_SELECTOR: &str = r#"button.btn.relative, button[type="submit"]"#;
const CLOSE_CONTROL_SELECTOR: &str = r#"[data-testid="close-button"], button[aria-label="Close"], [role="dialog"] button[aria-label="Close"]"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTwoFactorState {
  Off,
  On,
  Indeterminate,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemoteTwoFactorSignals {
  pub toggle_visible: bool,
  pub aria_checked: Option<bool>,
  pub data_state_checked: Option<bool>,
  pub setup_dialog_visible: bool,
}

pub fn classify_remote_state(signals: &RemoteTwoFactorSignals) -> RemoteTwoFactorState {
  if !signals.toggle_visible || signals.setup_dialog_visible {
    return RemoteTwoFactorState::Indeterminate;
  }
  let explicit = [signals.aria_checked, signals.data_state_checked];
  if explicit[0].is_none() || explicit[1].is_none() || explicit[0] != explicit[1] {
    return RemoteTwoFactorState::Indeterminate;
  }
  if explicit[0] == Some(true) {
    RemoteTwoFactorState::On
  } else {
    RemoteTwoFactorState::Off
  }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TwoFactorError {
  #[error("remote authenticator is already enabled; reconciliation is required")]
  ReconciliationRequired,
  #[error("remote authenticator state is indeterminate")]
  RemoteStateIndeterminate,
  #[error("remote challenge escalation blocks authenticator settings")]
  ChallengeEscalated,
  #[error("remote authenticator probe failed: {0}")]
  ProbeFailed(String),
  #[error("could not begin authenticator setup: {0}")]
  BeginFailed(String),
  #[error("could not capture authenticator secret: {0}")]
  SecretCaptureFailed(String),
  #[error("secret capture callback failed: {0}")]
  SecretCallbackFailed(String),
  #[error("could not confirm authenticator setup: {0}")]
  ConfirmFailed(String),
  #[error("could not reopen Security settings: {0}")]
  ReopenFailed(String),
  #[error("remote authenticator verification failed: {0:?}")]
  RemoteVerificationFailed(RemoteTwoFactorState),
}

#[async_trait]
pub trait TwoFactorAdapter {
  async fn inspect_remote_state(&mut self) -> Result<RemoteTwoFactorState, TwoFactorError>;
  async fn begin_authenticator_setup(&mut self) -> Result<(), TwoFactorError>;
  /// Poll until the setup dialog is fully loaded and an interactive TOTP input is present.
  /// Default: no-op (fakes override when needed).
  async fn wait_for_setup_dialog_ready(&mut self) -> Result<(), TwoFactorError> {
    Ok(())
  }
  async fn scrape_normalized_secret(&mut self) -> Result<String, TwoFactorError>;
  async fn confirm_setup(&mut self, code: &str) -> Result<(), TwoFactorError>;
  async fn close_and_reopen_security(&mut self) -> Result<(), TwoFactorError>;
}

pub async fn enable_authenticator_two_factor<A, F>(
  adapter: &mut A,
  on_secret_captured: F,
) -> Result<String, TwoFactorError>
where
  A: TwoFactorAdapter + Send + ?Sized,
  F: FnOnce(&str) -> Result<(), TwoFactorError>,
{
  match adapter.inspect_remote_state().await? {
    RemoteTwoFactorState::Off => {}
    RemoteTwoFactorState::On => return Err(TwoFactorError::ReconciliationRequired),
    RemoteTwoFactorState::Indeterminate => {
      return Err(TwoFactorError::RemoteStateIndeterminate);
    }
  }

  adapter.begin_authenticator_setup().await?;
  adapter.wait_for_setup_dialog_ready().await?;
  let secret = adapter.scrape_normalized_secret().await?;
  on_secret_captured(&secret)?;
  let code = generate_totp_now(&secret)
    .map_err(|_| TwoFactorError::SecretCaptureFailed("captured secret is invalid".into()))?;
  adapter.confirm_setup(&code).await?;
  adapter.close_and_reopen_security().await?;
  let verified = adapter.inspect_remote_state().await?;
  if verified != RemoteTwoFactorState::On {
    return Err(TwoFactorError::RemoteVerificationFailed(verified));
  }
  Ok(secret)
}

pub(crate) struct BrowserTwoFactorAdapter<'a, B> {
  browser: &'a mut B,
  security_open: bool,
}

impl<'a, B> BrowserTwoFactorAdapter<'a, B> {
  pub(crate) fn new(browser: &'a mut B) -> Self {
    Self {
      browser,
      security_open: false,
    }
  }
}

#[async_trait]
impl<B: ChatGptBrowser + Send> TwoFactorAdapter for BrowserTwoFactorAdapter<'_, B> {
  async fn inspect_remote_state(&mut self) -> Result<RemoteTwoFactorState, TwoFactorError> {
    self.ensure_security_open().await?;
    let result = self
      .browser
      .evaluate(
        r#"(function(){
          const toggle = document.querySelector('[data-testid="mfa-authenticator-toggle"]');
          if (!toggle) return { toggleVisible: false, setupDialogVisible: false };
          const rect = toggle.getBoundingClientRect();
          const aria = toggle.getAttribute('aria-checked');
          const state = toggle.getAttribute('data-state');
          return {
            toggleVisible: rect.width > 0 && rect.height > 0,
            ariaChecked: aria === 'true' ? true : aria === 'false' ? false : null,
            dataStateChecked: state === 'checked' ? true : state === 'unchecked' ? false : null,
            setupDialogVisible: !!document.querySelector('#totp_otp, input[name="totp_otp"], input[name="code"], input[name="totp"], input[autocomplete="one-time-code"], input[inputmode="numeric"]')
          };
        })()"#,
        false,
      )
      .await
      .map_err(|_| TwoFactorError::ProbeFailed("Security state evaluation failed".into()))?;
    let value = result.get("value").cloned().unwrap_or_default();
    let signals = RemoteTwoFactorSignals {
      toggle_visible: value.get("toggleVisible").and_then(Value::as_bool) == Some(true),
      aria_checked: optional_bool(&value, "ariaChecked"),
      data_state_checked: optional_bool(&value, "dataStateChecked"),
      setup_dialog_visible: value.get("setupDialogVisible").and_then(Value::as_bool) == Some(true),
    };
    Ok(classify_remote_state(&signals))
  }

  async fn begin_authenticator_setup(&mut self) -> Result<(), TwoFactorError> {
    match click_first_visible(self.browser, MFA_TOGGLE_SELECTOR).await {
      Ok(true) => {
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        Ok(())
      }
      Ok(false) => Err(TwoFactorError::BeginFailed(
        "authenticator toggle is unavailable".into(),
      )),
      Err(_) => Err(TwoFactorError::BeginFailed(
        "authenticator toggle could not be activated".into(),
      )),
    }
  }

  async fn wait_for_setup_dialog_ready(&mut self) -> Result<(), TwoFactorError> {
    for attempt in 0u32..20u32 {
      let result = self
        .browser
        .evaluate(
          r#"(function(){
            const hasSecret = !!document.querySelector(
              'button[aria-label="Copy code"], code, pre, [data-testid*="secret"], [class*="secret"], [class*="mono"]'
            );
            const hasInput = !!document.querySelector(
              '#totp_otp, input[name="totp_otp"], input[name="code"], input[name="totp"], input[autocomplete="one-time-code"], input[inputmode="numeric"], input[placeholder*="6-digit"]'
            );
            const hasConfirm = (document.body.innerText || '').match(/continue|confirm|verify|enable|done|next|set up/i) !== null;
            return { hasSecret, hasInput, hasConfirm };
          })()"#,
          false,
        )
        .await
        .ok()
        .and_then(|result| result.get("value").cloned());
      if let Some(value) = result {
        let has_secret = value.get("hasSecret").and_then(Value::as_bool) == Some(true);
        let has_input = value.get("hasInput").and_then(Value::as_bool) == Some(true);
        if has_secret && has_input {
          return Ok(());
        }
        if has_input && attempt >= 2 {
          return Ok(());
        }
      }
      if attempt >= 19 {
        return Err(TwoFactorError::BeginFailed(
          "2FA setup dialog did not appear in time".into(),
        ));
      }
      tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    }
    Err(TwoFactorError::BeginFailed(
      "2FA setup dialog did not appear in time".into(),
    ))
  }

  async fn scrape_normalized_secret(&mut self) -> Result<String, TwoFactorError> {
    let revealed = click_first_visible(self.browser, REVEAL_SECRET_SELECTOR)
      .await
      .unwrap_or(false)
      || click_first_text(
        self.browser,
        &["can't scan", "enter the code", "show code"],
        "button, a, [role='button']",
      )
      .await
      .unwrap_or(false);
    if revealed {
      tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    }
    let _ = click_first_visible(self.browser, COPY_SECRET_SELECTOR).await;
    let _ = click_first_text(self.browser, &["copy code"], "button, [role='button']").await;

    let result = self
      .browser
      .evaluate(
        r#"(function(){
          const clean = (text) => (text || '').replace(/[\s\-]/g, '').toUpperCase();
          const isSecret = (text) => /^[A-Z2-7]{16,64}$/.test(text);
          const candidates = [];
          const push = (raw, score) => {
            const secret = clean(raw);
            if (isSecret(secret)) candidates.push({ secret, score: score + secret.length });
          };
          const copy = document.querySelector('button[aria-label="Copy code"]')
            || Array.from(document.querySelectorAll('button')).find((button) =>
              /copy code/i.test(button.innerText || button.textContent || button.getAttribute('aria-label') || '')
            );
          if (copy) {
            let root = copy.parentElement;
            for (let depth = 0; depth < 5 && root; depth += 1, root = root.parentElement) {
              push(root.innerText || root.textContent || '', 100 - depth * 5);
              for (const child of root.querySelectorAll('code, pre, span, div, p')) {
                const text = (child.innerText || child.textContent || '').trim();
                if (text && text.length <= 80) push(text, 120 - depth * 5);
              }
            }
          }
          for (const element of document.querySelectorAll(
            'code, pre, [data-testid*="secret"], [class*="secret"], [class*="mono"], span, div, p'
          )) {
            const text = (element.innerText || element.textContent || '').trim();
            if (!text || text.length < 16 || text.length > 80) continue;
            push(text, text.split(/\s+/).length === 1 ? 80 : 40);
          }
          const match = document.documentElement.innerHTML.match(/secret=([A-Z2-7]{16,64})/i);
          if (match && match[1]) push(match[1], 200);
          candidates.sort((left, right) => right.score - left.score);
          return candidates.length ? { found: true, secret: candidates[0].secret } : { found: false };
        })()"#,
        false,
      )
      .await
      .map_err(|_| TwoFactorError::SecretCaptureFailed("secret evaluation failed".into()))?;
    let value = result.get("value").cloned().unwrap_or_default();
    let raw = value
      .get("secret")
      .and_then(Value::as_str)
      .filter(|_| value.get("found").and_then(Value::as_bool) == Some(true))
      .ok_or_else(|| {
        TwoFactorError::SecretCaptureFailed("secret was not present after reveal/copy".into())
      })?;
    normalize_secret(raw)
      .map_err(|_| TwoFactorError::SecretCaptureFailed("captured secret is invalid".into()))
  }

  async fn confirm_setup(&mut self, code: &str) -> Result<(), TwoFactorError> {
    if self
      .browser
      .type_text(SETUP_TOTP_INPUT_SELECTOR, code)
      .await
      .is_err()
    {
      self.type_setup_code_via_dom(code).await?;
    }
    let mut confirmed = false;
    for _ in 0..6 {
      confirmed = click_first_text(
        self.browser,
        &[
          "continue", "confirm", "done", "verify", "enable", "next", "set up",
        ],
        "button",
      )
      .await
      .unwrap_or(false)
        || click_first_visible(self.browser, SETUP_CONFIRM_SELECTOR)
          .await
          .unwrap_or(false);
      if confirmed {
        break;
      }
      tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if !confirmed {
      return Err(TwoFactorError::ConfirmFailed(
        "confirmation control is unavailable".into(),
      ));
    }
    tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
    Ok(())
  }

  async fn close_and_reopen_security(&mut self) -> Result<(), TwoFactorError> {
    self.security_open = false;
    self
      .browser
      .navigate(CHATGPT_HOME, 20)
      .await
      .map_err(|_| TwoFactorError::ReopenFailed("could not reset to ChatGPT home".into()))?;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    self
      .ensure_security_open()
      .await
      .map_err(|error| TwoFactorError::ReopenFailed(error.to_string()))
  }
}

impl<B: ChatGptBrowser + Send> BrowserTwoFactorAdapter<'_, B> {
  async fn ensure_security_open(&mut self) -> Result<(), TwoFactorError> {
    if self.security_open && self.toggle_present().await {
      return Ok(());
    }
    let current = self.browser.current_url().await.unwrap_or_default();
    if !current.starts_with("https://chatgpt.com") {
      self
        .browser
        .navigate(CHATGPT_HOME, 20)
        .await
        .map_err(|_| TwoFactorError::ProbeFailed("could not open ChatGPT home".into()))?;
      tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    }
    self.dismiss_blockers().await;

    if self.open_security_via_profile().await.is_ok() && self.toggle_present().await {
      self.security_open = true;
      return Ok(());
    }
    if let Err(error) = self.open_security_via_hash().await {
      if self.challenge_present().await {
        return Err(TwoFactorError::ChallengeEscalated);
      }
      return Err(error);
    }
    if !self.toggle_present().await {
      if self.challenge_present().await {
        return Err(TwoFactorError::ChallengeEscalated);
      }
      return Err(TwoFactorError::ProbeFailed(
        "Security panel did not expose the authenticator toggle".into(),
      ));
    }
    self.security_open = true;
    Ok(())
  }

  async fn dismiss_blockers(&mut self) {
    let _ = click_first_visible(self.browser, CLOSE_CONTROL_SELECTOR).await;
    let _ = self
      .browser
      .evaluate(
        r#"(function(){
          document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', code: 'Escape', bubbles: true }));
          document.dispatchEvent(new KeyboardEvent('keyup', { key: 'Escape', code: 'Escape', bubbles: true }));
          return true;
        })()"#,
        false,
      )
      .await;
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
  }

  async fn open_security_via_profile(&mut self) -> Result<(), TwoFactorError> {
    self.open_profile_menu().await?;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let mut settings_opened = false;
    for _ in 0..6 {
      if click_first_visible(self.browser, SETTINGS_MENU_SELECTOR)
        .await
        .unwrap_or(false)
        || click_first_text(self.browser, &["settings"], SETTINGS_MENU_FALLBACK_SELECTOR)
          .await
          .unwrap_or(false)
      {
        settings_opened = true;
        break;
      }
      tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    }
    if !settings_opened {
      return Err(TwoFactorError::ProbeFailed(
        "Settings menu item is unavailable".into(),
      ));
    }
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    self.click_security_tab().await
  }

  async fn open_profile_menu(&mut self) -> Result<(), TwoFactorError> {
    let selector = serde_json::to_string(PROFILE_MENU_SELECTOR).unwrap_or_else(|_| "\"\"".into());
    let script = format!(
      r#"(function(){{
        let best = null;
        let bestArea = 0;
        for (const element of Array.from(document.querySelectorAll({selector}))) {{
          const rect = element.getBoundingClientRect();
          if (rect.width <= 0 || rect.height <= 0) continue;
          const area = rect.width * rect.height + (rect.width >= 120 ? 10000 : 0);
          if (area > bestArea) {{ best = element; bestArea = area; }}
        }}
        if (!best) return {{ found: false }};
        best.scrollIntoView({{ block: 'center', inline: 'center' }});
        const rect = best.getBoundingClientRect();
        return {{ found: true, x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }};
      }})()"#,
    );
    let result = self
      .browser
      .evaluate(&script, false)
      .await
      .map_err(|_| TwoFactorError::ProbeFailed("profile menu evaluation failed".into()))?;
    let value = result.get("value").cloned().unwrap_or_default();
    if value.get("found").and_then(Value::as_bool) != Some(true) {
      return Err(TwoFactorError::ProbeFailed(
        "profile menu control is unavailable".into(),
      ));
    }
    let x = value.get("x").and_then(Value::as_f64).ok_or_else(|| {
      TwoFactorError::ProbeFailed("profile menu control has no x coordinate".into())
    })?;
    let y = value.get("y").and_then(Value::as_f64).ok_or_else(|| {
      TwoFactorError::ProbeFailed("profile menu control has no y coordinate".into())
    })?;
    self
      .browser
      .click_point(x, y)
      .await
      .map_err(|_| TwoFactorError::ProbeFailed("profile menu click failed".into()))
  }

  async fn open_security_via_hash(&mut self) -> Result<(), TwoFactorError> {
    self
      .browser
      .evaluate(
        r#"(function(){
          try {
            if (location.hash !== '#settings/Security') location.hash = '#settings/Security';
            else { location.hash = '#settings'; location.hash = '#settings/Security'; }
            return true;
          } catch (_) { return false; }
        })()"#,
        false,
      )
      .await
      .map_err(|_| TwoFactorError::ProbeFailed("Security hash route failed".into()))?;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    self.dismiss_blockers().await;
    if self.toggle_present().await {
      return Ok(());
    }
    self.click_security_tab().await
  }

  async fn click_security_tab(&mut self) -> Result<(), TwoFactorError> {
    let clicked = click_first_visible(self.browser, SECURITY_TAB_SELECTOR)
      .await
      .unwrap_or(false)
      || click_first_text(
        self.browser,
        &["security and login", "security"],
        SECURITY_TAB_FALLBACK_SELECTOR,
      )
      .await
      .unwrap_or(false);
    if !clicked {
      return Err(TwoFactorError::ProbeFailed(
        "Security tab is unavailable".into(),
      ));
    }
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    Ok(())
  }

  async fn type_setup_code_via_dom(&mut self, code: &str) -> Result<(), TwoFactorError> {
    let code_json = serde_json::to_string(code).unwrap_or_else(|_| "\"\"".into());
    let script = format!(
      r#"(function(){{
        const code = {code};
        const visible = (element) => {{
          if (!element || element.disabled || element.readOnly) return false;
          const rect = element.getBoundingClientRect();
          return rect.width > 0 && rect.height > 0;
        }};
        const setValue = (input, value) => {{
          input.focus();
          const descriptor = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value');
          if (descriptor && descriptor.set) descriptor.set.call(input, value);
          else input.value = value;
          input.dispatchEvent(new Event('input', {{ bubbles: true }}));
          input.dispatchEvent(new Event('change', {{ bubbles: true }}));
          input.dispatchEvent(new Event('blur', {{ bubbles: true }}));
        }};
        const isTotpLike = (input) => {{
          const haystack = [
            input.id, input.name, input.autocomplete, input.inputMode, input.type,
            input.placeholder, input.getAttribute('aria-label')
          ].filter(Boolean).join(' ');
          return /totp|otp|code|one-time|numeric|tel|text|6-digit|verif/i.test(haystack) ||
            input.maxLength === 6 || input.maxLength === -1 || input.maxLength >= 6;
        }};

        // Strategy 1: search the nearest dialog / two-factor container
        const setupRoot = Array.from(document.querySelectorAll('[role="dialog"], form, section, main, div'))
          .find((element) => /authenticator|two-factor|2fa|mfa|6-digit|verif|scan the qr|setup key/i
            .test(element.innerText || element.textContent || '')) || document;

        // Strategy 2: all visible inputs across the whole page
        const allVisible = Array.from(document.querySelectorAll('input')).filter(visible);

        // Prefer inputs inside the setup root
        const rootInputs = allVisible.filter((input) => setupRoot.contains(input));
        const candidates = rootInputs.length >= 1 ? rootInputs : allVisible;

        const totpCandidates = candidates.filter(isTotpLike);
        const single = totpCandidates.find((input) => input.maxLength !== 1);
        if (single) {{
          setValue(single, code);
          return {{ ok: String(single.value || '').trim() === code, mode: 'single' }};
        }}

        // Strategy 3: any visible, non-disabled input whose maxLength != 1
        const anySingle = candidates.find((input) =>
          input.maxLength !== 1 && !input.disabled && !input.readOnly &&
          /text|tel|number|numeric/i.test(input.type || 'text')
        );
        if (anySingle) {{
          setValue(anySingle, code);
          return {{ ok: String(anySingle.value || '').trim() === code, mode: 'single_fallback' }};
        }}

        // Strategy 4: multi-digit inputs (maxLength == 1 per digit)
        const digits = candidates.filter((input) => input.maxLength === 1 || /digit|code|otp/i.test([
          input.id, input.name, input.getAttribute('aria-label')
        ].filter(Boolean).join(' '))).slice(0, code.length);
        if (digits.length >= code.length) {{
          for (let index = 0; index < code.length; index += 1) setValue(digits[index], code[index]);
          const accepted = digits.every((input, index) => String(input.value || '') === code[index]);
          return {{ ok: accepted, mode: 'multi' }};
        }}

        return {{ ok: false, reason: 'not_found', candidateCount: candidates.length, rootInputCount: rootInputs.length, allVisibleCount: allVisible.length }};
      }})()"#,
      code = code_json,
    );
    let result = self
      .browser
      .evaluate(&script, false)
      .await
      .map_err(|_| TwoFactorError::ConfirmFailed("TOTP input evaluation failed".into()))?;
    let value = result.get("value").cloned().unwrap_or_default();
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
      Ok(())
    } else {
      let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("value_not_accepted");
      Err(TwoFactorError::ConfirmFailed(format!(
        "TOTP input is unavailable: {reason}"
      )))
    }
  }

  async fn challenge_present(&mut self) -> bool {
    self
      .browser
      .evaluate(
        r#"(function(){
          const body = (document.body && (document.body.innerText || document.body.textContent) || '').toLowerCase();
          const href = String(location.href || '').toLowerCase();
          return href.includes('/challenge')
            || href.includes('/checkpoint')
            || href.includes('challenges.cloudflare.com')
            || body.includes('verify you are human')
            || body.includes('performing security verification')
            || body.includes('unusual activity')
            || !!document.querySelector('.cf-turnstile, #cf-turnstile, iframe[src*="challenges.cloudflare.com"]');
        })()"#,
        false,
      )
      .await
      .ok()
      .and_then(|result| result.get("value").and_then(Value::as_bool))
      == Some(true)
  }

  async fn toggle_present(&mut self) -> bool {
    self
      .browser
      .evaluate(
        r#"(function(){
          const toggle = document.querySelector('[data-testid="mfa-authenticator-toggle"]');
          if (!toggle) return false;
          const rect = toggle.getBoundingClientRect();
          return rect.width > 0 && rect.height > 0;
        })()"#,
        false,
      )
      .await
      .ok()
      .and_then(|result| result.get("value").and_then(Value::as_bool))
      == Some(true)
  }
}

fn optional_bool(value: &Value, key: &str) -> Option<bool> {
  value.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
mod tests {
  use std::collections::VecDeque;
  use std::sync::{Arc, Mutex};

  use async_trait::async_trait;

  use super::*;

  const SECRET: &str = "JBSWY3DPEHPK3PXP";

  #[derive(Default)]
  struct FakeBrowser {
    events: Vec<String>,
    navigate_fails: bool,
    toggle_present: bool,
    challenge_present: bool,
    type_text_fails: bool,
    dom_totp_accepts: bool,
  }

  impl FakeBrowser {
    fn event_index(&self, event: &str) -> usize {
      self
        .events
        .iter()
        .position(|recorded| recorded == event)
        .unwrap_or_else(|| panic!("missing browser event: {event}"))
    }
  }

  #[async_trait]
  impl ChatGptBrowser for FakeBrowser {
    async fn navigate(&mut self, url: &str, _timeout_secs: u64) -> Result<(), String> {
      self.events.push(format!("navigate:{url}"));
      if self.navigate_fails {
        Err("test navigation failure".into())
      } else {
        Ok(())
      }
    }

    async fn evaluate(&mut self, expression: &str, _await_promise: bool) -> Result<Value, String> {
      if expression.contains("const code =") {
        self.events.push("dom_totp_fallback".into());
        return Ok(
          serde_json::json!({ "value": { "ok": self.dom_totp_accepts, "mode": "single" } }),
        );
      }
      if expression.contains("ariaChecked") {
        self.events.push("inspect_state".into());
        return Ok(serde_json::json!({
          "value": {
            "toggleVisible": true,
            "ariaChecked": true,
            "dataStateChecked": true,
            "setupDialogVisible": false,
          }
        }));
      }
      if expression.contains("location.hash") {
        self.events.push("open_security".into());
        return Ok(serde_json::json!({ "value": true }));
      }
      if expression.contains("const selector") {
        self.events.push("profile_probe".into());
        return Ok(serde_json::json!({ "value": { "found": false } }));
      }
      if expression.contains("performing security verification") {
        self.events.push("challenge_probe".into());
        return Ok(serde_json::json!({ "value": self.challenge_present }));
      }
      if expression.contains("mfa-authenticator-toggle") {
        self.events.push("toggle_probe".into());
        return Ok(serde_json::json!({ "value": self.toggle_present }));
      }
      if expression.contains("querySelectorAll") {
        let found = expression.contains("const labels = [\"continue\"");
        return Ok(serde_json::json!({ "value": { "found": found, "x": 10.0, "y": 10.0 } }));
      }
      Ok(serde_json::json!({ "value": true }))
    }

    async fn current_url(&mut self) -> Result<String, String> {
      Ok(CHATGPT_HOME.into())
    }

    async fn type_text(&mut self, _selector: &str, _value: &str) -> Result<(), String> {
      self.events.push("type_text".into());
      if self.type_text_fails {
        Err("test input unavailable".into())
      } else {
        Ok(())
      }
    }

    async fn click_point(&mut self, _x: f64, _y: f64) -> Result<(), String> {
      Ok(())
    }
  }

  #[tokio::test]
  async fn production_reopen_navigates_and_opens_security_before_second_inspect() {
    let mut browser = FakeBrowser {
      toggle_present: true,
      ..FakeBrowser::default()
    };
    let mut adapter = BrowserTwoFactorAdapter::new(&mut browser);
    adapter.security_open = true;

    adapter.close_and_reopen_security().await.unwrap();
    assert_eq!(
      adapter.inspect_remote_state().await.unwrap(),
      RemoteTwoFactorState::On
    );

    let navigate = browser.event_index(&format!("navigate:{CHATGPT_HOME}"));
    let reopen = browser.event_index("open_security");
    let inspect = browser.event_index("inspect_state");
    assert!(navigate < reopen);
    assert!(reopen < inspect);
  }

  #[tokio::test]
  async fn production_reopen_reports_navigation_failure() {
    let mut browser = FakeBrowser {
      navigate_fails: true,
      ..FakeBrowser::default()
    };
    let mut adapter = BrowserTwoFactorAdapter::new(&mut browser);

    let error = adapter.close_and_reopen_security().await.unwrap_err();

    assert_eq!(
      error,
      TwoFactorError::ReopenFailed("could not reset to ChatGPT home".into())
    );
  }

  #[tokio::test]
  async fn production_confirm_uses_dom_fallback_when_direct_totp_selector_misses() {
    let mut browser = FakeBrowser {
      type_text_fails: true,
      dom_totp_accepts: true,
      ..FakeBrowser::default()
    };
    let mut adapter = BrowserTwoFactorAdapter::new(&mut browser);

    adapter.confirm_setup("123456").await.unwrap();

    assert!(browser.events.contains(&"type_text".to_string()));
    assert!(browser.events.contains(&"dom_totp_fallback".to_string()));
    assert!(browser.events.iter().all(|event| !event.contains("123456")));
  }

  #[tokio::test]
  async fn production_confirm_fails_closed_when_dom_totp_fallback_is_rejected() {
    let mut browser = FakeBrowser {
      type_text_fails: true,
      dom_totp_accepts: false,
      ..FakeBrowser::default()
    };
    let mut adapter = BrowserTwoFactorAdapter::new(&mut browser);

    let error = adapter.confirm_setup("123456").await.unwrap_err();

    assert!(matches!(error, TwoFactorError::ConfirmFailed(_)));
    assert!(browser.events.contains(&"dom_totp_fallback".to_string()));
  }

  #[tokio::test]
  async fn production_security_probe_classifies_explicit_challenge_escalation() {
    let mut browser = FakeBrowser {
      challenge_present: true,
      ..FakeBrowser::default()
    };
    let mut adapter = BrowserTwoFactorAdapter::new(&mut browser);

    let error = adapter.inspect_remote_state().await.unwrap_err();

    assert_eq!(error, TwoFactorError::ChallengeEscalated);
    assert!(browser.events.contains(&"challenge_probe".to_string()));
  }

  struct FakeTwoFactorAdapter {
    states: VecDeque<RemoteTwoFactorState>,
    events: Arc<Mutex<Vec<String>>>,
    confirmed_codes: Vec<String>,
  }

  impl FakeTwoFactorAdapter {
    fn new(states: impl IntoIterator<Item = RemoteTwoFactorState>) -> Self {
      Self {
        states: states.into_iter().collect(),
        events: Arc::new(Mutex::new(Vec::new())),
        confirmed_codes: Vec::new(),
      }
    }

    fn record(&self, event: &str) {
      self.events.lock().unwrap().push(event.into());
    }

    fn recorded_events(&self) -> Vec<String> {
      self.events.lock().unwrap().clone()
    }
  }

  #[async_trait]
  impl TwoFactorAdapter for FakeTwoFactorAdapter {
    async fn inspect_remote_state(&mut self) -> Result<RemoteTwoFactorState, TwoFactorError> {
      self.record("inspect");
      self
        .states
        .pop_front()
        .ok_or_else(|| TwoFactorError::ProbeFailed("test state queue exhausted".into()))
    }

    async fn begin_authenticator_setup(&mut self) -> Result<(), TwoFactorError> {
      self.record("begin");
      Ok(())
    }

    async fn scrape_normalized_secret(&mut self) -> Result<String, TwoFactorError> {
      self.record("capture");
      Ok(SECRET.into())
    }

    async fn confirm_setup(&mut self, code: &str) -> Result<(), TwoFactorError> {
      self.record("confirm");
      self.confirmed_codes.push(code.to_string());
      Ok(())
    }

    async fn close_and_reopen_security(&mut self) -> Result<(), TwoFactorError> {
      self.record("reopen");
      Ok(())
    }
  }

  #[test]
  fn remote_state_requires_visible_toggle_and_two_agreeing_explicit_signals() {
    let decisive = |enabled| RemoteTwoFactorSignals {
      toggle_visible: true,
      aria_checked: Some(enabled),
      data_state_checked: Some(enabled),
      setup_dialog_visible: false,
    };
    assert_eq!(
      classify_remote_state(&decisive(false)),
      RemoteTwoFactorState::Off
    );
    assert_eq!(
      classify_remote_state(&decisive(true)),
      RemoteTwoFactorState::On
    );

    for signals in [
      RemoteTwoFactorSignals {
        toggle_visible: true,
        aria_checked: Some(false),
        data_state_checked: None,
        setup_dialog_visible: false,
      },
      RemoteTwoFactorSignals {
        toggle_visible: true,
        aria_checked: None,
        data_state_checked: Some(false),
        setup_dialog_visible: false,
      },
      RemoteTwoFactorSignals {
        toggle_visible: true,
        aria_checked: Some(false),
        data_state_checked: Some(true),
        setup_dialog_visible: false,
      },
      RemoteTwoFactorSignals {
        toggle_visible: false,
        ..decisive(false)
      },
      RemoteTwoFactorSignals {
        setup_dialog_visible: true,
        ..decisive(false)
      },
    ] {
      assert_eq!(
        classify_remote_state(&signals),
        RemoteTwoFactorState::Indeterminate
      );
    }
  }

  #[tokio::test]
  async fn off_state_runs_capture_callback_confirm_reopen_and_independent_on_probe() {
    let mut adapter =
      FakeTwoFactorAdapter::new([RemoteTwoFactorState::Off, RemoteTwoFactorState::On]);
    let events = Arc::clone(&adapter.events);

    let secret = enable_authenticator_two_factor(&mut adapter, |captured| {
      assert_eq!(captured, SECRET);
      events.lock().unwrap().push("journal".into());
      Ok(())
    })
    .await
    .unwrap();

    assert_eq!(secret, SECRET);
    assert_eq!(
      adapter.recorded_events(),
      ["inspect", "begin", "capture", "journal", "confirm", "reopen", "inspect"]
    );
    assert_eq!(adapter.confirmed_codes.len(), 1);
    assert!(adapter.confirmed_codes[0]
      .chars()
      .all(|character| character.is_ascii_digit()));
    assert!(adapter
      .recorded_events()
      .iter()
      .all(|event| { !event.contains(SECRET) && !event.contains(&adapter.confirmed_codes[0]) }));
  }

  #[tokio::test]
  async fn callback_failure_prevents_confirm() {
    let mut adapter = FakeTwoFactorAdapter::new([RemoteTwoFactorState::Off]);

    let error = enable_authenticator_two_factor(&mut adapter, |_secret| {
      Err(TwoFactorError::SecretCallbackFailed(
        "journal unavailable".into(),
      ))
    })
    .await
    .unwrap_err();

    assert_eq!(
      error,
      TwoFactorError::SecretCallbackFailed("journal unavailable".into())
    );
    assert_eq!(adapter.recorded_events(), ["inspect", "begin", "capture"]);
    assert!(adapter.confirmed_codes.is_empty());
  }

  #[tokio::test]
  async fn on_and_indeterminate_states_never_begin_setup() {
    for (state, expected) in [
      (
        RemoteTwoFactorState::On,
        TwoFactorError::ReconciliationRequired,
      ),
      (
        RemoteTwoFactorState::Indeterminate,
        TwoFactorError::RemoteStateIndeterminate,
      ),
    ] {
      let mut adapter = FakeTwoFactorAdapter::new([state]);
      let error = enable_authenticator_two_factor(&mut adapter, |_secret| Ok(()))
        .await
        .unwrap_err();
      assert_eq!(error, expected);
      assert_eq!(adapter.recorded_events(), ["inspect"]);
    }
  }

  #[tokio::test]
  async fn remote_remaining_off_or_indeterminate_fails_closed_after_reopen() {
    for state in [
      RemoteTwoFactorState::Off,
      RemoteTwoFactorState::Indeterminate,
    ] {
      let mut adapter = FakeTwoFactorAdapter::new([RemoteTwoFactorState::Off, state]);
      let error = enable_authenticator_two_factor(&mut adapter, |_secret| Ok(()))
        .await
        .unwrap_err();
      assert_eq!(error, TwoFactorError::RemoteVerificationFailed(state));
      assert_eq!(
        adapter.recorded_events(),
        ["inspect", "begin", "capture", "confirm", "reopen", "inspect"]
      );
    }
  }
}
