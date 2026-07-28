// TODO: remove allow when browser integration is complete.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::types::{AccountCheckResult, AccountOutcome};
use crate::app_dirs::data_dir;

static STORE: std::sync::Mutex<Option<AccountCheckStore>> = std::sync::Mutex::new(None);

fn mount() -> std::sync::MutexGuard<'static, Option<AccountCheckStore>> {
  let mut guard = STORE.lock().unwrap();
  if guard.is_none() {
    *guard = Some(AccountCheckStore::new());
  }
  guard
}

pub struct AccountCheckStore {
  accounts: HashMap<String, AccountCheckResult>,
  base_dir: PathBuf,
}

impl AccountCheckStore {
  fn new() -> Self {
    let base_dir = data_dir().join("account_checks");
    let _ = fs::create_dir_all(&base_dir);

    let mut accounts = HashMap::new();
    if let Ok(entries) = fs::read_dir(&base_dir) {
      for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
          continue;
        }
        if let Ok(content) = fs::read_to_string(&path) {
          if let Ok(result) = serde_json::from_str::<AccountCheckResult>(&content) {
            let key = result.key();
            accounts.insert(key, result);
          }
        }
      }
    }

    Self { accounts, base_dir }
  }

  pub fn save(&mut self, result: &AccountCheckResult) -> Result<(), String> {
    let key = result.key();
    let file_path = self.base_dir.join(format!("{key}.json"));

    let json = serde_json::to_vec_pretty(result)
      .map_err(|error| format!("Failed to serialize account check result: {error}"))?;

    let mut temp_file = tempfile::NamedTempFile::new_in(&self.base_dir).map_err(|error| {
      format!(
        "Failed to create account check temp file in {}: {error}",
        self.base_dir.display()
      )
    })?;

    temp_file
      .write_all(&json)
      .map_err(|error| format!("Failed to write account check temp file: {error}"))?;

    temp_file
      .as_file()
      .sync_all()
      .map_err(|error| format!("Failed to sync account check temp file: {error}"))?;

    super::super::register::store::persist_temp_file(
      temp_file,
      &file_path,
      "account check result",
    )?;

    sync_parent_directory(&self.base_dir, "account checks directory")?;

    // Only update in-memory after successful disk commit.
    self.accounts.insert(key, result.clone());

    Ok(())
  }

  pub fn list_all(&self) -> Vec<AccountCheckResult> {
    let mut results: Vec<_> = self.accounts.values().cloned().collect();
    results.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    results
  }

  pub fn get(&self, email: &str) -> Option<AccountCheckResult> {
    let key = email.trim().to_lowercase();
    self.accounts.get(&key).cloned()
  }

  pub fn delete(&mut self, email: &str) -> bool {
    let key = email.trim().to_lowercase();
    if self.accounts.remove(&key).is_some() {
      let file_path = self.base_dir.join(format!("{key}.json"));
      let _ = fs::remove_file(file_path);
      true
    } else {
      false
    }
  }

  pub fn passed(&self) -> Vec<AccountCheckResult> {
    self
      .list_all()
      .into_iter()
      .filter(|r| r.outcome == AccountOutcome::Passed)
      .collect()
  }

  pub fn deactivated(&self) -> Vec<AccountCheckResult> {
    self
      .list_all()
      .into_iter()
      .filter(|r| r.outcome == AccountOutcome::Deactivated)
      .collect()
  }

  pub fn unresolved(&self) -> Vec<AccountCheckResult> {
    self
      .list_all()
      .into_iter()
      .filter(|r| r.outcome == AccountOutcome::Unresolved)
      .collect()
  }
}

#[cfg(not(windows))]
fn sync_parent_directory(directory: &Path, description: &str) -> Result<(), String> {
  fs::File::open(directory)
    .and_then(|file| file.sync_all())
    .map_err(|error| {
      format!(
        "Failed to sync {description} {}: {error}",
        directory.display()
      )
    })
}

#[cfg(windows)]
fn sync_parent_directory(_directory: &Path, _description: &str) -> Result<(), String> {
  Ok(())
}

// ── Public singleton API ───────────────────────────────────

pub fn insert_result(result: &AccountCheckResult) -> Result<(), String> {
  mount().as_mut().unwrap().save(result)
}

pub fn current_results() -> Vec<AccountCheckResult> {
  mount().as_ref().unwrap().list_all()
}

pub fn current_passed() -> Vec<AccountCheckResult> {
  mount().as_ref().unwrap().passed()
}

pub fn current_deactivated() -> Vec<AccountCheckResult> {
  mount().as_ref().unwrap().deactivated()
}

pub fn current_unresolved() -> Vec<AccountCheckResult> {
  mount().as_ref().unwrap().unresolved()
}

pub fn find_result(email: &str) -> Option<AccountCheckResult> {
  mount().as_ref().unwrap().get(email)
}

pub fn remove_result(email: &str) -> bool {
  mount().as_mut().unwrap().delete(email)
}

#[cfg(test)]
mod tests {
  use super::super::types::{AccountCheckResult, AccountOutcome, ReasonCode};
  use super::*;
  use chrono::Utc;
  use tempfile::TempDir;

  fn make_result(email: &str, outcome: AccountOutcome) -> AccountCheckResult {
    AccountCheckResult {
      email: email.into(),
      password: "pass123".into(),
      totp_secret: String::new(),
      outcome,
      reason_code: ReasonCode::OtpReached,
      created_at: Utc::now(),
    }
  }

  fn isolated_store(tmp: &TempDir) -> AccountCheckStore {
    let base_dir = tmp.path().join("account_checks");
    fs::create_dir_all(&base_dir).unwrap();

    let mut accounts = HashMap::new();
    if let Ok(entries) = fs::read_dir(&base_dir) {
      for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
          continue;
        }
        if let Ok(content) = fs::read_to_string(&path) {
          if let Ok(result) = serde_json::from_str::<AccountCheckResult>(&content) {
            let key = result.key();
            accounts.insert(key, result);
          }
        }
      }
    }

    AccountCheckStore { accounts, base_dir }
  }

  #[test]
  fn save_and_reload_persists() {
    let tmp = TempDir::new().unwrap();
    let result = make_result("test@example.com", AccountOutcome::Passed);

    {
      let mut store = isolated_store(&tmp);
      store.save(&result).unwrap();
      assert_eq!(store.list_all().len(), 1);
    }

    // Reload from same base_dir.
    let store = isolated_store(&tmp);
    let all = store.list_all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].email, "test@example.com");
    assert_eq!(all[0].outcome, AccountOutcome::Passed);
  }

  #[test]
  fn duplicate_email_overwrites() {
    let tmp = TempDir::new().unwrap();
    let mut store = isolated_store(&tmp);

    let r1 = make_result("dup@ex.com", AccountOutcome::Passed);
    store.save(&r1).unwrap();

    let mut r2 = make_result("dup@ex.com", AccountOutcome::Deactivated);
    r2.reason_code = ReasonCode::Deactivated;
    store.save(&r2).unwrap();

    let all = store.list_all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].outcome, AccountOutcome::Deactivated);
  }

  #[test]
  fn passed_and_deactivated_filters() {
    let tmp = TempDir::new().unwrap();
    let mut store = isolated_store(&tmp);

    store
      .save(&make_result("pass@ex.com", AccountOutcome::Passed))
      .unwrap();
    store
      .save(&make_result("deact@ex.com", AccountOutcome::Deactivated))
      .unwrap();
    store
      .save(&make_result("unres@ex.com", AccountOutcome::Unresolved))
      .unwrap();

    assert_eq!(store.passed().len(), 1);
    assert_eq!(store.deactivated().len(), 1);
    assert_eq!(store.unresolved().len(), 1);
    assert_eq!(store.list_all().len(), 3);
  }

  #[test]
  fn credential_line_format() {
    let r = AccountCheckResult {
      email: "user@ex.com".into(),
      password: "secret".into(),
      totp_secret: "JBSWY3DPEHPK3PXP".into(),
      outcome: AccountOutcome::Passed,
      reason_code: ReasonCode::OtpReached,
      created_at: Utc::now(),
    };
    assert_eq!(r.credential_line(), "user@ex.com|secret|JBSWY3DPEHPK3PXP");
  }

  #[test]
  fn credential_line_no_totp() {
    let r = AccountCheckResult {
      email: "user@ex.com".into(),
      password: "secret".into(),
      totp_secret: String::new(),
      outcome: AccountOutcome::Passed,
      reason_code: ReasonCode::OtpReached,
      created_at: Utc::now(),
    };
    assert_eq!(r.credential_line(), "user@ex.com|secret");
  }

  #[test]
  fn key_trims_and_lowercases() {
    let r = AccountCheckResult {
      email: "  User@Example.COM  ".into(),
      password: "p".into(),
      totp_secret: String::new(),
      outcome: AccountOutcome::Passed,
      reason_code: ReasonCode::OtpReached,
      created_at: Utc::now(),
    };
    assert_eq!(r.key(), "user@example.com");
  }

  #[test]
  fn delete_removes_entry() {
    let tmp = TempDir::new().unwrap();
    let mut store = isolated_store(&tmp);

    store
      .save(&make_result("del@ex.com", AccountOutcome::Passed))
      .unwrap();
    assert_eq!(store.list_all().len(), 1);

    assert!(store.delete("del@ex.com"));
    assert_eq!(store.list_all().len(), 0);
  }
}
