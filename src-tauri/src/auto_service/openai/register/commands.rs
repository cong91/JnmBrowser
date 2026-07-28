use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::execution::{prepare_registration, run_prepared_registration};
use super::store::{
  delete_cdk_inventory, delete_registered_account, list_cdk_inventory, list_registered_accounts,
  update_registered_account_note, update_registered_account_status,
};
use super::task;
use super::types::{
  AccountInventoryStatus, CdkInventoryRecord, RegistrationConfig, RegistrationResult,
};
use crate::auto_service::openai::two_factor_backfill::commands::{
  start_two_factor_backfill_task, TwoFactorBackfillStartRequest,
};
use crate::settings_manager::SettingsManager;

/// The existing-account repair operation shares the Auto Registration command
/// boundary but keeps its own backfill engine, journal and redacted events.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingAccountAutoRegistrationRequest {
  pub operation: ExistingAccountAutoRegistrationOperation,
  #[serde(flatten)]
  pub request: TwoFactorBackfillStartRequest,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExistingAccountAutoRegistrationOperation {
  ExistingAccount,
}

#[derive(Debug)]
pub enum AutoRegistrationRequest {
  ExistingAccount(ExistingAccountAutoRegistrationRequest),
  NewAccount(Box<RegistrationConfig>),
}

impl<'de> Deserialize<'de> for AutoRegistrationRequest {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.get("operation").is_some() {
      serde_json::from_value::<ExistingAccountAutoRegistrationRequest>(value)
        .map(Self::ExistingAccount)
        .map_err(D::Error::custom)
    } else {
      serde_json::from_value::<RegistrationConfig>(value)
        .map(|config| Self::NewAccount(Box::new(config)))
        .map_err(D::Error::custom)
    }
  }
}

/// Start a new auto-registration task or an existing-account 2FA repair task.
#[tauri::command]
pub async fn start_auto_registration(
  app_handle: tauri::AppHandle,
  config: AutoRegistrationRequest,
) -> Result<String, String> {
  let mut config = match config {
    AutoRegistrationRequest::ExistingAccount(existing) => {
      return start_two_factor_backfill_task(app_handle, existing.request);
    }
    AutoRegistrationRequest::NewAccount(config) => *config,
  };

  // The command boundary may resolve VI-OTP's token from encrypted settings;
  // all normalization and validation remains shared with the live harness.
  let viotp_requested = config
    .sms_provider
    .as_deref()
    .is_some_and(|provider| provider.trim().eq_ignore_ascii_case("viotp"));
  if viotp_requested
    && config
      .sms_token
      .as_ref()
      .is_none_or(|token| token.trim().is_empty())
  {
    let manager = SettingsManager::instance();
    config.sms_token = manager
      .get_sms_api_token(&app_handle)
      .await
      .ok()
      .flatten()
      .map(|token| token.trim().to_string())
      .filter(|token| !token.is_empty());
  }

  let cancel_flag = Arc::new(AtomicBool::new(false));
  let prepared = prepare_registration(config, cancel_flag.clone())?;
  let task_id = prepared.task_id().to_string();
  let task_id_for_log = task_id.clone();
  task::spawn_registered(task_id.clone(), cancel_flag, move || {
    if let Err(error) = run_prepared_registration(app_handle, prepared) {
      log::error!("Registration task {task_id_for_log} failed to start: {error}");
    }
  })?;

  Ok(task_id)
}

/// Start multiple registration batches in parallel.
///
/// Each batch runs independently with its own engine, worker profile, and
/// VPN peer pool. Starts are staggered by 2–5 seconds to avoid triggering
/// simultaneous Cloudflare challenges. Progress can be tracked via the
/// returned task IDs with the existing progress event stream.
#[tauri::command]
pub async fn start_parallel_registration(
  app_handle: tauri::AppHandle,
  configs: Vec<RegistrationConfig>,
) -> Result<Vec<String>, String> {
  super::parallel::launch_parallel_batches(app_handle, configs).await
}

/// Cancel multiple running registration tasks by their IDs.
/// Returns the number of tasks successfully cancelled.
#[tauri::command]
pub fn cancel_parallel_registration(task_ids: Vec<String>) -> Result<u32, String> {
  Ok(super::parallel::cancel_parallel_batches(&task_ids))
}

/// Cancel a running registration task.
#[tauri::command]
pub fn cancel_registration(task_id: String) -> Result<(), String> {
  if task::cancel_task(&task_id) {
    Ok(())
  } else {
    Err(format!("Task {task_id} not found"))
  }
}

/// List all stored registered accounts.
#[tauri::command]
pub fn list_registered_accounts_cmd() -> Result<Vec<RegistrationResult>, String> {
  list_registered_accounts()
}

/// Delete a stored registered account.
#[tauri::command]
pub fn delete_registered_account_cmd(account_id: String) -> Result<(), String> {
  if delete_registered_account(&account_id)? {
    Ok(())
  } else {
    Err(format!("Account {account_id} not found"))
  }
}

/// Update inventory status for one or many accounts (available/exported/sold/invalid/reserved).
#[tauri::command]
pub fn update_registered_account_status_cmd(
  account_ids: Vec<String>,
  status: String,
  note: Option<String>,
) -> Result<u32, String> {
  let status = parse_status(&status)?;
  let updated = update_registered_account_status(&account_ids, status, note)?;
  u32::try_from(updated).map_err(|_| "Updated account count exceeds u32".to_string())
}

/// Update free-form note for a stored account.
#[tauri::command]
pub fn update_registered_account_note_cmd(account_id: String, note: String) -> Result<(), String> {
  if update_registered_account_note(&account_id, note)? {
    Ok(())
  } else {
    Err(format!("Account {account_id} not found"))
  }
}

/// List CDK inventory stats (free-trial yes/no counts per CDK).
#[tauri::command]
pub fn list_cdk_inventory_cmd() -> Result<Vec<CdkInventoryRecord>, String> {
  Ok(list_cdk_inventory())
}

/// Delete a CDK inventory row.
#[tauri::command]
pub fn delete_cdk_inventory_cmd(cdk: String) -> Result<(), String> {
  if delete_cdk_inventory(&cdk) {
    Ok(())
  } else {
    Err(format!("CDK {cdk} not found in inventory"))
  }
}

fn parse_status(status: &str) -> Result<AccountInventoryStatus, String> {
  match status.to_ascii_lowercase().as_str() {
    "available" => Ok(AccountInventoryStatus::Available),
    "exported" => Ok(AccountInventoryStatus::Exported),
    "sold" | "used" => Ok(AccountInventoryStatus::Sold),
    "invalid" | "dead" => Ok(AccountInventoryStatus::Invalid),
    "reserved" => Ok(AccountInventoryStatus::Reserved),
    other => Err(format!("Unknown inventory status: {other}")),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn existing_account_request_deserializes_without_registration_cdks() {
    let request: AutoRegistrationRequest = serde_json::from_value(serde_json::json!({
      "operation": "existingAccount",
      "selectedAccountKeys": ["account-1"],
      "browser": "chromium",
      "network": {"kind": "none"},
      "mode": "canary",
      "allowFreeTrialNo": false,
      "acknowledgeLegacyAccess": false
    }))
    .unwrap();

    let AutoRegistrationRequest::ExistingAccount(request) = request else {
      panic!("existing-account operation must not deserialize as new registration");
    };
    assert_eq!(request.request.selected_account_keys, vec!["account-1"]);
  }

  #[test]
  fn normal_registration_request_still_deserializes_as_new_account() {
    let request: AutoRegistrationRequest = serde_json::from_value(serde_json::json!({
      "cdks": ["GMAIL-TEST"],
      "browserType": "chromium"
    }))
    .unwrap();

    let AutoRegistrationRequest::NewAccount(request) = request else {
      panic!("legacy registration payload must remain new-account mode");
    };
    assert_eq!(request.cdks, vec!["GMAIL-TEST"]);
  }

  #[test]
  fn operation_tag_requires_a_valid_existing_account_payload() {
    let error = serde_json::from_value::<AutoRegistrationRequest>(serde_json::json!({
      "operation": "existingAccount",
      "selectedAccountKeys": [],
      "browser": "chromium"
    }))
    .unwrap_err()
    .to_string();
    assert!(error.contains("missing field") || error.contains("network"));
  }
}
