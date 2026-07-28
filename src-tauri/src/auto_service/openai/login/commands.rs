use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::execution::{prepare_login, run_prepared_login};
use super::store::{
  delete_login_result, export_login_results_json, get_login_result, list_login_results,
  list_successful_login_results, save_login_result, update_login_result_fields,
  update_login_result_note, update_login_result_status,
};
use super::sub2api::Sub2ApiClient;
use super::task;
use super::types::{LoginConfig, LoginResult, LoginResultStatus};
use crate::settings_manager::SettingsManager;

/// Start a new auto-login task. Returns the task_id.
#[tauri::command]
pub async fn start_auto_login(
  app_handle: tauri::AppHandle,
  config: LoginConfig,
) -> Result<String, String> {
  let cancel_flag = Arc::new(AtomicBool::new(false));
  let prepared = prepare_login(&app_handle, config, cancel_flag.clone()).await?;
  let task_id = prepared.task_id().to_string();
  let task_id_for_log = task_id.clone();
  task::spawn_registered(task_id.clone(), cancel_flag, move || {
    if let Err(error) = run_prepared_login(app_handle, prepared) {
      log::error!("Login task {task_id_for_log} failed to start: {error}");
    }
  })?;

  Ok(task_id)
}

/// Cancel a running login task.
#[tauri::command]
pub fn cancel_login(task_id: String) -> Result<(), String> {
  if task::cancel_task(&task_id) {
    Ok(())
  } else {
    Err(format!("Task {task_id} not found"))
  }
}

/// List all stored login results.
#[tauri::command]
pub fn list_login_results_cmd() -> Result<Vec<LoginResult>, String> {
  Ok(list_login_results())
}

/// Delete a stored login result.
#[tauri::command]
pub fn delete_login_result_cmd(account_id: String) -> Result<(), String> {
  if delete_login_result(&account_id) {
    Ok(())
  } else {
    Err(format!("Account {account_id} not found"))
  }
}

/// Update status for one or many login results.
#[tauri::command]
pub fn update_login_result_status_cmd(
  account_ids: Vec<String>,
  status: String,
  note: Option<String>,
) -> Result<u32, String> {
  let status = parse_status(&status)?;
  let n = update_login_result_status(&account_ids, status, note);
  Ok(n as u32)
}

/// Update free-form note for a stored login result.
#[tauri::command]
pub fn update_login_result_note_cmd(account_id: String, note: String) -> Result<(), String> {
  if update_login_result_note(&account_id, note) {
    Ok(())
  } else {
    Err(format!("Account {account_id} not found"))
  }
}

/// Edit stored login account fields (email/password/totp/note/phone/status).
#[tauri::command]
pub fn update_login_result_fields_cmd(
  account_id: String,
  email: Option<String>,
  password: Option<String>,
  totp_secret: Option<String>,
  note: Option<String>,
  phone_number: Option<String>,
  status: Option<String>,
) -> Result<LoginResult, String> {
  let status = match status {
    Some(s) if !s.trim().is_empty() => Some(parse_status(&s)?),
    _ => None,
  };
  update_login_result_fields(
    &account_id,
    email,
    password,
    totp_secret,
    note,
    phone_number,
    status,
  )
}

/// Export stored login results as pretty JSON (1 or many accounts).
///
/// Empty `account_ids` exports all successful accounts with tokens.
/// When `mark_exported` is true, status becomes `exported` after serialize.
#[tauri::command]
pub fn export_login_results_cmd(
  account_ids: Option<Vec<String>>,
  include_failed: Option<bool>,
  mark_exported: Option<bool>,
) -> Result<String, String> {
  export_login_results_json(
    account_ids.as_deref().unwrap_or(&[]),
    include_failed.unwrap_or(false),
    mark_exported.unwrap_or(false),
  )
}

/// Push already-stored successful login results to Sub2API (batch after login).
///
/// Empty `account_ids` pushes every successful stored account that still has tokens.
/// Updates each result's `sub2api_account_id` / `push_error` in the local store.
#[tauri::command]
pub async fn push_login_results_to_sub2api_cmd(
  app_handle: tauri::AppHandle,
  account_ids: Option<Vec<String>>,
  sub2api_url: Option<String>,
  sub2api_api_key: Option<String>,
  sub2api_proxy_id: Option<i64>,
  sub2api_group_ids: Option<Vec<i64>>,
) -> Result<PushLoginResultsResponse, String> {
  let mut url = sub2api_url.unwrap_or_default();
  let mut key = sub2api_api_key.unwrap_or_default();
  if url.trim().is_empty() || key.trim().is_empty() {
    let settings = SettingsManager::instance()
      .get_sub2api_settings(&app_handle)
      .await;
    if url.trim().is_empty() {
      url = settings.0;
    }
    if key.trim().is_empty() {
      key = settings.1;
    }
  }
  if url.trim().is_empty() || key.trim().is_empty() {
    return Err("Sub2API URL and API key are required".into());
  }

  let client = Sub2ApiClient::new(&url, &key);
  let ids = account_ids.unwrap_or_default();
  let targets: Vec<LoginResult> = if ids.is_empty() {
    list_successful_login_results()
  } else {
    ids
      .iter()
      .filter_map(|id| get_login_result(id))
      .filter(|r| r.success && !r.access_token.is_empty())
      .collect()
  };

  if targets.is_empty() {
    return Err("No successful login results with tokens to push".into());
  }

  let mut pushed = 0u32;
  let mut failed = 0u32;
  let mut errors = Vec::new();

  for mut result in targets {
    match client
      .import_oauth_tokens(
        &result.access_token,
        &result.refresh_token,
        &result.email,
        sub2api_proxy_id,
        sub2api_group_ids.clone(),
        Some(&result.email),
      )
      .await
    {
      Ok(import) => {
        result.sub2api_account_id = import
          .items
          .first()
          .and_then(|item| item.get("account_id").and_then(|id| id.as_i64()));
        result.push_error.clear();
        save_login_result(&result);
        pushed += 1;
      }
      Err(e) => {
        result.push_error = e.clone();
        save_login_result(&result);
        failed += 1;
        errors.push(format!("{}: {e}", result.email));
      }
    }
  }

  Ok(PushLoginResultsResponse {
    pushed,
    failed,
    errors,
  })
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushLoginResultsResponse {
  pub pushed: u32,
  pub failed: u32,
  pub errors: Vec<String>,
}

fn parse_status(status: &str) -> Result<LoginResultStatus, String> {
  match status.to_ascii_lowercase().as_str() {
    "available" => Ok(LoginResultStatus::Available),
    "exported" => Ok(LoginResultStatus::Exported),
    "used" => Ok(LoginResultStatus::Used),
    "invalid" | "dead" => Ok(LoginResultStatus::Invalid),
    other => Err(format!("Unknown status: {other}")),
  }
}
