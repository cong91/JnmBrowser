use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;

use super::types::{LoginResult, LoginResultStatus};
use crate::app_dirs::data_dir;

static STORE: Lazy<Mutex<LoginResultStore>> = Lazy::new(|| Mutex::new(LoginResultStore::new()));

struct LoginResultStore {
  accounts: HashMap<String, LoginResult>,
  base_dir: PathBuf,
}

impl LoginResultStore {
  fn new() -> Self {
    let base_dir = data_dir().join("login_results");
    let _ = fs::create_dir_all(&base_dir);

    let mut accounts = HashMap::new();
    if let Ok(entries) = fs::read_dir(&base_dir) {
      for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
          if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(result) = serde_json::from_str::<LoginResult>(&content) {
              let key = Self::result_key(&result);
              accounts.insert(key, result);
            }
          }
        }
      }
    }

    Self { accounts, base_dir }
  }

  fn result_key(result: &LoginResult) -> String {
    if !result.account_id.is_empty() {
      result.account_id.clone()
    } else if !result.email.is_empty() {
      result.email.clone()
    } else {
      format!("unknown-{}", Utc::now().timestamp_millis())
    }
  }

  fn save(&mut self, result: &LoginResult) {
    let key = Self::result_key(result);
    self.accounts.insert(key.clone(), result.clone());

    let file_path = self.base_dir.join(format!("{key}.json"));
    if let Ok(json) = serde_json::to_string_pretty(result) {
      let _ = fs::write(&file_path, json);
    }
  }

  fn list_all(&self) -> Vec<LoginResult> {
    let mut results: Vec<_> = self.accounts.values().cloned().collect();
    results.sort_by_key(|b| std::cmp::Reverse(b.created_at));
    results
  }

  fn delete(&mut self, account_id: &str) -> bool {
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

  fn update_status(
    &mut self,
    account_ids: &[String],
    status: LoginResultStatus,
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
      if status == LoginResultStatus::Exported {
        account.exported_at = Some(now);
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

  fn get(&self, account_id: &str) -> Option<LoginResult> {
    if let Some(v) = self.accounts.get(account_id) {
      return Some(v.clone());
    }
    self
      .accounts
      .values()
      .find(|v| v.account_id == account_id || v.email == account_id)
      .cloned()
  }
}

pub fn save_login_result(result: &LoginResult) {
  STORE.lock().unwrap().save(result);
}

pub fn list_login_results() -> Vec<LoginResult> {
  STORE.lock().unwrap().list_all()
}

pub fn delete_login_result(account_id: &str) -> bool {
  STORE.lock().unwrap().delete(account_id)
}

pub fn get_login_result(account_id: &str) -> Option<LoginResult> {
  STORE.lock().unwrap().get(account_id)
}

pub fn update_login_result_status(
  account_ids: &[String],
  status: LoginResultStatus,
  note: Option<String>,
) -> usize {
  STORE
    .lock()
    .unwrap()
    .update_status(account_ids, status, note)
}

pub fn update_login_result_note(account_id: &str, note: String) -> bool {
  STORE.lock().unwrap().update_note(account_id, note)
}

/// Codex CLI official OAuth client id (matches login engine / Sub2API).
const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Decode JWT `exp` claim to RFC3339 UTC, if present.
fn expires_at_from_access_token(access_token: &str) -> Option<String> {
  let parts: Vec<&str> = access_token.split('.').collect();
  if parts.len() < 2 {
    return None;
  }
  let payload = parts[1];
  let decoded =
    base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload).ok()?;
  let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
  let exp = json.get("exp")?.as_i64()?;
  DateTime::from_timestamp(exp, 0).map(|dt| dt.to_rfc3339())
}

/// Build one Sub2API `DataAccount` entry from a local login result.
fn login_result_to_sub2api_account(r: &LoginResult) -> serde_json::Value {
  let mut credentials = serde_json::Map::new();
  credentials.insert(
    "access_token".into(),
    serde_json::Value::String(r.access_token.clone()),
  );
  if !r.refresh_token.is_empty() {
    credentials.insert(
      "refresh_token".into(),
      serde_json::Value::String(r.refresh_token.clone()),
    );
  }
  credentials.insert(
    "client_id".into(),
    serde_json::Value::String(OPENAI_CODEX_CLIENT_ID.into()),
  );
  if !r.email.is_empty() {
    credentials.insert("email".into(), serde_json::Value::String(r.email.clone()));
  }
  if !r.account_id.is_empty() {
    credentials.insert(
      "chatgpt_account_id".into(),
      serde_json::Value::String(r.account_id.clone()),
    );
  }
  if let Some(exp) = expires_at_from_access_token(&r.access_token) {
    credentials.insert("expires_at".into(), serde_json::Value::String(exp));
  }

  let name = if !r.email.is_empty() {
    r.email.clone()
  } else if !r.account_id.is_empty() {
    r.account_id.clone()
  } else {
    "Imported Account".into()
  };

  let mut notes_parts = Vec::new();
  if !r.note.trim().is_empty() {
    notes_parts.push(r.note.trim().to_string());
  }
  if !r.phone_number.trim().is_empty() {
    notes_parts.push(format!("phone={}", r.phone_number.trim()));
  }
  let notes = if notes_parts.is_empty() {
    serde_json::Value::Null
  } else {
    serde_json::Value::String(notes_parts.join("; "))
  };

  // Sub2API account scheduling defaults used by working import files:
  // concurrency=10, priority=1 (lower = higher priority), rate_multiplier=1.
  serde_json::json!({
    "name": name,
    "notes": notes,
    "platform": "openai",
    "type": "oauth",
    "credentials": credentials,
    "extra": {},
    "concurrency": 10,
    "priority": 1,
    "rate_multiplier": 1,
    "expires_at": null,
    "auto_pause_on_expired": true,
  })
}

/// Export login results as Sub2API data-export JSON.
///
/// - `account_ids` empty → all successful accounts with tokens
/// - otherwise → matching ids/emails (success only unless `include_failed`)
///
/// Shape matches Sub2API admin Import Data (`type: sub2api-data`):
/// ```json
/// {
///   "type": "sub2api-data",
///   "version": 1,
///   "exported_at": "...",
///   "proxies": [],
///   "accounts": [
///     {
///       "name": "user@example.com",
///       "platform": "openai",
///       "type": "oauth",
///       "credentials": {
///         "access_token": "...",
///         "refresh_token": "...",
///         "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
///         "email": "...",
///         "chatgpt_account_id": "..."
///       },
///       "concurrency": 10,
///       "priority": 1,
///       "rate_multiplier": 1,
///       "auto_pause_on_expired": true
///     }
///   ]
/// }
/// ```
/// Whether a result is eligible for the default (empty ids) export set.
/// Excludes already-exported inventory so silent re-export cannot happen.
fn is_default_export_candidate(r: &LoginResult, include_failed: bool) -> bool {
  if include_failed {
    return r.status != LoginResultStatus::Exported;
  }
  r.success && !r.access_token.is_empty() && r.status != LoginResultStatus::Exported
}

/// Whether a result is eligible when the caller passed explicit account ids
/// (intentional re-export of previously exported rows is allowed).
fn is_explicit_export_candidate(r: &LoginResult, include_failed: bool) -> bool {
  include_failed || (r.success && !r.access_token.is_empty())
}

pub fn export_login_results_json(
  account_ids: &[String],
  include_failed: bool,
  mark_exported: bool,
) -> Result<String, String> {
  let mut store = STORE.lock().unwrap();
  let mut selected: Vec<LoginResult> = if account_ids.is_empty() {
    store
      .list_all()
      .into_iter()
      .filter(|r| is_default_export_candidate(r, include_failed))
      .collect()
  } else {
    account_ids
      .iter()
      .filter_map(|id| store.get(id))
      .filter(|r| is_explicit_export_candidate(r, include_failed))
      .collect()
  };

  // Stable newest-first
  selected.sort_by_key(|b| std::cmp::Reverse(b.created_at));

  if selected.is_empty() {
    return Err("No login results to export".into());
  }

  let exported_at = Utc::now();
  let accounts: Vec<serde_json::Value> = selected
    .iter()
    .map(login_result_to_sub2api_account)
    .collect();

  // Sub2API ImportDataModal requires type/version + arrays `proxies` and `accounts`.
  let payload = serde_json::json!({
    "type": "sub2api-data",
    "version": 1,
    "exported_at": exported_at.to_rfc3339(),
    "proxies": [],
    "accounts": accounts,
  });

  if mark_exported {
    let ids: Vec<String> = selected
      .iter()
      .map(|r| {
        if !r.account_id.is_empty() {
          r.account_id.clone()
        } else {
          r.email.clone()
        }
      })
      .collect();
    let _ = store.update_status(&ids, LoginResultStatus::Exported, None);
  }

  serde_json::to_string_pretty(&payload).map_err(|e| format!("Serialize export JSON: {e}"))
}

/// List success results that still have tokens (for batch push).
pub fn list_successful_login_results() -> Vec<LoginResult> {
  STORE
    .lock()
    .unwrap()
    .list_all()
    .into_iter()
    .filter(|r| r.success && !r.access_token.is_empty())
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::auto_service::openai::login::types::LoginResult;

  #[test]
  fn login_result_maps_to_sub2api_data_account() {
    // JWT payload: {"exp": 1893456000, "https://api.openai.com/auth": {"chatgpt_account_id": "acc-1"}}
    // We only need a decodable middle segment for expires_at helper; full signature not required.
    let payload = base64::Engine::encode(
      &base64::engine::general_purpose::URL_SAFE_NO_PAD,
      br#"{"exp":1893456000}"#,
    );
    let access_token = format!("aaa.{payload}.sig");

    let result = LoginResult {
      success: true,
      email: "user@example.com".into(),
      account_id: "acc-1".into(),
      access_token,
      refresh_token: "rt_test".into(),
      sub2api_account_id: None,
      error_message: String::new(),
      push_error: String::new(),
      step_logs: vec![],
      created_at: Utc::now(),
      phone_number: String::new(),
      status: LoginResultStatus::Available,
      note: String::new(),
      exported_at: None,
    };

    let account = login_result_to_sub2api_account(&result);
    assert_eq!(account["name"], "user@example.com");
    assert_eq!(account["platform"], "openai");
    assert_eq!(account["type"], "oauth");
    assert_eq!(account["credentials"]["client_id"], OPENAI_CODEX_CLIENT_ID);
    assert_eq!(account["credentials"]["refresh_token"], "rt_test");
    assert_eq!(account["credentials"]["chatgpt_account_id"], "acc-1");
    assert_eq!(account["credentials"]["email"], "user@example.com");
    assert!(account["credentials"]["expires_at"].as_str().is_some());
    assert_eq!(account["concurrency"], 10);
    assert_eq!(account["priority"], 1);
    assert_eq!(account["rate_multiplier"], 1);
    assert_eq!(account["auto_pause_on_expired"], true);
  }

  #[test]
  fn sub2api_export_envelope_shape() {
    // Build the same envelope export_login_results_json produces (without store I/O).
    let result = LoginResult {
      success: true,
      email: "a@x.com".into(),
      account_id: "id-1".into(),
      access_token: "tok".into(),
      refresh_token: "rt".into(),
      sub2api_account_id: None,
      error_message: String::new(),
      push_error: String::new(),
      step_logs: vec![],
      created_at: Utc::now(),
      phone_number: String::new(),
      status: LoginResultStatus::Available,
      note: String::new(),
      exported_at: None,
    };
    let payload = serde_json::json!({
      "type": "sub2api-data",
      "version": 1,
      "exported_at": Utc::now().to_rfc3339(),
      "proxies": [],
      "accounts": [login_result_to_sub2api_account(&result)],
    });

    assert_eq!(payload["type"], "sub2api-data");
    assert_eq!(payload["version"], 1);
    assert!(payload["proxies"].is_array());
    assert_eq!(payload["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(payload["accounts"][0]["platform"], "openai");
    assert_eq!(payload["accounts"][0]["type"], "oauth");
    // Must NOT be the old custom shape.
    assert!(payload.get("count").is_none());
    assert!(payload["accounts"][0].get("codexSession").is_none());
    assert!(payload["accounts"][0].get("accessToken").is_none());
  }

  fn sample_result(status: LoginResultStatus, success: bool, token: &str) -> LoginResult {
    LoginResult {
      success,
      email: "a@x.com".into(),
      account_id: "id-1".into(),
      access_token: token.into(),
      refresh_token: "rt".into(),
      sub2api_account_id: None,
      error_message: String::new(),
      push_error: String::new(),
      step_logs: vec![],
      created_at: Utc::now(),
      phone_number: String::new(),
      status,
      note: String::new(),
      exported_at: None,
    }
  }

  #[test]
  fn default_export_excludes_exported_status() {
    let available = sample_result(LoginResultStatus::Available, true, "tok");
    let exported = sample_result(LoginResultStatus::Exported, true, "tok");
    let used = sample_result(LoginResultStatus::Used, true, "tok");
    let no_token = sample_result(LoginResultStatus::Available, true, "");
    let failed = sample_result(LoginResultStatus::Invalid, false, "");

    assert!(is_default_export_candidate(&available, false));
    assert!(!is_default_export_candidate(&exported, false));
    // Used is not "exported" — still success+token; UI default is available-only,
    // but backend empty-id only blocks already-exported silent re-export.
    assert!(is_default_export_candidate(&used, false));
    assert!(!is_default_export_candidate(&no_token, false));
    assert!(!is_default_export_candidate(&failed, false));
  }

  #[test]
  fn explicit_export_allows_reexport_of_exported() {
    let exported = sample_result(LoginResultStatus::Exported, true, "tok");
    assert!(is_explicit_export_candidate(&exported, false));
    let no_token = sample_result(LoginResultStatus::Exported, true, "");
    assert!(!is_explicit_export_candidate(&no_token, false));
  }
}
