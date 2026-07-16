use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::engine::RegistrationEngine;
use super::store::{delete_registered_account, list_registered_accounts};
use super::task;
use super::types::{RegistrationConfig, RegistrationResult};
use crate::email::gmail_cdk::GmailCdkService;

/// Start a new auto-registration task. Returns the task_id.
#[tauri::command]
pub async fn start_auto_registration(
  app_handle: tauri::AppHandle,
  config: RegistrationConfig,
) -> Result<String, String> {
  let cancel_flag = Arc::new(AtomicBool::new(false));
  let cancel_flag_clone = cancel_flag.clone();

  let mut engine = RegistrationEngine::with_cancel_flag(config, cancel_flag);
  let task_id = engine.task_id().to_string();

  let join_handle = tokio::task::spawn_blocking(move || {
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    let email_service = GmailCdkService::new();
    rt.block_on(async { engine.run(app_handle, &email_service).await })
  });

  task::register_task(
    task_id.clone(),
    task::TaskHandle {
      cancel_flag: cancel_flag_clone,
      join_handle,
    },
  );

  Ok(task_id)
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
  Ok(list_registered_accounts())
}

/// Delete a stored registered account.
#[tauri::command]
pub fn delete_registered_account_cmd(account_id: String) -> Result<(), String> {
  if delete_registered_account(&account_id) {
    Ok(())
  } else {
    Err(format!("Account {account_id} not found"))
  }
}
