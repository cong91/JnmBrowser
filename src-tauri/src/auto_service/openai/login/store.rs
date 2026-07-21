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
    let mut all_json_paths: Vec<PathBuf> = Vec::new();

    if let Ok(entries) = fs::read_dir(&base_dir) {
      for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
          continue;
        }
        all_json_paths.push(path.clone());
        let Ok(content) = fs::read_to_string(&path) else {
          continue;
        };
        let Ok(result) = serde_json::from_str::<LoginResult>(&content) else {
          continue;
        };
        let key = Self::result_key(&result);
        // Prefer newer / successful row when multiple files map to same email.
        let replace = match accounts.get(&key) {
          None => true,
          Some(existing) => Self::is_preferable(&result, existing),
        };
        if replace {
          accounts.insert(key, result);
        }
      }
    }

    let mut store = Self { accounts, base_dir };

    // Rewrite under canonical email keys and drop legacy accountId-named files.
    let snapshot: Vec<LoginResult> = store.accounts.values().cloned().collect();
    for result in snapshot {
      store.save(&result);
    }
    for path in all_json_paths {
      let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
      if !store.accounts.contains_key(stem) {
        let _ = fs::remove_file(path);
      }
    }

    store
  }

  /// Stable inventory key: always email (lowercased). Failures have empty account_id;
  /// successes used to key by account_id and left a second file for the same email.
  fn result_key(result: &LoginResult) -> String {
    let email = result.email.trim().to_lowercase();
    if !email.is_empty() {
      email
    } else if !result.account_id.is_empty() {
      result.account_id.clone()
    } else {
      format!("unknown-{}", Utc::now().timestamp_millis())
    }
  }

  /// Prefer success with tokens, else newer created_at.
  fn is_preferable(candidate: &LoginResult, existing: &LoginResult) -> bool {
    let cand_ok = candidate.success && !candidate.access_token.is_empty();
    let exist_ok = existing.success && !existing.access_token.is_empty();
    match (cand_ok, exist_ok) {
      (true, false) => true,
      (false, true) => false,
      _ => candidate.created_at >= existing.created_at,
    }
  }

  fn save(&mut self, result: &LoginResult) {
    let key = Self::result_key(result);

    // Drop any previous in-memory entries for the same email under a different key
    // (legacy account_id keys) before inserting the canonical one.
    let email = result.email.trim().to_lowercase();
    if !email.is_empty() {
      let stale_keys: Vec<String> = self
        .accounts
        .iter()
        .filter(|(k, v)| {
          *k != &key
            && (v.email.trim().eq_ignore_ascii_case(&email)
              || (!result.account_id.is_empty() && v.account_id == result.account_id)
              || *k == &result.account_id)
        })
        .map(|(k, _)| k.clone())
        .collect();
      for stale in stale_keys {
        self.accounts.remove(&stale);
        let stale_path = self.base_dir.join(format!("{stale}.json"));
        let _ = fs::remove_file(stale_path);
      }
    }

    // Also remove legacy file named by account_id if different from email key.
    if !result.account_id.is_empty() && result.account_id != key {
      let legacy = self.base_dir.join(format!("{}.json", result.account_id));
      let _ = fs::remove_file(legacy);
    }

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
    let needle = account_id.trim().to_lowercase();
    let key = if self.accounts.contains_key(account_id) {
      account_id.to_string()
    } else if self.accounts.contains_key(&needle) {
      needle.clone()
    } else {
      self
        .accounts
        .iter()
        .find(|(_, v)| {
          v.account_id == account_id
            || v.email.eq_ignore_ascii_case(account_id)
            || v.email.trim().to_lowercase() == needle
        })
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| account_id.to_string())
    };

    let removed_val = self.accounts.remove(&key);
    let removed = removed_val.is_some();
    if removed {
      let file_path = self.base_dir.join(format!("{key}.json"));
      let _ = fs::remove_file(file_path);
      // Clean legacy accountId-named file if present.
      if let Some(r) = removed_val {
        if !r.account_id.is_empty() && r.account_id != key {
          let legacy = self.base_dir.join(format!("{}.json", r.account_id));
          let _ = fs::remove_file(legacy);
        }
      }
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

  /// Patch editable fields for a stored account.
  /// When email changes, the row is re-keyed and the old email file is removed.
  #[allow(clippy::too_many_arguments)]
  fn update_fields(
    &mut self,
    account_id: &str,
    email: Option<String>,
    password: Option<String>,
    totp_secret: Option<String>,
    note: Option<String>,
    phone_number: Option<String>,
    status: Option<LoginResultStatus>,
  ) -> Result<LoginResult, String> {
    let Some(mut account) = self.get(account_id) else {
      return Err(format!("Account {account_id} not found"));
    };
    let old_key = Self::result_key(&account);

    if let Some(e) = email {
      let e = e.trim().to_string();
      if e.is_empty() {
        return Err("Email cannot be empty".into());
      }
      // Collision: another row already owns this email key.
      let new_key = e.to_lowercase();
      if new_key != old_key {
        if let Some(other) = self.accounts.get(&new_key) {
          if other.account_id != account.account_id
            || (!other.account_id.is_empty() && other.email != account.email)
          {
            // Same email key already used by a different stored row.
            if other.email.eq_ignore_ascii_case(&account.email) {
              // no-op
            } else {
              return Err(format!("Another stored account already uses email {e}"));
            }
          }
        }
      }
      account.email = e;
    }
    if let Some(p) = password {
      account.password = p;
    }
    if let Some(t) = totp_secret {
      account.totp_secret = t.trim().replace(' ', "").to_uppercase();
    }
    if let Some(n) = note {
      account.note = n;
    }
    if let Some(phone) = phone_number {
      account.phone_number = phone.trim().to_string();
    }
    if let Some(s) = status {
      account.status = s;
      if account.status == LoginResultStatus::Exported {
        account.exported_at = Some(Utc::now());
      }
    }

    // If email changed, drop old key/file before saving under the new key.
    let new_key = Self::result_key(&account);
    if new_key != old_key {
      self.accounts.remove(&old_key);
      let old_path = self.base_dir.join(format!("{old_key}.json"));
      let _ = fs::remove_file(old_path);
    }

    self.save(&account);
    Ok(account)
  }

  fn get(&self, account_id: &str) -> Option<LoginResult> {
    if let Some(v) = self.accounts.get(account_id) {
      return Some(v.clone());
    }
    let needle = account_id.trim().to_lowercase();
    if let Some(v) = self.accounts.get(&needle) {
      return Some(v.clone());
    }
    self
      .accounts
      .values()
      .find(|v| {
        v.account_id == account_id
          || v.email.eq_ignore_ascii_case(account_id)
          || v.email.trim().to_lowercase() == needle
      })
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

/// Update editable credential/profile fields for a stored login result.
pub fn update_login_result_fields(
  account_id: &str,
  email: Option<String>,
  password: Option<String>,
  totp_secret: Option<String>,
  note: Option<String>,
  phone_number: Option<String>,
  status: Option<LoginResultStatus>,
) -> Result<LoginResult, String> {
  STORE.lock().unwrap().update_fields(
    account_id,
    email,
    password,
    totp_secret,
    note,
    phone_number,
    status,
  )
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
      password: "secret".into(),
      totp_secret: String::new(),
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
      password: "secret".into(),
      totp_secret: String::new(),
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

  #[test]
  fn result_key_prefers_email_over_account_id() {
    let mut r = sample_result(LoginResultStatus::Available, true, "tok");
    r.email = "User@X.com".into();
    r.account_id = "acc-uuid".into();
    assert_eq!(LoginResultStore::result_key(&r), "user@x.com");
  }

  #[test]
  fn is_preferable_prefers_success_token() {
    let ok = sample_result(LoginResultStatus::Available, true, "tok");
    let bad = sample_result(LoginResultStatus::Invalid, false, "");
    assert!(LoginResultStore::is_preferable(&ok, &bad));
    assert!(!LoginResultStore::is_preferable(&bad, &ok));
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
      password: "secret".into(),
      totp_secret: String::new(),
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
