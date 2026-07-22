use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};

use super::types::{AccountInventoryStatus, CdkInventoryRecord, RegistrationResult};
use crate::app_dirs::data_dir;

static STORE: Lazy<Mutex<CredentialStore>> = Lazy::new(|| Mutex::new(CredentialStore::new()));
static CDK_STORE: Lazy<Arc<Mutex<CdkStore>>> = Lazy::new(|| Arc::new(Mutex::new(CdkStore::new())));

pub const MAX_ACCOUNTS_PER_CDK: u32 = 6;
const USAGE_LEDGER_FILE: &str = "usage-ledger.json";

/// Thread-safe JSON file store for registration results.
struct CredentialStore {
  accounts: HashMap<String, RegistrationResult>,
  base_dir: PathBuf,
}

/// Thread-safe JSON file store for per-CDK stats.
struct CdkStore {
  records: HashMap<String, CdkInventoryRecord>,
  usage: HashMap<String, u32>,
  reservations: HashMap<String, HashMap<String, u32>>,
  base_dir: PathBuf,
}

pub struct CdkSlotReservation {
  store: Arc<Mutex<CdkStore>>,
  cdk: String,
  task_id: String,
  remaining: u32,
}

impl CdkSlotReservation {
  pub fn claim_slot(&mut self) -> Result<(), String> {
    if self.remaining == 0 {
      return Err(format!("no reserved account slots remain for {}", self.cdk));
    }
    self
      .store
      .lock()
      .map_err(|error| format!("Failed to lock CDK inventory: {error}"))?
      .claim_slot(&self.cdk, &self.task_id)?;
    self.remaining -= 1;
    Ok(())
  }

  /// Roll back a claimed slot when the registration attempt did not succeed
  /// (failure or cancel). Decrements `usage` and re-absorbs the slot into
  /// `remaining` so the same task can retry, and the next task sees capacity.
  /// Safe to call multiple times per claim — only rolls back once per claim.
  pub fn release_slot(&mut self) -> Result<(), String> {
    // Match `claim_slot`: we can only release a slot that was previously claimed
    // (i.e. still reflected as `usage += 1` but not yet re-absorbed into `remaining`).
    // We track this implicitly: release only acts when we have no remaining
    // budget left to release via Drop. Caller should call release_slot exactly
    // once after a failed claim_slot.
    let mut store = self
      .store
      .lock()
      .map_err(|error| format!("Failed to lock CDK inventory: {error}"))?;
    store.release_usage(&self.cdk, &self.task_id)?;
    self.remaining += 1;
    Ok(())
  }
}

impl Drop for CdkSlotReservation {
  fn drop(&mut self) {
    if self.remaining == 0 {
      return;
    }
    if let Ok(mut store) = self.store.lock() {
      store.release_reservation(&self.cdk, &self.task_id, self.remaining);
    }
  }
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
    Self::with_base_dir(base_dir)
  }

  fn with_base_dir(base_dir: impl AsRef<Path>) -> Self {
    let base_dir = base_dir.as_ref().to_path_buf();
    let _ = fs::create_dir_all(&base_dir);

    let mut records = HashMap::new();
    if let Ok(entries) = fs::read_dir(&base_dir) {
      for entry in entries.flatten() {
        let path = entry.path();
        if path
          .file_name()
          .is_some_and(|name| name == USAGE_LEDGER_FILE)
        {
          continue;
        }
        if path.extension().is_some_and(|ext| ext == "json") {
          if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(record) = serde_json::from_str::<CdkInventoryRecord>(&content) {
              let key = canonical_cdk(&record.cdk);
              let should_replace = records
                .get(&key)
                .is_none_or(|existing: &CdkInventoryRecord| {
                  record.updated_at > existing.updated_at
                });
              if should_replace {
                records.insert(key, record);
              }
            }
          }
        }
      }
    }

    let ledger_path = base_dir.join(USAGE_LEDGER_FILE);
    let parsed_usage = fs::read_to_string(&ledger_path)
      .ok()
      .and_then(|content| serde_json::from_str::<HashMap<String, u32>>(&content).ok());
    let (usage, ledger_needs_write) = if let Some(parsed) = parsed_usage {
      let mut migrated = HashMap::<String, u32>::new();
      let mut changed = false;
      for (key, count) in parsed {
        let hashed = if is_usage_key(&key) {
          key
        } else {
          changed = true;
          usage_key(&key)
        };
        let used = migrated.entry(hashed).or_default();
        *used = (*used).max(count);
      }
      (migrated, changed)
    } else {
      let usage = records
        .iter()
        .map(|(key, record)| (usage_key(key), record.attempted))
        .collect();
      (usage, true)
    };

    let store = Self {
      records,
      usage,
      reservations: HashMap::new(),
      base_dir,
    };
    if ledger_needs_write && !store.usage.is_empty() {
      let _ = store.persist_usage();
    }
    store
  }

  fn save(&mut self, record: &CdkInventoryRecord) {
    let key = canonical_cdk(&record.cdk);
    self.records.insert(key.clone(), record.clone());
    let file_path = self.base_dir.join(format!("{}.json", cdk_file_key(&key)));
    if let Ok(json) = serde_json::to_string_pretty(record) {
      let _ = fs::write(file_path, json);
    }
  }

  fn list_all(&self) -> Vec<CdkInventoryRecord> {
    let mut results: Vec<_> = self.records.values().cloned().collect();
    for record in &mut results {
      // Always recompute from the usage ledger + reservations; never trust disk.
      record.remaining = self.remaining_capacity(&record.cdk);
    }
    results.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
    results
  }

  fn get(&self, cdk: &str) -> Option<CdkInventoryRecord> {
    self.records.get(&canonical_cdk(cdk)).cloned()
  }

  fn delete(&mut self, cdk: &str) -> bool {
    let key = canonical_cdk(cdk);
    let removed = self.records.remove(&key).is_some();
    if removed {
      let canonical_path = self.base_dir.join(format!("{}.json", cdk_file_key(&key)));
      let _ = fs::remove_file(canonical_path);
      if let Ok(entries) = fs::read_dir(&self.base_dir) {
        for entry in entries.flatten() {
          let path = entry.path();
          if path.extension().is_none_or(|extension| extension != "json")
            || path
              .file_name()
              .is_some_and(|name| name == USAGE_LEDGER_FILE)
          {
            continue;
          }
          let matches_cdk = fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str::<CdkInventoryRecord>(&content).ok())
            .is_some_and(|record| canonical_cdk(&record.cdk) == key);
          if matches_cdk {
            let _ = fs::remove_file(path);
          }
        }
      }
      // Reset the usage ledger for this CDK so a deleted stats row also frees
      // its reserved quota — otherwise retrying the same CDK stays blocked by
      // stale `used` counts. Drop any in-memory reservations for the same CDK
      // as well, since the user explicitly cleared its stats.
      let usage_key = usage_key(&key);
      if self.usage.remove(&usage_key).is_some() {
        let _ = self.persist_usage();
      }
      self.reservations.remove(&key);
    }
    removed
  }

  fn check_capacity(&self, cdk: &str, requested: u32) -> Result<(), String> {
    let cdk = canonical_cdk(cdk);
    let used = self
      .usage
      .get(&usage_key(&cdk))
      .copied()
      .unwrap_or_default();
    let reserved: u32 = self
      .reservations
      .get(&cdk)
      .into_iter()
      .flat_map(HashMap::values)
      .copied()
      .sum();
    if used.saturating_add(reserved).saturating_add(requested) > MAX_ACCOUNTS_PER_CDK {
      return Err(format!(
        "CDK/card {cdk} has {used} used and {reserved} reserved slot(s); cannot reserve {requested} more (max {MAX_ACCOUNTS_PER_CDK})"
      ));
    }
    Ok(())
  }

  /// Remaining slots on a CDK after subtracting used + reserved from the max.
  /// Drives the retry-cap flow: a partial run can be retried for the leftover
  /// capacity without the user having to delete CDK stats first.
  fn remaining_capacity(&self, cdk: &str) -> u32 {
    let cdk = canonical_cdk(cdk);
    let used = self
      .usage
      .get(&usage_key(&cdk))
      .copied()
      .unwrap_or_default();
    let reserved: u32 = self
      .reservations
      .get(&cdk)
      .into_iter()
      .flat_map(HashMap::values)
      .copied()
      .sum();
    MAX_ACCOUNTS_PER_CDK.saturating_sub(used.saturating_add(reserved))
  }

  fn reserve_unchecked(
    &mut self,
    store: &Arc<Mutex<Self>>,
    cdk: &str,
    task_id: &str,
    requested: u32,
  ) -> CdkSlotReservation {
    let cdk = canonical_cdk(cdk);
    let task_reservations = self.reservations.entry(cdk.clone()).or_default();
    let task_reserved = task_reservations.entry(task_id.to_string()).or_default();
    *task_reserved = task_reserved.saturating_add(requested);
    CdkSlotReservation {
      store: store.clone(),
      cdk,
      task_id: task_id.to_string(),
      remaining: requested,
    }
  }

  fn claim_slot(&mut self, cdk: &str, task_id: &str) -> Result<(), String> {
    {
      let task_reservations = self
        .reservations
        .get_mut(cdk)
        .ok_or_else(|| format!("no account slots reserved for {cdk}"))?;
      let reserved = task_reservations
        .get_mut(task_id)
        .ok_or_else(|| format!("task {task_id} has no account slots reserved for {cdk}"))?;
      if *reserved == 0 {
        return Err(format!(
          "task {task_id} has no account slots remaining for {cdk}"
        ));
      }
      *reserved -= 1;
    }

    let usage_key = usage_key(cdk);
    let previous_usage = self.usage.get(&usage_key).copied().unwrap_or_default();
    self
      .usage
      .insert(usage_key.clone(), previous_usage.saturating_add(1));
    if let Err(error) = self.persist_usage() {
      if let Some(reserved) = self
        .reservations
        .get_mut(cdk)
        .and_then(|reservations| reservations.get_mut(task_id))
      {
        *reserved += 1;
      }
      self.usage.insert(usage_key, previous_usage);
      return Err(error);
    }

    let remove_card = if let Some(task_reservations) = self.reservations.get_mut(cdk) {
      if task_reservations.get(task_id).copied() == Some(0) {
        task_reservations.remove(task_id);
      }
      task_reservations.is_empty()
    } else {
      false
    };
    if remove_card {
      self.reservations.remove(cdk);
    }
    Ok(())
  }

  fn release_reservation(&mut self, cdk: &str, task_id: &str, count: u32) {
    let Some(task_reservations) = self.reservations.get_mut(cdk) else {
      return;
    };
    let Some(reserved) = task_reservations.get_mut(task_id) else {
      return;
    };
    *reserved = reserved.saturating_sub(count);
    if *reserved == 0 {
      task_reservations.remove(task_id);
    }
    if task_reservations.is_empty() {
      self.reservations.remove(cdk);
    }
  }

  /// Roll back one claimed slot: decrement `usage` by 1 and persist.
  /// Called when a registration attempt failed or was cancelled after
  /// `claim_slot` already incremented usage. Re-absorbs the slot so the
  /// next `check_capacity` sees the freed budget and retry is possible.
  fn release_usage(&mut self, cdk: &str, task_id: &str) -> Result<(), String> {
    let cdk = canonical_cdk(cdk);
    let usage_key = usage_key(&cdk);
    let previous_usage = self.usage.get(&usage_key).copied().unwrap_or_default();
    if previous_usage == 0 {
      // Nothing to roll back — likely already released or never claimed.
      // Re-create the reservation entry so Drop doesn't double-release.
      let task_reservations = self.reservations.entry(cdk.clone()).or_default();
      let reserved = task_reservations.entry(task_id.to_string()).or_default();
      *reserved = reserved.saturating_add(1);
      return Ok(());
    }
    self.usage.insert(usage_key.clone(), previous_usage - 1);
    if let Err(error) = self.persist_usage() {
      // Restore in-memory and surface error.
      self.usage.insert(usage_key, previous_usage);
      return Err(error);
    }
    // Re-absorb the slot into this task's reservation so Drop releases it
    // cleanly if the task ends without retrying.
    let task_reservations = self.reservations.entry(cdk.clone()).or_default();
    let reserved = task_reservations.entry(task_id.to_string()).or_default();
    *reserved = reserved.saturating_add(1);
    Ok(())
  }

  fn persist_usage(&self) -> Result<(), String> {
    let json = serde_json::to_string_pretty(&self.usage)
      .map_err(|error| format!("Failed to serialize CDK usage ledger: {error}"))?;
    let ledger_path = self.base_dir.join(USAGE_LEDGER_FILE);
    let mut temp_file = tempfile::NamedTempFile::new_in(&self.base_dir)
      .map_err(|error| format!("Failed to create CDK usage temp file: {error}"))?;
    temp_file
      .write_all(json.as_bytes())
      .map_err(|error| format!("Failed to write CDK usage temp file: {error}"))?;
    temp_file
      .as_file()
      .sync_all()
      .map_err(|error| format!("Failed to sync CDK usage temp file: {error}"))?;
    temp_file
      .persist(&ledger_path)
      .map_err(|error| format!("Failed to replace CDK usage ledger: {}", error.error))?;
    Ok(())
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

fn usage_key(cdk: &str) -> String {
  let digest = Sha256::digest(canonical_cdk(cdk).as_bytes());
  let mut key = String::with_capacity(71);
  key.push_str("sha256:");
  for byte in digest {
    std::fmt::Write::write_fmt(&mut key, format_args!("{byte:02x}"))
      .expect("writing SHA-256 digest to String cannot fail");
  }
  key
}

fn is_usage_key(key: &str) -> bool {
  key
    .strip_prefix("sha256:")
    .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn canonical_cdk(cdk: &str) -> String {
  cdk.trim().to_ascii_uppercase()
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

fn reserve_slots_in_store(
  store: &Arc<Mutex<CdkStore>>,
  cdks: &[String],
  task_id: &str,
  requested_per_cdk: u32,
) -> Result<Vec<CdkSlotReservation>, String> {
  let mut locked = store
    .lock()
    .map_err(|error| format!("Failed to lock CDK inventory: {error}"))?;

  let mut requested_by_cdk = HashMap::<String, u32>::new();
  for cdk in cdks {
    let requested = requested_by_cdk.entry(canonical_cdk(cdk)).or_default();
    *requested = requested.saturating_add(requested_per_cdk);
  }
  for (cdk, requested) in requested_by_cdk {
    locked.check_capacity(&cdk, requested)?;
  }

  Ok(
    cdks
      .iter()
      .map(|cdk| locked.reserve_unchecked(store, cdk, task_id, requested_per_cdk))
      .collect(),
  )
}

fn reserve_slots_per_in_store(
  store: &Arc<Mutex<CdkStore>>,
  cdks_targets: &[(String, u32)],
  task_id: &str,
) -> Result<Vec<CdkSlotReservation>, String> {
  let mut locked = store
    .lock()
    .map_err(|error| format!("Failed to lock CDK inventory: {error}"))?;

  let mut requested_by_cdk = HashMap::<String, u32>::new();
  for (cdk, requested) in cdks_targets {
    if *requested == 0 {
      continue;
    }
    let total = requested_by_cdk.entry(canonical_cdk(cdk)).or_default();
    *total = total.saturating_add(*requested);
  }
  for (cdk, requested) in &requested_by_cdk {
    locked.check_capacity(cdk, *requested)?;
  }

  Ok(
    cdks_targets
      .iter()
      .filter(|(_, requested)| *requested > 0)
      .map(|(cdk, requested)| locked.reserve_unchecked(store, cdk, task_id, *requested))
      .collect(),
  )
}

pub fn reserve_cdk_slots(
  cdks: &[String],
  task_id: &str,
  requested_per_cdk: u32,
) -> Result<Vec<CdkSlotReservation>, String> {
  reserve_slots_in_store(&CDK_STORE, cdks, task_id, requested_per_cdk)
}

/// Reserve a different number of slots per CDK. Entries with `requested == 0`
/// are skipped (caller treats them as "CDK already full"). Duplicate CDKs in
/// the list still share a single batch budget — their requested counts are
/// summed before `check_capacity` runs, mirroring `reserve_cdk_slots`.
pub fn reserve_cdk_slots_per(
  cdks_targets: &[(String, u32)],
  task_id: &str,
) -> Result<Vec<CdkSlotReservation>, String> {
  reserve_slots_per_in_store(&CDK_STORE, cdks_targets, task_id)
}

/// How many account slots remain on a CDK after subtracting `used` (persisted
/// usage ledger) and `reserved` (in-flight reservations) from
/// `MAX_ACCOUNTS_PER_CDK`. The retry-cap flow uses this to clamp the per-CDK
/// request so a partial run can be retried without tripping quota validation.
pub fn cdk_remaining_capacity(cdk: &str) -> u32 {
  CDK_STORE
    .lock()
    .map(|store| store.remaining_capacity(cdk))
    .unwrap_or(MAX_ACCOUNTS_PER_CDK)
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

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Barrier;
  use tempfile::TempDir;

  fn test_store(temp: &TempDir) -> Arc<Mutex<CdkStore>> {
    Arc::new(Mutex::new(CdkStore::with_base_dir(temp.path())))
  }

  #[test]
  fn admits_fifth_plus_one_and_rejects_fifth_plus_two() {
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);
    {
      let mut locked = store.lock().unwrap();
      locked.usage.insert(usage_key("MAIL-TEST"), 5);
      locked.persist_usage().unwrap();
    }

    let mut one = reserve_slots_in_store(&store, &[" mail-test ".into()], "task-1", 1).unwrap();
    one[0].claim_slot().unwrap();
    drop(one);
    assert!(reserve_slots_in_store(&store, &["MAIL-TEST".into()], "task-2", 1).is_err());

    let second = test_store(&temp);
    assert!(reserve_slots_in_store(&second, &["MAIL-TEST".into()], "task-3", 1).is_err());
  }

  #[test]
  fn rejects_batch_without_leaving_partial_reservations() {
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);
    store
      .lock()
      .unwrap()
      .usage
      .insert(usage_key("MAIL-FULL"), MAX_ACCOUNTS_PER_CDK);

    let result = reserve_slots_in_store(
      &store,
      &["MAIL-FREE".into(), "MAIL-FULL".into()],
      "task-1",
      1,
    );
    assert!(result.is_err());
    assert!(store.lock().unwrap().reservations.is_empty());
  }

  #[test]
  fn active_reservations_are_atomic_and_release_on_drop() {
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);
    let first = reserve_slots_in_store(&store, &["MAIL-TEST".into()], "task-1", 6).unwrap();
    assert!(reserve_slots_in_store(&store, &["mail-test".into()], "task-2", 1).is_err());
    drop(first);
    assert!(reserve_slots_in_store(&store, &[" mail-test ".into()], "task-2", 6).is_ok());
  }

  #[test]
  fn release_slot_rolls_back_usage_so_retry_fits() {
    // Simulates the failed/cancelled registration path: claim_slot increments
    // usage, release_slot must decrement it so the same CDK can be retried
    // up to MAX_ACCOUNTS_PER_CDK again.
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);

    let mut reservations =
      reserve_slots_in_store(&store, &["MAIL-FAIL".into()], "task-1", 6).unwrap();
    let mut reservation = reservations.pop().unwrap();
    reservation.claim_slot().unwrap();
    // Usage ledger now reflects 1 used even though no account was created.
    assert_eq!(
      store
        .lock()
        .unwrap()
        .usage
        .get(&usage_key("MAIL-FAIL"))
        .copied(),
      Some(1)
    );

    // Registration failed — roll back the claimed slot.
    reservation.release_slot().unwrap();
    assert_eq!(
      store
        .lock()
        .unwrap()
        .usage
        .get(&usage_key("MAIL-FAIL"))
        .copied(),
      Some(0)
    );

    // Releasing the reservation (drop) must not double-count: retrying with
    // full 6 budget must succeed.
    drop(reservation);
    assert!(reserve_slots_in_store(&store, &["MAIL-FAIL".into()], "task-2", 6).is_ok());
  }

  #[test]
  fn release_slot_without_claim_is_noop_on_usage() {
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);
    let mut reservations = reserve_slots_in_store(&store, &["MAIL-X".into()], "task-1", 1).unwrap();
    let mut reservation = reservations.pop().unwrap();
    // Releasing without ever claiming should not push usage below zero.
    reservation.release_slot().unwrap();
    assert_eq!(
      store
        .lock()
        .unwrap()
        .usage
        .get(&usage_key("MAIL-X"))
        .copied(),
      None
    );
  }

  #[test]
  fn list_all_enriches_remaining_from_usage_and_reservations() {
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);
    {
      let mut locked = store.lock().unwrap();
      locked.usage.insert(usage_key("MAIL-REM"), 2);
      locked.persist_usage().unwrap();
      locked.save(&CdkInventoryRecord::new("MAIL-REM", 3, "task-list"));
      assert_eq!(locked.remaining_capacity("MAIL-REM"), 4);
    }

    let listed = store.lock().unwrap().list_all();
    let row = listed
      .iter()
      .find(|r| canonical_cdk(&r.cdk) == "MAIL-REM")
      .expect("MAIL-REM row");
    assert_eq!(row.remaining, 4);

    // Active reservation of 1 reduces remaining to 3 while held.
    let _hold = reserve_slots_in_store(&store, &["MAIL-REM".into()], "task-hold", 1).unwrap();
    let listed2 = store.lock().unwrap().list_all();
    let row2 = listed2
      .iter()
      .find(|r| canonical_cdk(&r.cdk) == "MAIL-REM")
      .expect("MAIL-REM row");
    assert_eq!(row2.remaining, 3);
  }

  #[test]
  fn concurrent_reservations_allow_only_one_winner() {
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
      .map(|index| {
        let store = store.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
          barrier.wait();
          reserve_slots_in_store(&store, &["MAIL-TEST".into()], &format!("task-{index}"), 6)
        })
      })
      .collect();
    let results: Vec<_> = handles
      .into_iter()
      .map(|handle| handle.join().unwrap())
      .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
  }

  #[test]
  fn duplicate_cards_share_one_batch_budget() {
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);
    let result = reserve_slots_in_store(
      &store,
      &["MAIL-TEST".into(), " mail-test ".into()],
      "task-1",
      4,
    );
    assert!(result.is_err());
    assert!(store.lock().unwrap().reservations.is_empty());
  }

  #[test]
  fn reload_uses_newest_canonical_record_and_repairs_ledger() {
    let temp = TempDir::new().unwrap();
    let mut older = CdkInventoryRecord::new("mail-test", 2, "task-old");
    older.attempted = 2;
    older.updated_at = Utc::now() - chrono::Duration::minutes(1);
    let mut newer = CdkInventoryRecord::new("MAIL-TEST", 5, "task-new");
    newer.attempted = 5;
    newer.updated_at = Utc::now();
    fs::write(
      temp.path().join("mail-test.json"),
      serde_json::to_string(&older).unwrap(),
    )
    .unwrap();
    fs::write(
      temp.path().join("MAIL-TEST.json"),
      serde_json::to_string(&newer).unwrap(),
    )
    .unwrap();
    fs::write(temp.path().join(USAGE_LEDGER_FILE), "invalid json").unwrap();

    let store = test_store(&temp);
    let locked = store.lock().unwrap();
    assert_eq!(locked.get(" mail-test ").unwrap().task_id, "task-new");
    assert_eq!(locked.usage.get(&usage_key("MAIL-TEST")), Some(&5));
    let repaired: HashMap<String, u32> =
      serde_json::from_str(&fs::read_to_string(temp.path().join(USAGE_LEDGER_FILE)).unwrap())
        .unwrap();
    assert_eq!(repaired.get(&usage_key("MAIL-TEST")), Some(&5));
    assert!(!fs::read_to_string(temp.path().join(USAGE_LEDGER_FILE))
      .unwrap()
      .contains("MAIL-TEST"));
  }

  #[test]
  fn valid_ledger_remains_quota_source_of_truth() {
    let temp = TempDir::new().unwrap();
    let mut record = CdkInventoryRecord::new("MAIL-TEST", 5, "task-old");
    record.attempted = 5;
    fs::write(
      temp.path().join("MAIL-TEST.json"),
      serde_json::to_string(&record).unwrap(),
    )
    .unwrap();
    fs::write(temp.path().join(USAGE_LEDGER_FILE), r#"{"MAIL-TEST":2}"#).unwrap();

    let store = test_store(&temp);
    let locked = store.lock().unwrap();
    assert_eq!(locked.usage.get(&usage_key("MAIL-TEST")), Some(&2));
    assert!(!fs::read_to_string(temp.path().join(USAGE_LEDGER_FILE))
      .unwrap()
      .contains("MAIL-TEST"));
  }

  #[test]
  fn deleting_inventory_record_resets_usage_so_retry_works() {
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);
    let mut reservation =
      reserve_slots_in_store(&store, &["MAIL-TEST".into()], "task-1", 6).unwrap();
    for _ in 0..6 {
      reservation[0].claim_slot().unwrap();
    }
    drop(reservation);
    {
      let mut locked = store.lock().unwrap();
      locked.save(&CdkInventoryRecord::new("MAIL-TEST", 6, "task-1"));
      fs::write(
        temp.path().join("mail-test.json"),
        serde_json::to_string(&CdkInventoryRecord::new("mail-test", 1, "legacy")).unwrap(),
      )
      .unwrap();
      assert!(locked.delete("MAIL-TEST"));
    }
    assert!(!temp.path().join("MAIL-TEST.json").exists());
    assert!(!temp.path().join("mail-test.json").exists());
    assert!(temp.path().join(USAGE_LEDGER_FILE).exists());

    // Deleting the stats row must also reset the usage ledger — otherwise the
    // user cannot retry the same CDK because stale `used` counts keep blocking
    // `check_capacity`. After delete, a full 6-slot reservation must succeed.
    let reloaded = test_store(&temp);
    assert!(reserve_slots_in_store(&reloaded, &["MAIL-TEST".into()], "task-2", 6).is_ok());
    let persisted: HashMap<String, u32> =
      serde_json::from_str(&fs::read_to_string(temp.path().join(USAGE_LEDGER_FILE)).unwrap())
        .unwrap();
    assert!(!persisted.contains_key(&usage_key("MAIL-TEST")));
  }

  #[test]
  fn remaining_capacity_reflects_used_and_reserved() {
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);
    {
      let mut locked = store.lock().unwrap();
      locked.usage.insert(usage_key("MAIL-TEST"), 2);
      locked.persist_usage().unwrap();
    }
    // 6 - 2 used = 4 free before any reservation.
    assert_eq!(store.lock().unwrap().remaining_capacity("mail-test"), 4);
    // Holding a 3-slot reservation drops visible capacity to 1.
    let _r = reserve_slots_in_store(&store, &["MAIL-TEST".into()], "task-1", 3).unwrap();
    assert_eq!(store.lock().unwrap().remaining_capacity("MAIL-TEST"), 1);
  }

  #[test]
  fn reserve_per_cdk_caps_partial_retry_and_skips_full() {
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);
    {
      let mut locked = store.lock().unwrap();
      locked.usage.insert(usage_key("MAIL-PARTIAL"), 2);
      locked
        .usage
        .insert(usage_key("MAIL-FULL"), MAX_ACCOUNTS_PER_CDK);
      locked.persist_usage().unwrap();
    }
    // Retry asks for 6 each; capacity must cap PARTIAL to 4 and skip FULL.
    let targets = vec![
      ("MAIL-PARTIAL".to_string(), 4u32),
      ("MAIL-FULL".to_string(), 0u32),
    ];
    let mut reservations = reserve_slots_per_in_store(&store, &targets, "task-retry").unwrap();
    assert_eq!(reservations.len(), 1);
    let mut reservation = reservations.pop().unwrap();
    for _ in 0..4 {
      reservation.claim_slot().unwrap();
    }
    // The full CDK must remain blocked.
    assert!(
      reserve_slots_per_in_store(&store, &[("MAIL-FULL".into(), 1u32)], "task-retry-2").is_err()
    );
  }

  #[test]
  fn reserve_per_cdk_rejects_when_target_exceeds_remaining() {
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);
    store
      .lock()
      .unwrap()
      .usage
      .insert(usage_key("MAIL-TEST"), 5);
    // Asking for 2 when only 1 remains must fail and leave no reservations.
    let result = reserve_slots_per_in_store(&store, &[("MAIL-TEST".into(), 2u32)], "task-1");
    assert!(result.is_err());
    assert!(store.lock().unwrap().reservations.is_empty());
  }

  #[test]
  fn reserve_per_cdk_sums_duplicate_targets_before_capacity_check() {
    let temp = TempDir::new().unwrap();
    let store = test_store(&temp);
    let result = reserve_slots_per_in_store(
      &store,
      &[("MAIL-TEST".into(), 3u32), (" mail-test ".into(), 4u32)],
      "task-1",
    );
    // 3 + 4 = 7 > MAX_ACCOUNTS_PER_CDK — must reject atomically.
    assert!(result.is_err());
    assert!(store.lock().unwrap().reservations.is_empty());
  }
}
