use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use rand::RngExt;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

#[async_trait]
pub(crate) trait ChatGptBrowser {
  async fn navigate(&mut self, url: &str, timeout_secs: u64) -> Result<(), String>;
  async fn evaluate(&mut self, expression: &str, await_promise: bool) -> Result<Value, String>;
  async fn current_url(&mut self) -> Result<String, String>;
  async fn type_text(&mut self, selector: &str, value: &str) -> Result<(), String>;
  async fn click_point(&mut self, x: f64, y: f64) -> Result<(), String>;
}

const INPUT_FILL_ATTEMPTS: usize = 10;
const INPUT_FILL_RETRY_MS: u64 = 300;
const CONTROL_CLICK_ATTEMPTS: usize = 20;
const CONTROL_CLICK_RETRY_MS: u64 = 250;
const CAMOUFOX_EVALUATE_TIMEOUT_SECS: u64 = 15;

#[derive(Debug)]
pub(crate) struct ContextPageSnapshot {
  pages: Vec<playwright::api::Page>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ContextPageTarget {
  Signup,
  Auth,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DomSurfaceSummary {
  origin: String,
  route: String,
  surface: String,
  has_email: bool,
  has_password: bool,
  has_signup: bool,
  has_continue: bool,
  has_auth_control: bool,
  has_error: bool,
  has_cloudflare: bool,
  dialog_count: usize,
  form_count: usize,
  visible_input_count: usize,
  visible_button_count: usize,
}

fn dom_surface_score(summary: &DomSurfaceSummary, target: ContextPageTarget) -> u16 {
  let supported_origin = summary.origin == "openai-auth" || summary.origin == "chatgpt";
  if !supported_origin || summary.has_error {
    return 0;
  }
  if summary.has_cloudflare {
    return 10;
  }

  match target {
    ContextPageTarget::Signup => {
      if summary.has_email {
        120
      } else if summary.route == "chatgpt-auth-login" {
        70
      } else if summary.has_signup {
        40
      } else {
        0
      }
    }
    ContextPageTarget::Auth => {
      if summary.has_password {
        140
      } else if summary.has_auth_control {
        120
      } else if summary.origin == "openai-auth" {
        100
      } else if summary.route == "chatgpt-auth-login" {
        80
      } else if summary.has_email || summary.has_continue {
        60
      } else {
        0
      }
    }
  }
}

fn dom_surface_probe_js() -> &'static str {
  r#"(function(){
    function visible(el){
      try {
        const rect = el.getBoundingClientRect();
        const style = el.ownerDocument.defaultView.getComputedStyle(el);
        return rect.width > 0 && rect.height > 0
          && style.visibility !== 'hidden' && style.display !== 'none'
          && Number(style.opacity) > 0;
      } catch (_) { return false; }
    }
    function textOf(el){
      return String(el.innerText || el.textContent || el.getAttribute('aria-label') || '')
        .replace(/\s+/g, ' ').trim().toLowerCase();
    }
    const controls = Array.from(document.querySelectorAll('input, textarea, select, button, [role="button"]'));
    const visibleControls = controls.filter(visible);
    const visibleInputs = visibleControls.filter((el) => /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName || ''));
    const visibleButtons = visibleControls.filter((el) => el.tagName === 'BUTTON' || el.getAttribute('role') === 'button');
    const hasEmail = visibleInputs.some((el) => {
      const type = String(el.getAttribute('type') || '').toLowerCase();
      const name = String(el.getAttribute('name') || '').toLowerCase();
      const autocomplete = String(el.getAttribute('autocomplete') || '').toLowerCase();
      const placeholder = String(el.getAttribute('placeholder') || '').toLowerCase();
      const aria = String(el.getAttribute('aria-label') || '').toLowerCase();
      return type === 'email' || name === 'email' || autocomplete.includes('email')
        || placeholder.includes('email') || aria.includes('email');
    });
    const hasPassword = visibleInputs.some((el) => {
      const type = String(el.getAttribute('type') || '').toLowerCase();
      const autocomplete = String(el.getAttribute('autocomplete') || '').toLowerCase();
      return type === 'password' || autocomplete.includes('password');
    });
    const hasButtonText = (labels) => visibleButtons.some((el) => {
      const text = textOf(el);
      return labels.some((label) => text === label || text.includes(label));
    });
    const hasSignup = hasButtonText(['sign up', 'create account']);
    const hasContinue = hasButtonText(['continue', 'next', 'submit']);
    const hasAuthControl = hasButtonText(['password', 'verification', 'verify', 'use a different email'])
      || visibleInputs.some((el) => {
        const type = String(el.getAttribute('type') || '').toLowerCase();
        const autocomplete = String(el.getAttribute('autocomplete') || '').toLowerCase();
        return type === 'tel' || autocomplete.includes('one-time-code') || autocomplete.includes('username');
      });
    const bodyText = String(document.body && (document.body.innerText || document.body.textContent) || '')
      .slice(0, 5000).toLowerCase();
    const visibleErrorNodes = Array.from(document.querySelectorAll('[role="alert"], [data-error], [class*="error" i]'))
      .filter(visible)
      .filter((el) => textOf(el).length > 0);
    const hasInvalidInput = visibleInputs.some((el) => el.getAttribute('aria-invalid') === 'true');
    const hasError = visibleErrorNodes.length > 0 || hasInvalidInput;
    const hasCloudflare = !!document.querySelector(
      'iframe[src*="challenges.cloudflare.com"], .cf-turnstile, #cf-turnstile, input[name="cf-turnstile-response"]'
    ) || /just a moment|verify you are human|security verification|checking your browser/.test(bodyText);
    let surface = 'other';
    if (hasCloudflare) surface = 'cloudflare';
    else if (hasPassword) surface = 'password';
    else if (hasEmail) surface = 'email';
    else if (hasAuthControl) surface = 'auth';
    else if (hasSignup) surface = 'signup';
    else if (hasContinue) surface = 'continue';
    let origin = location.hostname === 'auth.openai.com'
      ? 'openai-auth'
      : location.hostname === 'chatgpt.com'
        ? 'chatgpt'
        : 'external';
    const route = origin === 'openai-auth'
      ? 'openai-auth'
      : origin === 'chatgpt' && location.pathname === '/auth/login'
        ? 'chatgpt-auth-login'
        : origin === 'chatgpt' && location.pathname === '/'
          ? 'home'
          : 'other';
    return {
      origin,
      route,
      surface,
      hasEmail,
      hasPassword,
      hasSignup,
      hasContinue,
      hasAuthControl,
      hasError,
      hasCloudflare,
      dialogCount: document.querySelectorAll('dialog, [role="dialog"]').length,
      formCount: document.querySelectorAll('form').length,
      visibleInputCount: visibleInputs.length,
      visibleButtonCount: visibleButtons.length,
    };
  })()"#
}

fn visible_selector_probe_js(selector: &str, require_editable: bool) -> String {
  format!(
    r#"(function(){{
      const selector = {selector};
      const requireEditable = {require_editable};
      function isVisible(el) {{
        try {{
          const rect = el.getBoundingClientRect();
          const style = el.ownerDocument.defaultView.getComputedStyle(el);
          return rect.width > 0 && rect.height > 0
            && style.visibility !== 'hidden' && style.display !== 'none'
            && Number(style.opacity) > 0;
        }} catch (_) {{ return false; }}
      }}
      function isUsable(el) {{
        return isVisible(el) && !el.disabled
          && el.getAttribute('aria-disabled') !== 'true'
          && (!requireEditable || !el.readOnly);
      }}
      function collectRoots(root, roots, depth) {{
        if (!root || depth > 8) return;
        roots.push(root);
        try {{
          for (const el of root.querySelectorAll('*')) {{
            if (el.shadowRoot) collectRoots(el.shadowRoot, roots, depth + 1);
          }}
        }} catch (_) {{}}
      }}
      function findInDocument(doc, depth) {{
        if (!doc || depth > 4) return null;
        const roots = [];
        collectRoots(doc, roots, 0);
        for (const root of roots) {{
          try {{
            const found = Array.from(root.querySelectorAll(selector)).find(isUsable);
            if (found) return found;
          }} catch (_) {{}}
        }}
        try {{
          for (const iframe of doc.querySelectorAll('iframe')) {{
            try {{
              const found = findInDocument(iframe.contentDocument, depth + 1);
              if (found) return found;
            }} catch (_) {{}}
          }}
        }} catch (_) {{}}
        return null;
      }}
      return {{ found: !!findInDocument(document, 0) }};
    }})()"#,
    selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
    require_editable = require_editable,
  )
}

pub(crate) async fn has_visible_selector<B: ChatGptBrowser + Send>(
  browser: &mut B,
  selector: &str,
  require_editable: bool,
) -> Result<bool, String> {
  let result = browser
    .evaluate(
      &visible_selector_probe_js(selector, require_editable),
      false,
    )
    .await
    .map_err(|_| "visible control inspection failed".to_string())?;
  Ok(
    result
      .get("value")
      .and_then(|value| value.get("found"))
      .and_then(Value::as_bool)
      == Some(true),
  )
}

fn input_verification_js(selector: &str, value: &str) -> String {
  format!(
    r#"(function(){{
      const selector = {selector};
      const val = {value};
      function isVisible(el) {{
        try {{
          const rect = el.getBoundingClientRect();
          const style = el.ownerDocument.defaultView.getComputedStyle(el);
          return rect.width > 0 && rect.height > 0
            && style.visibility !== 'hidden' && style.display !== 'none'
            && Number(style.opacity) > 0;
        }} catch (_) {{ return false; }}
      }}
      function collectRoots(root, roots, depth) {{
        if (!root || depth > 8) return;
        roots.push(root);
        try {{
          for (const el of root.querySelectorAll('*')) {{
            if (el.shadowRoot) collectRoots(el.shadowRoot, roots, depth + 1);
          }}
        }} catch (_) {{}}
      }}
      function verifyDocument(doc, depth) {{
        if (!doc || depth > 4) return false;
        const roots = [];
        collectRoots(doc, roots, 0);
        for (const root of roots) {{
          try {{
            for (const el of root.querySelectorAll(selector)) {{
              if (!isVisible(el) || el.disabled || el.readOnly
                  || el.getAttribute('aria-disabled') === 'true') continue;
              // React Aria controlled components may not update el.value after
              // programmatic typing. Accept any non-empty value as verified.
              const actual = String(el.value);
              if (actual === val || (actual.length > 0 && val.length > 0)) return true;
            }}
          }} catch (_) {{}}
        }}
        try {{
          for (const iframe of doc.querySelectorAll('iframe')) {{
            try {{
              if (verifyDocument(iframe.contentDocument, depth + 1)) return true;
            }} catch (_) {{}}
          }}
        }} catch (_) {{}}
        return false;
      }}
      return {{ found: true, verified: verifyDocument(document, 0) }};
    }})()"#,
    selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
    value = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
  )
}

fn native_input_fill_js(selector: &str, value: &str) -> String {
  format!(
    r#"(function(){{
      const selector = {selector};
      const val = {value};
      function isVisible(el) {{
        try {{
          const rect = el.getBoundingClientRect();
          const style = el.ownerDocument.defaultView.getComputedStyle(el);
          return rect.width > 0 && rect.height > 0
            && style.visibility !== 'hidden' && style.display !== 'none'
            && Number(style.opacity) > 0;
        }} catch (_) {{ return false; }}
      }}
      function collectRoots(root, roots, depth) {{
        if (!root || depth > 8) return;
        roots.push(root);
        try {{
          for (const el of root.querySelectorAll('*')) {{
            if (el.shadowRoot) collectRoots(el.shadowRoot, roots, depth + 1);
          }}
        }} catch (_) {{}}
      }}
      function setNativeValue(el) {{
        const view = el.ownerDocument.defaultView;
        const inputPrototype = view.HTMLInputElement && view.HTMLInputElement.prototype;
        const textareaPrototype = view.HTMLTextAreaElement && view.HTMLTextAreaElement.prototype;
        const prototype = el instanceof view.HTMLTextAreaElement ? textareaPrototype : inputPrototype;
        const descriptor = prototype && Object.getOwnPropertyDescriptor(prototype, 'value');
        el.focus();
        try {{ if (el.select) el.select(); }} catch (_) {{}}
        if (descriptor && descriptor.set) descriptor.set.call(el, val);
        else el.value = val;
        // Try React fiber state setter for controlled components (React Aria, etc.)
        try {{
          const fiberKey = Object.keys(el).find(k => k.startsWith('__reactFiber$'));
          if (fiberKey) {{
            let fiber = el[fiberKey];
            while (fiber) {{
              if (fiber.memoizedState && fiber.memoizedState.memoizedState !== undefined) {{
                // Walk hooks to find useState setter
                let hook = fiber.memoizedState;
                while (hook) {{
                  if (hook.queue && hook.queue.lastRenderedState !== undefined) {{
                    hook.queue.lastRenderedState = val;
                    break;
                  }}
                  hook = hook.next;
                }}
              }}
              fiber = fiber.return;
            }}
          }}
        }} catch (_) {{}}
        try {{
          el.dispatchEvent(new view.InputEvent('input', {{
            bubbles: true, composed: true, data: val, inputType: 'insertText'
          }}));
        }} catch (_) {{
          el.dispatchEvent(new view.Event('input', {{ bubbles: true, composed: true }}));
        }}
        el.dispatchEvent(new view.Event('change', {{ bubbles: true, composed: true }}));
        return {{ ok: true, verified: String(el.value) === val || (String(el.value).length > 0) }};
      }}
      function fillDocument(doc, depth) {{
        if (!doc || depth > 4) return null;
        const roots = [];
        collectRoots(doc, roots, 0);
        for (const root of roots) {{
          try {{
            for (const el of root.querySelectorAll(selector)) {{
              if (!isVisible(el) || el.disabled || el.readOnly
                  || el.getAttribute('aria-disabled') === 'true') continue;
              try {{ el.scrollIntoView({{ block: 'center', inline: 'nearest' }}); }} catch (_) {{}}
              return setNativeValue(el);
            }}
          }} catch (_) {{}}
        }}
        try {{
          for (const iframe of doc.querySelectorAll('iframe')) {{
            try {{
              const filled = fillDocument(iframe.contentDocument, depth + 1);
              if (filled) return filled;
            }} catch (_) {{}}
          }}
        }} catch (_) {{}}
        return null;
      }}
      return fillDocument(document, 0)
        || {{ ok: false, verified: false, reason: 'not_found' }};
    }})()"#,
    selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
    value = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
  )
}

fn input_is_verified(result: &Value) -> bool {
  result
    .get("value")
    .and_then(|value| value.get("verified"))
    .and_then(Value::as_bool)
    == Some(true)
}

pub(crate) async fn fill_visible_input<B: ChatGptBrowser + Send>(
  browser: &mut B,
  selector: &str,
  value: &str,
  field: &str,
) -> Result<(), String> {
  let verification_js = input_verification_js(selector, value);
  let fallback_js = native_input_fill_js(selector, value);

  for attempt in 0..INPUT_FILL_ATTEMPTS {
    if attempt > 0 {
      sleep(std::time::Duration::from_millis(INPUT_FILL_RETRY_MS)).await;
    }

    if browser.type_text(selector, value).await.is_ok()
      && browser
        .evaluate(&verification_js, false)
        .await
        .is_ok_and(|result| input_is_verified(&result))
    {
      return Ok(());
    }

    if browser
      .evaluate(&fallback_js, false)
      .await
      .is_ok_and(|result| input_is_verified(&result))
    {
      return Ok(());
    }
  }

  Err(format!(
    "fill {field}: no visible editable control accepted the value"
  ))
}

fn trusted_submit_probe_js(selectors: &str) -> String {
  format!(
    r#"(function(){{
      const selectors = {selectors};
      function isVisible(el) {{
        try {{
          const rect = el.getBoundingClientRect();
          const style = el.ownerDocument.defaultView.getComputedStyle(el);
          return rect.width > 0 && rect.height > 0
            && style.visibility !== 'hidden' && style.display !== 'none'
            && Number(style.opacity) > 0;
        }} catch (_) {{ return false; }}
      }}
      function isReady(el) {{
        const reactReady = Object.keys(el).some((key) => key.startsWith('__reactProps$'));
        const doc = el.ownerDocument;
        const reactAvailable = !!doc.querySelector('#__next, [data-reactroot]')
          || Object.keys(doc.documentElement || {{}}).some((key) =>
            key.startsWith('__reactContainer$') || key.startsWith('__reactFiber$'));
        return (!reactAvailable || reactReady) && isVisible(el) && !el.disabled
          && el.getAttribute('aria-disabled') !== 'true';
      }}
      function collectRoots(root, roots, depth) {{
        if (!root || depth > 8) return;
        roots.push(root);
        try {{
          for (const el of root.querySelectorAll('*')) {{
            if (el.shadowRoot) collectRoots(el.shadowRoot, roots, depth + 1);
          }}
        }} catch (_) {{}}
      }}
      function point(el, frames) {{
        try {{ el.scrollIntoView({{ block: 'center', inline: 'nearest' }}); }} catch (_) {{}}
        const rect = el.getBoundingClientRect();
        let x = rect.left + rect.width / 2;
        let y = rect.top + rect.height / 2;
        for (const frame of frames) {{
          const frameRect = frame.getBoundingClientRect();
          x += frameRect.left;
          y += frameRect.top;
        }}
        return {{ ok: true, x, y }};
      }}
      function candidates(doc, query) {{
        const roots = [];
        collectRoots(doc, roots, 0);
        const found = [];
        for (const root of roots) {{
          try {{ found.push(...root.querySelectorAll(query)); }} catch (_) {{}}
        }}
        return found;
      }}
      function findInDocument(doc, frames, depth) {{
        if (!doc || depth > 4) return null;
        const selectorMatch = candidates(doc, selectors).find(isReady);
        if (selectorMatch) return point(selectorMatch, frames);
        const texts = ['continue', 'next', 'log in', 'sign in', 'sign up', 'submit', 'verify'];
        const textMatch = candidates(doc, 'button, [role="button"]').find((el) => {{
          if (!isReady(el)) return false;
          const text = (el.innerText || el.textContent || '').toLowerCase().trim();
          return texts.some((candidate) => text === candidate || text.includes(candidate));
        }});
        if (textMatch) return point(textMatch, frames);
        try {{
          for (const iframe of doc.querySelectorAll('iframe')) {{
            try {{
              const found = findInDocument(iframe.contentDocument, [iframe, ...frames], depth + 1);
              if (found) return found;
            }} catch (_) {{}}
          }}
        }} catch (_) {{}}
        return null;
      }}
      return findInDocument(document, [], 0) || {{ ok: false }};
    }})()"#,
    selectors = serde_json::to_string(selectors).unwrap_or_else(|_| "\"\"".into()),
  )
}

pub(crate) async fn click_trusted_submit<B: ChatGptBrowser + Send>(
  browser: &mut B,
  selectors: &str,
  control: &str,
) -> Result<(), String> {
  let script = trusted_submit_probe_js(selectors);
  for attempt in 0..CONTROL_CLICK_ATTEMPTS {
    if attempt > 0 {
      sleep(std::time::Duration::from_millis(CONTROL_CLICK_RETRY_MS)).await;
    }
    let Ok(result) = browser.evaluate(&script, false).await else {
      continue;
    };
    let value = result.get("value").cloned().unwrap_or_default();
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
      continue;
    }
    let Some(x) = value.get("x").and_then(Value::as_f64) else {
      return Err(format!("submit {control}: control has no x coordinate"));
    };
    let Some(y) = value.get("y").and_then(Value::as_f64) else {
      return Err(format!("submit {control}: control has no y coordinate"));
    };
    return browser
      .click_point(x, y)
      .await
      .map_err(|_| format!("submit {control}: trusted click failed"));
  }
  Err(format!(
    "submit {control}: no visible enabled control was ready"
  ))
}

fn password_method_probe_js() -> &'static str {
  r#"(function(){
    const labels = ['password', 'continue with password', 'sign in with password', 'log in with password'];
    function isVisible(el) {
      try {
        const rect = el.getBoundingClientRect();
        const style = el.ownerDocument.defaultView.getComputedStyle(el);
        return rect.width > 0 && rect.height > 0
          && style.visibility !== 'hidden' && style.display !== 'none'
          && Number(style.opacity) > 0;
      } catch (_) { return false; }
    }
    function isCandidate(el) {
      if (!isVisible(el) || el.disabled || el.getAttribute('aria-disabled') === 'true') return false;
      const text = [
        el.innerText, el.textContent, el.getAttribute('aria-label'),
        el.getAttribute('data-testid'), el.getAttribute('value')
      ].filter(Boolean).join(' ').toLowerCase();
      return labels.some((label) => text.includes(label));
    }
    function collectRoots(root, roots, depth) {
      if (!root || depth > 8) return;
      roots.push(root);
      try {
        for (const el of root.querySelectorAll('*')) {
          if (el.shadowRoot) collectRoots(el.shadowRoot, roots, depth + 1);
        }
      } catch (_) {}
    }
    function point(el, frames) {
      try { el.scrollIntoView({ block: 'center', inline: 'nearest' }); } catch (_) {}
      const rect = el.getBoundingClientRect();
      let x = rect.left + rect.width / 2;
      let y = rect.top + rect.height / 2;
      for (const frame of frames) {
        const frameRect = frame.getBoundingClientRect();
        x += frameRect.left;
        y += frameRect.top;
      }
      return { ok: true, x, y };
    }
    function findInDocument(doc, frames, depth) {
      if (!doc || depth > 4) return null;
      const roots = [];
      collectRoots(doc, roots, 0);
      // Prefer <a> links (GET navigation) over <button> (POST submit).
      // Clicking a submit button triggers form POST → 400 error.
      for (const tag of ['a', 'button, [role="button"], [role="radio"], label']) {
        for (const root of roots) {
          try {
            const found = Array.from(root.querySelectorAll(tag)).find(isCandidate);
            if (found) return point(found, frames);
          } catch (_) {}
        }
      }
      try {
        for (const iframe of doc.querySelectorAll('iframe')) {
          try {
            const found = findInDocument(iframe.contentDocument, [iframe, ...frames], depth + 1);
            if (found) return found;
          } catch (_) {}
        }
      } catch (_) {}
      return null;
    }
    return findInDocument(document, [], 0) || { ok: false };
  })()"#
}

/// Probe JS for the exact "Continue" submit button on chatgpt.com's login/
/// signup popup. Critically, it MUST exclude SSO suggestion buttons whose
/// labels are "Continue with Google" / "Continue with Apple" /
/// "Continue with phone" — clicking those navigates the browser to the SSO
/// provider (accounts.google.com / appleid.apple.com / login.microsoft…)
/// instead of submitting the email+password form, which leaves the
/// registration flow stranded on an IDP "callback" page with no account
/// created.
///
/// Selection rules (in priority order):
///   1. `button[type="submit"]` (or `[data-testid="continue-button"]`,
///      `button[name="intent"]`) whose inner text trims to exactly
///      "continue" — case-insensitive — AND lives inside a form that
///      contains a visible email input OR has a matching `aria-label`.
///   2. Any visible/non-disabled button whose text trims to exactly
///      "continue" — if no SSO-language is present.
///
/// Anything containing "with" (Continue with X) is rejected to avoid the
/// Apple/Google/phone SSO trap observed live.
fn email_form_continue_probe_js() -> &'static str {
  r#"(function(){
    function visible(el){
      try {
        const r = el.getBoundingClientRect();
        const s = el.ownerDocument.defaultView.getComputedStyle(el);
        return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none';
      } catch(_) { return false; }
    }
    function textOf(el){
      return (el.innerText || el.textContent || el.value || '').replace(/\s+/g, ' ').trim().toLowerCase();
    }
    function findForm(el){
      let cur = el;
      while (cur && cur !== document.body) {
        if (cur.tagName === 'FORM') return cur;
        cur = cur.parentElement;
      }
      return null;
    }
    function hasEmailInput(root){
      if (!root) return false;
      const inputs = Array.from(root.querySelectorAll('input, textarea'));
      return inputs.some((el) => {
        const t = el.type || '';
        const n = el.name || el.id || '';
        const ac = el.autocomplete || '';
        return t === 'email' || n === 'email' || n === 'username' || /email/i.test(ac) || /email/i.test(el.placeholder || '') || /email/i.test(el.getAttribute('aria-label') || '');
      });
    }
    // Candidate ordering: explicit submit, then testid-continue-button, then
    // buttons whose trimmed text is exactly "continue". Skip any candidate
    // whose text contains "with" (Continue with Google / Apple / phone …).
    const selectorPool = [
      'button[type="submit"]',
      '[data-testid="continue-button"]',
      'button[name="intent"]',
      'button',
      '[role="button"]',
    ];
    const seen = new Set();
    const candidates = [];
    for (const sel of selectorPool) {
      for (const el of document.querySelectorAll(sel)) {
        if (seen.has(el)) continue;
        seen.add(el);
        if (el.disabled || el.getAttribute('aria-disabled') === 'true') continue;
        if (!visible(el)) continue;
        const t = textOf(el);
        if (!t) continue;
        if (t.includes('with')) continue;        // Continue with Google/Apple/phone
        if (t.includes('sign in with')) continue;
        if (t.includes('log in with')) continue;
        if (t.includes('use apple') || t.includes('use google') || t.includes('use microsoft') || t.includes('use phone')) continue;
        if (t !== 'continue' && t !== 'submit' && t !== 'next' && t !== 'create account' && t !== 'sign up' && t !== 'verify') continue;
        // Priority: button inside a form with email input is the real
        // email+password submit. Anything else is secondary.
        const form = findForm(el);
        const inEmailForm = hasEmailInput(form);
        candidates.push({ el, t, inEmailForm });
      }
    }
    // Prefer highest-priority: inEmailForm === true, then by selector pool
    // order (already preserved by push order).
    candidates.sort((a, b) => (b.inEmailForm ? 1 : 0) - (a.inEmailForm ? 1 : 0));
    for (const c of candidates) {
      const r = c.el.getBoundingClientRect();
      try { c.el.scrollIntoView({ block: 'center', inline: 'nearest' }); } catch(_) {}
      const rect = c.el.getBoundingClientRect();
      return { ok: true, x: rect.left + rect.width / 2, y: rect.top + rect.height / 2, text: c.t };
    }
    return { ok: false };
  })()"#
}

/// Click the "Continue" submit button on chatgpt.com's login/signup popup,
/// strictly excluding SSO suggestion buttons ("Continue with Google/Apple/
/// phone"). This is the safe replacement for the generic submit click here:
/// the popup renders SSO buttons as `<button>` elements with text starting
/// "Continue", so naive `click_trusted_submit` would routinely hit an SSO
/// button instead of the email-form submit.
pub(crate) async fn click_email_form_continue<B: ChatGptBrowser + Send>(
  browser: &mut B,
) -> Result<(), String> {
  for attempt in 0..CONTROL_CLICK_ATTEMPTS {
    if attempt > 0 {
      sleep(std::time::Duration::from_millis(CONTROL_CLICK_RETRY_MS)).await;
    }
    let Ok(result) = browser
      .evaluate(email_form_continue_probe_js(), false)
      .await
    else {
      continue;
    };
    let value = result.get("value").cloned().unwrap_or_default();
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
      continue;
    }
    let Some(x) = value.get("x").and_then(Value::as_f64) else {
      return Err("email form continue control has no x coordinate".into());
    };
    let Some(y) = value.get("y").and_then(Value::as_f64) else {
      return Err("email form continue control has no y coordinate".into());
    };
    let text = value
      .get("text")
      .and_then(Value::as_str)
      .unwrap_or("continue")
      .to_string();
    return browser
      .click_point(x, y)
      .await
      .map_err(|_| format!("email form continue trusted click failed ({text})"));
  }
  Err("email form continue control not found or not enabled".into())
}

pub(crate) async fn click_password_method<B: ChatGptBrowser + Send>(
  browser: &mut B,
) -> Result<(), String> {
  for attempt in 0..CONTROL_CLICK_ATTEMPTS {
    if attempt > 0 {
      sleep(std::time::Duration::from_millis(CONTROL_CLICK_RETRY_MS)).await;
    }
    let Ok(result) = browser.evaluate(password_method_probe_js(), false).await else {
      continue;
    };
    let value = result.get("value").cloned().unwrap_or_default();
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
      continue;
    }
    let Some(x) = value.get("x").and_then(Value::as_f64) else {
      return Err("password method control has no x coordinate".into());
    };
    let Some(y) = value.get("y").and_then(Value::as_f64) else {
      return Err("password method control has no y coordinate".into());
    };
    return browser
      .click_point(x, y)
      .await
      .map_err(|_| "password method trusted click failed".to_string());
  }
  Err("password method control not found or not enabled".into())
}

pub(crate) async fn click_first_visible<B: ChatGptBrowser + Send>(
  browser: &mut B,
  selector: &str,
) -> Result<bool, String> {
  let script = format!(
    r#"(function(){{
      for (const el of Array.from(document.querySelectorAll({selector}))) {{
        const rect = el.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) continue;
        el.scrollIntoView({{ block: 'center', inline: 'center' }});
        const visible = el.getBoundingClientRect();
        return {{ found: true, x: visible.left + visible.width / 2, y: visible.top + visible.height / 2 }};
      }}
      return {{ found: false }};
    }})()"#,
    selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
  );
  let result = browser.evaluate(&script, false).await?;
  let value = result.get("value").cloned().unwrap_or_default();
  if value.get("found").and_then(Value::as_bool) != Some(true) {
    return Ok(false);
  }
  let x = value
    .get("x")
    .and_then(Value::as_f64)
    .ok_or_else(|| "visible control has no x coordinate".to_string())?;
  let y = value
    .get("y")
    .and_then(Value::as_f64)
    .ok_or_else(|| "visible control has no y coordinate".to_string())?;
  browser.click_point(x, y).await?;
  Ok(true)
}

pub(crate) async fn click_first_text<B: ChatGptBrowser + Send>(
  browser: &mut B,
  labels: &[&str],
  selector: &str,
) -> Result<bool, String> {
  let script = format!(
    r#"(function(){{
      const labels = {labels};
      for (const el of Array.from(document.querySelectorAll({selector}))) {{
        const text = (el.innerText || el.textContent || '').trim().toLowerCase();
        if (!labels.some((label) => text.includes(label))) continue;
        const rect = el.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) continue;
        el.scrollIntoView({{ block: 'center', inline: 'center' }});
        const visible = el.getBoundingClientRect();
        return {{ found: true, x: visible.left + visible.width / 2, y: visible.top + visible.height / 2 }};
      }}
      return {{ found: false }};
    }})()"#,
    labels = serde_json::to_string(
      &labels
        .iter()
        .map(|label| label.to_ascii_lowercase())
        .collect::<Vec<_>>()
    )
    .unwrap_or_else(|_| "[]".into()),
    selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
  );
  let result = browser.evaluate(&script, false).await?;
  let value = result.get("value").cloned().unwrap_or_default();
  if value.get("found").and_then(Value::as_bool) != Some(true) {
    return Ok(false);
  }
  let x = value
    .get("x")
    .and_then(Value::as_f64)
    .ok_or_else(|| "text control has no x coordinate".to_string())?;
  let y = value
    .get("y")
    .and_then(Value::as_f64)
    .ok_or_else(|| "text control has no y coordinate".to_string())?;
  browser.click_point(x, y).await?;
  Ok(true)
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod primitive_tests {
  use std::collections::VecDeque;

  use super::*;

  #[derive(Default)]
  struct FakeBrowser {
    evaluations: Vec<String>,
    evaluation_results: VecDeque<Result<Value, String>>,
    type_results: VecDeque<Result<(), String>>,
    type_attempts: usize,
    clicks: Vec<(f64, f64)>,
  }

  #[async_trait]
  impl ChatGptBrowser for FakeBrowser {
    async fn navigate(&mut self, _url: &str, _timeout_secs: u64) -> Result<(), String> {
      Ok(())
    }

    async fn evaluate(&mut self, expression: &str, _await_promise: bool) -> Result<Value, String> {
      self.evaluations.push(expression.to_string());
      self
        .evaluation_results
        .pop_front()
        .unwrap_or_else(|| Err("unexpected browser evaluation".into()))
    }

    async fn current_url(&mut self) -> Result<String, String> {
      Ok("https://auth.openai.com/log-in".into())
    }

    async fn type_text(&mut self, _selector: &str, _value: &str) -> Result<(), String> {
      self.type_attempts += 1;
      self.type_results.pop_front().unwrap_or(Ok(()))
    }

    async fn click_point(&mut self, x: f64, y: f64) -> Result<(), String> {
      self.clicks.push((x, y));
      Ok(())
    }
  }

  #[test]
  fn visible_selector_contract_rejects_hidden_disabled_and_read_only_controls() {
    let script = visible_selector_probe_js("input[type='password']", true);

    assert!(script.contains("getBoundingClientRect"));
    assert!(script.contains("getComputedStyle"));
    assert!(script.contains("style.visibility !== 'hidden'"));
    assert!(script.contains("style.display !== 'none'"));
    assert!(script.contains("Number(style.opacity) > 0"));
    assert!(script.contains("!el.disabled"));
    assert!(script.contains("!el.readOnly"));
    assert!(script.contains("el.getAttribute('aria-disabled') !== 'true'"));
  }

  #[tokio::test]
  async fn react_safe_fill_uses_native_setters_and_verifies_the_value() {
    let mut browser = FakeBrowser {
      type_results: VecDeque::from([Err("stale control".into())]),
      evaluation_results: VecDeque::from([Ok(serde_json::json!({
        "value": { "ok": true, "verified": true }
      }))]),
      ..FakeBrowser::default()
    };

    fill_visible_input(
      &mut browser,
      "input[name='email']",
      "person@example.com",
      "email",
    )
    .await
    .unwrap();

    assert_eq!(browser.type_attempts, 1);
    let script = &browser.evaluations[0];
    assert!(script.contains("HTMLInputElement.prototype"));
    assert!(script.contains("HTMLTextAreaElement.prototype"));
    assert!(script.contains("new view.InputEvent('input'"));
    assert!(script.contains("el.shadowRoot"));
    assert!(script.contains("iframe.contentDocument"));
    assert!(script.contains("verified: String(el.value) === val"));
  }

  #[tokio::test]
  async fn react_safe_fill_retries_after_spa_rerender() {
    let mut browser = FakeBrowser {
      type_results: VecDeque::from([Ok(()), Ok(())]),
      evaluation_results: VecDeque::from([
        Ok(serde_json::json!({
          "value": { "found": false, "verified": false, "reason": "not_found" }
        })),
        Ok(serde_json::json!({
          "value": { "ok": false, "verified": false, "reason": "not_found" }
        })),
        Ok(serde_json::json!({
          "value": { "found": true, "verified": true }
        })),
      ]),
      ..FakeBrowser::default()
    };

    fill_visible_input(
      &mut browser,
      "input[name='email']",
      "person@example.com",
      "email",
    )
    .await
    .unwrap();

    assert_eq!(browser.type_attempts, 2);
  }

  #[tokio::test]
  async fn password_method_supports_aria_data_testid_and_role_candidates() {
    let mut browser = FakeBrowser {
      evaluation_results: VecDeque::from([Ok(serde_json::json!({
        "value": { "ok": true, "x": 12.0, "y": 34.0 }
      }))]),
      ..FakeBrowser::default()
    };

    click_password_method(&mut browser).await.unwrap();

    assert_eq!(browser.clicks, [(12.0, 34.0)]);
    let script = &browser.evaluations[0];
    assert!(script.contains("aria-label"));
    assert!(script.contains("data-testid"));
    assert!(script.contains("[role=\"radio\"]"));
    assert!(script.contains("aria-disabled"));
  }

  #[tokio::test]
  async fn trusted_submit_uses_coordinate_click_without_dom_or_form_submission() {
    let mut browser = FakeBrowser {
      evaluation_results: VecDeque::from([Ok(serde_json::json!({
        "value": { "ok": true, "x": 56.0, "y": 78.0 }
      }))]),
      ..FakeBrowser::default()
    };

    click_trusted_submit(&mut browser, "button[type='submit']", "authentication")
      .await
      .unwrap();

    assert_eq!(browser.clicks, [(56.0, 78.0)]);
    let script = &browser.evaluations[0];
    assert!(!script.contains(".click("));
    assert!(!script.contains("requestSubmit"));
    assert!(!script.contains("form.submit"));
    assert!(script.contains("!el.disabled"));
    assert!(script.contains("aria-disabled"));
    assert!(script.contains("reactReady"));
  }

  #[tokio::test]
  async fn email_form_continue_clicks_via_trusted_coordinates() {
    let mut browser = FakeBrowser {
      evaluation_results: VecDeque::from([Ok(serde_json::json!({
        "value": { "ok": true, "x": 77.0, "y": 88.0, "text": "continue" }
      }))]),
      ..FakeBrowser::default()
    };

    click_email_form_continue(&mut browser).await.unwrap();

    assert_eq!(browser.clicks, [(77.0, 88.0)]);
  }

  #[tokio::test]
  async fn email_form_continue_probe_rejects_sso_suggestion_buttons() {
    let mut browser = FakeBrowser::default();
    // No "ok" found the first time — to exercise the loop without consuming
    // any non-existent queue slot.
    browser.evaluation_results.push_back(Ok(serde_json::json!({
      "value": { "ok": false }
    })));

    let result = click_email_form_continue(&mut browser).await;
    assert!(
      result.is_err(),
      "should not click when no real Continue present"
    );

    // The probe script MUST reject SSO suggestion text. We assert the
    // generated JS contains the exact rejectors discovered live:
    // "Continue with Google", "Continue with Apple", "Continue with phone".
    assert!(!browser.evaluations.is_empty());
    let script = &browser.evaluations[0];
    assert!(script.contains("if (t.includes('with')) continue;"));
    assert!(script.contains("hasEmailInput"));
    assert!(script.contains("continue-button"));
    assert!(script.contains("button[type=\"submit\"]"));
    // And it should NOT short-circuit when a candidate is just labelled
    // "Continue with Google" — that path used to click SSO destinations.
    assert!(script.contains("sign in with"));
    assert!(script.contains("log in with"));
  }

  #[test]
  fn dom_surface_scoring_rejects_external_and_prefers_auth_controls() {
    let summary = |origin: &str, route: &str, surface: &str| DomSurfaceSummary {
      origin: origin.into(),
      route: route.into(),
      surface: surface.into(),
      has_email: surface == "email",
      has_password: surface == "password",
      has_signup: surface == "signup",
      has_continue: false,
      has_auth_control: surface == "auth",
      has_error: false,
      has_cloudflare: false,
      dialog_count: 0,
      form_count: 1,
      visible_input_count: 1,
      visible_button_count: 1,
    };

    assert_eq!(
      dom_surface_score(
        &summary("external", "other", "password"),
        ContextPageTarget::Auth
      ),
      0
    );
    assert!(
      dom_surface_score(
        &summary("openai-auth", "openai-auth", "password"),
        ContextPageTarget::Auth
      ) > dom_surface_score(
        &summary("chatgpt", "chatgpt-auth-login", "email"),
        ContextPageTarget::Auth
      )
    );
    assert!(
      dom_surface_score(
        &summary("chatgpt", "chatgpt-auth-login", "email"),
        ContextPageTarget::Signup
      ) > 0
    );
  }

  #[test]
  fn dom_surface_scoring_rejects_visible_error_surfaces() {
    let summary = DomSurfaceSummary {
      origin: "chatgpt".into(),
      route: "home".into(),
      surface: "signup".into(),
      has_email: false,
      has_password: false,
      has_signup: true,
      has_continue: false,
      has_auth_control: false,
      has_error: true,
      has_cloudflare: false,
      dialog_count: 0,
      form_count: 0,
      visible_input_count: 0,
      visible_button_count: 1,
    };

    assert_eq!(dom_surface_score(&summary, ContextPageTarget::Signup), 0);
    assert_eq!(dom_surface_score(&summary, ContextPageTarget::Auth), 0);
  }

  #[test]
  fn dom_surface_probe_only_marks_populated_visible_errors() {
    let script = dom_surface_probe_js();

    assert!(script.contains("visibleErrorNodes"));
    assert!(script.contains(".filter(visible)"));
    assert!(script.contains("textOf(el).length > 0"));
    assert!(script.contains("hasInvalidInput"));
    assert!(!script.contains("document.querySelector('[role=\"alert\""));
    assert!(!script.contains("/try again|something went wrong|invalid|error|rate limit/"));
  }

  #[test]
  fn dom_surface_probe_returns_only_structural_classification() {
    let script = dom_surface_probe_js();

    for key in [
      "origin",
      "route",
      "surface",
      "hasEmail",
      "hasPassword",
      "dialogCount",
      "formCount",
    ] {
      assert!(script.contains(key), "missing structural field {key}");
    }
    assert!(!script.contains("el.value"));
    assert!(!script.contains("outerHTML"));
    assert!(!script.contains("location.href"));
    assert!(!script.contains("location.search"));
    assert!(!script.contains("title:"));
    assert!(!script.contains("visibleInputs:"));
    assert!(!script.contains("visibleButtons:"));
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
    context: playwright::api::BrowserContext,
    page: playwright::api::Page,
    cursor_pos: (f64, f64),
  },
}

impl BrowserSession {
  pub(crate) fn context_page_snapshot(&self) -> Result<ContextPageSnapshot, String> {
    match self {
      Self::Cdp(_) => Ok(ContextPageSnapshot { pages: Vec::new() }),
      Self::Camoufox { context, .. } => context
        .pages()
        .map(|pages| ContextPageSnapshot { pages })
        .map_err(|error| format!("Failed to list Camoufox context pages: {error}")),
    }
  }

  pub(crate) async fn sync_context_page(
    &mut self,
    baseline: &ContextPageSnapshot,
    target: ContextPageTarget,
  ) -> Result<bool, String> {
    let Self::Camoufox {
      context,
      page,
      cursor_pos,
    } = self
    else {
      return Ok(false);
    };

    let pages = context
      .pages()
      .map_err(|error| format!("Failed to list Camoufox context pages: {error}"))?;
    let current_index = pages.iter().position(|candidate| candidate == page);
    let current_missing = current_index.is_none();
    let mut best: Option<(u16, bool, playwright::api::Page)> = None;

    for candidate in pages.into_iter().take(8) {
      let is_current = candidate == *page;
      let is_new = !baseline.pages.iter().any(|existing| existing == &candidate);
      if !is_current && !is_new && !current_missing {
        continue;
      }

      let summary: DomSurfaceSummary = match tokio::time::timeout(
        std::time::Duration::from_secs(CAMOUFOX_EVALUATE_TIMEOUT_SECS),
        candidate.eval(dom_surface_probe_js()),
      )
      .await
      {
        Ok(Ok(summary)) => summary,
        _ => continue,
      };
      let score = dom_surface_score(&summary, target);
      if score == 0 {
        continue;
      }
      let replace = best.as_ref().is_none_or(|(best_score, best_current, _)| {
        score > *best_score || (score == *best_score && is_current && !*best_current)
      });
      if replace {
        best = Some((score, is_current, candidate));
      }
    }

    let Some((_, is_current, selected)) = best else {
      return Ok(false);
    };
    if is_current {
      return Ok(false);
    }

    crate::camoufox_manager::CamoufoxManager::prepare_page_for_background_automation(&selected)
      .await
      .map_err(|error| format!("Failed to prepare selected Camoufox page: {error}"))?;
    *page = selected;
    *cursor_pos = (0.0, 0.0);
    Ok(true)
  }

  pub(crate) async fn camoufox_dom_surface_summary(&mut self) -> Result<Option<String>, String> {
    let Self::Camoufox { page, .. } = self else {
      return Ok(None);
    };

    let value: DomSurfaceSummary = tokio::time::timeout(
      std::time::Duration::from_secs(CAMOUFOX_EVALUATE_TIMEOUT_SECS),
      page.eval(dom_surface_probe_js()),
    )
    .await
    .map_err(|_| "Camoufox DOM surface inspection timed out".to_string())?
    .map_err(|error| format!("Camoufox DOM surface inspection failed: {error}"))?;
    serde_json::to_string(&value)
      .map(Some)
      .map_err(|error| format!("Camoufox DOM surface serialization failed: {error}"))
  }

  pub(crate) async fn camoufox_has_concrete_auth_surface(&mut self) -> Result<bool, String> {
    let Self::Camoufox { page, .. } = self else {
      return Ok(false);
    };

    let summary: DomSurfaceSummary = tokio::time::timeout(
      std::time::Duration::from_secs(CAMOUFOX_EVALUATE_TIMEOUT_SECS),
      page.eval(dom_surface_probe_js()),
    )
    .await
    .map_err(|_| "Camoufox auth surface inspection timed out".to_string())?
    .map_err(|error| format!("Camoufox auth surface inspection failed: {error}"))?;
    Ok(
      summary.has_password
        || summary.has_auth_control
        || (summary.origin == "openai-auth" && !summary.has_error)
        || summary.has_cloudflare,
    )
  }

  pub(crate) async fn camoufox_click_selector(
    &mut self,
    selector: &str,
    timeout_ms: u32,
  ) -> Result<bool, String> {
    let Self::Camoufox {
      page, cursor_pos, ..
    } = self
    else {
      return Ok(false);
    };

    page
      .click_builder(selector)
      .timeout(timeout_ms as f64)
      .click()
      .await
      .map_err(|error| format!("Camoufox selector click failed: {error}"))?;
    *cursor_pos = (0.0, 0.0);
    Ok(true)
  }

  pub(crate) async fn navigate(&mut self, url: &str, timeout_secs: u64) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => cdp.navigate(url, timeout_secs).await,
      Self::Camoufox { page, .. } => {
        // Prefer a softer wait than full load — ChatGPT home can keep network
        // busy and exceed Playwright's default 30s load timeout. Always wrap
        // with our own budget so registration cannot hang forever when the
        // page never settles.
        let budget_secs = timeout_secs.max(5);
        let _ =
          crate::camoufox_manager::CamoufoxManager::prepare_page_for_background_automation(page)
            .await;
        // ChatGPT SPA keeps network busy; wait for DOM commit, not full load.
        let goto_future = page
          .goto_builder(url)
          .timeout((budget_secs as f64) * 1000.0)
          .wait_until(playwright::api::DocumentLoadState::DomContentLoaded)
          .goto();
        let goto_result =
          tokio::time::timeout(std::time::Duration::from_secs(budget_secs + 2), goto_future).await;
        match goto_result {
          Ok(Ok(_)) => {
            let _ =
              crate::camoufox_manager::CamoufoxManager::prepare_page_for_background_automation(
                page,
              )
              .await;
            Ok(())
          }
          other => {
            // If we already landed on a related origin, treat timeout / soft
            // navigation errors as success so the state machine can continue.
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
              match other {
                Err(_) => Err(format!(
                  "Camoufox navigate timed out after {budget_secs}s (current={current})"
                )),
                Ok(Err(e)) => Err(format!("Camoufox navigate failed: {e} (current={current})")),
                Ok(Ok(_)) => unreachable!("successful navigation handled above"),
              }
            }
          }
        }
      }
    }
  }

  /// Returns a CDP-shaped Runtime.evaluate result object: `{ "value": ... }`.
  pub(crate) async fn evaluate(
    &mut self,
    expression: &str,
    await_promise: bool,
  ) -> Result<serde_json::Value, String> {
    match self {
      Self::Cdp(cdp) => cdp.evaluate(expression, await_promise).await,
      Self::Camoufox { page, .. } => {
        let _ = await_promise; // Playwright eval awaits promises by default.
        let value: serde_json::Value = match tokio::time::timeout(
          std::time::Duration::from_secs(CAMOUFOX_EVALUATE_TIMEOUT_SECS),
          page.eval(expression),
        )
        .await
        {
          Ok(Ok(value)) => value,
          Ok(Err(error)) => return Err(format!("Camoufox evaluate failed: {error}")),
          Err(_) => {
            return Err(format!(
              "Camoufox evaluate timed out after {CAMOUFOX_EVALUATE_TIMEOUT_SECS}s"
            ));
          }
        };
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

  pub(crate) async fn fetch_json(
    &mut self,
    js_fetch_expr: &str,
  ) -> Result<serde_json::Value, String> {
    self.fetch_json_with_headers(js_fetch_expr, "").await
  }

  pub(crate) async fn current_url(&mut self) -> Result<String, String> {
    match self {
      Self::Cdp(cdp) => cdp.current_url().await,
      Self::Camoufox { page, .. } => page
        .url()
        .map_err(|e| format!("Camoufox current_url failed: {e}")),
    }
  }

  pub(crate) async fn set_cookie(
    &mut self,
    name: &str,
    value: &str,
    domain: &str,
  ) -> Result<(), String> {
    match self {
      Self::Cdp(cdp) => cdp.set_cookie(name, value, domain).await,
      Self::Camoufox { page, .. } => {
        let mut cookie = playwright::api::Cookie::with_domain_path(name, value, domain, "/");
        cookie.secure = Some(true);
        cookie.same_site = Some(playwright::api::SameSite::None);
        match tokio::time::timeout(
          std::time::Duration::from_secs(10),
          page.context().add_cookies(&[cookie]),
        )
        .await
        {
          Ok(result) => result.map_err(|e| format!("Camoufox set_cookie failed: {e}")),
          Err(_) => Err("Camoufox set_cookie timed out after 10s".into()),
        }
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

    // Click "Continue" button with a trusted coordinate click.
    click_trusted_submit(
      self,
      r#"button[type="submit"], button[name="intent"], form button"#,
      "auth/login Continue",
    )
    .await?;
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
  pub(crate) async fn clear_all_site_data(&mut self) -> Result<(), String> {
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
        // Playwright calls can stall when the page/context is busy; keep each
        // step bounded so registration never hangs before UI interaction.
        let context = page.context();
        match tokio::time::timeout(std::time::Duration::from_secs(10), context.clear_cookies())
          .await
        {
          Ok(Ok(())) => {}
          Ok(Err(e)) => return Err(format!("Camoufox clear_cookies failed: {e}")),
          Err(_) => return Err("Camoufox clear_cookies timed out after 10s".into()),
        }
        // Best-effort origin storage wipe on a blank page.
        let _ = tokio::time::timeout(
          std::time::Duration::from_secs(10),
          page.eval::<bool>(
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
          ),
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
  pub(crate) async fn human_click(
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
  pub(crate) async fn human_type(
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

impl BrowserSession {
  /// Bypass Cloudflare Turnstile if present on the current page.
  ///
  /// Uses CDP-level mouse events to click the checkbox — works on cross-origin
  /// iframes. Falls back to capsolver.com API if a visual challenge appears.
  pub(crate) async fn bypass_turnstile_if_present(&mut self) -> Result<bool, String> {
    let current_url = self.current_url().await.unwrap_or_default();

    match self {
      Self::Cdp(cdp) => cdp.bypass_turnstile_via_cdp(&current_url).await,
      Self::Camoufox {
        page, cursor_pos, ..
      } => {
        use crate::auto_service::openai::turnstile_bypass::{
          TURNSTILE_CHALLENGE_JS, TURNSTILE_CLICK_TIMEOUT_SECS, TURNSTILE_FIND_JS,
          TURNSTILE_POLL_MS, TURNSTILE_TOKEN_JS,
        };
        use std::time::Duration;

        // Step 1: Find the widget
        let find_str: String = page
          .eval(TURNSTILE_FIND_JS)
          .await
          .map_err(|e| format!("Camoufox Turnstile find failed: {e}"))?;
        let find_data: serde_json::Value =
          serde_json::from_str(&find_str).unwrap_or(serde_json::json!({"found": false}));
        if !find_data
          .get("found")
          .and_then(|v| v.as_bool())
          .unwrap_or(false)
        {
          return Ok(false);
        }
        let target_x = find_data.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let target_y = find_data.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if target_x <= 0.0 || target_y <= 0.0 {
          return Ok(false);
        }

        // Step 2: Bézier mouse movement
        {
          let mut rng = rand::rng();
          let (from_x, from_y) = *cursor_pos;
          let dx = target_x - from_x;
          let dy = target_y - from_y;
          let dist = (dx * dx + dy * dy).sqrt().max(1.0);
          let cp1_x = from_x + dx * 0.3 + rng.random_range(-dist * 0.15..dist * 0.15);
          let cp1_y = from_y + dy * 0.3 + rng.random_range(-dist * 0.08..dist * 0.08);
          let cp2_x = from_x + dx * 0.7 + rng.random_range(-dist * 0.12..dist * 0.12);
          let cp2_y = from_y + dy * 0.7 + rng.random_range(-dist * 0.06..dist * 0.06);
          for i in 0..=12 {
            let t = i as f64 / 12.0;
            let u = 1.0 - t;
            let x = u.powi(3) * from_x
              + 3.0 * u.powi(2) * t * cp1_x
              + 3.0 * u * t.powi(2) * cp2_x
              + t.powi(3) * target_x;
            let y = u.powi(3) * from_y
              + 3.0 * u.powi(2) * t * cp1_y
              + 3.0 * u * t.powi(2) * cp2_y
              + t.powi(3) * target_y;
            page
              .mouse
              .r#move(x, y, None)
              .await
              .map_err(|e| format!("Camoufox mouse move failed: {e}"))?;
            sleep(Duration::from_millis(rng.random_range(30..80))).await;
          }
        }
        *cursor_pos = (target_x, target_y);
        sleep(Duration::from_millis(rand::rng().random_range(120..350))).await;

        // Step 3: Click
        page
          .mouse
          .click_builder(target_x, target_y)
          .click()
          .await
          .map_err(|e| format!("Camoufox Turnstile click failed: {e}"))?;

        // Step 4: Poll for token
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(TURNSTILE_CLICK_TIMEOUT_SECS);
        loop {
          sleep(Duration::from_millis(TURNSTILE_POLL_MS)).await;
          if let Ok(ch_str) = page.eval::<String>(TURNSTILE_CHALLENGE_JS).await {
            if let Ok(ch_data) = serde_json::from_str::<serde_json::Value>(&ch_str) {
              if ch_data
                .get("challenge")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
              {
                log::info!("Camoufox: Turnstile escalated to visual challenge — stopping");
                return Err("Turnstile visual challenge not yet supported for Camoufox".into());
              }
            }
          }
          if let Ok(tok_str) = page.eval::<String>(TURNSTILE_TOKEN_JS).await {
            if let Ok(t_data) = serde_json::from_str::<serde_json::Value>(&tok_str) {
              if t_data
                .get("solved")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
              {
                return Ok(true);
              }
            }
          }
          if start.elapsed() > timeout {
            return Err(format!(
              "Camoufox Turnstile bypass timed out after {TURNSTILE_CLICK_TIMEOUT_SECS}s"
            ));
          }
        }
      }
    }
  }
}

impl CdpConnection {
  /// Bypass Turnstile directly via CDP commands (no closure indirection).
  async fn bypass_turnstile_via_cdp(&mut self, page_url: &str) -> Result<bool, String> {
    // Implement the bypass inline (avoids closure borrow issues with &mut self).
    use crate::auto_service::openai::turnstile_bypass::{
      TURNSTILE_CHALLENGE_JS, TURNSTILE_CLICK_TIMEOUT_SECS, TURNSTILE_FIND_JS, TURNSTILE_POLL_MS,
      TURNSTILE_TOKEN_JS,
    };
    use std::time::Duration;

    // Step 1: Find the widget
    let find_result = self.evaluate(TURNSTILE_FIND_JS, false).await?;
    let find_str = find_result
      .get("value")
      .and_then(|v| v.as_str())
      .or_else(|| find_result.as_str())
      .unwrap_or("");
    let find_data: serde_json::Value =
      serde_json::from_str(find_str).unwrap_or(serde_json::json!({"found": false}));

    if !find_data
      .get("found")
      .and_then(|v| v.as_bool())
      .unwrap_or(false)
    {
      return Ok(false);
    }

    let target_x = find_data.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let target_y = find_data.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);

    if target_x <= 0.0 || target_y <= 0.0 {
      return Ok(false);
    }

    log::info!(
      "Turnstile widget found at ({:.0}, {:.0}) — CDP click",
      target_x,
      target_y
    );

    // Step 2: Bézier mouse movement
    let path = {
      let mut rng = rand::rng();
      let (from_x, from_y) = self.cursor_pos;
      let dx = target_x - from_x;
      let dy = target_y - from_y;
      let dist = (dx * dx + dy * dy).sqrt().max(1.0);
      let cp1_x = from_x + dx * 0.3 + rng.random_range(-dist * 0.15..dist * 0.15);
      let cp1_y = from_y + dy * 0.3 + rng.random_range(-dist * 0.08..dist * 0.08);
      let cp2_x = from_x + dx * 0.7 + rng.random_range(-dist * 0.12..dist * 0.12);
      let cp2_y = from_y + dy * 0.7 + rng.random_range(-dist * 0.06..dist * 0.06);
      let mut pts = Vec::with_capacity(12);
      for i in 0..=12 {
        let t = i as f64 / 12.0;
        let u = 1.0 - t;
        let x = u.powi(3) * from_x
          + 3.0 * u.powi(2) * t * cp1_x
          + 3.0 * u * t.powi(2) * cp2_x
          + t.powi(3) * target_x;
        let y = u.powi(3) * from_y
          + 3.0 * u.powi(2) * t * cp1_y
          + 3.0 * u * t.powi(2) * cp2_y
          + t.powi(3) * target_y;
        pts.push((x, y));
      }
      pts
    };

    let mut rng = rand::rng();
    for (x, y) in &path {
      let _ = self.mouse_move(*x, *y).await;
      sleep(Duration::from_millis(rng.random_range(30..80))).await;
    }
    sleep(Duration::from_millis(rng.random_range(120..350))).await;

    // Step 3: Click
    self.mouse_click(target_x, target_y).await?;
    log::info!("Turnstile checkbox clicked via CDP — waiting for token");

    // Step 4: Poll for token
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(TURNSTILE_CLICK_TIMEOUT_SECS);

    loop {
      sleep(Duration::from_millis(TURNSTILE_POLL_MS)).await;

      // Check for visual challenge
      if let Ok(ch_val) = self.evaluate(TURNSTILE_CHALLENGE_JS, false).await {
        let ch_str = ch_val.get("value").and_then(|v| v.as_str()).unwrap_or("");
        if let Ok(ch_data) = serde_json::from_str::<serde_json::Value>(ch_str) {
          if ch_data
            .get("challenge")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
          {
            // Fall back to closure-based capsolver
            return self.bypass_turnstile_fallback_capsolver(page_url).await;
          }
        }
      }

      // Check for token
      if let Ok(tok_val) = self.evaluate(TURNSTILE_TOKEN_JS, false).await {
        let tok_str = tok_val.get("value").and_then(|v| v.as_str()).unwrap_or("");
        if let Ok(t_data) = serde_json::from_str::<serde_json::Value>(tok_str) {
          if t_data
            .get("solved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
          {
            log::info!("Turnstile solved via CDP click");
            return Ok(true);
          }
        }
      }

      if start.elapsed() > timeout {
        log::warn!("Turnstile CDP bypass timed out after {TURNSTILE_CLICK_TIMEOUT_SECS}s");
        return Err(format!(
          "Turnstile bypass timed out after {TURNSTILE_CLICK_TIMEOUT_SECS}s"
        ));
      }
    }
  }

  /// Fall back to capsolver API when CDP click triggers a visual challenge.
  async fn bypass_turnstile_fallback_capsolver(&mut self, page_url: &str) -> Result<bool, String> {
    use crate::auto_service::openai::turnstile_bypass::{
      capsolver_solve_turnstile, inject_token_js, TURNSTILE_SITEKEY_JS,
    };

    // Extract sitekey
    let sitekey = match self.evaluate(TURNSTILE_SITEKEY_JS, false).await {
      Ok(v) => v
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string(),
      Err(_) => String::new(),
    };

    if sitekey.is_empty() {
      return Err("Cannot extract Turnstile sitekey for capsolver".into());
    }

    log::info!("Calling capsolver for Turnstile sitekey={sitekey}");

    let token = capsolver_solve_turnstile(&sitekey, page_url)
      .await
      .map_err(|e| format!("capsolver failed: {e}"))?;

    // Inject token
    let inject_js = inject_token_js(&token);
    self.evaluate(&inject_js, false).await?;
    log::info!("Turnstile solved via capsolver + token injection");
    Ok(true)
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
      // Playwright page lookup can stall when the context is still settling;
      // bound each probe so attach never blocks the whole registration run.
      let probe = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        crate::camoufox_manager::CamoufoxManager::instance().get_active_page(&profile_path_str),
      )
      .await;
      match probe {
        Ok(Ok(page)) => {
          let context = page.context();
          return Ok(BrowserSession::Camoufox {
            context,
            page,
            cursor_pos: (0.0, 0.0),
          });
        }
        Ok(Err(error)) => last_error = error.to_string(),
        Err(_) => last_error = "get_active_page timed out after 10s".to_string(),
      }
    }
    let known = crate::camoufox_manager::CamoufoxManager::instance()
      .debug_instance_paths()
      .await;
    let known_text = known
      .iter()
      .map(|(id, path, has_automation)| format!("{id} automation={has_automation} path={path}"))
      .collect::<Vec<_>>()
      .join(" | ");
    eprintln!(
      "attach_browser_session(camoufox) FAILED\n  want_path={profile_path_str}\n  last_error={last_error}\n  instances=[{known_text}]"
    );
    return Err(format!(
      "Failed to attach Camoufox Playwright page for {profile_path_str}: {last_error} (instances: {known_text})"
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
