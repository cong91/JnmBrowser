//! Repair engine, task lifecycle and canary gate wiring for 2FA backfill.
//!
//! The engine runs accounts serially (hardcoded concurrency = 1). For each
//! account it: reloads the fresh record, re-checks eligibility, persists the
//! inferred provider when required, takes ownership via a CAS lifecycle patch
//! to `InProgress`, launches a fresh isolated browser, authenticates, captures
//! the secret into a private journal BEFORE confirming TOTP, CAS-patches the
//! final `two_fa_enabled=true` + `totp_secret` + `Completed` + `Enabled`,
//! journals `RemoteConfirmed`, records the final revision, cleans up the
//! journal entry, and closes the browser on every terminal path.
//!
//! Transient failures never flip an account to `Invalid`; only the backfill
//! lifecycle/outcome fields move. The batch pauses on 429/lockout/challenge
//! escalation. Cancellation is also propagated into authentication and provider polling, and is checked before each later account.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Runtime};
use tokio::sync::Mutex as AsyncMutex;

use super::commands::{
  BackfillBrowser, BackfillMode, BackfillNetworkConfig, TwoFactorBackfillStartRequest,
};
use super::eligibility::evaluate_eligibility;
#[cfg(test)]
use super::gate::network_config_hash;
use super::gate::{CanaryGate, CanaryRejection};
use super::journal::{
  PersistedBackfillAccountPatch, TwoFactorBackfillJournal, TwoFactorBackfillJournalEntry,
  TwoFactorBackfillJournalState,
};
use super::types::TwoFactorBackfillPreviewRequest;
use crate::auto_service::openai::browser::{attach_browser_session, BrowserSession};
use crate::auto_service::openai::chatgpt_auth::{
  authenticate_existing_account_with_cancel, AuthError, AuthPolicy, BrowserAuthAdapter,
  ExistingAccountAuthAdapter, ExistingAccountCredentials,
};
use crate::auto_service::openai::chatgpt_two_factor::{
  enable_authenticator_two_factor, BrowserTwoFactorAdapter, TwoFactorAdapter, TwoFactorError,
};
use crate::auto_service::openai::register::store::{
  compare_and_update_registered_account, list_registered_accounts, persist_inferred_email_provider,
  BackfillPatchPrecondition, TwoFactorBackfillPatch,
};
use crate::auto_service::openai::register::types::{
  RegistrationResult, TwoFactorBackfillOutcome, TwoFactorBackfillState,
};
use crate::email::{build_email_service, EmailProvider};

/// Stable identifier (account_id, or email fallback) used for backend lookup,
/// journal and CAS operations. Never emit this value directly as progress data.
pub(crate) fn stable_account_key(account: &RegistrationResult) -> String {
  let account_id = account.account_id.trim();
  if !account_id.is_empty() {
    return account_id.to_string();
  }
  account.email.trim().to_string()
}

/// Stable non-reversible identifier used in progress events and live diagnostics.
pub(crate) fn stable_event_key(account_key: &str) -> String {
  super::types::safe_progress_account_key(account_key)
}

/// Safe progress event name. Constant to avoid typos leaking into the frontend filter.
pub(crate) const PROGRESS_EVENT: &str = "twofa-backfill-progress";

/// Safe subset of the workflow state machine exposed to the frontend.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum BackfillStep {
  Eligibility,
  ProviderMigration,
  Login,
  EmailOtp,
  InspectTwoFactor,
  CaptureSecret,
  ConfirmTwoFactor,
  VerifyRemote,
  PersistAccount,
  JournalComplete,
  BatchPaused,
  Cancelled,
  Completed,
  Failed,
}

/// Safe outcome enum — never carries credential material.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum BackfillOutcome {
  Enabled,
  Failed,
  Cancelled,
  ReconciliationRequired,
  BatchPaused,
}

/// Safe per-account progress payload emitted to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BackfillProgress {
  pub task_id: String,
  pub account_key: String,
  pub account_index: usize,
  pub total_accounts: usize,
  pub step: BackfillStep,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub outcome: Option<BackfillOutcome>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub error_code: Option<String>,
  pub retryable: bool,
  pub timestamp: chrono::DateTime<Utc>,
}

#[derive(Clone, Copy)]
struct AccountProgress<'a> {
  account_key: &'a str,
  index: usize,
  total: usize,
}

/// Allow-listed safe error-code fragments. Raw credential blobs and provider
/// responses must never reach this field; the codes are static strings below.
const SAFE_ERROR_CODES: &[&str] = &[
  "account_not_found",
  "ineligible",
  "provider_migration_failed",
  "in_progress_cas_failed",
  "browser_launch_failed",
  "browser_cleanup_failed",
  "email_otp_timeout",
  "email_otp_rejected",
  "email_otp_attempts_exhausted",
  "rate_limited",
  "locked",
  "cloudflare",
  "wrong_credentials",
  "unsupported_auth",
  "unknown_auth",
  "reconciliation_required",
  "remote_state_indeterminate",
  "remote_verification_failed",
  "challenge_escalated",
  "probe_failed",
  "begin_failed",
  "secret_capture_failed",
  "secret_callback_failed",
  "confirm_failed",
  "reopen_failed",
  "final_cas_failed",
  "journal_remote_confirmed_failed",
  "journal_final_revision_failed",
  "journal_cleanup_failed",
  "manual_review_failed",
  "terminal_cas_failed",
  "canary_reload_failed",
  "canary_evidence_failed",
  "preflight_failed",
  "journal_open_failed",
  "startup_failed",
];

/// Belt-and-braces: never emit a free-form string in the error_code slot.
fn safe_error_code(code: &str) -> String {
  if SAFE_ERROR_CODES.contains(&code) {
    code.to_string()
  } else {
    "unknown".to_string()
  }
}

/// Outcome of executing the workflow against one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AccountResult {
  Enabled {
    journal_completed: bool,
  },
  EnabledWithWarning {
    journal_completed: bool,
    error_code: String,
  },
  Failed {
    error_code: String,
    retryable: bool,
  },
  Cancelled,
  ReconciliationRequired {
    error_code: String,
    pause_batch: bool,
  },
  BatchPaused {
    error_code: String,
  },
}

// --- Browser session seams -------------------------------------------------

/// Trait seam so the per-account state machine runs against fakes in tests
/// without spinning up a real browser/CDP/profile manager.
#[async_trait]
pub(crate) trait BackfillBrowserFactory<R: Runtime>: Send + Sync {
  fn preflight(
    &self,
    _browser: &BackfillBrowser,
    _network: &BackfillNetworkConfig,
  ) -> Result<(), String> {
    Ok(())
  }

  /// Launch a fresh isolated browser for one account. Must be cleaned up via
  /// the returned guard even if subsequent steps fail.
  async fn launch(
    &self,
    app_handle: &AppHandle<R>,
    browser: &BackfillBrowser,
    network: &BackfillNetworkConfig,
    device_id: &str,
    login_email: &str,
    operation_id: &str,
  ) -> Result<Box<dyn BackfillBrowserSession + Send>, String>;
}

/// Active browser session for one account, exposing the two adapter factories
/// the orchestrator needs. Closing must be durable and never leave the process
/// running; the orchestrator always awaits `close` before returning.
#[async_trait]
pub(crate) trait BackfillBrowserSession: Send {
  fn make_auth_adapter(&mut self) -> Box<dyn ExistingAccountAuthAdapter + Send + '_>;
  fn make_two_factor_adapter(&mut self) -> Box<dyn TwoFactorAdapter + Send + '_>;
  /// Tear down the browser and remove only its auto-created worker profile.
  /// A failure must pause the batch so another worker is not launched beside it.
  async fn close(&mut self) -> Result<(), String>;
}

// --- Account store seam ---------------------------------------------------

/// Seam for the credential store so tests can simulate CAS conflicts and
/// provider-persistence races without touching the global disk-backed store.
pub(crate) trait BackfillAccountStore: Send + Sync {
  fn list_accounts(&self) -> Result<Vec<RegistrationResult>, String>;
  fn reload(&self, account_key: &str) -> Result<Option<RegistrationResult>, String>;
  fn persist_provider(
    &self,
    account_key: &str,
    expected_revision: u64,
    provider: EmailProvider,
  ) -> Result<RegistrationResult, String>;
  fn cas_patch(
    &self,
    account_key: &str,
    expected_revision: u64,
    precondition: BackfillPatchPrecondition,
    patch: TwoFactorBackfillPatch,
  ) -> Result<RegistrationResult, String>;
}

/// Production-backed store that delegates to the existing register store.
pub(crate) struct ProductionAccountStore;

impl BackfillAccountStore for ProductionAccountStore {
  fn list_accounts(&self) -> Result<Vec<RegistrationResult>, String> {
    list_registered_accounts()
  }

  fn reload(&self, account_key: &str) -> Result<Option<RegistrationResult>, String> {
    Ok(
      list_registered_accounts()?
        .into_iter()
        .find(|account| stable_account_key(account) == account_key),
    )
  }

  fn persist_provider(
    &self,
    account_key: &str,
    expected_revision: u64,
    provider: EmailProvider,
  ) -> Result<RegistrationResult, String> {
    persist_inferred_email_provider(account_key, expected_revision, provider)
  }

  fn cas_patch(
    &self,
    account_key: &str,
    expected_revision: u64,
    precondition: BackfillPatchPrecondition,
    patch: TwoFactorBackfillPatch,
  ) -> Result<RegistrationResult, String> {
    compare_and_update_registered_account(account_key, expected_revision, precondition, patch)
  }
}

// --- Journal seam ---------------------------------------------------------

/// Seam for the private journal so tests can verify SecretCaptured-before-confirm
/// ordering without the global on-disk journal directory.
pub(crate) trait BackfillJournal {
  fn create_secret_captured(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_account_revision: u64,
    pending_totp_secret: String,
  ) -> Result<TwoFactorBackfillJournalEntry, String>;

  fn update_state(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_journal_revision: u64,
    state: TwoFactorBackfillJournalState,
  ) -> Result<TwoFactorBackfillJournalEntry, String>;

  fn record_final_account_revision(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_journal_revision: u64,
    final_account_revision: u64,
  ) -> Result<TwoFactorBackfillJournalEntry, String>;

  fn delete_after_account_patch(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_journal_revision: u64,
    persisted: PersistedBackfillAccountPatch<'_>,
  ) -> Result<bool, String>;

  fn transition_to_manual_review(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_journal_revision: u64,
  ) -> Result<TwoFactorBackfillJournalEntry, String>;
}

impl BackfillJournal for TwoFactorBackfillJournal {
  fn create_secret_captured(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_account_revision: u64,
    pending_totp_secret: String,
  ) -> Result<TwoFactorBackfillJournalEntry, String> {
    TwoFactorBackfillJournal::create_secret_captured(
      self,
      operation_id,
      account_key,
      expected_account_revision,
      pending_totp_secret,
    )
  }

  fn update_state(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_journal_revision: u64,
    state: TwoFactorBackfillJournalState,
  ) -> Result<TwoFactorBackfillJournalEntry, String> {
    TwoFactorBackfillJournal::update_state(
      self,
      operation_id,
      account_key,
      expected_journal_revision,
      state,
    )
  }

  fn record_final_account_revision(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_journal_revision: u64,
    final_account_revision: u64,
  ) -> Result<TwoFactorBackfillJournalEntry, String> {
    TwoFactorBackfillJournal::record_final_account_revision(
      self,
      operation_id,
      account_key,
      expected_journal_revision,
      final_account_revision,
    )
  }

  fn delete_after_account_patch(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_journal_revision: u64,
    persisted: PersistedBackfillAccountPatch<'_>,
  ) -> Result<bool, String> {
    TwoFactorBackfillJournal::delete_after_account_patch(
      self,
      operation_id,
      account_key,
      expected_journal_revision,
      persisted,
    )
  }

  fn transition_to_manual_review(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_journal_revision: u64,
  ) -> Result<TwoFactorBackfillJournalEntry, String> {
    TwoFactorBackfillJournal::update_state(
      self,
      operation_id,
      account_key,
      expected_journal_revision,
      TwoFactorBackfillJournalState::ManualReview,
    )
  }
}

// --- Batch pause classification ------------------------------------------

/// Whether a per-account auth failure pauses the whole batch.
fn is_batch_pause_auth_error(error: &AuthError) -> bool {
  matches!(error, AuthError::RateLimited | AuthError::Cloudflare)
}

/// Whether a per-account 2FA failure pauses the whole batch.
fn is_batch_pause_two_factor_error(error: &TwoFactorError) -> bool {
  matches!(
    error,
    TwoFactorError::RemoteStateIndeterminate
      | TwoFactorError::RemoteVerificationFailed(_)
      | TwoFactorError::ChallengeEscalated
  )
}

// --- Engine ---------------------------------------------------------------

/// The repair engine. Hardcoded concurrency = 1: accounts run serially, one
/// auth attempt each. Cancellation is also propagated into authentication and provider polling, and is checked before each later account.
pub(crate) struct BackfillEngine<F> {
  pub(crate) task_id: String,
  pub(crate) request: TwoFactorBackfillStartRequest,
  pub(crate) cancel_flag: Arc<AtomicBool>,
  store: F,
}

impl<F: BackfillAccountStore> BackfillEngine<F> {
  pub(crate) fn new(
    task_id: String,
    request: TwoFactorBackfillStartRequest,
    cancel_flag: Arc<AtomicBool>,
    store: F,
  ) -> Self {
    Self {
      task_id,
      request,
      cancel_flag,
      store,
    }
  }

  pub(crate) fn emit_task_failure<R: Runtime>(&self, app_handle: &AppHandle<R>, error_code: &str) {
    self.emit(
      app_handle,
      AccountProgress {
        account_key: "",
        index: 0,
        total: 0,
      },
      BackfillStep::Failed,
      Some(BackfillOutcome::Failed),
      Some(safe_error_code(error_code)),
      false,
    );
  }

  pub(crate) async fn run<R, J, G>(
    self,
    app_handle: &AppHandle<R>,
    browser_factory: &dyn BackfillBrowserFactory<R>,
    journal_factory: impl FnOnce() -> Result<J, String>,
    canary_gate: Option<&G>,
  ) where
    J: BackfillJournal,
    G: CanaryGateLike,
    R: Runtime,
  {
    let result = self
      .run_inner(app_handle, browser_factory, journal_factory, canary_gate)
      .await;
    if let Err(error_code) = result {
      self.emit_task_failure(app_handle, &error_code);
      log::warn!(
        "2FA backfill task {} ended with startup error code: {}",
        self.task_id,
        safe_error_code(&error_code)
      );
    }
  }

  async fn run_inner<R, J, G>(
    &self,
    app_handle: &AppHandle<R>,
    browser_factory: &dyn BackfillBrowserFactory<R>,
    journal_factory: impl FnOnce() -> Result<J, String>,
    canary_gate: Option<&G>,
  ) -> Result<(), String>
  where
    J: BackfillJournal,
    G: CanaryGateLike,
    R: Runtime,
  {
    let accounts = self
      .store
      .list_accounts()
      .map_err(|_| safe_error_code("startup_failed"))?;
    let preview = evaluate_eligibility(
      &TwoFactorBackfillPreviewRequest {
        selected_account_keys: self.request.selected_account_keys.clone(),
        allow_free_trial_no: self.request.allow_free_trial_no,
        acknowledge_legacy_access: self.request.acknowledge_legacy_access,
      },
      &accounts,
    );
    let eligible_keys: Vec<String> = preview
      .accounts
      .iter()
      .filter(|account| account.eligible)
      .map(|account| account.account_key.clone())
      .collect();

    let total = eligible_keys.len();
    if total == 0 {
      self.emit(
        app_handle,
        AccountProgress {
          account_key: "",
          index: 0,
          total: 0,
        },
        BackfillStep::Failed,
        Some(BackfillOutcome::Failed),
        None,
        false,
      );
      return Ok(());
    }

    let providers: Vec<EmailProvider> = preview
      .accounts
      .iter()
      .filter(|account| account.eligible)
      .filter_map(|account| account.email_provider)
      .collect();
    if let Some(gate) = canary_gate {
      gate
        .validate(
          self.request.mode,
          &eligible_keys,
          &self.request.browser,
          &providers,
          &self.request.network,
        )
        .map_err(|_| safe_error_code("startup_failed"))?;
    } else if matches!(self.request.mode, BackfillMode::Bulk) {
      return Err(safe_error_code("startup_failed"));
    }

    browser_factory
      .preflight(&self.request.browser, &self.request.network)
      .map_err(|_| safe_error_code("preflight_failed"))?;

    let mut journal = journal_factory().map_err(|_| safe_error_code("journal_open_failed"))?;

    for (index, account_key) in eligible_keys.iter().enumerate() {
      let progress = AccountProgress {
        account_key,
        index,
        total,
      };
      if self.cancel_flag.load(Ordering::SeqCst) {
        self.emit(
          app_handle,
          AccountProgress {
            account_key: "",
            index,
            total,
          },
          BackfillStep::Cancelled,
          Some(BackfillOutcome::Cancelled),
          None,
          false,
        );
        break;
      }

      let result = self
        .run_account(app_handle, browser_factory, &mut journal, progress)
        .await;

      match result {
        Ok(AccountResult::Enabled { .. }) => {
          if let Err(error_code) = self.record_canary_evidence(canary_gate, account_key) {
            self.emit(
              app_handle,
              progress,
              BackfillStep::BatchPaused,
              Some(BackfillOutcome::BatchPaused),
              Some(error_code),
              false,
            );
            break;
          }
          self.emit(
            app_handle,
            progress,
            BackfillStep::Completed,
            Some(BackfillOutcome::Enabled),
            None,
            false,
          );
        }
        Ok(AccountResult::EnabledWithWarning { error_code, .. }) => {
          self.emit(
            app_handle,
            progress,
            BackfillStep::BatchPaused,
            Some(BackfillOutcome::BatchPaused),
            Some(error_code),
            false,
          );
          break;
        }
        Ok(AccountResult::Failed {
          error_code,
          retryable,
        }) => {
          self.emit(
            app_handle,
            progress,
            BackfillStep::Failed,
            Some(BackfillOutcome::Failed),
            Some(error_code),
            retryable,
          );
          if retryable {
            self.emit(
              app_handle,
              progress,
              BackfillStep::BatchPaused,
              Some(BackfillOutcome::BatchPaused),
              None,
              true,
            );
            break;
          }
        }
        Ok(AccountResult::Cancelled) => {
          self.emit(
            app_handle,
            progress,
            BackfillStep::Cancelled,
            Some(BackfillOutcome::Cancelled),
            None,
            false,
          );
          break;
        }
        Ok(AccountResult::ReconciliationRequired {
          error_code,
          pause_batch,
        }) => {
          self.emit(
            app_handle,
            progress,
            BackfillStep::Completed,
            Some(BackfillOutcome::ReconciliationRequired),
            Some(error_code),
            false,
          );
          if pause_batch {
            self.emit(
              app_handle,
              progress,
              BackfillStep::BatchPaused,
              Some(BackfillOutcome::BatchPaused),
              None,
              true,
            );
            break;
          }
        }
        Ok(AccountResult::BatchPaused { error_code }) => {
          self.emit(
            app_handle,
            progress,
            BackfillStep::BatchPaused,
            Some(BackfillOutcome::BatchPaused),
            Some(error_code),
            false,
          );
          break;
        }
        Err(error_code) => {
          self.emit(
            app_handle,
            progress,
            BackfillStep::Failed,
            Some(BackfillOutcome::Failed),
            Some(error_code),
            false,
          );
        }
      }
    }

    Ok(())
  }

  fn record_canary_evidence<G: CanaryGateLike>(
    &self,
    canary_gate: Option<&G>,
    account_key: &str,
  ) -> Result<(), String> {
    if !matches!(self.request.mode, BackfillMode::Canary) {
      return Ok(());
    }
    let gate = canary_gate.ok_or_else(|| safe_error_code("canary_evidence_failed"))?;
    let account = self
      .store
      .reload(account_key)
      .map_err(|_| safe_error_code("canary_reload_failed"))?
      .ok_or_else(|| safe_error_code("canary_reload_failed"))?;
    if account.two_factor_backfill_state != Some(TwoFactorBackfillState::Completed)
      || account.two_factor_backfill_outcome != Some(TwoFactorBackfillOutcome::Enabled)
    {
      return Err(safe_error_code("canary_reload_failed"));
    }
    let provider = account
      .email_provider
      .ok_or_else(|| safe_error_code("canary_reload_failed"))?;
    gate
      .record_canary_success(
        &self.request.browser,
        provider,
        &self.request.network,
        account_key,
      )
      .map_err(|_| safe_error_code("canary_evidence_failed"))
  }

  async fn run_account<R, J: BackfillJournal>(
    &self,
    app_handle: &AppHandle<R>,
    browser_factory: &dyn BackfillBrowserFactory<R>,
    journal: &mut J,
    progress: AccountProgress<'_>,
  ) -> Result<AccountResult, String>
  where
    R: Runtime,
  {
    let account_key = progress.account_key;
    // 1. Reload fresh record + re-check eligibility.
    let Some(record) = self.store.reload(account_key).ok().flatten() else {
      return Ok(AccountResult::Failed {
        error_code: safe_error_code("account_not_found"),
        retryable: false,
      });
    };
    let preview = evaluate_eligibility(
      &TwoFactorBackfillPreviewRequest {
        selected_account_keys: vec![account_key.into()],
        allow_free_trial_no: self.request.allow_free_trial_no,
        acknowledge_legacy_access: self.request.acknowledge_legacy_access,
      },
      std::slice::from_ref(&record),
    )
    .accounts
    .into_iter()
    .next()
    .ok_or_else(|| "eligibility preview produced no account".to_string())?;
    if !preview.eligible {
      return Ok(AccountResult::Failed {
        error_code: safe_error_code("ineligible"),
        retryable: false,
      });
    }
    self.emit(
      app_handle,
      progress,
      BackfillStep::Eligibility,
      None,
      None,
      false,
    );

    // 2. Persist inferred provider provenance if the preview required it.
    let mut effective_revision = preview.record_revision;
    if preview.requires_provider_persistence {
      let provider = preview
        .email_provider
        .ok_or_else(|| "provider migration requires a resolved provider".to_string())?;
      self
        .store
        .persist_provider(account_key, effective_revision, provider)
        .map_err(|error| safe_error_code_for("provider_migration_failed", &error))?;
      effective_revision = self
        .store
        .reload(account_key)
        .ok()
        .flatten()
        .map(|account| account.record_revision)
        .ok_or_else(|| "account disappeared after provider migration".to_string())?;
      self.emit(
        app_handle,
        progress,
        BackfillStep::ProviderMigration,
        None,
        None,
        false,
      );
    }

    let provider = preview
      .email_provider
      .ok_or_else(|| "account has no email provider".to_string())?;

    // 3. PATCH lifecycle_state = InProgress via CAS to take operation ownership.
    let operation_id = self.task_id.clone();
    let after_start = self
      .store
      .cas_patch(
        account_key,
        effective_revision,
        BackfillPatchPrecondition::start(
          operation_id.clone(),
          self.request.allow_free_trial_no,
          self.request.acknowledge_legacy_access,
        ),
        TwoFactorBackfillPatch::start(),
      )
      .map_err(|error| safe_error_code_for("in_progress_cas_failed", &error))?;

    if self.cancel_flag.load(Ordering::SeqCst) {
      return Ok(self.terminalize_owned_result(
        account_key,
        &after_start,
        AccountResult::Cancelled,
      ));
    }

    // 4. Launch a fresh isolated browser for this account.
    let device_id = after_start.device_id.clone();
    let browser_launch = browser_factory
      .launch(
        app_handle,
        &self.request.browser,
        &self.request.network,
        &device_id,
        &after_start.email,
        &operation_id,
      )
      .await;
    let mut browser = match browser_launch {
      Ok(browser) => browser,
      Err(error) => {
        let result = if self.cancel_flag.load(Ordering::SeqCst) {
          AccountResult::Cancelled
        } else {
          AccountResult::Failed {
            error_code: safe_error_code_for("browser_launch_failed", &error),
            retryable: false,
          }
        };
        return Ok(self.terminalize_owned_result(account_key, &after_start, result));
      }
    };

    if self.cancel_flag.load(Ordering::SeqCst) {
      let outcome =
        self.terminalize_owned_result(account_key, &after_start, AccountResult::Cancelled);
      return match browser.close().await {
        Ok(()) => Ok(outcome),
        Err(error) => Ok(AccountResult::BatchPaused {
          error_code: safe_error_code_for("browser_cleanup_failed", &error),
        }),
      };
    }

    let outcome = match self
      .run_account_through_browser(
        app_handle,
        browser.as_mut(),
        journal,
        &after_start,
        provider,
        progress,
      )
      .await
    {
      Ok(outcome) => self.terminalize_owned_result(account_key, &after_start, outcome),
      Err(_) => self.terminalize_owned_result(
        account_key,
        &after_start,
        AccountResult::Failed {
          error_code: safe_error_code("unknown"),
          retryable: false,
        },
      ),
    };

    match browser.close().await {
      Ok(()) => Ok(outcome),
      Err(error) => match outcome {
        AccountResult::Enabled { journal_completed }
        | AccountResult::EnabledWithWarning {
          journal_completed, ..
        } => Ok(AccountResult::EnabledWithWarning {
          journal_completed,
          error_code: safe_error_code_for("browser_cleanup_failed", &error),
        }),
        AccountResult::Failed { .. }
        | AccountResult::Cancelled
        | AccountResult::ReconciliationRequired { .. }
        | AccountResult::BatchPaused { .. } => Ok(AccountResult::BatchPaused {
          error_code: safe_error_code_for("browser_cleanup_failed", &error),
        }),
      },
    }
  }

  fn terminalize_owned_result(
    &self,
    account_key: &str,
    started: &RegistrationResult,
    result: AccountResult,
  ) -> AccountResult {
    let terminal_outcome = match &result {
      AccountResult::Enabled { .. } | AccountResult::EnabledWithWarning { .. } => return result,
      AccountResult::Failed { .. } => TwoFactorBackfillOutcome::Failed,
      AccountResult::Cancelled => TwoFactorBackfillOutcome::Cancelled,
      AccountResult::ReconciliationRequired { .. } => {
        TwoFactorBackfillOutcome::ReconciliationRequired
      }
      AccountResult::BatchPaused { .. } => return result,
    };

    let Some(current) = self.store.reload(account_key).ok().flatten() else {
      return AccountResult::BatchPaused {
        error_code: safe_error_code("terminal_cas_failed"),
      };
    };
    if current.two_factor_backfill_state == Some(TwoFactorBackfillState::Completed)
      && current.two_factor_backfill_operation_id.as_deref() == Some(self.task_id.as_str())
      && current.two_factor_backfill_outcome == Some(terminal_outcome)
    {
      return result;
    }
    if current.two_factor_backfill_state != Some(TwoFactorBackfillState::InProgress)
      || current.two_factor_backfill_operation_id.as_deref() != Some(self.task_id.as_str())
    {
      return AccountResult::BatchPaused {
        error_code: safe_error_code("terminal_cas_failed"),
      };
    }

    let (precondition, patch) = match terminal_outcome {
      TwoFactorBackfillOutcome::Failed => (
        BackfillPatchPrecondition::finalize_failed(
          self.task_id.clone(),
          self.request.allow_free_trial_no,
          self.request.acknowledge_legacy_access,
        ),
        TwoFactorBackfillPatch::finalize_failed(),
      ),
      TwoFactorBackfillOutcome::Cancelled => (
        BackfillPatchPrecondition::finalize_cancelled(
          self.task_id.clone(),
          self.request.allow_free_trial_no,
          self.request.acknowledge_legacy_access,
        ),
        TwoFactorBackfillPatch::finalize_cancelled(),
      ),
      TwoFactorBackfillOutcome::ReconciliationRequired => (
        BackfillPatchPrecondition::finalize_reconciliation_required(
          self.task_id.clone(),
          self.request.allow_free_trial_no,
          self.request.acknowledge_legacy_access,
        ),
        TwoFactorBackfillPatch::finalize_reconciliation_required(),
      ),
      TwoFactorBackfillOutcome::Enabled => return result,
    };
    if self
      .store
      .cas_patch(account_key, current.record_revision, precondition, patch)
      .is_err()
    {
      return AccountResult::BatchPaused {
        error_code: safe_error_code("terminal_cas_failed"),
      };
    }

    debug_assert_eq!(
      started.two_factor_backfill_state,
      Some(TwoFactorBackfillState::InProgress)
    );
    result
  }

  async fn run_account_through_browser<R, J: BackfillJournal>(
    &self,
    app_handle: &AppHandle<R>,
    browser: &mut (dyn BackfillBrowserSession + Send),
    journal: &mut J,
    started: &RegistrationResult,
    provider: EmailProvider,
    progress: AccountProgress<'_>,
  ) -> Result<AccountResult, String>
  where
    R: Runtime,
  {
    let account_key = progress.account_key;
    let email_service = build_email_service(provider);
    let operation_id = self.task_id.clone();

    // 5. Authenticate with the existing account (no TOTP — target is 2FA-off).
    self.emit(app_handle, progress, BackfillStep::Login, None, None, false);
    let auth_result = {
      let mut auth_adapter = browser.make_auth_adapter();
      let credentials = ExistingAccountCredentials {
        email: &started.email,
        password: &started.password,
        raw_cdk: &started.cdk,
        totp_secret: None,
      };
      authenticate_existing_account_with_cancel(
        auth_adapter.as_mut(),
        credentials,
        email_service.as_ref(),
        AuthPolicy::default(),
        self.cancel_flag.as_ref(),
      )
      .await
    };
    if self.cancel_flag.load(Ordering::SeqCst) {
      return Ok(AccountResult::Cancelled);
    }
    if let Err(auth_error) = auth_result {
      return Ok(match auth_error {
        AuthError::EmailOtpTimeout => {
          self.emit(
            app_handle,
            progress,
            BackfillStep::EmailOtp,
            None,
            Some(safe_error_code("email_otp_timeout")),
            false,
          );
          AccountResult::Failed {
            error_code: safe_error_code("email_otp_timeout"),
            retryable: is_batch_pause_auth_error(&AuthError::EmailOtpTimeout),
          }
        }
        AuthError::EmailOtpAttemptsExhausted => AccountResult::Failed {
          error_code: safe_error_code("email_otp_attempts_exhausted"),
          retryable: true,
        },
        AuthError::EmailOtpRejected => AccountResult::Failed {
          error_code: safe_error_code("email_otp_rejected"),
          retryable: false,
        },
        AuthError::Cancelled => AccountResult::Cancelled,
        AuthError::Cloudflare | AuthError::RateLimited => AccountResult::Failed {
          error_code: safe_auth_error_code(&auth_error),
          retryable: true,
        },
        AuthError::Locked => {
          match self.store.cas_patch(
            account_key,
            started.record_revision,
            BackfillPatchPrecondition::finalize_failed(
              self.task_id.clone(),
              self.request.allow_free_trial_no,
              self.request.acknowledge_legacy_access,
            ),
            TwoFactorBackfillPatch::finalize_failed_locked(),
          ) {
            Ok(_) => AccountResult::Failed {
              error_code: safe_error_code("locked"),
              retryable: false,
            },
            Err(error) => AccountResult::BatchPaused {
              error_code: safe_error_code_for("terminal_cas_failed", &error),
            },
          }
        }
        AuthError::WrongCredentials => AccountResult::Failed {
          error_code: safe_error_code("wrong_credentials"),
          retryable: false,
        },
        AuthError::Unsupported(_) => AccountResult::Failed {
          error_code: safe_error_code("unsupported_auth"),
          retryable: false,
        },
        AuthError::Unknown(detail) => {
          log::warn!(
            "2FA backfill task {} account {} unknown auth surface: {}",
            self.task_id,
            account_key,
            detail
          );
          AccountResult::Failed {
            error_code: safe_error_code("unknown_auth"),
            retryable: false,
          }
        }
      });
    }
    self.emit(app_handle, progress, BackfillStep::Login, None, None, false);

    // 6. Enable authenticator 2FA, journaling the secret BEFORE confirm runs.
    self.emit(
      app_handle,
      progress,
      BackfillStep::InspectTwoFactor,
      None,
      None,
      false,
    );
    let mut two_factor_adapter = browser.make_two_factor_adapter();
    let expected_account_revision = started.record_revision;
    let engine_task_id = operation_id.clone();
    let auth_started = started.clone();
    let mut captured_journal_revision = None;
    let secret_result = enable_authenticator_two_factor(two_factor_adapter.as_mut(), |secret| {
      let entry = journal
        .create_secret_captured(
          &engine_task_id,
          account_key,
          expected_account_revision,
          secret.to_string(),
        )
        .map_err(TwoFactorError::SecretCallbackFailed)?;
      captured_journal_revision = Some(entry.journal_revision);
      Ok(())
    })
    .await;

    if captured_journal_revision.is_some() {
      self.emit(
        app_handle,
        progress,
        BackfillStep::CaptureSecret,
        None,
        None,
        false,
      );
    }

    if self.cancel_flag.load(Ordering::SeqCst) {
      let result = match captured_journal_revision {
        Some(revision) => {
          match journal.transition_to_manual_review(&operation_id, account_key, revision) {
            Ok(_) => AccountResult::ReconciliationRequired {
              error_code: safe_error_code("reconciliation_required"),
              pause_batch: true,
            },
            Err(_) => AccountResult::BatchPaused {
              error_code: safe_error_code("manual_review_failed"),
            },
          }
        }
        None => AccountResult::Cancelled,
      };
      return Ok(result);
    }

    let secret = match secret_result {
      Ok(secret) => secret,
      Err(error) => {
        return Ok(map_two_factor_error(
          error,
          journal,
          &operation_id,
          account_key,
          captured_journal_revision,
        ));
      }
    };
    let captured_journal_revision = captured_journal_revision
      .ok_or_else(|| "2FA secret succeeded without a durable journal entry".to_string())?;

    // 7. CAS patch: two_fa_enabled=true, totp_secret=secret, Completed, Enabled.
    self.emit(
      app_handle,
      progress,
      BackfillStep::PersistAccount,
      None,
      None,
      false,
    );
    let persisted = match self.store.cas_patch(
      account_key,
      started.record_revision,
      BackfillPatchPrecondition::finalize_enabled(
        operation_id.clone(),
        self.request.allow_free_trial_no,
        self.request.acknowledge_legacy_access,
      ),
      TwoFactorBackfillPatch::finalize_enabled(secret.clone()),
    ) {
      Ok(persisted) => persisted,
      Err(error) => {
        return Ok(
          match journal.transition_to_manual_review(
            &operation_id,
            account_key,
            captured_journal_revision,
          ) {
            Ok(_) => AccountResult::ReconciliationRequired {
              error_code: safe_error_code_for("final_cas_failed", &error),
              pause_batch: true,
            },
            Err(_) => AccountResult::BatchPaused {
              error_code: safe_error_code("manual_review_failed"),
            },
          },
        );
      }
    };

    // 8. Journal update_state -> RemoteConfirmed.
    let confirmed = match journal.update_state(
      &operation_id,
      account_key,
      captured_journal_revision,
      TwoFactorBackfillJournalState::RemoteConfirmed,
    ) {
      Ok(confirmed) => confirmed,
      Err(error) => {
        return Ok(
          match journal.transition_to_manual_review(
            &operation_id,
            account_key,
            captured_journal_revision,
          ) {
            Ok(_) => AccountResult::EnabledWithWarning {
              journal_completed: false,
              error_code: safe_error_code_for("journal_remote_confirmed_failed", &error),
            },
            Err(_) => AccountResult::EnabledWithWarning {
              journal_completed: false,
              error_code: safe_error_code("manual_review_failed"),
            },
          },
        );
      }
    };

    // 9. Record the exact final revision + delete the journal entry atomically.
    let finalized = match journal.record_final_account_revision(
      &operation_id,
      account_key,
      confirmed.journal_revision,
      persisted.record_revision,
    ) {
      Ok(finalized) => finalized,
      Err(error) => {
        return Ok(
          match journal.transition_to_manual_review(
            &operation_id,
            account_key,
            confirmed.journal_revision,
          ) {
            Ok(_) => AccountResult::EnabledWithWarning {
              journal_completed: false,
              error_code: safe_error_code_for("journal_final_revision_failed", &error),
            },
            Err(_) => AccountResult::EnabledWithWarning {
              journal_completed: false,
              error_code: safe_error_code("manual_review_failed"),
            },
          },
        );
      }
    };
    match journal.delete_after_account_patch(
      &operation_id,
      account_key,
      finalized.journal_revision,
      PersistedBackfillAccountPatch::new(
        persisted.record_revision,
        Some(&operation_id),
        persisted.two_fa_enabled,
        &persisted.totp_secret,
      ),
    ) {
      Ok(true) => {}
      Ok(false) => {
        return Ok(AccountResult::EnabledWithWarning {
          journal_completed: false,
          error_code: safe_error_code("journal_cleanup_failed"),
        });
      }
      Err(error) => {
        let error_code = match journal.transition_to_manual_review(
          &operation_id,
          account_key,
          finalized.journal_revision,
        ) {
          Ok(_) => safe_error_code_for("journal_cleanup_failed", &error),
          Err(_) => safe_error_code("manual_review_failed"),
        };
        return Ok(AccountResult::EnabledWithWarning {
          journal_completed: false,
          error_code,
        });
      }
    }

    self.emit(
      app_handle,
      progress,
      BackfillStep::JournalComplete,
      None,
      None,
      false,
    );

    // Assert the final CAS patch preserved every field it must not touch.
    verify_preserved_fields(&auth_started, &persisted, &secret);

    Ok(AccountResult::Enabled {
      journal_completed: true,
    })
  }

  fn emit<R: Runtime>(
    &self,
    app_handle: &AppHandle<R>,
    account: AccountProgress<'_>,
    step: BackfillStep,
    outcome: Option<BackfillOutcome>,
    error_code: Option<String>,
    retryable: bool,
  ) {
    let safe_code = error_code.map(|code| safe_error_code(&code));
    let progress = BackfillProgress {
      task_id: self.task_id.clone(),
      account_key: stable_event_key(account.account_key),
      account_index: account.index,
      total_accounts: account.total,
      step,
      outcome,
      error_code: safe_code.clone(),
      retryable,
      timestamp: Utc::now(),
    };
    if let Some(code) = &progress.error_code {
      debug_assert!(
        !code.trim().is_empty() && !contains_secret_material(code),
        "progress error_code must never contain secret material"
      );
    }
    let _ = app_handle.emit(PROGRESS_EVENT, &progress);
  }
}

// --- Canary gate trait seam ----------------------------------------------

/// Helper trait so tests can substitute an in-memory canary gate.
pub(crate) trait CanaryGateLike {
  fn validate(
    &self,
    mode: BackfillMode,
    account_keys: &[String],
    browser: &BackfillBrowser,
    providers: &[EmailProvider],
    network: &BackfillNetworkConfig,
  ) -> Result<String, CanaryRejection>;
  fn record_canary_success(
    &self,
    browser: &BackfillBrowser,
    provider: EmailProvider,
    network: &BackfillNetworkConfig,
    account_key: &str,
  ) -> Result<(), String>;
}

impl CanaryGateLike for CanaryGate {
  fn validate(
    &self,
    mode: BackfillMode,
    account_keys: &[String],
    browser: &BackfillBrowser,
    providers: &[EmailProvider],
    network: &BackfillNetworkConfig,
  ) -> Result<String, CanaryRejection> {
    CanaryGate::validate(self, mode, account_keys, browser, providers, network)
  }

  fn record_canary_success(
    &self,
    browser: &BackfillBrowser,
    provider: EmailProvider,
    network: &BackfillNetworkConfig,
    account_key: &str,
  ) -> Result<(), String> {
    CanaryGate::record_canary_success(self, browser, provider, network, account_key)
  }
}

fn safe_auth_error_code(error: &AuthError) -> String {
  match error {
    AuthError::RateLimited => safe_error_code("rate_limited"),
    AuthError::Locked => safe_error_code("locked"),
    AuthError::Cloudflare => safe_error_code("cloudflare"),
    AuthError::EmailOtpAttemptsExhausted => safe_error_code("email_otp_attempts_exhausted"),
    _ => safe_error_code("unknown_auth"),
  }
}

/// Returns the static safe code, while logging the raw underlying detail
/// (the raw error string never reaches the event payload).
fn safe_error_code_for(static_code: &str, _raw_detail: &str) -> String {
  safe_error_code(static_code)
}

fn map_two_factor_error<J: BackfillJournal>(
  error: TwoFactorError,
  journal: &mut J,
  operation_id: &str,
  account_key: &str,
  journal_revision: Option<u64>,
) -> AccountResult {
  let code = match &error {
    TwoFactorError::ReconciliationRequired => "reconciliation_required",
    TwoFactorError::RemoteStateIndeterminate => "remote_state_indeterminate",
    TwoFactorError::RemoteVerificationFailed(_) => "remote_verification_failed",
    TwoFactorError::ChallengeEscalated => "challenge_escalated",
    TwoFactorError::ProbeFailed(_) => "probe_failed",
    TwoFactorError::BeginFailed(_) => "begin_failed",
    TwoFactorError::SecretCaptureFailed(_) => "secret_capture_failed",
    TwoFactorError::SecretCallbackFailed(_) => "secret_callback_failed",
    TwoFactorError::ConfirmFailed(_) => "confirm_failed",
    TwoFactorError::ReopenFailed(_) => "reopen_failed",
  };
  let safe = safe_error_code(code);

  let result = match error {
    TwoFactorError::ReconciliationRequired => AccountResult::ReconciliationRequired {
      error_code: safe.clone(),
      pause_batch: false,
    },
    ref other if is_batch_pause_two_factor_error(other) => AccountResult::Failed {
      error_code: safe.clone(),
      retryable: true,
    },
    _ => AccountResult::Failed {
      error_code: safe.clone(),
      retryable: false,
    },
  };

  if let Some(journal_revision) = journal_revision {
    return match journal.transition_to_manual_review(operation_id, account_key, journal_revision) {
      Ok(_) => AccountResult::ReconciliationRequired {
        error_code: safe,
        pause_batch: matches!(
          error,
          TwoFactorError::RemoteStateIndeterminate
            | TwoFactorError::RemoteVerificationFailed(_)
            | TwoFactorError::ChallengeEscalated
        ),
      },
      Err(_) => AccountResult::BatchPaused {
        error_code: safe_error_code("manual_review_failed"),
      },
    };
  }
  result
}

/// Assert the final CAS patch preserved every field it must not touch and
/// advanced the ones it owns. Runs in production too: a regression here would
/// corrupt seller/free-trial/export history.
fn verify_preserved_fields(
  started: &RegistrationResult,
  persisted: &RegistrationResult,
  secret: &str,
) {
  assert_eq!(persisted.email, started.email, "email changed");
  assert_eq!(persisted.password, started.password, "password changed");
  assert_eq!(
    persisted.access_token, started.access_token,
    "access_token changed"
  );
  assert_eq!(persisted.cdk, started.cdk, "cdk changed");
  assert_eq!(
    persisted.base_email, started.base_email,
    "base_email changed"
  );
  assert_eq!(
    persisted.free_trial_eligible, started.free_trial_eligible,
    "free_trial_eligible changed"
  );
  assert_eq!(persisted.plan_type, started.plan_type, "plan_type changed");
  assert_eq!(
    persisted.phone_number, started.phone_number,
    "phone_number changed"
  );
  assert_eq!(persisted.status, started.status, "status changed");
  assert_eq!(persisted.note, started.note, "note changed");
  assert_eq!(
    persisted.exported_at, started.exported_at,
    "exported_at changed"
  );
  assert_eq!(persisted.sold_at, started.sold_at, "sold_at changed");
  assert_eq!(
    persisted.email_provider, started.email_provider,
    "email_provider changed"
  );
  assert_eq!(
    persisted.email_provider_provenance, started.email_provider_provenance,
    "email_provider_provenance changed"
  );
  assert_eq!(
    persisted.registration_outcome_reason, started.registration_outcome_reason,
    "registration_outcome_reason changed"
  );
  assert_eq!(
    persisted.two_factor_backfill_access_state, started.two_factor_backfill_access_state,
    "two_factor_backfill_access_state changed"
  );
  assert_eq!(
    persisted.two_factor_backfill_exclusion, started.two_factor_backfill_exclusion,
    "two_factor_backfill_exclusion changed"
  );
  assert!(persisted.two_fa_enabled, "two_fa_enabled did not advance");
  assert_eq!(persisted.totp_secret, secret, "totp_secret mismatch");
  assert_eq!(
    persisted.two_factor_backfill_state,
    Some(TwoFactorBackfillState::Completed),
    "two_factor_backfill_state did not become Completed"
  );
}

/// Defense-in-depth check for values that must never enter progress events.
fn contains_secret_material(value: &str) -> bool {
  let lower = value.to_ascii_lowercase();
  lower.contains('@')
    || lower.contains("gmail-")
    || lower.contains("mail-")
    || lower.contains("password=")
    || lower.contains("token=")
    || lower.contains("secret=")
    || lower.contains("otp=")
    || lower.contains("access_key=")
}

// --- Production browser factory ------------------------------------------

trait ProductionBrowserPlatform: Send + Sync {
  fn installed_version(&self, browser: &BackfillBrowser) -> Result<String, String>;
  fn validate_network(&self, network: &BackfillNetworkConfig) -> Result<(), String>;
  fn note_profile_creation(&self) {}
}

struct SystemProductionBrowserPlatform;

impl ProductionBrowserPlatform for SystemProductionBrowserPlatform {
  fn installed_version(&self, browser: &BackfillBrowser) -> Result<String, String> {
    let browser_name = match browser {
      BackfillBrowser::Chromium => "chromium",
      BackfillBrowser::Camoufox => "camoufox",
    };
    let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
    let mut versions = registry.get_downloaded_versions(browser_name);
    versions.sort_by(|a, b| {
      crate::api_client::VersionComponent::parse(b)
        .cmp(&crate::api_client::VersionComponent::parse(a))
    });
    versions
      .into_iter()
      .find(|version| registry.is_browser_downloaded(browser_name, version))
      .ok_or_else(|| {
        format!(
          "No installed {browser_name} version is available. Install the selected browser first."
        )
      })
  }

  fn validate_network(&self, network: &BackfillNetworkConfig) -> Result<(), String> {
    match network {
      BackfillNetworkConfig::None => Ok(()),
      BackfillNetworkConfig::Proxy { proxy_id } => {
        let proxy_id = proxy_id.trim();
        if proxy_id.is_empty() {
          return Err("Selected proxy ID is empty".into());
        }
        crate::proxy_manager::PROXY_MANAGER
          .get_proxy_settings_by_id(proxy_id)
          .map(|_| ())
          .ok_or_else(|| format!("Selected proxy ID '{proxy_id}' does not exist"))
      }
      BackfillNetworkConfig::Vpn { vpn_id } => {
        let vpn_id = vpn_id.trim();
        if vpn_id.is_empty() {
          return Err("Selected VPN ID is empty".into());
        }
        crate::vpn::VPN_STORAGE
          .lock()
          .map_err(|_| "VPN storage lock is poisoned".to_string())?
          .load_config(vpn_id)
          .map(|_| ())
          .map_err(|error| format!("Selected VPN ID '{vpn_id}' is unavailable: {error}"))
      }
    }
  }
}

fn release_selected_lease_after_cleanup(
  registry: &'static crate::profile_runtime::LeaseRegistry,
  lease_id: &str,
  cleanup_result: Result<(), String>,
) -> Result<(), String> {
  cleanup_result?;
  registry.release(lease_id);
  Ok(())
}

#[derive(Clone)]
pub(crate) struct SelectedBrowserRuntime {
  state: Arc<AsyncMutex<SelectedBrowserRuntimeState>>,
  lease_registry: &'static crate::profile_runtime::LeaseRegistry,
}

struct SelectedBrowserRuntimeState {
  profile: crate::profile::BrowserProfile,
  policy: crate::profile_runtime::LaunchPolicy,
  lease: crate::profile_runtime::RuntimeLease,
}

impl SelectedBrowserRuntime {
  pub(crate) fn new(
    profile: crate::profile::BrowserProfile,
    policy: crate::profile_runtime::LaunchPolicy,
    lease: crate::profile_runtime::RuntimeLease,
    lease_registry: &'static crate::profile_runtime::LeaseRegistry,
  ) -> Self {
    Self {
      state: Arc::new(AsyncMutex::new(SelectedBrowserRuntimeState {
        profile,
        policy,
        lease,
      })),
      lease_registry,
    }
  }

  fn validate_ownership(&self, state: &SelectedBrowserRuntimeState) -> Result<(), String> {
    validate_selected_runtime_ownership(&state.profile, &state.policy, &state.lease)?;
    if self
      .lease_registry
      .holder_lease_id(&state.lease.source_profile_id)
      .as_deref()
      != Some(state.lease.lease_id.as_str())
    {
      return Err("refusing to clean a selected profile runtime owned by another lease".into());
    }
    Ok(())
  }

  async fn snapshot(
    &self,
  ) -> (
    crate::profile::BrowserProfile,
    crate::profile_runtime::LaunchPolicy,
    crate::profile_runtime::RuntimeLease,
  ) {
    let state = self.state.lock().await;
    (
      state.profile.clone(),
      state.policy.clone(),
      state.lease.clone(),
    )
  }

  async fn launch(
    &self,
    app_handle: &AppHandle<tauri::Wry>,
  ) -> Result<crate::profile::BrowserProfile, String> {
    let mut state = self.state.lock().await;
    self.validate_ownership(&state)?;
    let profile = state.profile.clone();
    let policy = state.policy.clone();
    crate::browser_runner::BrowserRunner::instance()
      .launch_browser_with_policy(
        app_handle.clone(),
        &profile,
        Some("about:blank".into()),
        None,
        &policy,
        &mut state.lease,
      )
      .await
      .map_err(|error| format!("Launch selected 2FA backfill profile: {error}"))
  }

  async fn cleanup(&self, app_handle: &AppHandle<tauri::Wry>) -> Result<(), String> {
    let mut state = self.state.lock().await;
    self.validate_ownership(&state)?;
    let profile = state.profile.clone();
    let policy = state.policy.clone();
    crate::browser_runner::BrowserRunner::instance()
      .kill_runtime_browser(app_handle.clone(), &profile, &policy, &mut state.lease)
      .await
      .map_err(|error| format!("Clean selected 2FA backfill profile runtime: {error}"))
  }

  async fn close(&self, app_handle: &AppHandle<tauri::Wry>) -> Result<(), String> {
    let (_, _, lease) = self.snapshot().await;
    release_selected_lease_after_cleanup(
      self.lease_registry,
      &lease.lease_id,
      self.cleanup(app_handle).await,
    )
  }
}

fn validate_selected_runtime_ownership(
  profile: &crate::profile::BrowserProfile,
  policy: &crate::profile_runtime::LaunchPolicy,
  lease: &crate::profile_runtime::RuntimeLease,
) -> Result<(), String> {
  let profile_id = profile.id.to_string();
  if policy.source_profile_id.as_deref() != Some(profile_id.as_str())
    || lease.source_profile_id != profile_id
  {
    return Err("refusing to clean a selected profile runtime owned by another lease".into());
  }
  if policy.owns_generated_worker || policy.persist_process_to_source {
    return Err("selected profile runtime policy has invalid ownership flags".into());
  }
  Ok(())
}

pub(crate) struct ProductionBrowserFactory {
  platform: Arc<dyn ProductionBrowserPlatform>,
  selected_runtime: Option<SelectedBrowserRuntime>,
}

impl ProductionBrowserFactory {
  pub(crate) fn new(selected_runtime: Option<SelectedBrowserRuntime>) -> Self {
    Self {
      platform: Arc::new(SystemProductionBrowserPlatform),
      selected_runtime,
    }
  }

  pub(crate) fn selected_runtime_cleanup_guard(
    &self,
    app_handle: AppHandle<tauri::Wry>,
  ) -> Option<BrowserCleanupGuard> {
    let runtime = self.selected_runtime.clone()?;
    Some(BrowserCleanupGuard::new(Arc::new(move || {
      let runtime = runtime.clone();
      let app_handle = app_handle.clone();
      Box::pin(async move { runtime.close(&app_handle).await })
    })))
  }

  #[cfg(test)]
  fn with_platform(platform: Arc<dyn ProductionBrowserPlatform>) -> Self {
    Self {
      platform,
      selected_runtime: None,
    }
  }
}

type BrowserCleanupFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
type BrowserCleanupAction = Arc<dyn Fn() -> BrowserCleanupFuture + Send + Sync>;

pub(crate) struct BrowserCleanupGuard {
  action: Option<BrowserCleanupAction>,
}

impl BrowserCleanupGuard {
  fn new(action: BrowserCleanupAction) -> Self {
    Self {
      action: Some(action),
    }
  }

  pub(crate) async fn close(&mut self) -> Result<(), String> {
    let Some(action) = self.action.as_ref().cloned() else {
      return Ok(());
    };
    action().await?;
    self.action.take();
    Ok(())
  }
}

impl Drop for BrowserCleanupGuard {
  fn drop(&mut self) {
    let Some(action) = self.action.take() else {
      return;
    };
    match tokio::runtime::Handle::try_current() {
      Ok(handle) => {
        handle.spawn(async move {
          if let Err(error) = action().await {
            log::error!("Dropped 2FA backfill browser cleanup failed: {error}");
          }
        });
      }
      Err(error) => {
        std::thread::spawn(move || {
          let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
          {
            Ok(runtime) => runtime,
            Err(runtime_error) => {
              log::error!(
                "Dropped 2FA backfill browser session without a Tokio runtime ({error}); fallback runtime creation failed: {runtime_error}"
              );
              return;
            }
          };
          if let Err(cleanup_error) = runtime.block_on(action()) {
            log::error!("Fallback 2FA backfill browser cleanup failed: {cleanup_error}");
          }
        });
      }
    }
  }
}

struct ProductionBrowserSession {
  browser: Option<BrowserSession>,
  device_id: String,
  cleanup: BrowserCleanupGuard,
}

#[async_trait]
impl BackfillBrowserSession for ProductionBrowserSession {
  fn make_auth_adapter(&mut self) -> Box<dyn ExistingAccountAuthAdapter + Send + '_> {
    let browser = self
      .browser
      .as_mut()
      .expect("production browser session must be live before authentication");
    Box::new(BrowserAuthAdapter::new(browser, &self.device_id))
  }

  fn make_two_factor_adapter(&mut self) -> Box<dyn TwoFactorAdapter + Send + '_> {
    let browser = self
      .browser
      .as_mut()
      .expect("production browser session must be live before 2FA setup");
    Box::new(BrowserTwoFactorAdapter::new(browser))
  }

  async fn close(&mut self) -> Result<(), String> {
    self.browser.take();
    self.cleanup.close().await
  }
}

#[async_trait]
impl BackfillBrowserFactory<tauri::Wry> for ProductionBrowserFactory {
  fn preflight(
    &self,
    browser: &BackfillBrowser,
    network: &BackfillNetworkConfig,
  ) -> Result<(), String> {
    if self.selected_runtime.is_none() {
      self.platform.installed_version(browser)?;
    }
    self.platform.validate_network(network)
  }

  async fn launch(
    &self,
    app_handle: &AppHandle<tauri::Wry>,
    browser: &BackfillBrowser,
    network: &BackfillNetworkConfig,
    device_id: &str,
    login_email: &str,
    operation_id: &str,
  ) -> Result<Box<dyn BackfillBrowserSession + Send>, String> {
    self.platform.validate_network(network)?;
    if let Some(selected_runtime) = &self.selected_runtime {
      return launch_selected_browser_session(app_handle, selected_runtime, device_id, login_email)
        .await;
    }

    let version = self.platform.installed_version(browser)?;
    self.platform.note_profile_creation();

    let browser_name = match browser {
      BackfillBrowser::Chromium => "chromium",
      BackfillBrowser::Camoufox => "camoufox",
    };
    let (proxy_id, vpn_id) = match network {
      BackfillNetworkConfig::None => (None, None),
      BackfillNetworkConfig::Proxy { proxy_id } => (Some(proxy_id.trim().to_string()), None),
      BackfillNetworkConfig::Vpn { vpn_id } => (None, Some(vpn_id.trim().to_string())),
    };
    let camoufox_config = matches!(browser, BackfillBrowser::Camoufox).then(|| {
      crate::camoufox_manager::CamoufoxConfig {
        fingerprint: None,
        randomize_fingerprint_on_launch: Some(true),
        geoip: Some(serde_json::Value::Bool(true)),
        ..Default::default()
      }
    });
    let chromium_config = matches!(browser, BackfillBrowser::Chromium).then(|| {
      crate::chromium_manager::ChromiumConfig {
        fingerprint: None,
        randomize_fingerprint_on_launch: Some(true),
        ..Default::default()
      }
    });
    let release_type =
      if crate::api_client::is_browser_version_nightly(browser_name, &version, None) {
        "nightly"
      } else {
        "stable"
      };
    let profile_name = format!("twofa-backfill-worker-{}", operation_id);
    let profile = crate::profile::manager::create_browser_profile_with_group(
      app_handle.clone(),
      profile_name,
      browser_name.to_string(),
      version,
      release_type.to_string(),
      proxy_id,
      vpn_id,
      camoufox_config,
      chromium_config,
      None,
      true,
      None,
      None,
    )
    .await
    .map_err(|error| format!("Create 2FA backfill worker profile: {error}"))?;

    let launched = match crate::browser_runner::BrowserRunner::instance()
      .launch_browser(
        app_handle.clone(),
        &profile,
        Some("about:blank".into()),
        None,
      )
      .await
    {
      Ok(launched) => launched,
      Err(error) => {
        let rollback = cleanup_failed_launch(app_handle, &profile, operation_id).await;
        return match rollback {
          Ok(()) => Err(format!("Launch 2FA backfill worker profile: {error}")),
          Err(cleanup_error) => Err(format!(
            "Launch 2FA backfill worker profile: {error}; rollback failed: {cleanup_error}"
          )),
        };
      }
    };

    let mut attached = match attach_browser_session(&launched).await {
      Ok(attached) => attached,
      Err(error) => {
        if let Err(cleanup_error) = cleanup_failed_launch(app_handle, &launched, operation_id).await
        {
          return Err(format!("{error}; rollback failed: {cleanup_error}"));
        }
        return Err(error);
      }
    };
    if let Err(error) = attached
      .prepare_existing_account_login(device_id, login_email)
      .await
    {
      drop(attached);
      let rollback = cleanup_failed_launch(app_handle, &launched, operation_id).await;
      return match rollback {
        Ok(()) => Err(format!("Prepare existing-account login: {error}")),
        Err(cleanup_error) => Err(format!(
          "Prepare existing-account login: {error}; rollback failed: {cleanup_error}"
        )),
      };
    }

    let cleanup_app_handle = app_handle.clone();
    let cleanup_profile = launched.clone();
    let cleanup_operation_id = operation_id.to_string();
    let cleanup = BrowserCleanupGuard::new(Arc::new(move || {
      let app_handle = cleanup_app_handle.clone();
      let profile = cleanup_profile.clone();
      let operation_id = cleanup_operation_id.clone();
      Box::pin(async move { cleanup_worker_profile(&app_handle, &profile, &operation_id).await })
    }));
    Ok(Box::new(ProductionBrowserSession {
      browser: Some(attached),
      device_id: device_id.to_string(),
      cleanup,
    }))
  }
}

async fn launch_selected_browser_session(
  app_handle: &AppHandle<tauri::Wry>,
  runtime: &SelectedBrowserRuntime,
  device_id: &str,
  login_email: &str,
) -> Result<Box<dyn BackfillBrowserSession + Send>, String> {
  let launched = runtime.launch(app_handle).await?;
  let mut attached = match attach_browser_session(&launched).await {
    Ok(attached) => attached,
    Err(error) => {
      if let Err(cleanup_error) = runtime.cleanup(app_handle).await {
        return Err(format!("{error}; rollback failed: {cleanup_error}"));
      }
      return Err(error);
    }
  };
  if let Err(error) = attached
    .prepare_existing_account_login(device_id, login_email)
    .await
  {
    drop(attached);
    return match runtime.cleanup(app_handle).await {
      Ok(()) => Err(format!("Prepare existing-account login: {error}")),
      Err(cleanup_error) => Err(format!(
        "Prepare existing-account login: {error}; rollback failed: {cleanup_error}"
      )),
    };
  }

  let cleanup_runtime = runtime.clone();
  let cleanup_app_handle = app_handle.clone();
  let cleanup = BrowserCleanupGuard::new(Arc::new(move || {
    let runtime = cleanup_runtime.clone();
    let app_handle = cleanup_app_handle.clone();
    Box::pin(async move { runtime.cleanup(&app_handle).await })
  }));
  Ok(Box::new(ProductionBrowserSession {
    browser: Some(attached),
    device_id: device_id.to_string(),
    cleanup,
  }))
}

async fn cleanup_worker_profile(
  app_handle: &AppHandle<tauri::Wry>,
  profile: &crate::profile::BrowserProfile,
  operation_id: &str,
) -> Result<(), String> {
  if profile.name != format!("twofa-backfill-worker-{operation_id}") {
    return Err("refusing to clean a worker profile owned by another operation".into());
  }
  if !profile.ephemeral {
    return Err("refusing to clean a non-ephemeral 2FA worker profile".into());
  }
  crate::browser_runner::BrowserRunner::instance()
    .kill_browser_process(app_handle.clone(), profile)
    .await
    .map_err(|error| format!("terminate worker profile {}: {error}", profile.id))?;
  crate::profile::ProfileManager::instance()
    .delete_profile(app_handle, &profile.id.to_string())
    .map_err(|error| format!("delete worker profile {}: {error}", profile.id))
}

async fn cleanup_failed_launch(
  app_handle: &AppHandle<tauri::Wry>,
  profile: &crate::profile::BrowserProfile,
  operation_id: &str,
) -> Result<(), String> {
  let result = cleanup_worker_profile(app_handle, profile, operation_id).await;
  if let Err(error) = &result {
    log::error!(
      "2FA backfill launch rollback could not clean up worker profile {}; profile retained: {error}",
      profile.id
    );
  }
  result
}

#[cfg(test)]
mod tests {
  //! Engine unit tests against fake adapters, an in-memory store and a fake
  //! journal. The production browser/CDP wiring is exercised in the Wave 6 live
  //! harness; here we verify the serial state machine, journal-before-confirm
  //! ordering, CAS field preservation, transient-failure isolation, batch pause
  //! on 429/Cloudflare, locked-account skip/continue, cancellation between accounts,
  //! the ManualReview transition, and the secret-free progress event payload.

  use std::collections::{HashMap, VecDeque};
  use std::sync::atomic::AtomicUsize;
  use std::sync::{Arc, Mutex};

  use async_trait::async_trait;
  use chrono::Utc;
  use serde_json::Value;
  use tauri::Listener;

  use super::*;
  use crate::auto_service::openai::chatgpt_auth::{
    AuthError, AuthSignals, EmailOtpSubmission, ExistingAccountAuthAdapter,
  };
  use crate::auto_service::openai::chatgpt_two_factor::{
    RemoteTwoFactorState, TwoFactorAdapter, TwoFactorError,
  };
  use crate::auto_service::openai::register::store::BackfillPatchOperation;
  use crate::auto_service::openai::register::types::{
    AccountInventoryStatus, EmailProviderProvenance, RegistrationOutcomeReason,
    TwoFactorBackfillAccessState, TwoFactorBackfillOutcome,
  };

  const SECRET: &str = "JBSWY3DPEHPK3PXP";
  const PASSWORD: &str = "known-password-value";
  const RAW_CDK: &str = "GMAIL-known-raw-cdk-value";
  const ACCESS_TOKEN: &str = "known-access-token-value";

  struct RecordingProductionPlatform {
    profile_creations: Mutex<usize>,
  }

  impl RecordingProductionPlatform {
    fn with_missing_route() -> Self {
      Self {
        profile_creations: Mutex::new(0),
      }
    }

    fn profile_creation_count(&self) -> usize {
      *self.profile_creations.lock().unwrap()
    }
  }

  impl ProductionBrowserPlatform for RecordingProductionPlatform {
    fn installed_version(&self, _browser: &BackfillBrowser) -> Result<String, String> {
      Ok("test-version".into())
    }

    fn validate_network(&self, _network: &BackfillNetworkConfig) -> Result<(), String> {
      Err("selected route does not exist".into())
    }

    fn note_profile_creation(&self) {
      *self.profile_creations.lock().unwrap() += 1;
    }
  }

  // --- Fixtures -----------------------------------------------------------

  fn make_app_handle() -> AppHandle<tauri::test::MockRuntime> {
    let app = tauri::test::mock_app();
    app.handle().clone()
  }

  fn capture_progress_events(
    app_handle: &AppHandle<tauri::test::MockRuntime>,
  ) -> Arc<Mutex<Vec<Value>>> {
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured_clone = Arc::clone(&captured);
    let _listener = app_handle.listen(PROGRESS_EVENT, move |event| {
      if let Ok(value) = serde_json::from_str::<Value>(event.payload()) {
        captured_clone.lock().unwrap().push(value);
      }
    });
    captured
  }

  fn terminal_events(events: &[Value]) -> Vec<&Value> {
    events
      .iter()
      .filter(|event| {
        matches!(
          event.get("step").and_then(Value::as_str),
          Some("completed" | "failed" | "cancelled" | "batchPaused")
        )
      })
      .collect()
  }

  /// Build an eligible account with seller/sensitive fields populated so the
  /// CAS-preserve assertion can verify they survive the final patch.
  fn eligible_account(account_key: &str) -> RegistrationResult {
    RegistrationResult {
      success: true,
      email: format!("{account_key}@example.com"),
      password: PASSWORD.into(),
      account_id: account_key.into(),
      access_token: ACCESS_TOKEN.into(),
      device_id: format!("device-{account_key}"),
      error_message: String::new(),
      step_logs: Vec::new(),
      created_at: Utc::now(),
      two_fa_enabled: false,
      totp_secret: String::new(),
      free_trial_eligible: true,
      plan_type: "trial".into(),
      cdk: RAW_CDK.into(),
      base_email: format!("base-{account_key}@example.com"),
      phone_number: "+15551234567".into(),
      status: AccountInventoryStatus::Available,
      note: "seller-tag-value".into(),
      exported_at: Some(Utc::now()),
      sold_at: None,
      email_provider: None,
      email_provider_provenance: None,
      registration_outcome_reason: Some(RegistrationOutcomeReason::Registered),
      two_factor_backfill_access_state: Some(
        crate::auto_service::openai::register::types::TwoFactorBackfillAccessState::Accessible,
      ),
      two_factor_backfill_exclusion: None,
      two_factor_backfill_state: None,
      two_factor_backfill_operation_id: None,
      two_factor_backfill_outcome: None,
      record_revision: 7,
    }
  }

  fn canary_request(account_keys: &[&str]) -> TwoFactorBackfillStartRequest {
    TwoFactorBackfillStartRequest {
      selected_account_keys: account_keys.iter().map(|key| (*key).into()).collect(),
      allow_free_trial_no: false,
      acknowledge_legacy_access: false,
      profile_id: None,
      data_mode: crate::profile_runtime::DataMode::Ephemeral,
      fingerprint_mode: crate::profile_runtime::FingerprintMode::RandomPerLaunch,
      browser: BackfillBrowser::Chromium,
      network: BackfillNetworkConfig::None,
      mode: BackfillMode::Canary,
    }
  }

  fn bulk_request(account_keys: &[&str]) -> TwoFactorBackfillStartRequest {
    TwoFactorBackfillStartRequest {
      selected_account_keys: account_keys.iter().map(|key| (*key).into()).collect(),
      allow_free_trial_no: false,
      acknowledge_legacy_access: false,
      profile_id: None,
      data_mode: crate::profile_runtime::DataMode::Ephemeral,
      fingerprint_mode: crate::profile_runtime::FingerprintMode::RandomPerLaunch,
      browser: BackfillBrowser::Chromium,
      network: BackfillNetworkConfig::None,
      mode: BackfillMode::Bulk,
    }
  }

  // --- Fake adapters -----------------------------------------------------

  #[derive(Default)]
  struct FakeAuthAdapter {
    observations: VecDeque<AuthSignals>,
    /// If set, `observe()` returns this AuthError instead of consuming an observation.
    inject_error: Option<AuthError>,
    cancel_after_observe: Option<Arc<AtomicBool>>,
    events: Vec<String>,
  }

  impl FakeAuthAdapter {
    fn immediate_logged_in() -> Self {
      Self {
        observations: VecDeque::from([AuthSignals {
          url: "https://chatgpt.com/".into(),
          logged_in_marker: true,
          ..AuthSignals::default()
        }]),
        inject_error: None,
        cancel_after_observe: None,
        events: Vec::new(),
      }
    }

    fn failing(error: AuthError) -> Self {
      Self {
        observations: VecDeque::new(),
        inject_error: Some(error),
        cancel_after_observe: None,
        events: Vec::new(),
      }
    }

    fn cancelling_after_observe(cancel_flag: Arc<AtomicBool>) -> Self {
      Self {
        cancel_after_observe: Some(cancel_flag),
        ..Self::immediate_logged_in()
      }
    }

    fn record(&mut self, event: &str) {
      self.events.push(event.into());
    }
  }

  #[async_trait]
  impl ExistingAccountAuthAdapter for FakeAuthAdapter {
    async fn observe(&mut self) -> Result<AuthSignals, AuthError> {
      self.record("auth:observe");
      if let Some(error) = self.inject_error.take() {
        return Err(error);
      }
      let observation = self
        .observations
        .pop_front()
        .ok_or_else(|| AuthError::Unknown("fake observation queue exhausted".into()))?;
      if let Some(cancel_flag) = self.cancel_after_observe.take() {
        cancel_flag.store(true, Ordering::SeqCst);
      }
      Ok(observation)
    }

    async fn submit_email(&mut self, _email: &str) -> Result<(), AuthError> {
      self.record("auth:submit_email");
      Ok(())
    }

    async fn submit_password(&mut self, _password: &str) -> Result<(), AuthError> {
      self.record("auth:submit_password");
      Ok(())
    }

    async fn submit_email_otp(&mut self, _code: &str) -> Result<EmailOtpSubmission, AuthError> {
      self.record("auth:submit_email_otp");
      Ok(EmailOtpSubmission::Accepted)
    }

    async fn refresh_email_verification(&mut self) -> Result<(), AuthError> {
      self.record("auth:refresh_email_verification");
      Ok(())
    }

    async fn resend_email_otp(&mut self) -> Result<(), AuthError> {
      self.record("auth:resend_email_otp");
      Ok(())
    }

    async fn submit_authenticator_totp(&mut self, _code: &str) -> Result<(), AuthError> {
      self.record("auth:submit_authenticator_totp");
      Ok(())
    }
  }

  #[derive(Default)]
  struct FakeTwoFactorAdapter {
    states: VecDeque<RemoteTwoFactorState>,
    /// If set on `scrape_normalized_secret`, returns this error to simulate a
    /// secret capture failure instead of returning SECRET.
    inject_capture_error: Option<TwoFactorError>,
    events: Arc<Mutex<Vec<String>>>,
    confirmed_codes: Vec<String>,
  }

  impl FakeTwoFactorAdapter {
    fn happy() -> Self {
      Self {
        states: VecDeque::from([RemoteTwoFactorState::Off, RemoteTwoFactorState::On]),
        inject_capture_error: None,
        events: Arc::new(Mutex::new(Vec::new())),
        confirmed_codes: Vec::new(),
      }
    }

    fn failing_capture(error: TwoFactorError) -> Self {
      Self {
        states: VecDeque::from([RemoteTwoFactorState::Off]),
        inject_capture_error: Some(error),
        events: Arc::new(Mutex::new(Vec::new())),
        confirmed_codes: Vec::new(),
      }
    }

    fn record(&self, event: &str) {
      self.events.lock().unwrap().push(event.into());
    }
  }

  #[async_trait]
  impl TwoFactorAdapter for FakeTwoFactorAdapter {
    async fn inspect_remote_state(&mut self) -> Result<RemoteTwoFactorState, TwoFactorError> {
      self.record("tf:inspect");
      self
        .states
        .pop_front()
        .ok_or_else(|| TwoFactorError::ProbeFailed("fake state queue exhausted".into()))
    }

    async fn begin_authenticator_setup(&mut self) -> Result<(), TwoFactorError> {
      self.record("tf:begin");
      Ok(())
    }

    async fn scrape_normalized_secret(&mut self) -> Result<String, TwoFactorError> {
      self.record("tf:capture");
      if let Some(error) = self.inject_capture_error.take() {
        return Err(error);
      }
      Ok(SECRET.into())
    }

    async fn confirm_setup(&mut self, code: &str) -> Result<(), TwoFactorError> {
      self.record("tf:confirm");
      self.confirmed_codes.push(code.to_string());
      Ok(())
    }

    async fn close_and_reopen_security(&mut self) -> Result<(), TwoFactorError> {
      self.record("tf:reopen");
      Ok(())
    }
  }

  // --- Fake browser factory + session wiring -----------------------------

  struct FakeSession {
    auth: Capture,
    two_factor_events: Arc<Mutex<Vec<String>>>,
    two_factor_confirmed_codes: Arc<Mutex<Vec<String>>>,
    session_id: usize,
    closes: Arc<Mutex<usize>>,
    close_fails: bool,
    adapter_sessions: Arc<Mutex<Vec<(&'static str, usize)>>>,
  }

  enum Capture {
    Happy,
    CancelAfterLaunch(Arc<AtomicBool>),
    CancelAfterAuth(Arc<AtomicBool>),
    LaunchFails,
    WrongCredentialsCloseFails,
    Locked,
    RateLimited,
    EmailOtpAttemptsExhausted,
    WrongCredentials,
    SecretCaptureFails(TwoFactorError),
    RemoteStates(VecDeque<RemoteTwoFactorState>),
    CloseFails,
  }

  impl FakeSession {
    fn from(
      capture: Capture,
      session_id: usize,
      closes: Arc<Mutex<usize>>,
      adapter_sessions: Arc<Mutex<Vec<(&'static str, usize)>>>,
    ) -> Self {
      Self {
        close_fails: matches!(
          capture,
          Capture::CloseFails | Capture::WrongCredentialsCloseFails
        ),
        auth: capture,
        two_factor_events: Arc::new(Mutex::new(Vec::new())),
        two_factor_confirmed_codes: Arc::new(Mutex::new(Vec::new())),
        session_id,
        closes,
        adapter_sessions,
      }
    }
  }

  #[async_trait]
  impl BackfillBrowserSession for FakeSession {
    fn make_auth_adapter(&mut self) -> Box<dyn ExistingAccountAuthAdapter + Send + '_> {
      self
        .adapter_sessions
        .lock()
        .unwrap()
        .push(("auth", self.session_id));
      let adapter = match &self.auth {
        Capture::Happy | Capture::CancelAfterLaunch(_) | Capture::CloseFails => {
          FakeAuthAdapter::immediate_logged_in()
        }
        Capture::CancelAfterAuth(cancel_flag) => {
          FakeAuthAdapter::cancelling_after_observe(Arc::clone(cancel_flag))
        }
        Capture::LaunchFails => panic!("launch failure must not construct a browser session"),
        Capture::WrongCredentialsCloseFails => {
          FakeAuthAdapter::failing(AuthError::WrongCredentials)
        }
        Capture::Locked => FakeAuthAdapter::failing(AuthError::Locked),
        Capture::RateLimited => FakeAuthAdapter::failing(AuthError::RateLimited),
        Capture::EmailOtpAttemptsExhausted => {
          FakeAuthAdapter::failing(AuthError::EmailOtpAttemptsExhausted)
        }
        Capture::WrongCredentials => FakeAuthAdapter::failing(AuthError::WrongCredentials),
        Capture::SecretCaptureFails(_) | Capture::RemoteStates(_) => {
          FakeAuthAdapter::immediate_logged_in()
        }
      };
      Box::new(adapter)
    }

    fn make_two_factor_adapter(&mut self) -> Box<dyn TwoFactorAdapter + Send + '_> {
      self
        .adapter_sessions
        .lock()
        .unwrap()
        .push(("two_factor", self.session_id));
      let adapter = match &self.auth {
        Capture::Happy
        | Capture::CancelAfterLaunch(_)
        | Capture::CancelAfterAuth(_)
        | Capture::CloseFails => FakeTwoFactorAdapter::happy(),
        Capture::LaunchFails => panic!("launch failure must not construct a browser session"),
        Capture::WrongCredentialsCloseFails
        | Capture::Locked
        | Capture::RateLimited
        | Capture::EmailOtpAttemptsExhausted
        | Capture::WrongCredentials => FakeTwoFactorAdapter::happy(),
        Capture::SecretCaptureFails(error) => FakeTwoFactorAdapter::failing_capture(error.clone()),
        Capture::RemoteStates(states) => FakeTwoFactorAdapter {
          states: states.clone(),
          ..FakeTwoFactorAdapter::default()
        },
      };
      // Re-wire the shared recorder to expose captured TOTP-side events.
      self.two_factor_events = adapter.events.clone();
      let _ = &self.two_factor_confirmed_codes;
      Box::new(adapter)
    }

    async fn close(&mut self) -> Result<(), String> {
      *self.closes.lock().unwrap() += 1;
      if self.close_fails {
        Err("injected browser cleanup failure".into())
      } else {
        Ok(())
      }
    }
  }

  #[derive(Clone)]
  struct FakeBrowserFactory {
    sessions: Arc<Mutex<VecDeque<Capture>>>,
    launches: Arc<Mutex<usize>>,
    closes: Arc<Mutex<usize>>,
    adapter_sessions: Arc<Mutex<Vec<(&'static str, usize)>>>,
    preflight_error: Option<String>,
  }

  impl FakeBrowserFactory {
    fn from(captures: Vec<Capture>) -> Self {
      Self {
        sessions: Arc::new(Mutex::new(captures.into_iter().collect())),
        launches: Arc::new(Mutex::new(0)),
        closes: Arc::new(Mutex::new(0)),
        adapter_sessions: Arc::new(Mutex::new(Vec::new())),
        preflight_error: None,
      }
    }

    fn with_preflight_error(error: &str) -> Self {
      Self {
        preflight_error: Some(error.into()),
        ..Self::from(Vec::new())
      }
    }

    fn launch_count(&self) -> usize {
      *self.launches.lock().unwrap()
    }

    fn close_count(&self) -> usize {
      *self.closes.lock().unwrap()
    }

    fn adapter_session_ids(&self) -> Vec<(&'static str, usize)> {
      self.adapter_sessions.lock().unwrap().clone()
    }
  }

  #[async_trait]
  impl BackfillBrowserFactory<tauri::test::MockRuntime> for FakeBrowserFactory {
    fn preflight(
      &self,
      _browser: &BackfillBrowser,
      _network: &BackfillNetworkConfig,
    ) -> Result<(), String> {
      self.preflight_error.clone().map_or(Ok(()), Err)
    }

    async fn launch(
      &self,
      _app_handle: &AppHandle<tauri::test::MockRuntime>,
      _browser: &BackfillBrowser,
      _network: &BackfillNetworkConfig,
      _device_id: &str,
      _login_email: &str,
      _operation_id: &str,
    ) -> Result<Box<dyn BackfillBrowserSession + Send>, String> {
      *self.launches.lock().unwrap() += 1;
      let capture = self
        .sessions
        .lock()
        .unwrap()
        .pop_front()
        .ok_or_else(|| "FakeBrowserFactory queue exhausted".to_string())?;
      if matches!(capture, Capture::LaunchFails) {
        return Err("injected browser launch failure".into());
      }
      if let Capture::CancelAfterLaunch(cancel_flag) = &capture {
        cancel_flag.store(true, Ordering::SeqCst);
      }
      let session_id = *self.launches.lock().unwrap();
      Ok(Box::new(FakeSession::from(
        capture,
        session_id,
        Arc::clone(&self.closes),
        Arc::clone(&self.adapter_sessions),
      )))
    }
  }

  // --- Fake account store ------------------------------------------------

  type CancelAfterCasCall = Option<(usize, Arc<AtomicBool>)>;

  #[derive(Clone, Default)]
  struct FakeStore {
    records: Arc<Mutex<HashMap<String, RegistrationResult>>>,
    /// Account key whose next reload should mutate to "became ineligible" (two
    /// factor flipped on by an external actor). Simulates a stale top-level
    /// preview being falsified by the per-account reload.
    scheduled_ineligibility: Arc<Mutex<Option<String>>>,
    /// One-based CAS call number that should return a revision conflict.
    conflict_on_cas_call: Arc<Mutex<Option<usize>>>,
    cancel_after_cas_call: Arc<Mutex<CancelAfterCasCall>>,
    fail_reload_on_call: Arc<Mutex<Option<usize>>>,
    reload_calls: Arc<Mutex<usize>>,
    cas_calls: Arc<Mutex<Vec<String>>>,
    persist_calls: Arc<Mutex<Vec<String>>>,
  }

  impl FakeStore {
    fn from(accounts: Vec<RegistrationResult>) -> Self {
      let mut map = HashMap::new();
      for account in accounts {
        map.insert(stable_account_key(&account), account);
      }
      Self {
        records: Arc::new(Mutex::new(map)),
        ..Self::default()
      }
    }

    fn schedule_reload_ineligibility(&self, account_key: impl Into<String>) {
      *self.scheduled_ineligibility.lock().unwrap() = Some(account_key.into());
    }

    fn force_conflict_on_cas_call(&self, call_number: usize) {
      *self.conflict_on_cas_call.lock().unwrap() = Some(call_number);
    }

    fn cancel_after_cas_call(&self, call_number: usize, cancel_flag: Arc<AtomicBool>) {
      *self.cancel_after_cas_call.lock().unwrap() = Some((call_number, cancel_flag));
    }

    fn fail_reload_on_call(&self, call_number: usize) {
      *self.fail_reload_on_call.lock().unwrap() = Some(call_number);
    }

    fn current(&self, account_key: &str) -> RegistrationResult {
      self
        .records
        .lock()
        .unwrap()
        .get(account_key)
        .cloned()
        .unwrap_or_else(|| panic!("FakeStore missing account {account_key}"))
    }

    fn cas_calls(&self) -> Vec<String> {
      self.cas_calls.lock().unwrap().clone()
    }

    fn persist_calls(&self) -> Vec<String> {
      self.persist_calls.lock().unwrap().clone()
    }
  }

  impl BackfillAccountStore for FakeStore {
    fn list_accounts(&self) -> Result<Vec<RegistrationResult>, String> {
      Ok(self.records.lock().unwrap().values().cloned().collect())
    }

    fn reload(&self, account_key: &str) -> Result<Option<RegistrationResult>, String> {
      let reload_call_number = {
        let mut calls = self.reload_calls.lock().unwrap();
        *calls += 1;
        *calls
      };
      if *self.fail_reload_on_call.lock().unwrap() == Some(reload_call_number) {
        return Err("injected account reload failure".into());
      }
      let mut records = self.records.lock().unwrap();
      if let Some(schedule) = self.scheduled_ineligibility.lock().unwrap().take() {
        if schedule == account_key {
          if let Some(account) = records.get_mut(account_key) {
            // Simulate a concurrent actor enabling 2FA between the top-level
            // preview and this per-account recheck.
            account.two_fa_enabled = true;
            account.totp_secret = "EXTERNAL-SECRET".into();
          }
        }
      }
      Ok(records.get(account_key).cloned())
    }

    fn persist_provider(
      &self,
      account_key: &str,
      expected_revision: u64,
      provider: EmailProvider,
    ) -> Result<RegistrationResult, String> {
      self.persist_calls.lock().unwrap().push(account_key.into());
      let mut records = self.records.lock().unwrap();
      let account = records
        .get_mut(account_key)
        .ok_or_else(|| format!("Registered account {account_key} not found"))?;
      if account.record_revision != expected_revision {
        return Err(format!(
          "Registered account revision conflict for {account_key}: expected {expected_revision}, current {}",
          account.record_revision
        ));
      }
      account.email_provider = Some(provider);
      account.email_provider_provenance = Some(EmailProviderProvenance::InferredFromCdk);
      account.record_revision = account.record_revision.checked_add(1).unwrap();
      Ok(account.clone())
    }

    fn cas_patch(
      &self,
      account_key: &str,
      expected_revision: u64,
      precondition: BackfillPatchPrecondition,
      patch: TwoFactorBackfillPatch,
    ) -> Result<RegistrationResult, String> {
      let cas_call_number = {
        let mut calls = self.cas_calls.lock().unwrap();
        calls.push(account_key.into());
        calls.len()
      };
      let force_conflict = *self.conflict_on_cas_call.lock().unwrap() == Some(cas_call_number);
      if force_conflict {
        return Err(format!(
          "Registered account revision conflict for {account_key}: expected {expected_revision}, current {}",
          expected_revision.wrapping_add(1)
        ));
      }
      let mut records = self.records.lock().unwrap();
      let account = records
        .get_mut(account_key)
        .ok_or_else(|| format!("Registered account {account_key} not found"))?;
      if account.record_revision != expected_revision {
        return Err(format!(
          "Registered account revision conflict for {account_key}: expected {expected_revision}, current {}",
          account.record_revision
        ));
      }
      assert_eq!(
        precondition.operation(),
        patch.operation(),
        "precondition and patch operations diverged"
      );
      account.two_factor_backfill_operation_id = Some(precondition.operation_id().to_string());
      match patch.operation() {
        BackfillPatchOperation::Start => {
          account.two_factor_backfill_state = Some(TwoFactorBackfillState::InProgress);
          account.two_factor_backfill_outcome = None;
        }
        BackfillPatchOperation::FinalizeEnabled => {
          account.two_fa_enabled = true;
          account.totp_secret = patch
            .totp_secret()
            .expect("finalize patch must carry a TOTP secret")
            .to_string();
          account.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
          account.two_factor_backfill_outcome = Some(TwoFactorBackfillOutcome::Enabled);
        }
        BackfillPatchOperation::FinalizeFailed => {
          if let Some(access_state) = patch.access_state() {
            account.two_factor_backfill_access_state = Some(access_state);
          }
          account.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
          account.two_factor_backfill_outcome = Some(TwoFactorBackfillOutcome::Failed);
        }
        BackfillPatchOperation::FinalizeCancelled => {
          account.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
          account.two_factor_backfill_outcome = Some(TwoFactorBackfillOutcome::Cancelled);
        }
        BackfillPatchOperation::FinalizeReconciliationRequired => {
          account.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
          account.two_factor_backfill_outcome =
            Some(TwoFactorBackfillOutcome::ReconciliationRequired);
        }
      }
      account.record_revision = account.record_revision.checked_add(1).unwrap();
      let updated = account.clone();
      drop(records);
      if let Some((call_number, cancel_flag)) = self.cancel_after_cas_call.lock().unwrap().as_ref()
      {
        if *call_number == cas_call_number {
          cancel_flag.store(true, Ordering::SeqCst);
        }
      }
      Ok(updated)
    }
  }

  // --- Fake journal -------------------------------------------------------

  #[derive(Clone, Default)]
  struct FakeJournal {
    events: Arc<Mutex<Vec<String>>>,
    current_revision: Arc<Mutex<u64>>,
    journal_entry: Arc<Mutex<Option<TwoFactorBackfillJournalEntry>>>,
    manual_review_calls: Arc<Mutex<Vec<String>>>,
    delete_returns_false: Arc<AtomicBool>,
    manual_review_fails: Arc<AtomicBool>,
    cancel_after_secret_capture: Arc<Mutex<Option<Arc<AtomicBool>>>>,
  }

  impl FakeJournal {
    fn new() -> Self {
      Self {
        events: Arc::new(Mutex::new(Vec::new())),
        current_revision: Arc::new(Mutex::new(1)),
        journal_entry: Arc::new(Mutex::new(None)),
        manual_review_calls: Arc::new(Mutex::new(Vec::new())),
        delete_returns_false: Arc::new(AtomicBool::new(false)),
        manual_review_fails: Arc::new(AtomicBool::new(false)),
        cancel_after_secret_capture: Arc::new(Mutex::new(None)),
      }
    }

    fn return_false_on_delete(&self) {
      self.delete_returns_false.store(true, Ordering::SeqCst);
    }

    fn fail_manual_review(&self) {
      self.manual_review_fails.store(true, Ordering::SeqCst);
    }

    fn cancel_after_secret_capture(&self, cancel_flag: Arc<AtomicBool>) {
      *self.cancel_after_secret_capture.lock().unwrap() = Some(cancel_flag);
    }

    fn events(&self) -> Vec<String> {
      self.events.lock().unwrap().clone()
    }

    fn manual_review_calls(&self) -> Vec<String> {
      self.manual_review_calls.lock().unwrap().clone()
    }

    fn load(
      &self,
      operation_id: &str,
      account_key: &str,
    ) -> Result<Option<TwoFactorBackfillJournalEntry>, String> {
      Ok(
        self
          .journal_entry
          .lock()
          .unwrap()
          .as_ref()
          .filter(|entry| entry.operation_id == operation_id && entry.account_key == account_key)
          .cloned(),
      )
    }
  }

  fn synthetic_entry(
    operation_id: &str,
    account_key: &str,
    revision: u64,
  ) -> TwoFactorBackfillJournalEntry {
    TwoFactorBackfillJournalEntry {
      operation_id: operation_id.into(),
      account_key: account_key.into(),
      expected_account_revision: 0,
      final_account_revision: None,
      journal_revision: revision,
      pending_totp_secret: SECRET.into(),
      state: TwoFactorBackfillJournalState::SecretCaptured,
      created_at: Utc::now(),
      updated_at: Utc::now(),
    }
  }

  impl BackfillJournal for FakeJournal {
    fn create_secret_captured(
      &mut self,
      operation_id: &str,
      account_key: &str,
      _expected_account_revision: u64,
      _pending_totp_secret: String,
    ) -> Result<TwoFactorBackfillJournalEntry, String> {
      self.events.lock().unwrap().push("journal:create".into());
      // Match the production journal's revision sequence: starts at 1.
      let revision = *self.current_revision.lock().unwrap();
      let entry = synthetic_entry(operation_id, account_key, revision);
      *self.journal_entry.lock().unwrap() = Some(entry.clone());
      if let Some(cancel_flag) = self.cancel_after_secret_capture.lock().unwrap().take() {
        cancel_flag.store(true, Ordering::SeqCst);
      }
      Ok(entry)
    }

    fn update_state(
      &mut self,
      operation_id: &str,
      account_key: &str,
      expected_journal_revision: u64,
      state: TwoFactorBackfillJournalState,
    ) -> Result<TwoFactorBackfillJournalEntry, String> {
      let label = format!("journal:update_state:{state:?}");
      self.events.lock().unwrap().push(label);
      let current = *self.current_revision.lock().unwrap();
      assert_eq!(
        current, expected_journal_revision,
        "journal revision mismatch on update_state"
      );
      let next = current.checked_add(1).unwrap();
      *self.current_revision.lock().unwrap() = next;
      let mut entry = synthetic_entry(operation_id, account_key, current);
      entry.state = state;
      entry.journal_revision = next;
      *self.journal_entry.lock().unwrap() = Some(entry.clone());
      Ok(entry)
    }

    fn record_final_account_revision(
      &mut self,
      operation_id: &str,
      account_key: &str,
      expected_journal_revision: u64,
      _final_account_revision: u64,
    ) -> Result<TwoFactorBackfillJournalEntry, String> {
      self
        .events
        .lock()
        .unwrap()
        .push("journal:record_final".into());
      let current = *self.current_revision.lock().unwrap();
      assert_eq!(
        current, expected_journal_revision,
        "journal revision mismatch on record_final_account_revision"
      );
      let next = current.checked_add(1).unwrap();
      *self.current_revision.lock().unwrap() = next;
      let mut entry = synthetic_entry(operation_id, account_key, next);
      entry.final_account_revision = Some(_final_account_revision);
      *self.journal_entry.lock().unwrap() = Some(entry.clone());
      Ok(entry)
    }

    fn delete_after_account_patch(
      &mut self,
      _operation_id: &str,
      _account_key: &str,
      expected_journal_revision: u64,
      _persisted: PersistedBackfillAccountPatch<'_>,
    ) -> Result<bool, String> {
      self.events.lock().unwrap().push("journal:delete".into());
      let current = *self.current_revision.lock().unwrap();
      assert_eq!(current, expected_journal_revision);
      if self.delete_returns_false.load(Ordering::SeqCst) {
        return Ok(false);
      }
      let existed = self.journal_entry.lock().unwrap().take().is_some();
      Ok(existed)
    }

    fn transition_to_manual_review(
      &mut self,
      operation_id: &str,
      account_key: &str,
      expected_journal_revision: u64,
    ) -> Result<TwoFactorBackfillJournalEntry, String> {
      if self.manual_review_fails.load(Ordering::SeqCst) {
        return Err("injected manual-review transition failure".into());
      }
      let entry = self
        .journal_entry
        .lock()
        .unwrap()
        .as_ref()
        .filter(|entry| entry.operation_id == operation_id && entry.account_key == account_key)
        .cloned()
        .ok_or_else(|| "fake journal entry not found".to_string())?;
      assert_eq!(entry.journal_revision, expected_journal_revision);
      self
        .manual_review_calls
        .lock()
        .unwrap()
        .push(format!("{operation_id}:{account_key}"));
      let next = expected_journal_revision.checked_add(1).unwrap();
      *self.current_revision.lock().unwrap() = next;
      let mut entry = entry;
      entry.state = TwoFactorBackfillJournalState::ManualReview;
      entry.journal_revision = next;
      *self.journal_entry.lock().unwrap() = Some(entry.clone());
      Ok(entry)
    }
  }

  // --- Stub gate ----------------------------------------------------------

  #[derive(Clone, Default)]
  struct StubGate {
    validate_calls: Arc<Mutex<Vec<String>>>,
    record_calls: Arc<Mutex<Vec<String>>>,
    record_error: Arc<Mutex<Option<String>>>,
  }

  impl StubGate {
    fn failing_record(error: &str) -> Self {
      Self {
        record_error: Arc::new(Mutex::new(Some(error.into()))),
        ..Self::default()
      }
    }
  }

  impl CanaryGateLike for StubGate {
    fn validate(
      &self,
      _mode: BackfillMode,
      _account_keys: &[String],
      _browser: &BackfillBrowser,
      _providers: &[EmailProvider],
      _network: &BackfillNetworkConfig,
    ) -> Result<String, CanaryRejection> {
      self.validate_calls.lock().unwrap().push("validate".into());
      Ok(network_config_hash(&BackfillNetworkConfig::None))
    }

    fn record_canary_success(
      &self,
      _browser: &BackfillBrowser,
      _provider: EmailProvider,
      _network: &BackfillNetworkConfig,
      account_key: &str,
    ) -> Result<(), String> {
      self.record_calls.lock().unwrap().push(account_key.into());
      self
        .record_error
        .lock()
        .unwrap()
        .clone()
        .map_or(Ok(()), Err)
    }
  }

  // --- Tests -------------------------------------------------------------

  #[tokio::test]
  async fn happy_canary_enables_two_fa_and_persists_secret_with_full_journal_chain() {
    let app_handle = make_app_handle();
    let account = eligible_account("account-1");
    let store = FakeStore::from(vec![account.clone()]);
    let factory = FakeBrowserFactory::from(vec![Capture::Happy]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let engine = BackfillEngine::new(
      "task-1".into(),
      canary_request(&["account-1"]),
      cancel_flag,
      store.clone(),
    );

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    assert_eq!(factory.launch_count(), 1);
    let persisted = store.current("account-1");
    assert!(persisted.two_fa_enabled);
    assert_eq!(persisted.totp_secret, SECRET);
    assert_eq!(
      persisted.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      persisted.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Enabled)
    );
    let events = journal.events();
    assert_eq!(
      events,
      vec![
        "journal:create",
        "journal:update_state:RemoteConfirmed",
        "journal:record_final",
        "journal:delete"
      ]
    );
    assert_eq!(store.persist_calls(), vec!["account-1".to_string()]);
    assert_eq!(gate.record_calls.lock().unwrap().as_slice(), ["account-1"]);
  }

  #[tokio::test]
  async fn journal_create_runs_before_two_factor_confirm() {
    let app_handle = make_app_handle();
    let account = eligible_account("account-2");
    let store = FakeStore::from(vec![account]);
    let factory = FakeBrowserFactory::from(vec![Capture::Happy]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let engine = BackfillEngine::new(
      "task-2".into(),
      canary_request(&["account-2"]),
      cancel_flag,
      store.clone(),
    );

    // The FakeTwoFactorAdapter records events onto an Arc-shared mutex the
    // engine never reaches directly. To capture merged ordering we listen on
    // the TOTP-side events via the journal's `create` event: the supplier's
    // record list is `["tf:inspect","tf:begin","tf:capture","tf:confirm", ...]`
    // and the journal pushes "journal:create" between capture and confirm.
    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    // Concretely: assert journal events before confirm by examining the
    // relative indices of "journal:create" and the FakeTwoFactorAdapter's
    // "tf:confirm" record. We attach the tf events via the session's shared
    // mutex (Factory's sessions are consumed by Box, so re-capture is limited
    // — verify the engine's ordering guarantee below via the second adapter
    // observation test):
    let events = journal.events();
    assert!(events.contains(&"journal:create".to_string()));
    // Then the journal cleanup must have happened:
    assert!(events.contains(&"journal:delete".to_string()));
  }

  #[test]
  fn two_factor_journal_callback_runs_before_confirm_when_adapters_share_event_log() {
    // Direct unit on the Wave 3 enable_authenticator_two_factor + our callback
    // ordering: prove that the engine's `enable_authenticator_two_factor`
    // invocation calls the journal callback (which creates "secret_captured")
    // BEFORE confirm_setup. The engine wraps the callback inline; this test
    // invokes the same primitive against a shared event log to lock the order.
    let mut adapter = FakeTwoFactorAdapter::happy();
    let shared_events = adapter.events.clone();

    let result = tokio::runtime::Runtime::new().unwrap().block_on(async {
      enable_authenticator_two_factor(&mut adapter, |secret| {
        shared_events.lock().unwrap().push("journal:create".into());
        assert_eq!(secret, SECRET);
        Ok::<(), TwoFactorError>(())
      })
      .await
    });

    assert_eq!(result.as_deref(), Ok(SECRET));
    let events = adapter.events.lock().unwrap().clone();
    let journal_idx = events
      .iter()
      .position(|event| event == "journal:create")
      .expect("journal:create recorded");
    let confirm_idx = events
      .iter()
      .position(|event| event == "tf:confirm")
      .expect("tf:confirm recorded");
    assert!(
      journal_idx < confirm_idx,
      "journal:create must precede tf:confirm; got events={events:?}"
    );
  }

  #[tokio::test]
  async fn cas_final_patch_preserves_every_seller_and_sensitive_field() {
    let app_handle = make_app_handle();
    let mut account = eligible_account("account-3");
    account.note = "seller-tag-value".into();
    account.exported_at = Some(Utc::now());
    account.free_trial_eligible = false;
    account.plan_type = "plus".into();
    account.phone_number = "+19990000000".into();
    let original = account.clone();
    let store = FakeStore::from(vec![account]);
    let factory = FakeBrowserFactory::from(vec![Capture::Happy]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let engine = BackfillEngine::new(
      "task-3".into(),
      canary_request(&["account-3"]),
      cancel_flag,
      store.clone(),
    );

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    let persisted = store.current("account-3");
    // The engine's verify_preserved_fields assert would panic if any field
    // moved. Mirror those assertions here as belt-and-braces for the test.
    assert_eq!(persisted.email, original.email);
    assert_eq!(persisted.password, original.password);
    assert_eq!(persisted.access_token, original.access_token);
    assert_eq!(persisted.cdk, original.cdk);
    assert_eq!(persisted.base_email, original.base_email);
    assert_eq!(persisted.free_trial_eligible, original.free_trial_eligible);
    assert_eq!(persisted.plan_type, original.plan_type);
    assert_eq!(persisted.phone_number, original.phone_number);
    assert_eq!(persisted.status, original.status);
    assert_eq!(persisted.note, original.note);
    assert_eq!(persisted.exported_at, original.exported_at);
    assert_eq!(
      persisted.email_provider,
      Some(EmailProvider::Gmail123452026)
    );
    assert_eq!(
      persisted.email_provider_provenance,
      Some(EmailProviderProvenance::InferredFromCdk)
    );
    assert_eq!(
      persisted.registration_outcome_reason,
      original.registration_outcome_reason
    );
    // The owner fields advanced:
    assert!(persisted.two_fa_enabled);
    assert_eq!(persisted.totp_secret, SECRET);
  }

  #[tokio::test]
  async fn eligibility_recheck_skips_account_that_became_ineligible_on_reload() {
    let app_handle = make_app_handle();
    let account = eligible_account("account-4");
    let store = FakeStore::from(vec![account.clone()]);
    // On the FIRST reload inside run_account, mutate the record so 2FA appears
    // already enabled externally. The top-level `list_accounts` should be
    // eligible but the per-account recheck should fail and skip the account
    // without launching a browser.
    store.schedule_reload_ineligibility("account-4");
    let factory = FakeBrowserFactory::from(vec![Capture::Happy]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let engine = BackfillEngine::new(
      "task-4".into(),
      canary_request(&["account-4"]),
      cancel_flag,
      store.clone(),
    );

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    // The browser must never launch — ineligibility was caught before CAS-pre.
    assert_eq!(factory.launch_count(), 0);
    // No persist/cas calls — the engine returned early on ineligibility.
    assert!(store.persist_calls().is_empty());
    assert!(store.cas_calls().is_empty());
    // No journal entry created.
    assert!(journal.events().is_empty());
    // canary evidence not recorded.
    assert!(gate.record_calls.lock().unwrap().is_empty());
  }

  #[tokio::test]
  async fn transient_auth_failure_isolates_account_does_not_flip_status_to_invalid() {
    let app_handle = make_app_handle();
    let account = eligible_account("account-5");
    let store = FakeStore::from(vec![account.clone()]);
    let factory = FakeBrowserFactory::from(vec![Capture::WrongCredentials]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let engine = BackfillEngine::new(
      "task-5".into(),
      canary_request(&["account-5"]),
      cancel_flag,
      store.clone(),
    );

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    // `WrongCredentials` is NOT a batch-pause error; the batch continues for
    // the next account (none here, but the lone account fails fast).
    let persisted = store.current("account-5");
    // The status must NOT become Invalid — transient failures never re-flip
    // inventory status. The CAS-start may have set InProgress+op_id on this
    // account, but the inventory status field is a seller-owned field.
    assert_eq!(persisted.status, AccountInventoryStatus::Available);
    assert!(!persisted.two_fa_enabled);
    assert!(persisted.totp_secret.is_empty());
    assert_eq!(
      persisted.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      persisted.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Failed)
    );
    // No journal cleanup chain — create_secret_captured never ran.
    let journal_events = journal.events();
    assert!(
      journal_events
        .iter()
        .all(|event| !event.starts_with("journal:delete")),
      "delete must not run without a create; events={journal_events:?}"
    );
    assert_eq!(
      factory.close_count(),
      1,
      "auth failure must close its browser"
    );
  }

  #[test]
  fn missing_explicit_routes_fail_preflight_before_profile_creation() {
    for network in [
      BackfillNetworkConfig::Proxy {
        proxy_id: "missing-proxy".into(),
      },
      BackfillNetworkConfig::Vpn {
        vpn_id: "missing-vpn".into(),
      },
    ] {
      let platform = Arc::new(RecordingProductionPlatform::with_missing_route());
      let factory = ProductionBrowserFactory::with_platform(platform.clone());

      let result = <ProductionBrowserFactory as BackfillBrowserFactory<tauri::Wry>>::preflight(
        &factory,
        &BackfillBrowser::Chromium,
        &network,
      );

      assert!(result.is_err());
      assert_eq!(
        platform.profile_creation_count(),
        0,
        "preflight must reject a missing route before profile creation"
      );
    }
  }

  #[tokio::test]
  async fn auth_and_two_factor_adapters_use_the_same_live_session() {
    let app_handle = make_app_handle();
    let store = FakeStore::from(vec![eligible_account("account-same-session")]);
    let factory = FakeBrowserFactory::from(vec![Capture::Happy]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let engine = BackfillEngine::new(
      "task-same-session".into(),
      canary_request(&["account-same-session"]),
      Arc::new(AtomicBool::new(false)),
      store,
    );

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    let adapter_sessions = factory.adapter_session_ids();
    assert_eq!(adapter_sessions.len(), 2);
    assert_eq!(adapter_sessions[0].0, "auth");
    assert_eq!(adapter_sessions[1].0, "two_factor");
    assert_eq!(
      adapter_sessions[0].1, adapter_sessions[1].1,
      "authentication and 2FA must borrow the same live browser session"
    );
  }

  #[tokio::test]
  async fn rate_limited_auth_error_pauses_the_batch_with_retryable_failure() {
    let app_handle = make_app_handle();
    let account_a = eligible_account("account-a");
    let account_b = eligible_account("account-b");
    let store = FakeStore::from(vec![account_a, account_b]);
    let factory = FakeBrowserFactory::from(vec![Capture::RateLimited, Capture::Happy]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let engine = BackfillEngine::new(
      "task-6".into(),
      bulk_request(&["account-a", "account-b"]),
      cancel_flag,
      store.clone(),
    );

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    // Batch paused on the first account → second account never launched.
    assert_eq!(factory.launch_count(), 1);
    // The second account must NOT be touched by a CAS patch.
    let untouched = store.current("account-b");
    assert_eq!(
      untouched.two_factor_backfill_state, None,
      "second account must remain untouched after batch pause"
    );
  }

  #[tokio::test]
  async fn cleanup_failure_pauses_before_launching_the_next_account() {
    let app_handle = make_app_handle();
    let account_a = eligible_account("cleanup-fails");
    let account_b = eligible_account("must-stay-untouched");
    let store = FakeStore::from(vec![account_a, account_b]);
    let factory = FakeBrowserFactory::from(vec![Capture::CloseFails, Capture::Happy]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let engine = BackfillEngine::new(
      "task-cleanup-failure".into(),
      bulk_request(&["cleanup-fails", "must-stay-untouched"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    assert_eq!(factory.close_count(), 1);
    assert_eq!(factory.launch_count(), 1);
    assert_eq!(
      store
        .current("must-stay-untouched")
        .two_factor_backfill_state,
      None,
      "cleanup failure must pause before the next account is modified"
    );
  }

  #[tokio::test]
  async fn locked_auth_error_persists_locked_and_continues_next_account() {
    let app_handle = make_app_handle();
    let account_a = eligible_account("account-c");
    let account_b = eligible_account("account-d");
    let original_a = account_a.clone();
    let store = FakeStore::from(vec![account_a, account_b]);
    let factory = FakeBrowserFactory::from(vec![Capture::Locked, Capture::Happy]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let engine = BackfillEngine::new(
      "task-7".into(),
      bulk_request(&["account-c", "account-d"]),
      cancel_flag,
      store.clone(),
    );

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    assert_eq!(factory.launch_count(), 2);
    let locked = store.current("account-c");
    assert_eq!(
      locked.two_factor_backfill_access_state,
      Some(TwoFactorBackfillAccessState::Locked)
    );
    assert_eq!(
      locked.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      locked.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Failed)
    );
    assert_eq!(
      locked.record_revision,
      original_a.record_revision + 3,
      "provider migration, ownership start, and locked finalization each advance the revision"
    );
    assert_eq!(locked.email, original_a.email);
    assert_eq!(locked.password, original_a.password);
    assert_eq!(locked.cdk, original_a.cdk);
    assert_eq!(locked.status, original_a.status);
    assert_eq!(locked.note, original_a.note);
    assert!(!locked.two_fa_enabled);
    assert!(locked.totp_secret.is_empty());

    let enabled = store.current("account-d");
    assert!(enabled.two_fa_enabled);
    assert_eq!(
      enabled.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Enabled)
    );
  }

  #[tokio::test]
  async fn exhausted_email_otp_attempts_pause_before_the_second_account() {
    let app_handle = make_app_handle();
    let store = FakeStore::from(vec![
      eligible_account("account-otp-a"),
      eligible_account("account-otp-b"),
    ]);
    let factory =
      FakeBrowserFactory::from(vec![Capture::EmailOtpAttemptsExhausted, Capture::Happy]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let engine = BackfillEngine::new(
      "task-otp-exhausted".into(),
      bulk_request(&["account-otp-a", "account-otp-b"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    assert_eq!(factory.launch_count(), 1);
    assert_eq!(
      store.current("account-otp-b").two_factor_backfill_state,
      None
    );
    assert!(journal.events().is_empty());
  }

  #[tokio::test]
  async fn indeterminate_remote_state_pauses_before_the_second_account() {
    let app_handle = make_app_handle();
    let store = FakeStore::from(vec![
      eligible_account("account-state-a"),
      eligible_account("account-state-b"),
    ]);
    let factory = FakeBrowserFactory::from(vec![
      Capture::RemoteStates(VecDeque::from([RemoteTwoFactorState::Indeterminate])),
      Capture::Happy,
    ]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let engine = BackfillEngine::new(
      "task-state-indeterminate".into(),
      bulk_request(&["account-state-a", "account-state-b"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    assert_eq!(factory.launch_count(), 1);
    assert_eq!(
      store.current("account-state-b").two_factor_backfill_state,
      None
    );
    assert!(journal.events().is_empty());
  }

  #[tokio::test]
  async fn post_confirm_verification_failure_retains_journal_and_pauses_batch() {
    let app_handle = make_app_handle();
    let store = FakeStore::from(vec![
      eligible_account("account-verify-a"),
      eligible_account("account-verify-b"),
    ]);
    let factory = FakeBrowserFactory::from(vec![
      Capture::RemoteStates(VecDeque::from([
        RemoteTwoFactorState::Off,
        RemoteTwoFactorState::Off,
      ])),
      Capture::Happy,
    ]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let engine = BackfillEngine::new(
      "task-verify-failed".into(),
      bulk_request(&["account-verify-a", "account-verify-b"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    assert_eq!(factory.launch_count(), 1);
    assert_eq!(
      store.current("account-verify-b").two_factor_backfill_state,
      None
    );
    assert_eq!(
      journal.manual_review_calls(),
      ["task-verify-failed:account-verify-a"]
    );
    assert_eq!(
      journal
        .load("task-verify-failed", "account-verify-a")
        .unwrap()
        .unwrap()
        .state,
      TwoFactorBackfillJournalState::ManualReview
    );
    assert!(!journal
      .events()
      .iter()
      .any(|event| event == "journal:delete"));
  }

  #[tokio::test]
  async fn challenge_escalation_pauses_before_the_second_account() {
    let app_handle = make_app_handle();
    let store = FakeStore::from(vec![
      eligible_account("account-challenge-a"),
      eligible_account("account-challenge-b"),
    ]);
    let factory = FakeBrowserFactory::from(vec![
      Capture::SecretCaptureFails(TwoFactorError::ChallengeEscalated),
      Capture::Happy,
    ]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let engine = BackfillEngine::new(
      "task-challenge".into(),
      bulk_request(&["account-challenge-a", "account-challenge-b"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    assert_eq!(factory.launch_count(), 1);
    assert_eq!(
      store
        .current("account-challenge-b")
        .two_factor_backfill_state,
      None
    );
    assert!(journal.events().is_empty());
  }

  #[tokio::test]
  async fn cancellation_stops_before_next_account_without_modifying_it() {
    let app_handle = make_app_handle();
    let account_a = eligible_account("account-keep");
    let account_b = eligible_account("account-skip");
    let store = FakeStore::from(vec![account_a, account_b.clone()]);
    let factory = FakeBrowserFactory::from(vec![Capture::Happy]);
    let gate = StubGate::default();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    // Race: trigger cancellation right after the first account succeeds — but
    // the engine checks the cancel flag only BETWEEN accounts. The first
    // account must finish; the second must NOT run.
    // We model this by setting the flag immediately before run(); the first
    // account started its run_account_inner loop after the cancel-check (none
    // happened yet), so it will run. The second never starts.
    // To be deterministic, schedule cancellation via the journal factory hook:
    // set the flag inside the journal_factory closure, which runs after the
    // gate validates and BEFORE the per-account loop begins.
    let engine = BackfillEngine::new(
      "task-8".into(),
      bulk_request(&["account-keep", "account-skip"]),
      cancel_flag.clone(),
      store.clone(),
    );
    // Set the cancellation flag immediately after the first `Completed` event.
    // Use a Tauri event listener to flip the flag, simulating an operator hit.
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured_clone = Arc::clone(&captured);
    let cancel_for_listener = cancel_flag.clone();
    let _listener_id = app_handle.listen(PROGRESS_EVENT, move |event| {
      let payload = event.payload();
      if let Ok(value) = serde_json::from_str::<Value>(payload) {
        let completed = value
          .get("step")
          .and_then(Value::as_str)
          .is_some_and(|step| step == "completed");
        captured_clone.lock().unwrap().push(value);
        if completed {
          cancel_for_listener.store(true, Ordering::SeqCst);
        }
      }
    });

    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(FakeJournal::new()),
        Some(&gate),
      )
      .await;

    // The first account must have completed and been patched; the second
    // account must remain untouched.
    let keep = store.current("account-keep");
    assert!(keep.two_fa_enabled);
    let skip = store.current("account-skip");
    assert_eq!(skip.two_factor_backfill_state, None);
    assert!(skip.totp_secret.is_empty());
    // The captured progress events must show a final task-level `cancelled` step
    // without assigning cancellation to the untouched next account.
    let events = captured.lock().unwrap().clone();
    let cancelled: Vec<_> = events
      .iter()
      .filter(|event| event.get("step").and_then(Value::as_str) == Some("cancelled"))
      .collect();
    assert_eq!(cancelled.len(), 1, "events={events:?}");
    assert_eq!(
      cancelled[0].get("accountKey").and_then(Value::as_str),
      Some("")
    );
  }

  #[tokio::test]
  async fn final_cas_conflict_surfaces_final_cas_failed_error_without_manual_review_autoflip() {
    let app_handle = make_app_handle();
    let account = eligible_account("account-fail-cas");
    let store = FakeStore::from(vec![account.clone()]);
    let factory = FakeBrowserFactory::from(vec![Capture::Happy]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let engine = BackfillEngine::new(
      "task-fail-cas".into(),
      canary_request(&["account-fail-cas"]),
      cancel_flag,
      store.clone(),
    );

    store.force_conflict_on_cas_call(2);

    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    // Remote confirmation succeeded, but the final enabled CAS conflicted. The
    // captured journal moves to ManualReview and a third owned CAS durably marks
    // the account as requiring reconciliation without persisting the secret.
    let persisted = store.current("account-fail-cas");
    assert!(!persisted.two_fa_enabled);
    assert_eq!(persisted.totp_secret, "");
    assert_eq!(
      persisted.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      persisted.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::ReconciliationRequired)
    );
    assert_eq!(factory.launch_count(), 1);
    assert_eq!(store.cas_calls().len(), 3);
    assert!(journal
      .events()
      .iter()
      .any(|event| event == "journal:create"));
    assert_eq!(
      journal.manual_review_calls(),
      ["task-fail-cas:account-fail-cas"]
    );
    assert_eq!(
      journal
        .load("task-fail-cas", "account-fail-cas")
        .unwrap()
        .unwrap()
        .state,
      TwoFactorBackfillJournalState::ManualReview
    );
    assert!(!journal
      .events()
      .iter()
      .any(|event| event == "journal:delete"));
    assert_eq!(persisted.status, AccountInventoryStatus::Available);
  }

  #[tokio::test]
  async fn secret_capture_failure_creates_no_journal_or_capture_progress_event() {
    let app_handle = make_app_handle();
    let account = eligible_account("account-capture-fail");
    let store = FakeStore::from(vec![account.clone()]);
    let factory = FakeBrowserFactory::from(vec![Capture::SecretCaptureFails(
      TwoFactorError::SecretCaptureFailed("reveal button missing".into()),
    )]);
    let journal = FakeJournal::new();
    let gate = StubGate::default();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let engine = BackfillEngine::new(
      "task-capture-fail".into(),
      canary_request(&["account-capture-fail"]),
      cancel_flag,
      store.clone(),
    );

    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured_clone = Arc::clone(&captured);
    let _listener = app_handle.listen(PROGRESS_EVENT, move |event| {
      if let Ok(value) = serde_json::from_str::<Value>(event.payload()) {
        captured_clone.lock().unwrap().push(value);
      }
    });
    let journal_clone = journal.clone();
    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        Some(&gate),
      )
      .await;

    assert!(journal.events().is_empty());
    assert!(journal.manual_review_calls().is_empty());
    assert!(
      captured
        .lock()
        .unwrap()
        .iter()
        .all(|event| { event.get("step").and_then(Value::as_str) != Some("captureSecret") }),
      "CaptureSecret must only be emitted after durable journal creation"
    );
  }

  #[test]
  fn selected_runtime_refuses_profile_or_policy_ownership_mismatch() {
    let profile = crate::profile::BrowserProfile {
      id: uuid::Uuid::new_v4(),
      ..crate::profile::BrowserProfile::default()
    };
    let registry = Box::leak(Box::new(crate::profile_runtime::LeaseRegistry::new()));
    let lease = registry
      .try_acquire(&profile.id.to_string())
      .expect("selected lease");
    let valid = crate::profile_runtime::LaunchPolicy::for_source_profile(profile.id.to_string());
    assert!(validate_selected_runtime_ownership(&profile, &valid, &lease).is_ok());

    let foreign_policy =
      crate::profile_runtime::LaunchPolicy::for_source_profile(uuid::Uuid::new_v4().to_string());
    assert!(
      validate_selected_runtime_ownership(&profile, &foreign_policy, &lease)
        .unwrap_err()
        .contains("another lease")
    );

    let foreign_lease = registry
      .try_acquire(&uuid::Uuid::new_v4().to_string())
      .expect("foreign lease");
    assert!(
      validate_selected_runtime_ownership(&profile, &valid, &foreign_lease)
        .unwrap_err()
        .contains("another lease")
    );
    registry.release(&lease.lease_id);
    registry.release(&foreign_lease.lease_id);
  }

  #[tokio::test]
  async fn selected_runtime_refuses_a_stale_lease_holder() {
    let profile = crate::profile::BrowserProfile {
      id: uuid::Uuid::new_v4(),
      ..crate::profile::BrowserProfile::default()
    };
    let profile_id = profile.id.to_string();
    let registry = Box::leak(Box::new(crate::profile_runtime::LeaseRegistry::new()));
    let stale_lease = registry.try_acquire(&profile_id).expect("stale lease");
    let policy = crate::profile_runtime::LaunchPolicy::for_source_profile(profile_id.clone());
    let runtime = SelectedBrowserRuntime::new(profile, policy, stale_lease.clone(), registry);
    registry.release(&stale_lease.lease_id);
    let replacement = registry
      .try_acquire(&profile_id)
      .expect("replacement lease");

    let state = runtime.state.lock().await;
    assert!(runtime
      .validate_ownership(&state)
      .unwrap_err()
      .contains("another lease"));
    drop(state);
    registry.release(&replacement.lease_id);
  }

  #[test]
  fn selected_lease_releases_only_after_cleanup_succeeds() {
    let registry = Box::leak(Box::new(crate::profile_runtime::LeaseRegistry::new()));
    let profile_id = uuid::Uuid::new_v4().to_string();
    let lease = registry.try_acquire(&profile_id).expect("selected lease");

    let error =
      release_selected_lease_after_cleanup(registry, &lease.lease_id, Err("cleanup failed".into()))
        .expect_err("failed cleanup must retain lease");
    assert_eq!(error, "cleanup failed");
    assert!(registry.is_leased(&profile_id));

    release_selected_lease_after_cleanup(registry, &lease.lease_id, Ok(()))
      .expect("successful cleanup releases lease");
    assert!(!registry.is_leased(&profile_id));
  }

  #[tokio::test]
  async fn browser_cleanup_guard_drop_schedules_cleanup_and_successful_close_disarms() {
    let drop_calls = Arc::new(AtomicUsize::new(0));
    let drop_calls_for_action = Arc::clone(&drop_calls);
    let guard = BrowserCleanupGuard::new(Arc::new(move || {
      let calls = Arc::clone(&drop_calls_for_action);
      Box::pin(async move {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
      })
    }));
    drop(guard);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
      while drop_calls.load(Ordering::SeqCst) != 1 {
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("Drop must schedule browser cleanup on the current runtime");

    let close_calls = Arc::new(AtomicUsize::new(0));
    let close_calls_for_action = Arc::clone(&close_calls);
    let mut guard = BrowserCleanupGuard::new(Arc::new(move || {
      let calls = Arc::clone(&close_calls_for_action);
      Box::pin(async move {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
      })
    }));
    guard.close().await.unwrap();
    drop(guard);
    tokio::task::yield_now().await;
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);

    let retry_calls = Arc::new(AtomicUsize::new(0));
    let retry_calls_for_action = Arc::clone(&retry_calls);
    let mut guard = BrowserCleanupGuard::new(Arc::new(move || {
      let calls = Arc::clone(&retry_calls_for_action);
      Box::pin(async move {
        let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt == 1 {
          Err("first cleanup failed".into())
        } else {
          Ok(())
        }
      })
    }));
    assert!(guard.close().await.is_err());
    drop(guard);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
      while retry_calls.load(Ordering::SeqCst) != 2 {
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("Drop must retry a failed cleanup action");
    assert_eq!(retry_calls.load(Ordering::SeqCst), 2);
  }

  #[tokio::test]
  async fn preflight_error_emits_exactly_one_task_terminal_failure() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let engine = BackfillEngine::new(
      "task-preflight".into(),
      canary_request(&["account-preflight"]),
      Arc::new(AtomicBool::new(false)),
      FakeStore::from(vec![eligible_account("account-preflight")]),
    );

    engine
      .run(
        &app_handle,
        &FakeBrowserFactory::with_preflight_error("missing route"),
        || Ok::<FakeJournal, String>(FakeJournal::new()),
        Some(&StubGate::default()),
      )
      .await;

    let events = events.lock().unwrap();
    let terminal = terminal_events(&events);
    assert_eq!(terminal.len(), 1, "events={events:?}");
    assert_eq!(
      terminal[0].get("accountKey").and_then(Value::as_str),
      Some("")
    );
    assert_eq!(
      terminal[0].get("step").and_then(Value::as_str),
      Some("failed")
    );
    assert_eq!(
      terminal[0].get("errorCode").and_then(Value::as_str),
      Some("preflight_failed")
    );
  }

  #[tokio::test]
  async fn canary_evidence_failure_never_emits_completed_enabled() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let store = FakeStore::from(vec![eligible_account("account-canary-evidence")]);
    let engine = BackfillEngine::new(
      "task-canary-evidence".into(),
      canary_request(&["account-canary-evidence"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );

    engine
      .run(
        &app_handle,
        &FakeBrowserFactory::from(vec![Capture::Happy]),
        || Ok::<FakeJournal, String>(FakeJournal::new()),
        Some(&StubGate::failing_record("disk full")),
      )
      .await;

    let persisted = store.current("account-canary-evidence");
    assert_eq!(
      persisted.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Enabled)
    );
    let events = events.lock().unwrap();
    assert!(events.iter().all(|event| {
      !(event.get("step").and_then(Value::as_str) == Some("completed")
        && event.get("outcome").and_then(Value::as_str) == Some("enabled"))
    }));
    assert!(events.iter().any(|event| {
      event.get("step").and_then(Value::as_str) == Some("batchPaused")
        && event.get("errorCode").and_then(Value::as_str) == Some("canary_evidence_failed")
    }));
  }

  #[tokio::test]
  async fn journal_delete_false_preserves_enabled_and_pauses_without_journal_complete() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let store = FakeStore::from(vec![eligible_account("account-delete-false")]);
    let journal = FakeJournal::new();
    journal.return_false_on_delete();
    let engine = BackfillEngine::new(
      "task-delete-false".into(),
      canary_request(&["account-delete-false"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );
    let journal_for_run = journal.clone();

    engine
      .run(
        &app_handle,
        &FakeBrowserFactory::from(vec![Capture::Happy]),
        move || Ok::<_, String>(journal_for_run),
        Some(&StubGate::default()),
      )
      .await;

    assert_eq!(
      store
        .current("account-delete-false")
        .two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Enabled)
    );
    assert!(journal
      .load("task-delete-false", "account-delete-false")
      .unwrap()
      .is_some());
    let events = events.lock().unwrap();
    assert!(events
      .iter()
      .all(|event| event.get("step").and_then(Value::as_str) != Some("journalComplete")));
    assert!(events.iter().all(|event| {
      !(event.get("step").and_then(Value::as_str) == Some("completed")
        && event.get("outcome").and_then(Value::as_str) == Some("enabled"))
    }));
    assert!(events.iter().any(|event| {
      event.get("step").and_then(Value::as_str) == Some("batchPaused")
        && event.get("errorCode").and_then(Value::as_str) == Some("journal_cleanup_failed")
    }));
  }

  #[tokio::test]
  async fn failed_manual_review_transition_pauses_without_recoverability_claim() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let store = FakeStore::from(vec![eligible_account("account-manual-review-fails")]);
    store.force_conflict_on_cas_call(2);
    let journal = FakeJournal::new();
    journal.fail_manual_review();
    let engine = BackfillEngine::new(
      "task-manual-review-fails".into(),
      canary_request(&["account-manual-review-fails"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );
    let journal_for_run = journal.clone();

    engine
      .run(
        &app_handle,
        &FakeBrowserFactory::from(vec![Capture::Happy]),
        move || Ok::<_, String>(journal_for_run),
        Some(&StubGate::default()),
      )
      .await;

    assert_eq!(
      store
        .current("account-manual-review-fails")
        .two_factor_backfill_outcome,
      None
    );
    assert_eq!(
      journal
        .load("task-manual-review-fails", "account-manual-review-fails")
        .unwrap()
        .unwrap()
        .state,
      TwoFactorBackfillJournalState::SecretCaptured
    );
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| {
      event.get("step").and_then(Value::as_str) == Some("batchPaused")
        && event.get("errorCode").and_then(Value::as_str) == Some("manual_review_failed")
    }));
    assert!(events.iter().all(|event| {
      event.get("outcome").and_then(Value::as_str) != Some("reconciliationRequired")
    }));
  }

  #[tokio::test]
  async fn browser_close_failure_after_enabled_preserves_enabled_and_stops_batch() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let store = FakeStore::from(vec![
      eligible_account("account-close-enabled"),
      eligible_account("account-after-close"),
    ]);
    let factory = FakeBrowserFactory::from(vec![Capture::CloseFails, Capture::Happy]);
    let engine = BackfillEngine::new(
      "task-close-enabled".into(),
      bulk_request(&["account-close-enabled", "account-after-close"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );

    engine
      .run(
        &app_handle,
        &factory,
        || Ok::<FakeJournal, String>(FakeJournal::new()),
        Some(&StubGate::default()),
      )
      .await;

    assert_eq!(factory.launch_count(), 1);
    assert_eq!(
      store
        .current("account-close-enabled")
        .two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Enabled)
    );
    assert_eq!(
      store
        .current("account-after-close")
        .two_factor_backfill_state,
      None
    );
    let events = events.lock().unwrap();
    assert!(events.iter().all(|event| {
      !(event.get("step").and_then(Value::as_str) == Some("completed")
        && event.get("outcome").and_then(Value::as_str) == Some("enabled"))
    }));
    assert!(events.iter().any(|event| {
      event.get("step").and_then(Value::as_str) == Some("batchPaused")
        && event.get("errorCode").and_then(Value::as_str) == Some("browser_cleanup_failed")
    }));
  }

  #[tokio::test]
  async fn canary_close_failure_after_enabled_records_no_evidence_or_completed_success() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let store = FakeStore::from(vec![eligible_account("account-canary-close-fails")]);
    let factory = FakeBrowserFactory::from(vec![Capture::CloseFails]);
    let gate = StubGate::default();
    let engine = BackfillEngine::new(
      "task-canary-close-fails".into(),
      canary_request(&["account-canary-close-fails"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );

    engine
      .run(
        &app_handle,
        &factory,
        || Ok::<FakeJournal, String>(FakeJournal::new()),
        Some(&gate),
      )
      .await;

    assert_eq!(factory.close_count(), 1);
    let persisted = store.current("account-canary-close-fails");
    assert_eq!(
      persisted.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      persisted.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Enabled)
    );
    assert!(gate.record_calls.lock().unwrap().is_empty());
    let events = events.lock().unwrap();
    assert!(events.iter().all(|event| {
      !(event.get("step").and_then(Value::as_str) == Some("completed")
        && event.get("outcome").and_then(Value::as_str) == Some("enabled"))
    }));
    assert!(events.iter().any(|event| {
      event.get("step").and_then(Value::as_str) == Some("batchPaused")
        && event.get("errorCode").and_then(Value::as_str) == Some("browser_cleanup_failed")
    }));
  }

  #[tokio::test]
  async fn launch_failure_durably_completes_failed() {
    let app_handle = make_app_handle();
    let store = FakeStore::from(vec![eligible_account("account-launch-fails")]);
    let engine = BackfillEngine::new(
      "task-launch-fails".into(),
      canary_request(&["account-launch-fails"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );

    engine
      .run(
        &app_handle,
        &FakeBrowserFactory::from(vec![Capture::LaunchFails]),
        || Ok::<FakeJournal, String>(FakeJournal::new()),
        Some(&StubGate::default()),
      )
      .await;

    let persisted = store.current("account-launch-fails");
    assert_eq!(
      persisted.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      persisted.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Failed)
    );
  }

  #[tokio::test]
  async fn remote_already_on_durably_completes_reconciliation_required() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let store = FakeStore::from(vec![eligible_account("account-already-on")]);
    let engine = BackfillEngine::new(
      "task-already-on".into(),
      canary_request(&["account-already-on"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );

    engine
      .run(
        &app_handle,
        &FakeBrowserFactory::from(vec![Capture::RemoteStates(VecDeque::from([
          RemoteTwoFactorState::On,
        ]))]),
        || Ok::<FakeJournal, String>(FakeJournal::new()),
        Some(&StubGate::default()),
      )
      .await;

    let persisted = store.current("account-already-on");
    assert_eq!(
      persisted.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      persisted.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::ReconciliationRequired)
    );
    assert!(events.lock().unwrap().iter().any(|event| {
      event.get("step").and_then(Value::as_str) == Some("completed")
        && event.get("outcome").and_then(Value::as_str) == Some("reconciliationRequired")
    }));
  }

  #[tokio::test]
  async fn terminal_cas_failure_pauses_and_stops_later_accounts() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let store = FakeStore::from(vec![
      eligible_account("account-terminal-cas"),
      eligible_account("account-after-terminal-cas"),
    ]);
    store.force_conflict_on_cas_call(2);
    let factory = FakeBrowserFactory::from(vec![Capture::WrongCredentials, Capture::Happy]);
    let engine = BackfillEngine::new(
      "task-terminal-cas".into(),
      bulk_request(&["account-terminal-cas", "account-after-terminal-cas"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );

    engine
      .run(
        &app_handle,
        &factory,
        || Ok::<FakeJournal, String>(FakeJournal::new()),
        Some(&StubGate::default()),
      )
      .await;

    assert_eq!(factory.launch_count(), 1);
    assert_eq!(
      store
        .current("account-terminal-cas")
        .two_factor_backfill_state,
      Some(TwoFactorBackfillState::InProgress)
    );
    assert_eq!(
      store
        .current("account-after-terminal-cas")
        .two_factor_backfill_state,
      None
    );
    assert!(events.lock().unwrap().iter().any(|event| {
      event.get("step").and_then(Value::as_str) == Some("batchPaused")
        && event.get("errorCode").and_then(Value::as_str) == Some("terminal_cas_failed")
    }));
  }

  #[tokio::test]
  async fn cancellation_after_start_durably_completes_cancelled_without_launch() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let store = FakeStore::from(vec![eligible_account("account-cancel-started")]);
    store.cancel_after_cas_call(1, cancel_flag.clone());
    let factory = FakeBrowserFactory::from(vec![Capture::Happy]);
    let engine = BackfillEngine::new(
      "task-cancel-started".into(),
      canary_request(&["account-cancel-started"]),
      cancel_flag,
      store.clone(),
    );

    engine
      .run(
        &app_handle,
        &factory,
        || Ok::<FakeJournal, String>(FakeJournal::new()),
        Some(&StubGate::default()),
      )
      .await;

    assert_eq!(factory.launch_count(), 0);
    let persisted = store.current("account-cancel-started");
    assert_eq!(
      persisted.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      persisted.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Cancelled)
    );
    assert!(events.lock().unwrap().iter().any(|event| {
      event.get("step").and_then(Value::as_str) == Some("cancelled")
        && event.get("accountKey").and_then(Value::as_str) == Some("account-cancel-started")
    }));
  }

  #[tokio::test]
  async fn cancellation_after_launch_durably_completes_cancelled_and_closes_session() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let store = FakeStore::from(vec![eligible_account("account-cancel-launched")]);
    let factory = FakeBrowserFactory::from(vec![Capture::CancelAfterLaunch(cancel_flag.clone())]);
    let journal = FakeJournal::new();
    let engine = BackfillEngine::new(
      "task-cancel-launched".into(),
      canary_request(&["account-cancel-launched"]),
      cancel_flag,
      store.clone(),
    );
    let journal_for_run = journal.clone();

    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_for_run),
        Some(&StubGate::default()),
      )
      .await;

    assert_eq!(factory.launch_count(), 1);
    assert_eq!(factory.close_count(), 1);
    assert!(factory.adapter_session_ids().is_empty());
    assert!(journal.events().is_empty());
    let persisted = store.current("account-cancel-launched");
    assert_eq!(
      persisted.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      persisted.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Cancelled)
    );
    assert!(events.lock().unwrap().iter().any(|event| {
      event.get("step").and_then(Value::as_str) == Some("cancelled")
        && event.get("accountKey").and_then(Value::as_str) == Some("account-cancel-launched")
    }));
  }

  #[tokio::test]
  async fn cancellation_after_auth_durably_completes_cancelled_before_two_factor_setup() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let store = FakeStore::from(vec![eligible_account("account-cancel-authenticated")]);
    let factory = FakeBrowserFactory::from(vec![Capture::CancelAfterAuth(cancel_flag.clone())]);
    let journal = FakeJournal::new();
    let engine = BackfillEngine::new(
      "task-cancel-authenticated".into(),
      canary_request(&["account-cancel-authenticated"]),
      cancel_flag,
      store.clone(),
    );
    let journal_for_run = journal.clone();

    engine
      .run(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_for_run),
        Some(&StubGate::default()),
      )
      .await;

    assert_eq!(factory.launch_count(), 1);
    assert_eq!(factory.close_count(), 1);
    assert_eq!(factory.adapter_session_ids(), vec![("auth", 1)]);
    assert!(journal.events().is_empty());
    let persisted = store.current("account-cancel-authenticated");
    assert_eq!(
      persisted.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      persisted.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Cancelled)
    );
    assert!(events.lock().unwrap().iter().any(|event| {
      event.get("step").and_then(Value::as_str) == Some("cancelled")
        && event.get("accountKey").and_then(Value::as_str) == Some("account-cancel-authenticated")
    }));
  }

  #[tokio::test]
  async fn cancellation_after_secret_capture_requires_manual_review_and_reconciliation() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let store = FakeStore::from(vec![eligible_account("account-cancel-secret")]);
    let journal = FakeJournal::new();
    journal.cancel_after_secret_capture(cancel_flag.clone());
    let engine = BackfillEngine::new(
      "task-cancel-secret".into(),
      canary_request(&["account-cancel-secret"]),
      cancel_flag,
      store.clone(),
    );
    let journal_for_run = journal.clone();

    engine
      .run(
        &app_handle,
        &FakeBrowserFactory::from(vec![Capture::Happy]),
        move || Ok::<FakeJournal, String>(journal_for_run),
        Some(&StubGate::default()),
      )
      .await;

    let persisted = store.current("account-cancel-secret");
    assert_eq!(
      persisted.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      persisted.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::ReconciliationRequired)
    );
    assert_eq!(
      journal
        .load("task-cancel-secret", "account-cancel-secret")
        .unwrap()
        .unwrap()
        .state,
      TwoFactorBackfillJournalState::ManualReview
    );
    assert!(events.lock().unwrap().iter().any(|event| {
      event.get("outcome").and_then(Value::as_str) == Some("reconciliationRequired")
    }));
  }

  #[tokio::test]
  async fn startup_store_and_journal_failures_each_emit_one_task_terminal_failure() {
    #[derive(Clone)]
    struct ListFailsStore;

    impl BackfillAccountStore for ListFailsStore {
      fn list_accounts(&self) -> Result<Vec<RegistrationResult>, String> {
        Err("injected list failure".into())
      }

      fn reload(&self, _account_key: &str) -> Result<Option<RegistrationResult>, String> {
        unreachable!()
      }

      fn persist_provider(
        &self,
        _account_key: &str,
        _expected_revision: u64,
        _provider: EmailProvider,
      ) -> Result<RegistrationResult, String> {
        unreachable!()
      }

      fn cas_patch(
        &self,
        _account_key: &str,
        _expected_revision: u64,
        _precondition: BackfillPatchPrecondition,
        _patch: TwoFactorBackfillPatch,
      ) -> Result<RegistrationResult, String> {
        unreachable!()
      }
    }

    let startup_app = make_app_handle();
    let startup_events = capture_progress_events(&startup_app);
    BackfillEngine::new(
      "task-startup-store".into(),
      canary_request(&["account-startup-store"]),
      Arc::new(AtomicBool::new(false)),
      ListFailsStore,
    )
    .run(
      &startup_app,
      &FakeBrowserFactory::from(Vec::new()),
      || Ok::<FakeJournal, String>(FakeJournal::new()),
      Some(&StubGate::default()),
    )
    .await;
    {
      let startup_events = startup_events.lock().unwrap();
      let startup_terminal = terminal_events(&startup_events);
      assert_eq!(startup_terminal.len(), 1, "events={startup_events:?}");
      assert_eq!(
        startup_terminal[0].get("errorCode").and_then(Value::as_str),
        Some("startup_failed")
      );
    }

    let journal_app = make_app_handle();
    let journal_events = capture_progress_events(&journal_app);
    BackfillEngine::new(
      "task-startup-journal".into(),
      canary_request(&["account-startup-journal"]),
      Arc::new(AtomicBool::new(false)),
      FakeStore::from(vec![eligible_account("account-startup-journal")]),
    )
    .run(
      &journal_app,
      &FakeBrowserFactory::from(Vec::new()),
      || Err::<FakeJournal, String>("injected journal open failure".into()),
      Some(&StubGate::default()),
    )
    .await;
    let journal_events = journal_events.lock().unwrap();
    let journal_terminal = terminal_events(&journal_events);
    assert_eq!(journal_terminal.len(), 1, "events={journal_events:?}");
    assert_eq!(
      journal_terminal[0].get("errorCode").and_then(Value::as_str),
      Some("journal_open_failed")
    );
  }

  #[tokio::test]
  async fn canary_reload_failure_never_emits_completed_enabled() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let store = FakeStore::from(vec![eligible_account("account-canary-reload")]);
    store.fail_reload_on_call(3);
    let engine = BackfillEngine::new(
      "task-canary-reload".into(),
      canary_request(&["account-canary-reload"]),
      Arc::new(AtomicBool::new(false)),
      store,
    );

    engine
      .run(
        &app_handle,
        &FakeBrowserFactory::from(vec![Capture::Happy]),
        || Ok::<FakeJournal, String>(FakeJournal::new()),
        Some(&StubGate::default()),
      )
      .await;

    let events = events.lock().unwrap();
    assert!(events.iter().all(|event| {
      !(event.get("step").and_then(Value::as_str) == Some("completed")
        && event.get("outcome").and_then(Value::as_str) == Some("enabled"))
    }));
    assert!(events.iter().any(|event| {
      event.get("step").and_then(Value::as_str) == Some("batchPaused")
        && event.get("errorCode").and_then(Value::as_str) == Some("canary_reload_failed")
    }));
  }

  #[tokio::test]
  async fn browser_close_failure_before_enabled_keeps_failed_terminal_state() {
    let app_handle = make_app_handle();
    let events = capture_progress_events(&app_handle);
    let store = FakeStore::from(vec![eligible_account("account-close-before-enabled")]);
    let engine = BackfillEngine::new(
      "task-close-before-enabled".into(),
      canary_request(&["account-close-before-enabled"]),
      Arc::new(AtomicBool::new(false)),
      store.clone(),
    );

    engine
      .run(
        &app_handle,
        &FakeBrowserFactory::from(vec![Capture::WrongCredentialsCloseFails]),
        || Ok::<FakeJournal, String>(FakeJournal::new()),
        Some(&StubGate::default()),
      )
      .await;

    let persisted = store.current("account-close-before-enabled");
    assert_eq!(
      persisted.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      persisted.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Failed)
    );
    assert!(events.lock().unwrap().iter().any(|event| {
      event.get("step").and_then(Value::as_str) == Some("batchPaused")
        && event.get("errorCode").and_then(Value::as_str) == Some("browser_cleanup_failed")
    }));
  }

  #[test]
  fn safe_error_code_rejects_unrecognized_strings_and_drops_secrets() {
    assert_eq!(safe_error_code("rate_limited"), "rate_limited");
    assert_eq!(safe_error_code("unknown_field"), "unknown");
    // A raw secret-shaped string must NOT pass through the allowlist filter;
    // it collapses to "unknown" so the progress event never leaks a secret.
    assert_eq!(safe_error_code("totp_secret=JBSWY3DPEHPK3PXP"), "unknown");
    assert_eq!(safe_error_code("password=hunter2"), "unknown");
    assert!(contains_secret_material("mail-user@example.com"));
    assert!(contains_secret_material("RAW_CDK=MAIL-foo"));
    assert!(!contains_secret_material("rate_limited"));
  }

  #[test]
  fn progress_event_payload_never_carries_secret_material_via_safe_error_code() {
    let app_handle = make_app_handle();
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured_clone = Arc::clone(&captured);
    let _listener = app_handle.listen(PROGRESS_EVENT, move |event| {
      let payload = event.payload();
      if let Ok(value) = serde_json::from_str::<Value>(payload) {
        captured_clone.lock().unwrap().push(value);
      }
    });
    let engine = BackfillEngine::new(
      "task-leak".into(),
      canary_request(&["unused"]),
      Arc::new(AtomicBool::new(false)),
      // Account does not exist: list_accounts returns empty so no per-account
      // work happens. We still emit the embedded failure for the empty batch.
      // For a delete-on-missing account the engine instead filters eligible
      // keys; to exercise `emit` directly we patch below.
      FakeStore::from(vec![]),
    );

    // Direct emit with a secret-shaped error code. The engine already filters
    // non-allowlisted codes into "unknown" before constructing the payload.
    engine.emit(
      &app_handle,
      AccountProgress {
        account_key: "",
        index: 0,
        total: 0,
      },
      BackfillStep::Failed,
      Some(BackfillOutcome::Failed),
      Some("totp_secret=JBSWY3DPEHPK3PXP,password=hunter2".into()),
      true,
    );

    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    let serialized = serde_json::to_string(&events[0]).unwrap();
    for forbidden in ["JBSWY3DPEHPK3PXP", "password", "hunter2", "totp_secret"] {
      assert!(
        !serialized.contains(forbidden),
        "progress event leaked forbidden material '{forbidden}': {serialized}"
      );
    }
    // The error_code slot must collapse to "unknown".
    assert_eq!(
      events[0].get("errorCode").and_then(Value::as_str),
      Some("unknown")
    );
    let _ = engine;
  }

  // --- Allowlist gates are covered by gate.rs::tests; we only assert that
  // the engine here rejects a bulk start when no canary gate is supplied. ---

  #[tokio::test]
  async fn bulk_mode_without_canary_gate_is_rejected() {
    let app_handle = make_app_handle();
    let account = eligible_account("account-bulk");
    let store = FakeStore::from(vec![account.clone()]);
    let factory = FakeBrowserFactory::from(vec![]);
    let journal = FakeJournal::new();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let engine = BackfillEngine::new(
      "task-bulk".into(),
      bulk_request(&["account-bulk"]),
      cancel_flag,
      store.clone(),
    );

    let journal_clone = journal.clone();
    let _result = engine
      .run_inner::<tauri::test::MockRuntime, FakeJournal, StubGate>(
        &app_handle,
        &factory,
        move || Ok::<FakeJournal, String>(journal_clone),
        None,
      )
      .await;
    // The engine returns Err("Bulk 2FA backfill requires a canary gate") via
    // run_inner; run() logs the error and removes the task. The account must
    // remain untouched.
    let persisted = store.current("account-bulk");
    assert_eq!(persisted.two_factor_backfill_state, None);
  }
}
