//! Per-account browser check engine.
//! Reuses BrowserAuthAdapter from the proven auto-login flow.

#![allow(dead_code)]

use crate::auto_service::openai::browser::{
  click_trusted_submit, fill_visible_input, BrowserSession,
};
use crate::auto_service::openai::chatgpt_auth::{
  BrowserAuthAdapter, ExistingAccountAuthAdapter,
  PASSWORD_INPUT_SELECTOR, SUBMIT_SELECTOR,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};

use super::classify::{classify_auth_state, AuthSignals};
use super::store;
use super::types::{AccountCheckResult, CheckProgress, CheckStep, ReasonCode};

pub(crate) const PROGRESS_EVENT: &str = "openai-account-check-progress";

fn emit_progress(
  app_handle: &AppHandle, task_id: &str, account_key: &str,
  index: usize, total: usize, step: CheckStep, terminal: bool,
) {
  let _ = app_handle.emit(PROGRESS_EVENT, CheckProgress {
    task_id: task_id.into(), account_key: account_key.into(),
    credential_index: index, total_credentials: total,
    step, outcome: None, reason_code: None, terminal,
  });
}

fn emit_terminal(
  app_handle: &AppHandle, task_id: &str, account_key: &str,
  index: usize, total: usize,
  outcome: super::types::AccountOutcome, reason_code: ReasonCode,
) {
  let _ = app_handle.emit(PROGRESS_EVENT, CheckProgress {
    task_id: task_id.into(), account_key: account_key.into(),
    credential_index: index, total_credentials: total,
    step: CheckStep::Completed, outcome: Some(outcome),
    reason_code: Some(reason_code), terminal: true,
  });
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn check_one(
  app_handle: &AppHandle, task_id: &str,
  email: &str, password: &str, totp_secret: &str,
  index: usize, total: usize,
  cancel_flag: &Arc<AtomicBool>,
  session: &mut BrowserSession,
  device_id: &str,
) -> Option<AccountCheckResult> {
  let account_key = email.trim().to_lowercase();

  if cancel_flag.load(Ordering::SeqCst) {
    emit_terminal(app_handle, task_id, &account_key, index, total,
      super::types::AccountOutcome::Unresolved, ReasonCode::Cancelled);
    return None;
  }

  emit_progress(app_handle, task_id, &account_key, index, total, CheckStep::Launching, false);

  // Use adapter for email (proven flow)
  let mut adapter = BrowserAuthAdapter::new(session, device_id);

  // Step 1: Submit email
  emit_progress(app_handle, task_id, &account_key, index, total, CheckStep::EnteringEmail, false);

  if let Err(e) = adapter.submit_email(email).await {
    eprintln!("[engine] submit_email FAILED {}: {:?}", email, e);
    return Some(browser_error(email, password, totp_secret));
  }

  // Adapter borrow ends here; session released for direct password fill
  // (BrowserAuthAdapter doesn't implement Drop, NLL handles the borrow)

  // Wait for page to transition - Cloudflare/challenges may take time
  tokio::time::sleep(std::time::Duration::from_millis(5000)).await;

  if cancel_flag.load(Ordering::SeqCst) {
    emit_terminal(app_handle, task_id, &account_key, index, total,
      super::types::AccountOutcome::Unresolved, ReasonCode::Cancelled);
    return None;
  }

  // Step 2: Fill password directly (adapter's submit_password has interfering lock check)
  emit_progress(app_handle, task_id, &account_key, index, total, CheckStep::SubmittingPassword, false);

  let password_ok = match fill_visible_input(session, PASSWORD_INPUT_SELECTOR, password, "password").await {
    Ok(()) => {
      let _ = click_trusted_submit(session, SUBMIT_SELECTOR, "authentication").await;
      tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
      true
    }
    Err(e) => {
      eprintln!("[engine] fill password FAILED {}: {}", email, e);
      // Page may have redirected - retry once after wait
      tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
      match fill_visible_input(session, PASSWORD_INPUT_SELECTOR, password, "password").await {
        Ok(()) => {
          let _ = click_trusted_submit(session, SUBMIT_SELECTOR, "authentication").await;
          tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
          true
        }
        Err(_) => false
      }
    }
  };
  let _ = password_ok;

  if cancel_flag.load(Ordering::SeqCst) {
    emit_terminal(app_handle, task_id, &account_key, index, total,
      super::types::AccountOutcome::Unresolved, ReasonCode::Cancelled);
    return None;
  }

  // Step 3: Observe page state via adapter
  emit_progress(app_handle, task_id, &account_key, index, total, CheckStep::Observing, false);

  let mut adapter = BrowserAuthAdapter::new(session, device_id);
  let signals = match adapter.observe().await {
    Ok(s) => s,
    Err(e) => {
      eprintln!("[engine] observe FAILED {}: {:?}", email, e);
      return Some(browser_error(email, password, totp_secret));
    }
  };

  // Step 4: Classify using account checker's strict rules
  emit_progress(app_handle, task_id, &account_key, index, total, CheckStep::Classifying, false);

  let checker_signals = AuthSignals {
    body_text: signals.body_text,
    has_visible_verification_input: signals.has_authenticator_totp_input || signals.has_email_otp_input,
  };
  let (outcome, reason_code) = classify_auth_state(&checker_signals);

  let result = AccountCheckResult {
    email: email.into(), password: password.into(),
    totp_secret: totp_secret.into(),
    outcome, reason_code,
    created_at: chrono::Utc::now(),
  };

  let _ = store::insert_result(&result);
  emit_terminal(app_handle, task_id, &account_key, index, total, outcome, reason_code);
  Some(result)
}

fn browser_error(email: &str, password: &str, totp_secret: &str) -> AccountCheckResult {
  AccountCheckResult {
    email: email.into(), password: password.into(),
    totp_secret: totp_secret.into(),
    outcome: super::types::AccountOutcome::Unresolved,
    reason_code: ReasonCode::BrowserError,
    created_at: chrono::Utc::now(),
  }
}
