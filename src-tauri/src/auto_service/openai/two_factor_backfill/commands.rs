//! Tauri commands for the 2FA backfill repair workflow.
//!
//! Three commands drive the operator-facing flow:
//! - `preview_two_factor_backfill` — backend-authoritative eligibility preview.
//! - `start_two_factor_backfill` — launch a serial repair task (Canary or Bulk).
//! - `cancel_two_factor_backfill` — cooperatively stop the active account and
//!   prevent any later account from starting.
//!
//! The start request carries only selection + routing metadata. Credentials,
//! CDK material and TOTP secrets never appear in the request or the emitted
//! progress events; the engine pulls them from the credential store at runtime.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use super::eligibility::evaluate_eligibility;
use super::engine::{
  stable_account_key, BackfillEngine, ProductionAccountStore, ProductionBrowserFactory,
  SelectedBrowserRuntime,
};
use super::gate::CanaryGate;
use super::journal::{
  PersistedBackfillAccountPatch, TwoFactorBackfillJournal, TwoFactorBackfillJournalState,
};
use super::task;
use super::types::{
  TwoFactorBackfillPreview, TwoFactorBackfillPreviewRequest, TwoFactorBackfillRecoveryResult,
  TwoFactorBackfillRecoverySummary,
};
use crate::auto_service::openai::register::store::{
  compare_and_update_registered_account, list_registered_accounts, BackfillPatchPrecondition,
  TwoFactorBackfillPatch,
};
use crate::auto_service::openai::register::types::{
  RegistrationResult, TwoFactorBackfillOutcome, TwoFactorBackfillState,
};
use crate::profile::BrowserProfile;
use crate::profile_runtime::{
  DataMode, FingerprintMode, LaunchPolicy, LeaseCleanupGuard, LeaseError, LeaseRegistry,
};

/// Browser kernel used for the repair. Mirrors the auto-register choice so the
/// canary evidence stays scoped per kernel.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BackfillBrowser {
  Chromium,
  Camoufox,
}

/// Explicit network route. The operator must choose one because records do not
/// persist the proxy/VPN provenance used at registration time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackfillNetworkConfig {
  None,
  Proxy {
    #[serde(rename = "proxyId", alias = "proxy_id")]
    proxy_id: String,
  },
  Vpn {
    #[serde(rename = "vpnId", alias = "vpn_id")]
    vpn_id: String,
  },
}

/// Canary runs one account and unlocks matching bulk evidence; Bulk is rejected
/// until a fresh canary produced evidence for every browser/provider/network
/// combination present in the selection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BackfillMode {
  Canary,
  Bulk,
}

/// Start request. The operator is forced to acknowledge legacy/manual-review
/// policy implications so the workflow never silently repairs an account.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TwoFactorBackfillStartRequest {
  pub selected_account_keys: Vec<String>,
  #[serde(default)]
  pub allow_free_trial_no: bool,
  #[serde(default)]
  pub acknowledge_legacy_access: bool,
  #[serde(default)]
  pub profile_id: Option<String>,
  #[serde(default)]
  pub data_mode: DataMode,
  #[serde(default)]
  pub fingerprint_mode: FingerprintMode,
  pub browser: BackfillBrowser,
  pub network: BackfillNetworkConfig,
  pub mode: BackfillMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TwoFactorBackfillPreviewCommandRequest {
  pub selected_account_keys: Vec<String>,
  #[serde(default)]
  pub allow_free_trial_no: bool,
  #[serde(default)]
  pub acknowledge_legacy_access: bool,
  #[serde(default)]
  pub profile_id: Option<String>,
  #[serde(default)]
  pub data_mode: DataMode,
  #[serde(default)]
  pub fingerprint_mode: FingerprintMode,
  pub browser: BackfillBrowser,
  pub network: BackfillNetworkConfig,
}

/// Preview which selected accounts are eligible and whether matching canary
/// evidence unlocks Bulk for the requested browser and network route.
#[tauri::command]
pub fn preview_two_factor_backfill(
  request: TwoFactorBackfillPreviewCommandRequest,
) -> Result<TwoFactorBackfillPreview, String> {
  let accounts = crate::auto_service::openai::register::store::list_registered_accounts()?;
  let mut preview = evaluate_eligibility(
    &TwoFactorBackfillPreviewRequest {
      selected_account_keys: request.selected_account_keys,
      allow_free_trial_no: request.allow_free_trial_no,
      acknowledge_legacy_access: request.acknowledge_legacy_access,
    },
    &accounts,
  );
  let eligible_keys: Vec<String> = preview
    .accounts
    .iter()
    .filter(|account| account.eligible)
    .map(|account| account.account_key.clone())
    .collect();
  let providers = preview
    .accounts
    .iter()
    .filter(|account| account.eligible)
    .filter_map(|account| account.email_provider)
    .collect::<Vec<_>>();

  preview.bulk_available = !eligible_keys.is_empty()
    && CanaryGate::new()
      .map(|gate| {
        gate.bulk_available(
          &eligible_keys,
          &request.browser,
          &providers,
          &request.network,
        )
      })
      .unwrap_or(false);

  Ok(preview)
}

/// List incomplete private journals without exposing pending TOTP secrets.
#[tauri::command]
pub fn list_two_factor_backfill_recovery() -> Result<Vec<TwoFactorBackfillRecoverySummary>, String>
{
  let journal = TwoFactorBackfillJournal::new()?;
  journal.list().map(|entries| {
    entries
      .iter()
      .map(TwoFactorBackfillJournal::summarize)
      .collect()
  })
}

/// Reconcile a journal after a crash. This only auto-completes when the account
/// already contains the exact journaled secret under the owning operation; all
/// other states are moved to explicit manual review without remote actions.
#[tauri::command]
pub fn recover_two_factor_backfill_journal(
  operation_id: String,
  account_key: String,
) -> Result<TwoFactorBackfillRecoveryResult, String> {
  let mut journal = TwoFactorBackfillJournal::new()?;
  let account = list_registered_accounts()?
    .into_iter()
    .find(|account| stable_account_key(account) == account_key);
  recover_journal_entry(
    &mut journal,
    &operation_id,
    &account_key,
    account.as_ref(),
    |account| {
      compare_and_update_registered_account(
        &account_key,
        account.record_revision,
        BackfillPatchPrecondition::finalize_reconciliation_required(&operation_id, true, true),
        TwoFactorBackfillPatch::finalize_reconciliation_required(),
      )
      .map(|_| ())
    },
  )
}

fn recover_journal_entry(
  journal: &mut TwoFactorBackfillJournal,
  operation_id: &str,
  account_key: &str,
  account: Option<&RegistrationResult>,
  finalize_manual_review: impl FnOnce(&RegistrationResult) -> Result<(), String>,
) -> Result<TwoFactorBackfillRecoveryResult, String> {
  let Some(mut entry) = journal.load(operation_id, account_key)? else {
    return Err("2FA backfill recovery entry was not found".into());
  };
  let exact_final_state = entry.state != TwoFactorBackfillJournalState::ManualReview
    && account.is_some_and(|account| {
      account.two_fa_enabled
        && account.totp_secret == entry.pending_totp_secret
        && account.two_factor_backfill_state == Some(TwoFactorBackfillState::Completed)
        && account.two_factor_backfill_outcome == Some(TwoFactorBackfillOutcome::Enabled)
        && account.two_factor_backfill_operation_id.as_deref() == Some(operation_id)
    });

  if !exact_final_state {
    if entry.state != TwoFactorBackfillJournalState::ManualReview {
      entry =
        journal.transition_to_manual_review(operation_id, account_key, entry.journal_revision)?;
    }
    if let Some(account) = account {
      if account.two_factor_backfill_state == Some(TwoFactorBackfillState::InProgress)
        && account.two_factor_backfill_operation_id.as_deref() == Some(operation_id)
      {
        finalize_manual_review(account)?;
      }
    }
    return Ok(TwoFactorBackfillRecoveryResult {
      summary: TwoFactorBackfillJournal::summarize(&entry),
      recovered: false,
      requires_manual_review: true,
    });
  }

  let account = account.expect("exact final state requires an account");
  if entry.state == TwoFactorBackfillJournalState::SecretCaptured {
    entry = journal.update_state(
      operation_id,
      account_key,
      entry.journal_revision,
      TwoFactorBackfillJournalState::RemoteConfirmed,
    )?;
  }
  let final_revision = entry
    .final_account_revision
    .unwrap_or(account.record_revision);
  if final_revision != account.record_revision || final_revision <= entry.expected_account_revision
  {
    let reviewed =
      journal.transition_to_manual_review(operation_id, account_key, entry.journal_revision)?;
    return Ok(TwoFactorBackfillRecoveryResult {
      summary: TwoFactorBackfillJournal::summarize(&reviewed),
      recovered: false,
      requires_manual_review: true,
    });
  }
  if entry.final_account_revision.is_none() {
    entry = journal.record_final_account_revision(
      operation_id,
      account_key,
      entry.journal_revision,
      final_revision,
    )?;
  }
  journal.delete_after_account_patch(
    operation_id,
    account_key,
    entry.journal_revision,
    PersistedBackfillAccountPatch::new(
      account.record_revision,
      Some(operation_id),
      account.two_fa_enabled,
      &account.totp_secret,
    ),
  )?;

  Ok(TwoFactorBackfillRecoveryResult {
    summary: TwoFactorBackfillJournal::summarize(&entry),
    recovered: true,
    requires_manual_review: false,
  })
}

#[derive(Debug)]
enum StaleRecoveryAction {
  Finalize,
  CleanupOnly,
}

fn stale_recovery_action(
  account: &RegistrationResult,
  operation_id: &str,
  has_journal: bool,
) -> Result<StaleRecoveryAction, String> {
  if has_journal {
    return Err("stale 2FA backfill has a private journal entry; use journal recovery".into());
  }
  if account.two_factor_backfill_operation_id.as_deref() != Some(operation_id) {
    return Err("stale 2FA backfill operation ownership conflict".into());
  }
  match (
    account.two_factor_backfill_state,
    account.two_factor_backfill_outcome,
  ) {
    (Some(TwoFactorBackfillState::InProgress), None) => Ok(StaleRecoveryAction::Finalize),
    (Some(TwoFactorBackfillState::Completed), Some(TwoFactorBackfillOutcome::Cancelled)) => {
      Ok(StaleRecoveryAction::CleanupOnly)
    }
    _ => Err("stale 2FA backfill does not own a recoverable account state".into()),
  }
}

fn is_generated_backfill_worker(
  profile: &crate::profile::BrowserProfile,
  operation_id: &str,
) -> bool {
  profile.ephemeral && profile.name == format!("twofa-backfill-worker-{operation_id}")
}

pub async fn recover_stale_two_factor_backfill(
  app_handle: AppHandle,
  operation_id: String,
  account_key: String,
  profile_id: String,
) -> Result<(), String> {
  let operation_id = operation_id.trim();
  let account_key = account_key.trim();
  let profile_id = profile_id.trim();
  if operation_id.is_empty() || account_key.is_empty() || profile_id.is_empty() {
    return Err("stale 2FA backfill recovery requires operation, account and profile IDs".into());
  }

  let profile_id = uuid::Uuid::parse_str(profile_id)
    .map_err(|_| "stale worker profile ID is not a UUID".to_string())?
    .to_string();
  let profile = crate::profile::ProfileManager::instance()
    .list_profiles()
    .map_err(|error| format!("list worker profiles: {error}"))?
    .into_iter()
    .find(|profile| profile.id.to_string() == profile_id)
    .ok_or_else(|| "stale 2FA worker profile was not found".to_string())?;
  if !is_generated_backfill_worker(&profile, operation_id) {
    return Err("refusing to recover a worker profile owned by another operation".into());
  }

  let journal = TwoFactorBackfillJournal::new()?;
  let account = list_registered_accounts()?
    .into_iter()
    .find(|account| stable_account_key(account) == account_key)
    .ok_or_else(|| format!("Registered account {account_key} not found"))?;
  let recovery_action = stale_recovery_action(
    &account,
    operation_id,
    journal.load(operation_id, account_key)?.is_some(),
  )?;

  if matches!(recovery_action, StaleRecoveryAction::Finalize) {
    compare_and_update_registered_account(
      account_key,
      account.record_revision,
      BackfillPatchPrecondition::finalize_cancelled(operation_id, false, false),
      TwoFactorBackfillPatch::finalize_cancelled(),
    )?;
  }
  cleanup_stale_worker_profile(&app_handle, operation_id, &profile_id).await
}

async fn cleanup_stale_worker_profile(
  app_handle: &AppHandle,
  operation_id: &str,
  profile_id: &str,
) -> Result<(), String> {
  let profile_id = uuid::Uuid::parse_str(profile_id.trim())
    .map_err(|_| "stale worker profile ID is not a UUID".to_string())?
    .to_string();
  crate::ephemeral_dirs::recover_ephemeral_dirs();
  let profile = crate::profile::ProfileManager::instance()
    .list_profiles()
    .map_err(|error| format!("list worker profiles: {error}"))?
    .into_iter()
    .find(|profile| profile.id.to_string() == profile_id)
    .ok_or_else(|| "stale 2FA worker profile was not found".to_string())?;
  if !is_generated_backfill_worker(&profile, operation_id) {
    return Err("refusing to clean a worker profile owned by another operation".into());
  }
  crate::browser_runner::BrowserRunner::instance()
    .kill_browser_process(app_handle.clone(), &profile)
    .await
    .map_err(|error| format!("terminate worker profile {profile_id}: {error}"))?;
  crate::profile::ProfileManager::instance()
    .delete_profile(app_handle, &profile_id)
    .map_err(|error| format!("delete worker profile {profile_id}: {error}"))?;
  crate::ephemeral_dirs::remove_ephemeral_dir(&profile_id)
    .map(|_| ())
    .map_err(|error| format!("delete worker ephemeral directory {profile_id}: {error}"))
}

const SELECTED_PROFILE_BUSY_ERROR: &str =
  "Selected profile is busy; wait for the current automation task to finish or stop it before retrying";

struct PreparedSelectedBrowserRuntime {
  runtime: SelectedBrowserRuntime,
  lease_guard: Option<LeaseCleanupGuard>,
}

fn selected_profile_lease_error(error: LeaseError) -> String {
  match error {
    LeaseError::Busy { .. } => SELECTED_PROFILE_BUSY_ERROR.to_string(),
    LeaseError::InvalidSourceProfileId => "Selected source profile ID is invalid".to_string(),
  }
}

fn backfill_browser_name(browser: BackfillBrowser) -> &'static str {
  match browser {
    BackfillBrowser::Chromium => "chromium",
    BackfillBrowser::Camoufox => "camoufox",
  }
}

fn selected_launch_policy(
  request: &TwoFactorBackfillStartRequest,
  profile_id: &str,
) -> LaunchPolicy {
  let mut policy = LaunchPolicy::for_source_profile(profile_id);
  policy.data_mode = request.data_mode;
  policy.fingerprint_mode = request.fingerprint_mode;
  match &request.network {
    BackfillNetworkConfig::None => policy.clear_network = true,
    BackfillNetworkConfig::Proxy { proxy_id } => {
      policy.proxy_id = Some(proxy_id.trim().to_string());
    }
    BackfillNetworkConfig::Vpn { vpn_id } => {
      policy.vpn_id = Some(vpn_id.trim().to_string());
    }
  }
  policy
}

fn resolve_selected_backfill_profile(
  request: &TwoFactorBackfillStartRequest,
  profiles_dir: &std::path::Path,
) -> Result<Option<BrowserProfile>, String> {
  let Some(profile_id) = request
    .profile_id
    .as_deref()
    .map(str::trim)
    .filter(|profile_id| !profile_id.is_empty())
  else {
    return Ok(None);
  };
  let profile_uuid = uuid::Uuid::parse_str(profile_id)
    .map_err(|_| "Selected source profile ID is not a valid UUID".to_string())?;
  let metadata_path = profiles_dir
    .join(profile_uuid.to_string())
    .join("metadata.json");
  let metadata = std::fs::read(&metadata_path).map_err(|error| match error.kind() {
    std::io::ErrorKind::NotFound => "Selected source profile was not found".to_string(),
    _ => format!("Failed to read selected source profile: {error}"),
  })?;
  let profile: BrowserProfile = serde_json::from_slice(&metadata)
    .map_err(|error| format!("Selected source profile metadata is invalid: {error}"))?;
  if profile.id != profile_uuid {
    return Err("Selected source profile metadata ID does not match its directory".into());
  }
  if profile
    .process_id
    .is_some_and(crate::proxy_storage::is_process_running)
  {
    return Err("Selected source profile is running; stop it before starting 2FA Backfill".into());
  }
  if profile.is_cross_os() {
    return Err("Selected source profile was created on a different operating system".into());
  }
  let requested_browser = backfill_browser_name(request.browser);
  if !profile.browser.eq_ignore_ascii_case(requested_browser) {
    return Err(format!(
      "Selected source profile uses {}, but 2FA Backfill requested {requested_browser}",
      profile.browser
    ));
  }
  if request.data_mode == DataMode::Ephemeral && profile.browser.eq_ignore_ascii_case("camoufox") {
    return Err("Selected Camoufox profiles do not support ephemeral data for 2FA Backfill".into());
  }
  Ok(Some(profile))
}

fn prepare_selected_browser_runtime(
  request: &TwoFactorBackfillStartRequest,
  profiles_dir: &std::path::Path,
  lease_registry: &'static LeaseRegistry,
) -> Result<Option<PreparedSelectedBrowserRuntime>, String> {
  let Some(profile) = resolve_selected_backfill_profile(request, profiles_dir)? else {
    return Ok(None);
  };
  let profile_id = profile.id.to_string();
  let lease = lease_registry
    .try_acquire(&profile_id)
    .map_err(selected_profile_lease_error)?;
  let lease_guard = LeaseCleanupGuard::with_registry(lease_registry, lease.clone());
  let runtime = SelectedBrowserRuntime::new(
    profile,
    selected_launch_policy(request, &profile_id),
    lease,
    lease_registry,
  );
  Ok(Some(PreparedSelectedBrowserRuntime {
    runtime,
    lease_guard: Some(lease_guard),
  }))
}

/// Start a serial repair task and return its task ID. This internal starter is
/// shared by the dedicated repair command and the Auto Registration existing-account mode.
pub(crate) fn start_two_factor_backfill_task(
  app_handle: AppHandle,
  request: TwoFactorBackfillStartRequest,
) -> Result<String, String> {
  if request
    .selected_account_keys
    .iter()
    .all(|key| key.trim().is_empty())
  {
    return Err("2FA backfill requires at least one selected account".into());
  }

  let task_id = format!("backfill-{}", uuid::Uuid::new_v4());
  let cancel_flag = Arc::new(AtomicBool::new(false));
  let profiles_dir = crate::profile::ProfileManager::instance().get_profiles_dir();
  let selected_runtime =
    prepare_selected_browser_runtime(&request, &profiles_dir, LeaseRegistry::global())?;

  let engine = BackfillEngine::new(
    task_id.clone(),
    request,
    cancel_flag.clone(),
    ProductionAccountStore,
  );
  let browser_factory = ProductionBrowserFactory::new(
    selected_runtime
      .as_ref()
      .map(|prepared| prepared.runtime.clone()),
  );
  let task_id_for_log = task_id.clone();
  let app_handle_for_task = app_handle.clone();
  task::spawn_registered(task_id.clone(), cancel_flag, async move {
    let mut selected_runtime = selected_runtime;
    let canary_gate = match CanaryGate::new() {
      Ok(gate) => gate,
      Err(error) => {
        engine.emit_task_failure(&app_handle_for_task, "startup_failed");
        log::warn!("2FA backfill task {task_id_for_log} could not open the canary gate: {error}");
        return;
      }
    };
    let mut selected_cleanup =
      browser_factory.selected_runtime_cleanup_guard(app_handle_for_task.clone());
    if let Some(prepared) = selected_runtime.as_mut() {
      if let Some(lease_guard) = prepared.lease_guard.take() {
        let _ = lease_guard.into_lease_id();
      }
    }
    let journal_factory = move || TwoFactorBackfillJournal::new();
    engine
      .run(
        &app_handle_for_task,
        &browser_factory,
        journal_factory,
        Some(&canary_gate),
      )
      .await;
    if let Some(cleanup) = selected_cleanup.as_mut() {
      if let Err(error) = cleanup.close().await {
        log::error!("2FA backfill selected-profile cleanup failed: {error}");
      }
    }
  })?;

  Ok(task_id)
}

/// Start a serial repair task. The task emits redacted progress events and
/// supports cancellation; concurrency is hardcoded to 1.
#[tauri::command]
pub async fn start_two_factor_backfill(
  app_handle: AppHandle,
  request: TwoFactorBackfillStartRequest,
) -> Result<String, String> {
  start_two_factor_backfill_task(app_handle, request)
}

/// Cancel a running 2FA backfill task. Iteration stops before the next account
/// and the current account is moved to ManualReview when a pending secret exists.
#[tauri::command]
pub fn cancel_two_factor_backfill(task_id: String) -> Result<(), String> {
  if task::cancel_task(&task_id) {
    Ok(())
  } else {
    Err(format!("Task {task_id} not found"))
  }
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;

  use chrono::Utc;

  use super::{
    is_generated_backfill_worker, prepare_selected_browser_runtime, recover_journal_entry,
    resolve_selected_backfill_profile, selected_launch_policy, stale_recovery_action,
    BackfillBrowser, BackfillMode, BackfillNetworkConfig, DataMode, FingerprintMode, LeaseRegistry,
    RegistrationResult, StaleRecoveryAction, TwoFactorBackfillJournal,
    TwoFactorBackfillJournalState, TwoFactorBackfillOutcome,
    TwoFactorBackfillPreviewCommandRequest, TwoFactorBackfillStartRequest, TwoFactorBackfillState,
    SELECTED_PROFILE_BUSY_ERROR,
  };
  use crate::auto_service::openai::register::types::{
    AccountInventoryStatus, RegistrationOutcomeReason, TwoFactorBackfillAccessState,
  };
  use crate::auto_service::openai::two_factor_backfill::types::TwoFactorBackfillRecoveryState;
  use crate::profile::BrowserProfile;

  const OPERATION_ID: &str = "operation-1";
  const ACCOUNT_KEY: &str = "account-1";
  const PENDING_SECRET: &str = "JBSWY3DPEHPK3PXP";

  fn account() -> RegistrationResult {
    RegistrationResult {
      success: true,
      email: "account-1@example.com".into(),
      password: "known-password".into(),
      account_id: ACCOUNT_KEY.into(),
      access_token: "known-token".into(),
      device_id: "device-1".into(),
      error_message: String::new(),
      step_logs: Vec::new(),
      created_at: Utc::now(),
      two_fa_enabled: false,
      totp_secret: String::new(),
      free_trial_eligible: true,
      plan_type: "trial".into(),
      cdk: "GMAIL-known-cdk".into(),
      base_email: "base@example.com".into(),
      phone_number: String::new(),
      status: AccountInventoryStatus::Available,
      note: "seller-note".into(),
      exported_at: None,
      sold_at: None,
      email_provider: None,
      email_provider_provenance: None,
      registration_outcome_reason: Some(RegistrationOutcomeReason::Registered),
      two_factor_backfill_access_state: Some(TwoFactorBackfillAccessState::Accessible),
      two_factor_backfill_exclusion: None,
      two_factor_backfill_state: None,
      two_factor_backfill_operation_id: None,
      two_factor_backfill_outcome: None,
      record_revision: 7,
    }
  }

  fn journal(temp_dir: &tempfile::TempDir) -> TwoFactorBackfillJournal {
    TwoFactorBackfillJournal::with_base_dir(temp_dir.path()).unwrap()
  }

  fn captured_journal(temp_dir: &tempfile::TempDir) -> TwoFactorBackfillJournal {
    let mut journal = journal(temp_dir);
    journal
      .create_secret_captured(OPERATION_ID, ACCOUNT_KEY, 7, PENDING_SECRET.into())
      .unwrap();
    journal
  }

  fn completed_account() -> RegistrationResult {
    let mut account = account();
    account.two_fa_enabled = true;
    account.totp_secret = PENDING_SECRET.into();
    account.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
    account.two_factor_backfill_operation_id = Some(OPERATION_ID.into());
    account.two_factor_backfill_outcome = Some(TwoFactorBackfillOutcome::Enabled);
    account.record_revision = 8;
    account
  }

  fn in_progress_account() -> RegistrationResult {
    let mut account = account();
    account.two_factor_backfill_state = Some(TwoFactorBackfillState::InProgress);
    account.two_factor_backfill_operation_id = Some(OPERATION_ID.into());
    account
  }

  fn selected_request(profile_id: Option<String>) -> TwoFactorBackfillStartRequest {
    TwoFactorBackfillStartRequest {
      selected_account_keys: vec![ACCOUNT_KEY.into()],
      allow_free_trial_no: false,
      acknowledge_legacy_access: false,
      profile_id,
      data_mode: DataMode::Ephemeral,
      fingerprint_mode: FingerprintMode::RandomPerLaunch,
      browser: BackfillBrowser::Chromium,
      network: BackfillNetworkConfig::None,
      mode: BackfillMode::Canary,
    }
  }

  fn write_profile(
    profiles_dir: &std::path::Path,
    browser: &str,
  ) -> (BrowserProfile, std::path::PathBuf, Vec<u8>) {
    let profile = BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: "selected-backfill-profile".into(),
      browser: browser.into(),
      version: "1.0.0".into(),
      host_os: Some(crate::profile::types::get_host_os()),
      ..BrowserProfile::default()
    };
    let profile_dir = profiles_dir.join(profile.id.to_string());
    std::fs::create_dir_all(&profile_dir).unwrap();
    let metadata_path = profile_dir.join("metadata.json");
    let metadata = serde_json::to_vec_pretty(&profile).unwrap();
    std::fs::write(&metadata_path, &metadata).unwrap();
    (profile, metadata_path, metadata)
  }

  fn leaked_registry() -> &'static LeaseRegistry {
    Box::leak(Box::new(LeaseRegistry::new()))
  }

  #[test]
  fn start_request_profile_policy_defaults_and_camel_case_values() {
    let defaults: TwoFactorBackfillStartRequest = serde_json::from_value(serde_json::json!({
      "selectedAccountKeys": [ACCOUNT_KEY],
      "browser": "chromium",
      "network": { "kind": "none" },
      "mode": "canary"
    }))
    .unwrap();
    assert!(defaults.profile_id.is_none());
    assert_eq!(defaults.data_mode, DataMode::Ephemeral);
    assert_eq!(defaults.fingerprint_mode, FingerprintMode::RandomPerLaunch);

    let explicit: TwoFactorBackfillStartRequest = serde_json::from_value(serde_json::json!({
      "selectedAccountKeys": [ACCOUNT_KEY],
      "profileId": uuid::Uuid::new_v4().to_string(),
      "dataMode": "persistent",
      "fingerprintMode": "stable",
      "browser": "chromium",
      "network": { "kind": "proxy", "proxyId": "proxy-1" },
      "mode": "canary"
    }))
    .unwrap();
    assert_eq!(explicit.data_mode, DataMode::Persistent);
    assert_eq!(explicit.fingerprint_mode, FingerprintMode::Stable);
    assert!(explicit.profile_id.is_some());
  }

  #[test]
  fn preview_request_profile_policy_defaults_and_camel_case_values() {
    let defaults: TwoFactorBackfillPreviewCommandRequest =
      serde_json::from_value(serde_json::json!({
        "selectedAccountKeys": [ACCOUNT_KEY],
        "browser": "chromium",
        "network": { "kind": "none" }
      }))
      .unwrap();
    assert!(defaults.profile_id.is_none());
    assert_eq!(defaults.data_mode, DataMode::Ephemeral);
    assert_eq!(defaults.fingerprint_mode, FingerprintMode::RandomPerLaunch);

    let explicit: TwoFactorBackfillPreviewCommandRequest =
      serde_json::from_value(serde_json::json!({
        "selectedAccountKeys": [ACCOUNT_KEY],
        "profileId": uuid::Uuid::new_v4().to_string(),
        "dataMode": "persistent",
        "fingerprintMode": "stable",
        "browser": "chromium",
        "network": { "kind": "none" }
      }))
      .unwrap();
    assert_eq!(explicit.data_mode, DataMode::Persistent);
    assert_eq!(explicit.fingerprint_mode, FingerprintMode::Stable);
    assert!(explicit.profile_id.is_some());
  }

  #[test]
  fn selected_profile_resolution_is_read_only() {
    let temp = tempfile::tempdir().unwrap();
    let (profile, metadata_path, metadata) = write_profile(temp.path(), "chromium");
    let request = selected_request(Some(profile.id.to_string()));

    let resolved = resolve_selected_backfill_profile(&request, temp.path())
      .unwrap()
      .expect("selected profile");

    assert_eq!(resolved.id, profile.id);
    assert_eq!(std::fs::read(metadata_path).unwrap(), metadata);
  }

  #[test]
  fn selected_profile_validation_rejects_browser_and_ephemeral_camoufox() {
    let temp = tempfile::tempdir().unwrap();
    let (camoufox, _, _) = write_profile(temp.path(), "camoufox");
    let request = selected_request(Some(camoufox.id.to_string()));
    let mismatch = resolve_selected_backfill_profile(&request, temp.path()).unwrap_err();
    assert!(mismatch.contains("requested chromium"));

    let mut camoufox_request = request;
    camoufox_request.browser = BackfillBrowser::Camoufox;
    let ephemeral = resolve_selected_backfill_profile(&camoufox_request, temp.path()).unwrap_err();
    assert!(ephemeral.contains("ephemeral data"));
  }

  #[test]
  fn selected_profile_validation_rejects_a_running_profile() {
    let temp = tempfile::tempdir().unwrap();
    let (mut profile, metadata_path, _) = write_profile(temp.path(), "chromium");
    profile.process_id = Some(std::process::id());
    std::fs::write(&metadata_path, serde_json::to_vec_pretty(&profile).unwrap()).unwrap();
    let request = selected_request(Some(profile.id.to_string()));

    let error = resolve_selected_backfill_profile(&request, temp.path()).unwrap_err();

    assert!(error.contains("is running"));
  }

  #[test]
  fn selected_profile_preparation_fails_fast_and_drop_releases_lease() {
    let temp = tempfile::tempdir().unwrap();
    let (profile, _, _) = write_profile(temp.path(), "chromium");
    let profile_id = profile.id.to_string();
    let request = selected_request(Some(profile_id.clone()));
    let registry = leaked_registry();

    let prepared = prepare_selected_browser_runtime(&request, temp.path(), registry)
      .unwrap()
      .expect("selected runtime");
    assert!(registry.is_leased(&profile_id));
    let error = prepare_selected_browser_runtime(&request, temp.path(), registry)
      .err()
      .expect("second selected task must fail");
    assert_eq!(error, SELECTED_PROFILE_BUSY_ERROR);

    drop(prepared);
    assert!(!registry.is_leased(&profile_id));
  }

  #[test]
  fn selected_launch_policy_keeps_network_and_fingerprint_task_local() {
    let profile_id = uuid::Uuid::new_v4().to_string();
    let mut request = selected_request(Some(profile_id.clone()));
    request.data_mode = DataMode::Persistent;
    request.fingerprint_mode = FingerprintMode::Stable;
    request.network = BackfillNetworkConfig::Vpn {
      vpn_id: " vpn-1 ".into(),
    };

    let policy = selected_launch_policy(&request, &profile_id);

    assert_eq!(
      policy.source_profile_id.as_deref(),
      Some(profile_id.as_str())
    );
    assert_eq!(policy.data_mode, DataMode::Persistent);
    assert_eq!(policy.fingerprint_mode, FingerprintMode::Stable);
    assert_eq!(policy.vpn_id.as_deref(), Some("vpn-1"));
    assert!(policy.proxy_id.is_none());
    assert!(!policy.owns_generated_worker);
    assert!(!policy.persist_process_to_source);
  }

  #[test]
  fn stale_recovery_requires_ownership_and_no_private_journal() {
    assert!(matches!(
      stale_recovery_action(&in_progress_account(), OPERATION_ID, false),
      Ok(StaleRecoveryAction::Finalize)
    ));
    assert!(
      stale_recovery_action(&in_progress_account(), OPERATION_ID, true)
        .unwrap_err()
        .contains("private journal")
    );
    assert!(
      stale_recovery_action(&in_progress_account(), "other-operation", false)
        .unwrap_err()
        .contains("ownership")
    );
  }

  #[test]
  fn stale_recovery_is_idempotent_after_account_cancellation() {
    let mut account = in_progress_account();
    account.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
    account.two_factor_backfill_outcome = Some(TwoFactorBackfillOutcome::Cancelled);

    assert!(matches!(
      stale_recovery_action(&account, OPERATION_ID, false),
      Ok(StaleRecoveryAction::CleanupOnly)
    ));
  }

  #[test]
  fn stale_cleanup_accepts_only_the_exact_operation_worker() {
    let worker = BrowserProfile {
      name: format!("twofa-backfill-worker-{OPERATION_ID}"),
      ephemeral: true,
      ..BrowserProfile::default()
    };
    assert!(is_generated_backfill_worker(&worker, OPERATION_ID));
    assert!(!is_generated_backfill_worker(&worker, "other-operation"));

    let mut regular = worker.clone();
    regular.name = "personal-profile".into();
    assert!(!is_generated_backfill_worker(&regular, OPERATION_ID));

    let mut persistent = worker;
    persistent.ephemeral = false;
    assert!(!is_generated_backfill_worker(&persistent, OPERATION_ID));
  }

  #[test]
  fn recovery_summary_serialization_never_contains_pending_secret() {
    let temp_dir = tempfile::tempdir().unwrap();
    let journal = captured_journal(&temp_dir);
    let entry = journal.load(OPERATION_ID, ACCOUNT_KEY).unwrap().unwrap();
    let summary = TwoFactorBackfillJournal::summarize(&entry);
    let serialized = serde_json::to_string(&summary).unwrap();

    assert!(!serialized.contains(PENDING_SECRET));
    assert!(!serialized.contains("pendingTotpSecret"));
    assert_eq!(
      summary.state,
      TwoFactorBackfillRecoveryState::SecretCaptured
    );
  }

  #[test]
  fn exact_completed_account_is_recovered_and_journal_is_deleted() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut journal = captured_journal(&temp_dir);
    let result = recover_journal_entry(
      &mut journal,
      OPERATION_ID,
      ACCOUNT_KEY,
      Some(&completed_account()),
      |_| panic!("exact completion must not finalize manual review"),
    )
    .unwrap();

    assert!(result.recovered);
    assert!(!result.requires_manual_review);
    assert!(journal.load(OPERATION_ID, ACCOUNT_KEY).unwrap().is_none());
  }

  #[test]
  fn manual_review_never_auto_completes_an_exact_account() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut journal = captured_journal(&temp_dir);
    let entry = journal
      .transition_to_manual_review(OPERATION_ID, ACCOUNT_KEY, 1)
      .unwrap();
    assert_eq!(entry.state, TwoFactorBackfillJournalState::ManualReview);

    let result = recover_journal_entry(
      &mut journal,
      OPERATION_ID,
      ACCOUNT_KEY,
      Some(&completed_account()),
      |_| panic!("manual review must not auto-complete"),
    )
    .unwrap();

    assert!(!result.recovered);
    assert!(result.requires_manual_review);
    assert_eq!(
      result.summary.state,
      TwoFactorBackfillRecoveryState::ManualReview
    );
    assert!(journal.load(OPERATION_ID, ACCOUNT_KEY).unwrap().is_some());
  }

  #[test]
  fn final_revision_mismatch_fails_closed_to_manual_review() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut journal = captured_journal(&temp_dir);
    let confirmed = journal
      .update_state(
        OPERATION_ID,
        ACCOUNT_KEY,
        1,
        TwoFactorBackfillJournalState::RemoteConfirmed,
      )
      .unwrap();
    journal
      .record_final_account_revision(OPERATION_ID, ACCOUNT_KEY, confirmed.journal_revision, 8)
      .unwrap();
    let mut account = completed_account();
    account.record_revision = 9;

    let result = recover_journal_entry(
      &mut journal,
      OPERATION_ID,
      ACCOUNT_KEY,
      Some(&account),
      |_| panic!("revision mismatch must not finalize manual review"),
    )
    .unwrap();

    assert!(!result.recovered);
    assert!(result.requires_manual_review);
    assert_eq!(
      result.summary.state,
      TwoFactorBackfillRecoveryState::ManualReview
    );
  }

  #[test]
  fn owned_in_progress_account_is_released_to_reconciliation() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut journal = captured_journal(&temp_dir);
    let mut account = account();
    account.two_factor_backfill_state = Some(TwoFactorBackfillState::InProgress);
    account.two_factor_backfill_operation_id = Some(OPERATION_ID.into());
    let finalized = Cell::new(false);

    let result = recover_journal_entry(
      &mut journal,
      OPERATION_ID,
      ACCOUNT_KEY,
      Some(&account),
      |account| {
        finalized
          .set(account.two_factor_backfill_state == Some(TwoFactorBackfillState::InProgress));
        Ok(())
      },
    )
    .unwrap();

    assert!(!result.recovered);
    assert!(result.requires_manual_review);
    assert!(finalized.get());
    assert_eq!(
      result.summary.state,
      TwoFactorBackfillRecoveryState::ManualReview
    );
  }

  #[test]
  fn failed_reconciliation_update_is_propagated() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut journal = captured_journal(&temp_dir);
    let mut account = account();
    account.two_factor_backfill_state = Some(TwoFactorBackfillState::InProgress);
    account.two_factor_backfill_operation_id = Some(OPERATION_ID.into());

    let error = recover_journal_entry(
      &mut journal,
      OPERATION_ID,
      ACCOUNT_KEY,
      Some(&account),
      |_| Err("revision conflict".into()),
    )
    .unwrap_err();

    assert!(error.contains("revision conflict"));
    assert_eq!(
      journal
        .load(OPERATION_ID, ACCOUNT_KEY)
        .unwrap()
        .unwrap()
        .state,
      TwoFactorBackfillJournalState::ManualReview
    );
  }

  #[test]
  fn network_config_deserializes_camel_case_ids() {
    let proxy: BackfillNetworkConfig =
      serde_json::from_str(r#"{"kind":"proxy","proxyId":"proxy-1"}"#).unwrap();
    let vpn: BackfillNetworkConfig =
      serde_json::from_str(r#"{"kind":"vpn","vpnId":"vpn-1"}"#).unwrap();

    assert_eq!(
      proxy,
      BackfillNetworkConfig::Proxy {
        proxy_id: "proxy-1".into(),
      }
    );
    assert_eq!(
      vpn,
      BackfillNetworkConfig::Vpn {
        vpn_id: "vpn-1".into(),
      }
    );
  }

  #[test]
  fn network_config_accepts_snake_case_id_aliases() {
    let proxy: BackfillNetworkConfig =
      serde_json::from_str(r#"{"kind":"proxy","proxy_id":"proxy-1"}"#).unwrap();
    let vpn: BackfillNetworkConfig =
      serde_json::from_str(r#"{"kind":"vpn","vpn_id":"vpn-1"}"#).unwrap();

    assert_eq!(
      proxy,
      BackfillNetworkConfig::Proxy {
        proxy_id: "proxy-1".into(),
      }
    );
    assert_eq!(
      vpn,
      BackfillNetworkConfig::Vpn {
        vpn_id: "vpn-1".into(),
      }
    );
  }
}
