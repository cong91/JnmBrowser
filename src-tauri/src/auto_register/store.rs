use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use super::types::RegistrationResult;
use crate::app_dirs::data_dir;

static STORE: Lazy<Mutex<CredentialStore>> =
  Lazy::new(|| Mutex::new(CredentialStore::new()));

/// Thread-safe JSON file store for registration results.
struct CredentialStore {
  accounts: HashMap<String, RegistrationResult>,
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
        if path.extension().map_or(false, |ext| ext == "json") {
          if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(result) = serde_json::from_str::<RegistrationResult>(&content) {
              let key = result.account_id.clone();
              accounts.insert(key, result);
            }
          }
        }
      }
    }

    Self { accounts, base_dir }
  }

  fn save(&mut self, result: &RegistrationResult) {
    let key = if result.account_id.is_empty() {
      result.email.clone()
    } else {
      result.account_id.clone()
    };

    self.accounts.insert(key.clone(), result.clone());

    let file_path = self.base_dir.join(format!("{key}.json"));
    if let Ok(json) = serde_json::to_string_pretty(result) {
      let _ = fs::write(&file_path, json);
    }
  }

  fn list_all(&self) -> Vec<RegistrationResult> {
    let mut results: Vec<_> = self.accounts.values().cloned().collect();
    results.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    results
  }

  fn delete(&mut self, account_id: &str) -> bool {
    let removed = self.accounts.remove(account_id).is_some();
    if removed {
      let file_path = self.base_dir.join(format!("{account_id}.json"));
      let _ = fs::remove_file(file_path);
    }
    removed
  }

  fn get(&self, account_id: &str) -> Option<RegistrationResult> {
    self.accounts.get(account_id).cloned()
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
