use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::Utc;
use once_cell::sync::Lazy;

use super::types::{AccountInventoryStatus, CdkInventoryRecord, RegistrationResult};
use crate::app_dirs::data_dir;

static STORE: Lazy<Mutex<CredentialStore>> = Lazy::new(|| Mutex::new(CredentialStore::new()));
static CDK_STORE: Lazy<Mutex<CdkStore>> = Lazy::new(|| Mutex::new(CdkStore::new()));

/// Thread-safe JSON file store for registration results.
struct CredentialStore {
  accounts: HashMap<String, RegistrationResult>,
  base_dir: PathBuf,
}

/// Thread-safe JSON file store for per-CDK stats.
struct CdkStore {
  records: HashMap<String, CdkInventoryRecord>,
  base_dir: PathBuf,
}

impl CredentialStore {
  fn new() -> Self {
    let base_dir = data_dir().join("registered_accounts");
    let _ = fs::create_dir_all(&base_dir);

    let mut accounts = HashMap::new();
    if let Ok(entries) = fs::read_dir(&base_dir) {
      for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
          if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(result) = serde_json::from_str::<RegistrationResult>(&content) {
              let key = account_key(&result);
              accounts.insert(key, result);
            }
          }
        }
      }
    }

    Self { accounts, base_dir }
  }

  fn save(&mut self, result: &RegistrationResult) {
    let key = account_key(result);
    self.accounts.insert(key.clone(), result.clone());

    let file_path = self.base_dir.join(format!("{key}.json"));
    if let Ok(json) = serde_json::to_string_pretty(result) {
      let _ = fs::write(&file_path, json);
    }
  }

  fn list_all(&self) -> Vec<RegistrationResult> {
    let mut results: Vec<_> = self.accounts.values().cloned().collect();
    results.sort_by_key(|b| std::cmp::Reverse(b.created_at));
    results
  }

  fn delete(&mut self, account_id: &str) -> bool {
    // Support delete by account_id or email key.
    let key = if self.accounts.contains_key(account_id) {
      account_id.to_string()
    } else {
      self
        .accounts
        .iter()
        .find(|(_, v)| v.account_id == account_id || v.email == account_id)
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| account_id.to_string())
    };

    let removed = self.accounts.remove(&key).is_some();
    if removed {
      let file_path = self.base_dir.join(format!("{key}.json"));
      let _ = fs::remove_file(file_path);
    }
    removed
  }

  fn get(&self, account_id: &str) -> Option<RegistrationResult> {
    if let Some(v) = self.accounts.get(account_id) {
      return Some(v.clone());
    }
    self
      .accounts
      .values()
      .find(|v| v.account_id == account_id || v.email == account_id)
      .cloned()
  }

  fn update_status(
    &mut self,
    account_ids: &[String],
    status: AccountInventoryStatus,
    note: Option<String>,
  ) -> usize {
    let mut updated = 0usize;
    let now = Utc::now();
    for id in account_ids {
      let Some(mut account) = self.get(id) else {
        continue;
      };
      account.status = status.clone();
      if let Some(ref n) = note {
        account.note = n.clone();
      }
      match status {
        AccountInventoryStatus::Exported => {
          account.exported_at = Some(now);
        }
        AccountInventoryStatus::Sold => {
          account.sold_at = Some(now);
        }
        AccountInventoryStatus::Available => {
          // Keep history timestamps.
        }
        AccountInventoryStatus::Invalid | AccountInventoryStatus::Reserved => {}
      }
      self.save(&account);
      updated += 1;
    }
    updated
  }

  fn update_note(&mut self, account_id: &str, note: String) -> bool {
    let Some(mut account) = self.get(account_id) else {
      return false;
    };
    account.note = note;
    self.save(&account);
    true
  }
}

fn account_key(result: &RegistrationResult) -> String {
  if !result.account_id.is_empty() {
    result.account_id.clone()
  } else if !result.email.is_empty() {
    result.email.clone()
  } else {
    format!("unknown-{}", Utc::now().timestamp_millis())
  }
}

// --- Public API ---

pub fn save_registration_result(result: &RegistrationResult) {
  STORE.lock().unwrap().save(result);
}

pub fn list_registered_accounts() -> Vec<RegistrationResult> {
  STORE.lock().unwrap().list_all()
}

pub fn delete_registered_account(account_id: &str) -> bool {
  STORE.lock().unwrap().delete(account_id)
}

#[allow(dead_code)]
pub fn get_registered_account(account_id: &str) -> Option<RegistrationResult> {
  STORE.lock().unwrap().get(account_id)
}

pub fn update_registered_account_status(
  account_ids: &[String],
  status: AccountInventoryStatus,
  note: Option<String>,
) -> usize {
  STORE
    .lock()
    .unwrap()
    .update_status(account_ids, status, note)
}

pub fn update_registered_account_note(account_id: &str, note: String) -> bool {
  STORE.lock().unwrap().update_note(account_id, note)
}

// --- CDK inventory ---

impl CdkStore {
  fn new() -> Self {
    let base_dir = data_dir().join("cdk_inventory");
    let _ = fs::create_dir_all(&base_dir);

    let mut records = HashMap::new();
    if let Ok(entries) = fs::read_dir(&base_dir) {
      for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
          if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(record) = serde_json::from_str::<CdkInventoryRecord>(&content) {
              let key = cdk_file_key(&record.cdk);
              records.insert(key, record);
            }
          }
        }
      }
    }

    Self { records, base_dir }
  }

  fn save(&mut self, record: &CdkInventoryRecord) {
    let key = cdk_file_key(&record.cdk);
    self.records.insert(key.clone(), record.clone());
    let file_path = self.base_dir.join(format!("{key}.json"));
    if let Ok(json) = serde_json::to_string_pretty(record) {
      let _ = fs::write(file_path, json);
    }
  }

  fn list_all(&self) -> Vec<CdkInventoryRecord> {
    let mut results: Vec<_> = self.records.values().cloned().collect();
    results.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
    results
  }

  fn get(&self, cdk: &str) -> Option<CdkInventoryRecord> {
    let key = cdk_file_key(cdk);
    if let Some(v) = self.records.get(&key) {
      return Some(v.clone());
    }
    self
      .records
      .values()
      .find(|v| v.cdk.eq_ignore_ascii_case(cdk))
      .cloned()
  }

  fn delete(&mut self, cdk: &str) -> bool {
    let key = cdk_file_key(cdk);
    let removed = self.records.remove(&key).is_some();
    if removed {
      let file_path = self.base_dir.join(format!("{key}.json"));
      let _ = fs::remove_file(file_path);
    }
    removed
  }

  fn upsert_merge(&mut self, mut incoming: CdkInventoryRecord) -> CdkInventoryRecord {
    if let Some(existing) = self.get(&incoming.cdk) {
      // Keep earliest created_at; append new account entries not already present by email.
      incoming.created_at = existing.created_at;
      if incoming.base_email.is_empty() {
        incoming.base_email = existing.base_email;
      }
      // Prefer max counters from a continuous run; if task_id differs, accumulate.
      if existing.task_id != incoming.task_id && !existing.task_id.is_empty() {
        incoming.attempted = existing.attempted.saturating_add(incoming.attempted);
        incoming.free_trial_yes = existing
          .free_trial_yes
          .saturating_add(incoming.free_trial_yes);
        incoming.free_trial_no = existing
          .free_trial_no
          .saturating_add(incoming.free_trial_no);
        incoming.failed = existing.failed.saturating_add(incoming.failed);
        let mut accounts = existing.accounts;
        accounts.extend(incoming.accounts);
        incoming.accounts = accounts;
        if incoming.target_accounts < existing.target_accounts {
          incoming.target_accounts = existing.target_accounts;
        }
      }
    }
    self.save(&incoming);
    incoming
  }
}

fn cdk_file_key(cdk: &str) -> String {
  let cleaned: String = cdk
    .chars()
    .map(|c| {
      if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
        c
      } else {
        '_'
      }
    })
    .collect();
  if cleaned.is_empty() {
    format!("cdk-{}", Utc::now().timestamp_millis())
  } else {
    cleaned
  }
}

pub fn save_cdk_inventory_record(record: &CdkInventoryRecord) {
  CDK_STORE.lock().unwrap().upsert_merge(record.clone());
}

pub fn list_cdk_inventory() -> Vec<CdkInventoryRecord> {
  CDK_STORE.lock().unwrap().list_all()
}

pub fn get_cdk_inventory(cdk: &str) -> Option<CdkInventoryRecord> {
  CDK_STORE.lock().unwrap().get(cdk)
}

pub fn delete_cdk_inventory(cdk: &str) -> bool {
  CDK_STORE.lock().unwrap().delete(cdk)
}

/// Replace/save the exact record without cross-task accumulation (in-run updates).
pub fn put_cdk_inventory_record(record: &CdkInventoryRecord) {
  CDK_STORE.lock().unwrap().save(record);
}
