//! Cloudflare Turnstile auto-bypass engine.
//!
//! Approach (inspired by CloakBrowser):
//! 1. Find the Turnstile iframe bounding rect via `evaluate`.
//! 2. Move the mouse to the checkbox centre using a human-like Bézier curve.
//! 3. Click — CDP-level `Input.dispatchMouseEvent` works on cross-origin iframes.
//! 4. Poll for the `cf-turnstile-response` token to appear.
//! 5. If a visual challenge appears → fall back to capsolver.com API.
//!
//! This allows fully hands-free registration for users who cannot use a mouse.

use serde_json::Value;
use std::time::Duration;
use tokio::time::sleep;

// ── Timing constants (human-like) ──

/// Max seconds to wait for the Turnstile checkbox click to resolve into a token.
pub(crate) const TURNSTILE_CLICK_TIMEOUT_SECS: u64 = 15;

/// Poll interval while waiting for the token input to be filled.
pub(crate) const TURNSTILE_POLL_MS: u64 = 400;

/// Capsolver API base URL.
const CAPSOLVER_CREATE_TASK_URL: &str = "https://api.capsolver.com/createTask";
const CAPSOLVER_GET_TASK_URL: &str = "https://api.capsolver.com/getTaskResult";

// ── Turnstile detection JS snippets ──

/// Returns JS that finds the Turnstile widget iframe and returns its
/// bounding client rect plus the page-relative coordinates of the checkbox.
pub(crate) const TURNSTILE_FIND_JS: &str = r#"(function(){
  // Try the standard Turnstile iframe first
  const iframe = document.querySelector('iframe[src*="challenges.cloudflare.com/cdn-cgi/challenge-platform"]');
  if (iframe) {
    const r = iframe.getBoundingClientRect();
    // The checkbox is roughly centred in the iframe
    const cx = r.left + r.width / 2;
    const cy = r.top + r.height / 2;
    return JSON.stringify({
      found: true,
      source: 'iframe',
      x: cx,
      y: cy,
      width: r.width,
      height: r.height,
      top: r.top,
      left: r.left,
      viewportWidth: window.innerWidth,
      viewportHeight: window.innerHeight,
    });
  }
  // Fallback: look for the cf-turnstile wrapper div
  const wrapper = document.querySelector('.cf-turnstile, #cf-turnstile');
  if (wrapper) {
    const r = wrapper.getBoundingClientRect();
    return JSON.stringify({
      found: true,
      source: 'wrapper',
      x: r.left + r.width / 2,
      y: r.top + r.height / 2,
      width: r.width,
      height: r.height,
      top: r.top,
      left: r.left,
      viewportWidth: window.innerWidth,
      viewportHeight: window.innerHeight,
    });
  }
  return JSON.stringify({ found: false });
})()"#;

/// Returns JS that checks whether the Turnstile token has been generated.
pub(crate) const TURNSTILE_TOKEN_JS: &str = r#"(function(){
  const input = document.querySelector('input[name="cf-turnstile-response"], [name="cf-turnstile-response"]');
  if (input && input.value && input.value.length > 10) {
    return JSON.stringify({ solved: true, token: input.value.substring(0, 20) + '...' });
  }
  // Also check for Cloudflare clearance cookies / redirect
  const cfCleared = document.cookie.indexOf('cf_clearance') >= 0;
  return JSON.stringify({ solved: cfCleared, token: cfCleared ? 'cf_clearance cookie present' : null });
})()"#;

/// Returns JS that checks if a visual challenge (image grid) is showing.
pub(crate) const TURNSTILE_CHALLENGE_JS: &str = r#"(function(){
  // Visual challenge typically shows an iframe with /cdn-cgi/challenge-platform/h/g/
  const challengeFrame = document.querySelector('iframe[src*="challenge-platform/h/"], iframe[src*="challenge-platform/g/"]');
  if (challengeFrame) {
    return JSON.stringify({ challenge: true, width: challengeFrame.getBoundingClientRect().width });
  }
  // Alternative: check for challenge-specific body text
  const bodyText = document.body.innerText || '';
  const hasChallenge = /select all images|select all squares|click verify/i.test(bodyText);
  return JSON.stringify({ challenge: hasChallenge });
})()"#;

// ── Capsolver API integration ──

/// Submit a Turnstile challenge to capsolver.com for solving.
/// Requires a `CAPSOLVER_API_KEY` environment variable.
pub async fn capsolver_solve_turnstile(sitekey: &str, page_url: &str) -> Result<String, String> {
  let api_key = std::env::var("CAPSOLVER_API_KEY")
    .map_err(|_| "CAPSOLVER_API_KEY environment variable not set".to_string())?;

  let client = reqwest::Client::new();

  // Step 1: Create task
  let create_body = serde_json::json!({
      "clientKey": api_key,
      "task": {
          "type": "TurnstileTaskProxyless",
          "websiteURL": page_url,
          "websiteKey": sitekey,
      }
  });

  let create_resp: Value = client
    .post(CAPSOLVER_CREATE_TASK_URL)
    .json(&create_body)
    .send()
    .await
    .map_err(|e| format!("capsolver createTask request failed: {e}"))?
    .json()
    .await
    .map_err(|e| format!("capsolver createTask parse failed: {e}"))?;

  let task_id = create_resp
    .get("taskId")
    .and_then(|v| v.as_u64())
    .ok_or_else(|| {
      format!(
        "capsolver createTask returned no taskId: {}",
        serde_json::to_string(&create_resp).unwrap_or_default()
      )
    })?;

  log::info!("capsolver task created: {task_id}");

  // Step 2: Poll for result (up to 60 seconds)
  for attempt in 1..=40 {
    sleep(Duration::from_millis(1500)).await;

    let get_body = serde_json::json!({
        "clientKey": api_key,
        "taskId": task_id,
    });

    let get_resp: Value = client
      .post(CAPSOLVER_GET_TASK_URL)
      .json(&get_body)
      .send()
      .await
      .map_err(|e| format!("capsolver getTaskResult request failed: {e}"))?
      .json()
      .await
      .map_err(|e| format!("capsolver getTaskResult parse failed: {e}"))?;

    let status = get_resp
      .get("status")
      .and_then(|v| v.as_str())
      .unwrap_or("unknown");

    match status {
      "ready" => {
        let token = get_resp
          .get("solution")
          .and_then(|v| v.get("token"))
          .and_then(|v| v.as_str())
          .ok_or("capsolver returned ready but no token in solution")?;
        log::info!("capsolver solved Turnstile after {attempt} polls");
        return Ok(token.to_string());
      }
      "processing" => {
        // Continue polling
      }
      other => {
        return Err(format!(
          "capsolver task {task_id} ended with status: {other}"
        ));
      }
    }
  }

  Err(format!(
    "capsolver task {task_id} timed out after 60 seconds"
  ))
}

/// Inject a Turnstile token into the page.
/// Call this after capsolver returns a token, or when you have a token from
/// any other source.
pub fn inject_token_js(token: &str) -> String {
  let escaped = token.replace('\\', "\\\\").replace('\'', "\\'");
  format!(
    r#"(function(){{
  const input = document.querySelector('input[name="cf-turnstile-response"], [name="cf-turnstile-response"]');
  if (input) {{
    // Use native setter so React/Angular frameworks detect the change
    const nativeSetter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value');
    if (nativeSetter && nativeSetter.set) {{
      nativeSetter.set.call(input, '{escaped}');
    }} else {{
      input.value = '{escaped}';
    }}
    input.dispatchEvent(new Event('input', {{ bubbles: true }}));
    input.dispatchEvent(new Event('change', {{ bubbles: true }}));
    return JSON.stringify({{ injected: true, tokenLength: {len} }});
  }}
  return JSON.stringify({{ injected: false }});
}})()"#,
    escaped = escaped,
    len = token.len()
  )
}

/// Extract the Turnstile sitekey from the current page.
pub(crate) const TURNSTILE_SITEKEY_JS: &str = r#"(function(){
  // Look for the Turnstile widget div or script that contains the sitekey
  const div = document.querySelector('.cf-turnstile, #cf-turnstile');
  if (div && div.getAttribute('data-sitekey')) {
    return div.getAttribute('data-sitekey');
  }
  // Try to find it in a script tag
  const scripts = document.querySelectorAll('script');
  for (const s of scripts) {
    const m = (s.textContent || '').match(/sitekey['"]?\s*:\s*['"]([^'"]+)['"]/);
    if (m) return m[1];
  }
  return null;
})()"#;
