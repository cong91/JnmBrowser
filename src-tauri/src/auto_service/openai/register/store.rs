use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};

use super::types::{
  AccountInventoryStatus, CdkInventoryRecord, EmailProviderProvenance, RegistrationOutcomeReason,
  RegistrationResult, TwoFactorBackfillAccessState, TwoFactorBackfillOutcome,
  TwoFactorBackfillState,
};
use crate::app_dirs::data_dir;
use crate::email::EmailProvider;

static STORE: Lazy<Result<Mutex<CredentialStore>, String>> =
  Lazy::new(|| CredentialStore::new().map(Mutex::new));
static CDK_STORE: Lazy<Arc<Mutex<CdkStore>>> = Lazy::new(|| Arc::new(Mutex::new(CdkStore::new())));

pub const MAX_ACCOUNTS_PER_CDK: u32 = 6;
const USAGE_LEDGER_FILE: &str = "usage-ledger.json";

/// Thread-safe JSON file store for registration results.
struct CredentialStore {
  accounts: HashMap<String, RegistrationResult>,
  account_paths: HashMap<String, PathBuf>,
  stale_account_paths: HashMap<String, Vec<PathBuf>>,
  base_dir: PathBuf,
  #[cfg(test)]
  fail_account_writes: Vec<AtomicWriteFailureStage>,
}

/// Lifecycle mutations supported by the narrow 2FA backfill CAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackfillPatchOperation {
  Start,
  FinalizeEnabled,
  FinalizeFailed,
  FinalizeCancelled,
  FinalizeReconciliationRequired,
}

/// Backend policy state that must still hold when a backfill CAS acquires the store lock.
#[derive(Debug, Clone)]
pub(crate) struct BackfillPatchPrecondition {
  operation: BackfillPatchOperation,
  operation_id: String,
  allow_free_trial_no: bool,
  acknowledge_legacy_access: bool,
  finalize_new_registration: bool,
}

impl BackfillPatchPrecondition {
  pub(crate) fn start(
    operation_id: impl Into<String>,
    allow_free_trial_no: bool,
    acknowledge_legacy_access: bool,
  ) -> Self {
    Self {
      operation: BackfillPatchOperation::Start,
      operation_id: operation_id.into(),
      allow_free_trial_no,
      acknowledge_legacy_access,
      finalize_new_registration: false,
    }
  }

  pub(crate) fn finalize_enabled(
    operation_id: impl Into<String>,
    allow_free_trial_no: bool,
    acknowledge_legacy_access: bool,
  ) -> Self {
    Self::terminal(
      BackfillPatchOperation::FinalizeEnabled,
      operation_id,
      allow_free_trial_no,
      acknowledge_legacy_access,
    )
  }

  pub(crate) fn finalize_new_registration_enabled(operation_id: impl Into<String>) -> Self {
    Self {
      operation: BackfillPatchOperation::FinalizeEnabled,
      operation_id: operation_id.into(),
      allow_free_trial_no: false,
      acknowledge_legacy_access: false,
      finalize_new_registration: true,
    }
  }

  pub(crate) fn finalize_failed(
    operation_id: impl Into<String>,
    allow_free_trial_no: bool,
    acknowledge_legacy_access: bool,
  ) -> Self {
    Self::terminal(
      BackfillPatchOperation::FinalizeFailed,
      operation_id,
      allow_free_trial_no,
      acknowledge_legacy_access,
    )
  }

  pub(crate) fn finalize_cancelled(
    operation_id: impl Into<String>,
    allow_free_trial_no: bool,
    acknowledge_legacy_access: bool,
  ) -> Self {
    Self::terminal(
      BackfillPatchOperation::FinalizeCancelled,
      operation_id,
      allow_free_trial_no,
      acknowledge_legacy_access,
    )
  }

  pub(crate) fn finalize_reconciliation_required(
    operation_id: impl Into<String>,
    allow_free_trial_no: bool,
    acknowledge_legacy_access: bool,
  ) -> Self {
    Self::terminal(
      BackfillPatchOperation::FinalizeReconciliationRequired,
      operation_id,
      allow_free_trial_no,
      acknowledge_legacy_access,
    )
  }

  fn terminal(
    operation: BackfillPatchOperation,
    operation_id: impl Into<String>,
    allow_free_trial_no: bool,
    acknowledge_legacy_access: bool,
  ) -> Self {
    Self {
      operation,
      operation_id: operation_id.into(),
      allow_free_trial_no,
      acknowledge_legacy_access,
      finalize_new_registration: false,
    }
  }

  #[cfg(test)]
  pub(crate) fn operation(&self) -> BackfillPatchOperation {
    self.operation
  }

  #[cfg(test)]
  pub(crate) fn operation_id(&self) -> &str {
    &self.operation_id
  }
}

/// Operation-specific payload for the narrow 2FA backfill CAS.
#[derive(Debug, Clone)]
pub(crate) struct TwoFactorBackfillPatch {
  operation: BackfillPatchOperation,
  totp_secret: Option<String>,
  access_state: Option<TwoFactorBackfillAccessState>,
  finalize_new_registration: bool,
}

impl TwoFactorBackfillPatch {
  pub(crate) fn start() -> Self {
    Self {
      operation: BackfillPatchOperation::Start,
      totp_secret: None,
      access_state: None,
      finalize_new_registration: false,
    }
  }

  pub(crate) fn finalize_enabled(totp_secret: impl Into<String>) -> Self {
    Self {
      operation: BackfillPatchOperation::FinalizeEnabled,
      totp_secret: Some(totp_secret.into()),
      access_state: None,
      finalize_new_registration: false,
    }
  }

  pub(crate) fn finalize_new_registration_enabled(totp_secret: impl Into<String>) -> Self {
    Self {
      operation: BackfillPatchOperation::FinalizeEnabled,
      totp_secret: Some(totp_secret.into()),
      access_state: None,
      finalize_new_registration: true,
    }
  }

  pub(crate) fn finalize_failed() -> Self {
    Self::terminal(BackfillPatchOperation::FinalizeFailed)
  }

  pub(crate) fn finalize_failed_locked() -> Self {
    Self {
      operation: BackfillPatchOperation::FinalizeFailed,
      totp_secret: None,
      access_state: Some(TwoFactorBackfillAccessState::Locked),
      finalize_new_registration: false,
    }
  }

  pub(crate) fn finalize_cancelled() -> Self {
    Self::terminal(BackfillPatchOperation::FinalizeCancelled)
  }

  pub(crate) fn finalize_reconciliation_required() -> Self {
    Self::terminal(BackfillPatchOperation::FinalizeReconciliationRequired)
  }

  fn terminal(operation: BackfillPatchOperation) -> Self {
    Self {
      operation,
      totp_secret: None,
      access_state: None,
      finalize_new_registration: false,
    }
  }

  #[cfg(test)]
  pub(crate) fn operation(&self) -> BackfillPatchOperation {
    self.operation
  }

  #[cfg(test)]
  pub(crate) fn totp_secret(&self) -> Option<&str> {
    self.totp_secret.as_deref()
  }

  #[cfg(test)]
  pub(crate) fn access_state(&self) -> Option<TwoFactorBackfillAccessState> {
    self.access_state
  }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtomicWriteFailureStage {
  Serialize,
  Write,
  Replace,
  SyncDestinationDirectory,
  RollbackDestination,
  DeleteStale,
  SyncStaleDirectory,
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
  fn new() -> Result<Self, String> {
    Self::with_base_dir(data_dir().join("registered_accounts"))
  }

  fn with_base_dir(base_dir: impl AsRef<Path>) -> Result<Self, String> {
    let base_dir = base_dir.as_ref().to_path_buf();
    fs::create_dir_all(&base_dir).map_err(|error| {
      format!(
        "Failed to create registered account directory {}: {error}",
        base_dir.display()
      )
    })?;

    let entries = fs::read_dir(&base_dir).map_err(|error| {
      format!(
        "Failed to read registered account directory {}: {error}",
        base_dir.display()
      )
    })?;
    let mut accounts = HashMap::new();
    let mut account_paths = HashMap::new();
    let mut stale_account_paths = HashMap::new();
    let mut entries = entries
      .map(|entry| {
        entry.map_err(|error| {
          format!(
            "Failed to read an entry from registered account directory {}: {error}",
            base_dir.display()
          )
        })
      })
      .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
      let path = entry.path();
      if path.extension().is_some_and(|ext| ext == "json") {
        if let Ok(content) = fs::read_to_string(&path) {
          if let Ok(result) = serde_json::from_str::<RegistrationResult>(&content) {
            insert_loaded_account(
              &mut accounts,
              &mut account_paths,
              &mut stale_account_paths,
              result,
              path,
            );
          }
        }
      }
    }

    Ok(Self {
      accounts,
      account_paths,
      stale_account_paths,
      base_dir,
      #[cfg(test)]
      fail_account_writes: Vec::new(),
    })
  }

  fn save(&mut self, result: &RegistrationResult) -> Result<(), String> {
    let key = account_key(result);
    let stale_key = self.find_stale_account_key(result, &key);
    if let (Some(stale_key), Some(destination)) = (stale_key.as_deref(), self.accounts.get(&key)) {
      let stale = self
        .accounts
        .get(stale_key)
        .expect("stale key was selected from the account map");
      if !records_match_for_key_migration(stale, destination) {
        return Err(format!(
          "Registered account migration collision from {stale_key} to {key}; destination belongs to a different record"
        ));
      }
    }

    let source_key = if self.accounts.contains_key(&key) {
      Some(key.clone())
    } else {
      stale_key.clone()
    };
    let existing = source_key
      .as_deref()
      .and_then(|source_key| self.accounts.get(source_key));
    if let Some(existing) = existing {
      if result.record_revision < existing.record_revision {
        return Err(format!(
          "Registered account revision conflict for {key}: incoming {}, current {}",
          result.record_revision, existing.record_revision
        ));
      }
    }
    let key_changed = source_key
      .as_deref()
      .is_some_and(|source_key| source_key != key);
    let mut source_paths = source_key
      .as_deref()
      .map(|source_key| self.tracked_account_paths(source_key))
      .unwrap_or_default();
    if let Some(stale_key) = stale_key.as_deref() {
      if source_key.as_deref() != Some(stale_key) {
        source_paths.extend(self.tracked_account_paths(stale_key));
      }
    }
    source_paths.sort();
    source_paths.dedup();

    let file_path = self.account_file_path_for_key(&key);
    let previous_bytes = read_optional_account_file(&file_path)?;
    if let Some(bytes) = previous_bytes.as_deref() {
      let canonical_path_is_known_source = self.account_paths.get(&key) == Some(&file_path);
      if !canonical_path_is_known_source {
        let destination = serde_json::from_slice::<RegistrationResult>(bytes).map_err(|error| {
          format!(
            "Registered account migration collision at {}; destination is not a valid record: {error}",
            file_path.display()
          )
        })?;
        let compatible = existing.is_some_and(|existing| {
          if key_changed {
            records_match_for_key_migration(existing, &destination)
          } else {
            same_canonical_stored_record(existing, &destination)
          }
        }) || existing.is_none()
          && same_canonical_stored_record(result, &destination);
        if !compatible {
          return Err(format!(
            "Registered account migration collision at {}; destination belongs to a different record",
            file_path.display()
          ));
        }
      }
    }

    let mut stale_files = Vec::new();
    for stale_path in source_paths {
      if stale_path == file_path {
        continue;
      }
      let Some(stale_bytes) = read_optional_account_file(&stale_path)? else {
        continue;
      };
      let stale = serde_json::from_slice::<RegistrationResult>(&stale_bytes).map_err(|error| {
        format!(
          "Registered account migration collision at {}; source is not a valid record: {error}",
          stale_path.display()
        )
      })?;
      let compatible = existing.is_some_and(|existing| {
        if key_changed {
          records_match_for_key_migration(existing, &stale)
        } else {
          same_canonical_stored_record(existing, &stale)
        }
      }) || existing.is_none() && same_canonical_stored_record(result, &stale);
      if !compatible {
        return Err(format!(
          "Registered account migration collision from {} to {key}; source belongs to a different record",
          stale_path.display()
        ));
      }
      stale_files.push((stale_path, stale_bytes));
    }

    self.persist_account(result, &file_path, previous_bytes.clone())?;

    let cleanup_result = (|| {
      for (stale_path, _) in &stale_files {
        #[cfg(test)]
        if self.take_failure(AtomicWriteFailureStage::DeleteStale) {
          return Err("Injected registered account stale-file deletion failure".into());
        }
        remove_account_file(stale_path)?;
      }
      if !stale_files.is_empty() {
        #[cfg(test)]
        if self.take_failure(AtomicWriteFailureStage::SyncStaleDirectory) {
          return Err("Injected registered account stale-directory sync failure".into());
        }
        sync_parent_directory(&self.base_dir, "registered account migration directory")?;
      }
      Ok::<(), String>(())
    })();
    if let Err(cleanup_error) = cleanup_result {
      let mut rollback_errors = Vec::new();
      for (stale_path, stale_bytes) in stale_files {
        if let Err(error) = restore_account_file(&self.base_dir, &stale_path, Some(stale_bytes)) {
          rollback_errors.push(format!("stale file: {error}"));
        }
      }
      if let Err(error) = restore_account_file(&self.base_dir, &file_path, previous_bytes) {
        rollback_errors.push(format!("canonical file: {error}"));
      }
      return if rollback_errors.is_empty() {
        Err(cleanup_error)
      } else {
        Err(format!(
          "{cleanup_error}; failed to roll back registered account key migration: {}",
          rollback_errors.join("; ")
        ))
      };
    }

    self.accounts.insert(key.clone(), result.clone());
    if let Some(source_key) = source_key {
      if source_key != key {
        self.accounts.remove(&source_key);
        self.account_paths.remove(&source_key);
        self.stale_account_paths.remove(&source_key);
      }
    }
    if let Some(stale_key) = stale_key {
      if stale_key != key {
        self.accounts.remove(&stale_key);
        self.account_paths.remove(&stale_key);
        self.stale_account_paths.remove(&stale_key);
      }
    }
    self.account_paths.insert(key.clone(), file_path);
    self.stale_account_paths.remove(&key);
    Ok(())
  }

  fn find_stale_account_key(&self, result: &RegistrationResult, key: &str) -> Option<String> {
    let result_id = result.account_id.trim();
    let result_email = result.email.trim();
    if result_id.is_empty() {
      return None;
    }
    self
      .accounts
      .iter()
      .filter(|(existing_key, account)| {
        existing_key.as_str() != key
          && ((account.account_id.trim() == result_id && !account.account_id.trim().is_empty())
            || (account.account_id.trim().is_empty()
              && !result_email.is_empty()
              && account.email.trim() == result_email))
      })
      .map(|(existing_key, _)| existing_key.clone())
      .min()
  }

  fn tracked_account_paths(&self, key: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = self.account_paths.get(key) {
      paths.push(path.clone());
    }
    if let Some(stale_paths) = self.stale_account_paths.get(key) {
      paths.extend(stale_paths.iter().cloned());
    }
    let canonical_path = self.account_file_path_for_key(key);
    if !paths.iter().any(|path| path == &canonical_path) {
      paths.push(canonical_path);
    }
    paths
  }

  fn account_file_path_for_key(&self, key: &str) -> PathBuf {
    self.base_dir.join(format!("{key}.json"))
  }

  fn persist_account(
    &mut self,
    result: &RegistrationResult,
    file_path: &Path,
    previous_bytes: Option<Vec<u8>>,
  ) -> Result<(), String> {
    #[cfg(test)]
    if self.take_failure(AtomicWriteFailureStage::Serialize) {
      return Err("Injected registered account serialization failure".into());
    }
    let json = serde_json::to_vec_pretty(result)
      .map_err(|error| format!("Failed to serialize registered account: {error}"))?;
    let mut temp_file = tempfile::NamedTempFile::new_in(&self.base_dir).map_err(|error| {
      format!(
        "Failed to create registered account temp file in {}: {error}",
        self.base_dir.display()
      )
    })?;
    #[cfg(test)]
    if self.take_failure(AtomicWriteFailureStage::Write) {
      return Err("Injected registered account write failure".into());
    }
    temp_file
      .write_all(&json)
      .map_err(|error| format!("Failed to write registered account temp file: {error}"))?;
    temp_file
      .as_file()
      .sync_all()
      .map_err(|error| format!("Failed to sync registered account temp file: {error}"))?;
    #[cfg(test)]
    if self.take_failure(AtomicWriteFailureStage::Replace) {
      return Err("Injected registered account replacement failure".into());
    }
    persist_temp_file(temp_file, file_path, "registered account")?;
    #[cfg(test)]
    let sync_result = if self.take_failure(AtomicWriteFailureStage::SyncDestinationDirectory) {
      Err("Injected registered account destination-directory sync failure".into())
    } else {
      sync_parent_directory(&self.base_dir, "registered account directory")
    };
    #[cfg(not(test))]
    let sync_result = sync_parent_directory(&self.base_dir, "registered account directory");
    if let Err(sync_error) = sync_result {
      #[cfg(test)]
      let rollback_result = if self.take_failure(AtomicWriteFailureStage::RollbackDestination) {
        Err("Injected registered account destination rollback failure".into())
      } else {
        restore_account_file(&self.base_dir, file_path, previous_bytes)
      };
      #[cfg(not(test))]
      let rollback_result = restore_account_file(&self.base_dir, file_path, previous_bytes);
      return match rollback_result {
        Ok(()) => Err(sync_error),
        Err(rollback_error) => Err(format!(
          "{sync_error}; failed to roll back registered account destination: {rollback_error}"
        )),
      };
    }
    Ok(())
  }

  #[cfg(test)]
  fn fail_next_account_write(&mut self, stage: AtomicWriteFailureStage) {
    self.fail_account_writes.push(stage);
  }

  #[cfg(test)]
  fn fail_account_writes(&mut self, stages: &[AtomicWriteFailureStage]) {
    self
      .fail_account_writes
      .extend(stages.iter().rev().copied());
  }

  #[cfg(test)]
  fn take_failure(&mut self, stage: AtomicWriteFailureStage) -> bool {
    if self.fail_account_writes.last() == Some(&stage) {
      self.fail_account_writes.pop();
      true
    } else {
      false
    }
  }

  fn list_all(&self) -> Vec<RegistrationResult> {
    let mut results: Vec<_> = self.accounts.values().cloned().collect();
    results.sort_by_key(|b| std::cmp::Reverse(b.created_at));
    results
  }

  fn delete(&mut self, account_id: &str) -> Result<bool, String> {
    let Some(key) = self.lookup_account_key(account_id) else {
      return Ok(false);
    };
    let account = self
      .accounts
      .get(&key)
      .expect("lookup_account_key returned a present account")
      .clone();
    let mut files = Vec::new();
    for path in self.tracked_account_paths(&key) {
      let Some(bytes) = read_optional_account_file(&path)? else {
        continue;
      };
      let stored = serde_json::from_slice::<RegistrationResult>(&bytes).map_err(|error| {
        format!(
          "Registered account migration collision at {}; source is not a valid record: {error}",
          path.display()
        )
      })?;
      if !same_canonical_stored_record(&account, &stored) {
        return Err(format!(
          "Registered account migration collision at {}; source belongs to a different record",
          path.display()
        ));
      }
      files.push((path, bytes));
    }

    let mut removed: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for (path, bytes) in &files {
      if let Err(error) = remove_account_file(path) {
        for (removed_path, removed_bytes) in removed {
          let _ = restore_account_file(&self.base_dir, &removed_path, Some(removed_bytes));
        }
        return Err(error);
      }
      removed.push((path.clone(), bytes.clone()));
    }
    if let Err(error) = sync_parent_directory(&self.base_dir, "registered account deletion") {
      for (removed_path, removed_bytes) in removed {
        let _ = restore_account_file(&self.base_dir, &removed_path, Some(removed_bytes));
      }
      return Err(error);
    }
    self.accounts.remove(&key);
    self.account_paths.remove(&key);
    self.stale_account_paths.remove(&key);
    Ok(true)
  }

  fn get(&self, account_id: &str) -> Option<RegistrationResult> {
    self
      .lookup_account_key(account_id)
      .and_then(|key| self.accounts.get(&key).cloned())
  }

  fn lookup_account_key(&self, account_id: &str) -> Option<String> {
    let requested_key = canonical_account_key(account_id, "")?;
    if self.accounts.contains_key(&requested_key) {
      return Some(requested_key);
    }
    let mut matches = self
      .accounts
      .iter()
      .filter(|(_, account)| account.email.trim() == requested_key)
      .map(|(key, _)| key.clone());
    let key = matches.next()?;
    matches.next().is_none().then_some(key)
  }

  fn update_status(
    &mut self,
    account_ids: &[String],
    status: AccountInventoryStatus,
    note: Option<String>,
  ) -> Result<usize, String> {
    let mut updated = 0usize;
    let now = Utc::now();
    for id in account_ids {
      let Some(mut account) = self.get(id) else {
        continue;
      };
      if status == AccountInventoryStatus::Available
        && account.status != AccountInventoryStatus::Available
        && !is_available_inventory_ready(&account)
      {
        return Err(format!(
          "Registered account {id} cannot be marked available until registration and 2FA are complete"
        ));
      }
      account.status = status.clone();
      if let Some(ref note) = note {
        account.note.clone_from(note);
      }
      match status {
        AccountInventoryStatus::Exported => account.exported_at = Some(now),
        AccountInventoryStatus::Sold => account.sold_at = Some(now),
        AccountInventoryStatus::Available
        | AccountInventoryStatus::Invalid
        | AccountInventoryStatus::Reserved => {}
      }
      account.record_revision = next_revision(account.record_revision)?;
      self.save(&account)?;
      updated += 1;
    }
    Ok(updated)
  }

  fn update_note(&mut self, account_id: &str, note: String) -> Result<bool, String> {
    let Some(mut account) = self.get(account_id) else {
      return Ok(false);
    };
    account.note = note;
    account.record_revision = next_revision(account.record_revision)?;
    self.save(&account)?;
    Ok(true)
  }

  fn compare_and_update(
    &mut self,
    account_key: &str,
    expected_revision: u64,
    precondition: BackfillPatchPrecondition,
    patch: TwoFactorBackfillPatch,
  ) -> Result<RegistrationResult, String> {
    let current = self
      .get(account_key)
      .ok_or_else(|| format!("Registered account {account_key} not found"))?;
    if current.record_revision != expected_revision {
      return Err(format!(
        "Registered account revision conflict for {account_key}: expected {expected_revision}, current {}",
        current.record_revision
      ));
    }
    validate_backfill_precondition(&current, &precondition, &patch)?;

    let mut updated = current;
    apply_backfill_patch(&mut updated, &precondition, patch)?;
    updated.record_revision = next_revision(updated.record_revision)?;
    self.save(&updated)?;
    Ok(updated)
  }

  fn persist_inferred_email_provider(
    &mut self,
    account_key: &str,
    expected_revision: u64,
    provider: EmailProvider,
  ) -> Result<RegistrationResult, String> {
    let current = self
      .get(account_key)
      .ok_or_else(|| format!("Registered account {account_key} not found"))?;
    if current.record_revision != expected_revision {
      return Err(format!(
        "Registered account revision conflict for {account_key}: expected {expected_revision}, current {}",
        current.record_revision
      ));
    }
    match (current.email_provider, current.email_provider_provenance) {
      (None, None) => {
        let mut updated = current;
        updated.email_provider = Some(provider);
        updated.email_provider_provenance = Some(EmailProviderProvenance::InferredFromCdk);
        updated.record_revision = next_revision(updated.record_revision)?;
        self.save(&updated)?;
        Ok(updated)
      }
      (Some(existing_provider), Some(EmailProviderProvenance::InferredFromCdk))
        if existing_provider == provider =>
      {
        Ok(current)
      }
      _ => Err(format!(
        "Registered account provider conflict for {account_key}; refusing inferred provider overwrite"
      )),
    }
  }
}

fn same_stored_record(left: &RegistrationResult, right: &RegistrationResult) -> bool {
  match (serde_json::to_vec(left), serde_json::to_vec(right)) {
    (Ok(left), Ok(right)) => left == right,
    _ => false,
  }
}

fn records_match_for_key_migration(left: &RegistrationResult, right: &RegistrationResult) -> bool {
  same_stored_record(left, right) || same_canonical_stored_record(left, right)
}

fn same_canonical_stored_record(left: &RegistrationResult, right: &RegistrationResult) -> bool {
  let mut left = left.clone();
  let mut right = right.clone();
  left.account_id = left.account_id.trim().to_string();
  left.email = left.email.trim().to_string();
  right.account_id = right.account_id.trim().to_string();
  right.email = right.email.trim().to_string();
  same_stored_record(&left, &right)
}

fn insert_loaded_account(
  accounts: &mut HashMap<String, RegistrationResult>,
  account_paths: &mut HashMap<String, PathBuf>,
  stale_account_paths: &mut HashMap<String, Vec<PathBuf>>,
  incoming: RegistrationResult,
  path: PathBuf,
) {
  let incoming_key = account_key(&incoming);
  let incoming_id = incoming.account_id.trim();
  let incoming_email = incoming.email.trim();
  let mut related_keys = accounts
    .iter()
    .filter(|(key, existing)| {
      if key.as_str() == incoming_key {
        return true;
      }
      let existing_id = existing.account_id.trim();
      !incoming_email.is_empty()
        && existing.email.trim() == incoming_email
        && (incoming_id.is_empty() != existing_id.is_empty())
    })
    .map(|(key, _)| key.clone())
    .collect::<Vec<_>>();
  related_keys.sort();

  let mut winner = incoming;
  let mut winner_path = path.clone();
  let mut all_paths = vec![path];
  for related_key in &related_keys {
    let existing = accounts
      .get(related_key)
      .expect("related key was selected from the account map");
    let existing_path = account_paths
      .get(related_key)
      .expect("loaded account path must accompany its account");
    if is_preferred_loaded_account(existing, existing_path, &winner, &winner_path) {
      winner = existing.clone();
      winner_path = existing_path.clone();
    }
    all_paths.push(existing_path.clone());
    if let Some(paths) = stale_account_paths.get(related_key) {
      all_paths.extend(paths.iter().cloned());
    }
  }

  for related_key in related_keys {
    accounts.remove(&related_key);
    account_paths.remove(&related_key);
    stale_account_paths.remove(&related_key);
  }

  let winner_key = account_key(&winner);
  all_paths.sort();
  all_paths.dedup();
  all_paths.retain(|candidate| candidate != &winner_path);
  accounts.insert(winner_key.clone(), winner);
  account_paths.insert(winner_key.clone(), winner_path);
  if all_paths.is_empty() {
    stale_account_paths.remove(&winner_key);
  } else {
    stale_account_paths.insert(winner_key, all_paths);
  }
}

fn is_preferred_loaded_account(
  incoming: &RegistrationResult,
  incoming_path: &Path,
  existing: &RegistrationResult,
  existing_path: &Path,
) -> bool {
  let incoming_has_account_id = !incoming.account_id.trim().is_empty();
  let existing_has_account_id = !existing.account_id.trim().is_empty();
  if incoming_has_account_id != existing_has_account_id {
    return incoming_has_account_id;
  }
  match incoming.record_revision.cmp(&existing.record_revision) {
    std::cmp::Ordering::Greater => return true,
    std::cmp::Ordering::Less => return false,
    std::cmp::Ordering::Equal => {}
  }
  let incoming_canonical_path = canonical_account_filename_matches(incoming, incoming_path);
  let existing_canonical_path = canonical_account_filename_matches(existing, existing_path);
  if incoming_canonical_path != existing_canonical_path {
    return incoming_canonical_path;
  }
  incoming_path < existing_path
}

fn canonical_account_filename_matches(account: &RegistrationResult, path: &Path) -> bool {
  path
    .file_name()
    .and_then(|name| name.to_str())
    .is_some_and(|name| name == format!("{}.json", account_key(account)))
}

fn read_optional_account_file(path: &Path) -> Result<Option<Vec<u8>>, String> {
  match fs::read(path) {
    Ok(bytes) => Ok(Some(bytes)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(format!(
      "Failed to read registered account file {} before replacement: {error}",
      path.display()
    )),
  }
}

fn remove_account_file(path: &Path) -> Result<(), String> {
  match fs::remove_file(path) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(format!(
      "Failed to delete stale registered account file {}: {error}",
      path.display()
    )),
  }
}

fn restore_account_file(
  base_dir: &Path,
  file_path: &Path,
  previous_bytes: Option<Vec<u8>>,
) -> Result<(), String> {
  if let Some(previous_bytes) = previous_bytes {
    let mut temp_file = tempfile::NamedTempFile::new_in(base_dir).map_err(|error| {
      format!(
        "Failed to create registered account rollback temp file in {}: {error}",
        base_dir.display()
      )
    })?;
    temp_file
      .write_all(&previous_bytes)
      .map_err(|error| format!("Failed to write registered account rollback file: {error}"))?;
    temp_file
      .as_file()
      .sync_all()
      .map_err(|error| format!("Failed to sync registered account rollback file: {error}"))?;
    persist_temp_file(temp_file, file_path, "registered account rollback")?;
  } else {
    remove_account_file(file_path)?;
  }
  sync_parent_directory(base_dir, "registered account rollback directory")
}

fn validate_owned_backfill_operation(
  account: &RegistrationResult,
  precondition: &BackfillPatchPrecondition,
) -> Result<(), String> {
  if account.two_factor_backfill_state != Some(TwoFactorBackfillState::InProgress)
    || account.two_factor_backfill_outcome.is_some()
  {
    return Err("2FA backfill precondition failed: lifecycle is not in progress".into());
  }
  if account.two_factor_backfill_operation_id.as_deref() != Some(precondition.operation_id.as_str())
  {
    return Err("2FA backfill operation ownership conflict".into());
  }
  Ok(())
}

fn has_complete_backfill_credentials(account: &RegistrationResult) -> bool {
  !account.email.trim().is_empty()
    && !account.password.trim().is_empty()
    && !account.cdk.trim().is_empty()
}

fn has_complete_email_provider(account: &RegistrationResult) -> bool {
  matches!(
    (account.email_provider, account.email_provider_provenance),
    (Some(_), Some(_))
  )
}

fn is_new_registration_two_factor_base(account: &RegistrationResult) -> bool {
  account.record_revision >= 1
    && !account.success
    && account.free_trial_eligible
    && account.status == AccountInventoryStatus::Reserved
    && account.registration_outcome_reason == Some(RegistrationOutcomeReason::Registered)
    && has_complete_backfill_credentials(account)
    && has_complete_email_provider(account)
    && account.two_factor_backfill_access_state == Some(TwoFactorBackfillAccessState::Accessible)
    && account.two_factor_backfill_exclusion.is_none()
    && !account.two_fa_enabled
    && account.totp_secret.trim().is_empty()
}

pub(crate) fn is_retryable_new_registration_two_factor_failure(
  account: &RegistrationResult,
) -> bool {
  is_new_registration_two_factor_base(account)
    && account.two_factor_backfill_state == Some(TwoFactorBackfillState::Completed)
    && account.two_factor_backfill_outcome == Some(TwoFactorBackfillOutcome::Failed)
}

fn is_in_progress_new_registration_two_factor(account: &RegistrationResult) -> bool {
  is_new_registration_two_factor_base(account)
    && account.two_factor_backfill_state == Some(TwoFactorBackfillState::InProgress)
    && account.two_factor_backfill_outcome.is_none()
}

fn validate_nonempty_totp_secret(patch: &TwoFactorBackfillPatch) -> Result<(), String> {
  if patch
    .totp_secret
    .as_deref()
    .is_none_or(|secret| secret.trim().is_empty())
  {
    return Err("Enabling 2FA requires a non-empty TOTP secret".into());
  }
  Ok(())
}

fn is_available_inventory_ready(account: &RegistrationResult) -> bool {
  account.success
    && account.free_trial_eligible
    && account.two_fa_enabled
    && !account.totp_secret.trim().is_empty()
}

fn validate_backfill_precondition(
  account: &RegistrationResult,
  precondition: &BackfillPatchPrecondition,
  patch: &TwoFactorBackfillPatch,
) -> Result<(), String> {
  if precondition.operation_id.trim().is_empty() {
    return Err("2FA backfill precondition requires an operation ID".into());
  }
  if patch.operation != precondition.operation {
    return Err("2FA backfill patch operation does not match its precondition".into());
  }
  if patch.finalize_new_registration != precondition.finalize_new_registration {
    return Err("2FA backfill registration-finalize policy does not match its precondition".into());
  }

  if matches!(
    precondition.operation,
    BackfillPatchOperation::FinalizeFailed
      | BackfillPatchOperation::FinalizeCancelled
      | BackfillPatchOperation::FinalizeReconciliationRequired
  ) {
    validate_owned_backfill_operation(account, precondition)?;
    if patch.totp_secret.is_some() {
      return Err("Terminal 2FA backfill outcome cannot carry a TOTP secret".into());
    }
    if patch.access_state.is_some_and(|state| {
      state != TwoFactorBackfillAccessState::Locked
        || precondition.operation != BackfillPatchOperation::FinalizeFailed
    }) {
      return Err("Only failed 2FA backfill outcomes may mark an account locked".into());
    }
    return Ok(());
  }

  if precondition.finalize_new_registration {
    validate_owned_backfill_operation(account, precondition)?;
    if !is_in_progress_new_registration_two_factor(account) {
      return Err(
        "2FA registration-finalize precondition failed: provisional record is not eligible".into(),
      );
    }
    validate_nonempty_totp_secret(patch)?;
    return Ok(());
  }

  let invalid_free_trial_no = account.status == AccountInventoryStatus::Invalid
    && account.registration_outcome_reason == Some(RegistrationOutcomeReason::FreeTrialNo);
  let allowed_free_trial_no = invalid_free_trial_no && precondition.allow_free_trial_no;
  let retryable_registration_failure = precondition.operation == BackfillPatchOperation::Start
    && is_retryable_new_registration_two_factor_failure(account);
  let in_progress_registration_retry = precondition.operation
    == BackfillPatchOperation::FinalizeEnabled
    && is_in_progress_new_registration_two_factor(account);
  if !account.success
    && !allowed_free_trial_no
    && !retryable_registration_failure
    && !in_progress_registration_retry
  {
    return Err("2FA backfill precondition failed: registration was unsuccessful".into());
  }
  if !has_complete_backfill_credentials(account) {
    return Err("2FA backfill precondition failed: account credentials are incomplete".into());
  }
  match account.status {
    AccountInventoryStatus::Available => {}
    AccountInventoryStatus::Invalid if allowed_free_trial_no => {}
    AccountInventoryStatus::Reserved
      if retryable_registration_failure || in_progress_registration_retry => {}
    _ => {
      return Err("2FA backfill precondition failed: inventory status is not eligible".into());
    }
  }
  if !has_complete_email_provider(account) {
    return Err("2FA backfill precondition failed: email provider provenance is missing".into());
  }
  if account.two_fa_enabled || !account.totp_secret.trim().is_empty() {
    return Err("2FA backfill precondition failed: local 2FA state is inconsistent".into());
  }
  match account.two_factor_backfill_access_state {
    Some(TwoFactorBackfillAccessState::Accessible) => {}
    None if precondition.acknowledge_legacy_access => {}
    Some(TwoFactorBackfillAccessState::Locked) => {
      return Err("2FA backfill precondition failed: account access is locked".into());
    }
    None => {
      return Err("2FA backfill precondition failed: legacy access was not acknowledged".into());
    }
  }
  if account.two_factor_backfill_exclusion.is_some() {
    return Err("2FA backfill precondition failed: account is explicitly excluded".into());
  }

  match precondition.operation {
    BackfillPatchOperation::Start => match (
      account.two_factor_backfill_state,
      account.two_factor_backfill_outcome,
      account.two_factor_backfill_operation_id.as_deref(),
    ) {
      (None, None, None) => {}
      (
        Some(TwoFactorBackfillState::Completed),
        Some(TwoFactorBackfillOutcome::Failed | TwoFactorBackfillOutcome::Cancelled),
        _,
      ) => {}
      _ => return Err("2FA backfill precondition failed: lifecycle is not startable".into()),
    },
    BackfillPatchOperation::FinalizeEnabled => {
      validate_owned_backfill_operation(account, precondition)?;
      validate_nonempty_totp_secret(patch)?;
    }
    BackfillPatchOperation::FinalizeFailed
    | BackfillPatchOperation::FinalizeCancelled
    | BackfillPatchOperation::FinalizeReconciliationRequired => {
      unreachable!("non-enabled terminal operations are validated before eligibility policy")
    }
  }

  Ok(())
}

fn next_revision(current: u64) -> Result<u64, String> {
  current
    .checked_add(1)
    .ok_or_else(|| "Registered account record revision overflow".to_string())
}

fn apply_backfill_patch(
  account: &mut RegistrationResult,
  precondition: &BackfillPatchPrecondition,
  patch: TwoFactorBackfillPatch,
) -> Result<(), String> {
  match patch.operation {
    BackfillPatchOperation::Start => {
      account.two_factor_backfill_state = Some(TwoFactorBackfillState::InProgress);
      account.two_factor_backfill_operation_id = Some(precondition.operation_id.clone());
      account.two_factor_backfill_outcome = None;
    }
    BackfillPatchOperation::FinalizeEnabled => {
      let promote_new_registration = precondition.finalize_new_registration
        || is_in_progress_new_registration_two_factor(account);
      let secret = patch
        .totp_secret
        .filter(|secret| !secret.trim().is_empty())
        .ok_or_else(|| "Enabling 2FA requires a non-empty TOTP secret".to_string())?;
      if promote_new_registration {
        account.success = true;
        account.status = AccountInventoryStatus::Available;
        account.error_message.clear();
        account.note.clear();
      }
      account.two_fa_enabled = true;
      account.totp_secret = secret;
      apply_terminal_backfill_outcome(
        account,
        &precondition.operation_id,
        TwoFactorBackfillOutcome::Enabled,
      );
    }
    BackfillPatchOperation::FinalizeFailed => {
      if let Some(access_state) = patch.access_state {
        account.two_factor_backfill_access_state = Some(access_state);
      }
      apply_terminal_backfill_outcome(
        account,
        &precondition.operation_id,
        TwoFactorBackfillOutcome::Failed,
      )
    }
    BackfillPatchOperation::FinalizeCancelled => apply_terminal_backfill_outcome(
      account,
      &precondition.operation_id,
      TwoFactorBackfillOutcome::Cancelled,
    ),
    BackfillPatchOperation::FinalizeReconciliationRequired => apply_terminal_backfill_outcome(
      account,
      &precondition.operation_id,
      TwoFactorBackfillOutcome::ReconciliationRequired,
    ),
  }
  Ok(())
}

fn apply_terminal_backfill_outcome(
  account: &mut RegistrationResult,
  operation_id: &str,
  outcome: TwoFactorBackfillOutcome,
) {
  account.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
  account.two_factor_backfill_operation_id = Some(operation_id.to_string());
  account.two_factor_backfill_outcome = Some(outcome);
}

#[cfg(not(windows))]
pub(crate) fn persist_temp_file(
  temp_file: tempfile::NamedTempFile,
  destination: &Path,
  description: &str,
) -> Result<(), String> {
  temp_file
    .persist(destination)
    .map(|_| ())
    .map_err(|error| format!("Failed to replace {description}: {}", error.error))
}

#[cfg(windows)]
pub(crate) fn persist_temp_file(
  temp_file: tempfile::NamedTempFile,
  destination: &Path,
  description: &str,
) -> Result<(), String> {
  use std::os::windows::ffi::OsStrExt;
  use windows::core::PCWSTR;
  use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
  };

  let (file, temp_path) = temp_file
    .keep()
    .map_err(|error| format!("Failed to prepare {description} temp file: {}", error.error))?;
  drop(file);
  let source: Vec<u16> = temp_path
    .as_os_str()
    .encode_wide()
    .chain(std::iter::once(0))
    .collect();
  let target: Vec<u16> = destination
    .as_os_str()
    .encode_wide()
    .chain(std::iter::once(0))
    .collect();
  let result = unsafe {
    MoveFileExW(
      PCWSTR(source.as_ptr()),
      PCWSTR(target.as_ptr()),
      MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
    )
  };
  if let Err(error) = result {
    let _ = fs::remove_file(&temp_path);
    return Err(format!("Failed to replace {description}: {error}"));
  }
  Ok(())
}

#[cfg(not(windows))]
pub(crate) fn sync_parent_directory(directory: &Path, description: &str) -> Result<(), String> {
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
pub(crate) fn sync_parent_directory(_directory: &Path, _description: &str) -> Result<(), String> {
  Ok(())
}

fn canonical_account_key(account_id: &str, email: &str) -> Option<String> {
  let account_id = account_id.trim();
  if !account_id.is_empty() {
    return Some(account_id.to_string());
  }
  let email = email.trim();
  if !email.is_empty() {
    return Some(email.to_string());
  }
  None
}

fn account_key(result: &RegistrationResult) -> String {
  canonical_account_key(&result.account_id, &result.email)
    .unwrap_or_else(|| format!("unknown-{}", Utc::now().timestamp_millis()))
}

// --- Public API ---

fn credential_store() -> Result<&'static Mutex<CredentialStore>, String> {
  STORE
    .as_ref()
    .map_err(|error| format!("Registered account store unavailable: {error}"))
}

fn lock_credential_store() -> Result<std::sync::MutexGuard<'static, CredentialStore>, String> {
  credential_store()?
    .lock()
    .map_err(|error| format!("Failed to lock registered account store: {error}"))
}

pub fn save_registration_result(result: &RegistrationResult) -> Result<(), String> {
  lock_credential_store()?.save(result)
}

pub fn list_registered_accounts() -> Result<Vec<RegistrationResult>, String> {
  Ok(lock_credential_store()?.list_all())
}

pub fn delete_registered_account(account_id: &str) -> Result<bool, String> {
  lock_credential_store()?.delete(account_id)
}

#[allow(dead_code)]
pub fn get_registered_account(account_id: &str) -> Result<Option<RegistrationResult>, String> {
  Ok(lock_credential_store()?.get(account_id))
}

pub fn update_registered_account_status(
  account_ids: &[String],
  status: AccountInventoryStatus,
  note: Option<String>,
) -> Result<usize, String> {
  lock_credential_store()?.update_status(account_ids, status, note)
}

pub fn update_registered_account_note(account_id: &str, note: String) -> Result<bool, String> {
  lock_credential_store()?.update_note(account_id, note)
}

#[allow(dead_code)]
pub(crate) fn compare_and_update_registered_account(
  account_key: &str,
  expected_revision: u64,
  precondition: BackfillPatchPrecondition,
  patch: TwoFactorBackfillPatch,
) -> Result<RegistrationResult, String> {
  lock_credential_store()?.compare_and_update(account_key, expected_revision, precondition, patch)
}

#[allow(dead_code)]
pub(crate) fn persist_inferred_email_provider(
  account_key: &str,
  expected_revision: u64,
  provider: EmailProvider,
) -> Result<RegistrationResult, String> {
  lock_credential_store()?.persist_inferred_email_provider(account_key, expected_revision, provider)
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
  use crate::auto_service::openai::register::types::{
    EmailProviderProvenance, RegistrationOutcomeReason, TwoFactorBackfillAccessState,
    TwoFactorBackfillExclusion, TwoFactorBackfillOutcome, TwoFactorBackfillState,
  };
  use crate::auto_service::openai::two_factor_backfill::{
    evaluate_eligibility, TwoFactorBackfillPreviewRequest,
  };
  use crate::email::EmailProvider;
  use std::sync::Barrier;
  use tempfile::TempDir;

  const ORIGINAL_PASSWORD: &str = "known-password";
  const ORIGINAL_ACCESS_TOKEN: &str = "known-access-token";
  const ORIGINAL_CDK: &str = "GMAIL-known-cdk";

  fn registration_result(account_id: &str) -> RegistrationResult {
    RegistrationResult {
      success: true,
      email: format!("{account_id}@example.com"),
      password: ORIGINAL_PASSWORD.into(),
      account_id: account_id.into(),
      access_token: ORIGINAL_ACCESS_TOKEN.into(),
      device_id: "device-1".into(),
      error_message: "original-error".into(),
      step_logs: vec!["original-log".into()],
      created_at: Utc::now(),
      two_fa_enabled: false,
      totp_secret: String::new(),
      free_trial_eligible: true,
      plan_type: "trial".into(),
      cdk: ORIGINAL_CDK.into(),
      base_email: "base@example.com".into(),
      phone_number: "+10000000000".into(),
      status: AccountInventoryStatus::Available,
      note: "original-note".into(),
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

  fn account_file(temp: &TempDir, account_id: &str) -> PathBuf {
    temp.path().join(format!("{account_id}.json"))
  }

  fn backfill_ready_result(account_id: &str) -> RegistrationResult {
    let mut account = registration_result(account_id);
    account.email_provider = Some(EmailProvider::Gmail123452026);
    account.email_provider_provenance = Some(EmailProviderProvenance::InferredFromCdk);
    account
  }

  fn new_registration_provisional(account_id: &str, operation_id: &str) -> RegistrationResult {
    let mut account = backfill_ready_result(account_id);
    account.success = false;
    account.status = AccountInventoryStatus::Reserved;
    account.error_message = "2FA setup pending".into();
    account.note = "two_factor_pending".into();
    account.two_factor_backfill_state = Some(TwoFactorBackfillState::InProgress);
    account.two_factor_backfill_operation_id = Some(operation_id.into());
    account.two_factor_backfill_outcome = None;
    account.record_revision = 1;
    account
  }

  fn retryable_new_registration_failure(account_id: &str) -> RegistrationResult {
    let mut account = new_registration_provisional(account_id, "registration-operation");
    account.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
    account.two_factor_backfill_outcome = Some(TwoFactorBackfillOutcome::Failed);
    account
  }

  fn precondition_for(
    operation: BackfillPatchOperation,
    operation_id: &str,
    allow_free_trial_no: bool,
  ) -> BackfillPatchPrecondition {
    match operation {
      BackfillPatchOperation::Start => {
        BackfillPatchPrecondition::start(operation_id, allow_free_trial_no, false)
      }
      BackfillPatchOperation::FinalizeEnabled => {
        BackfillPatchPrecondition::finalize_enabled(operation_id, allow_free_trial_no, false)
      }
      BackfillPatchOperation::FinalizeFailed => {
        BackfillPatchPrecondition::finalize_failed(operation_id, allow_free_trial_no, false)
      }
      BackfillPatchOperation::FinalizeCancelled => {
        BackfillPatchPrecondition::finalize_cancelled(operation_id, allow_free_trial_no, false)
      }
      BackfillPatchOperation::FinalizeReconciliationRequired => {
        BackfillPatchPrecondition::finalize_reconciliation_required(
          operation_id,
          allow_free_trial_no,
          false,
        )
      }
    }
  }

  fn patch_for(operation: BackfillPatchOperation) -> TwoFactorBackfillPatch {
    match operation {
      BackfillPatchOperation::Start => TwoFactorBackfillPatch::start(),
      BackfillPatchOperation::FinalizeEnabled => {
        TwoFactorBackfillPatch::finalize_enabled("JBSWY3DPEHPK3PXP")
      }
      BackfillPatchOperation::FinalizeFailed => TwoFactorBackfillPatch::finalize_failed(),
      BackfillPatchOperation::FinalizeCancelled => TwoFactorBackfillPatch::finalize_cancelled(),
      BackfillPatchOperation::FinalizeReconciliationRequired => {
        TwoFactorBackfillPatch::finalize_reconciliation_required()
      }
    }
  }

  fn account_for_operation(
    operation: BackfillPatchOperation,
    operation_id: &str,
  ) -> RegistrationResult {
    let mut account = backfill_ready_result("account-1");
    if operation != BackfillPatchOperation::Start {
      account.two_factor_backfill_state = Some(TwoFactorBackfillState::InProgress);
      account.two_factor_backfill_operation_id = Some(operation_id.into());
    }
    account
  }

  fn assert_cas_rejection_preserves(
    account: RegistrationResult,
    precondition: BackfillPatchPrecondition,
    patch: TwoFactorBackfillPatch,
  ) -> String {
    let temp = TempDir::new().unwrap();
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&account).unwrap();
    let disk_before = fs::read(account_file(&temp, "account-1")).unwrap();
    let memory_before = serde_json::to_value(store.get("account-1").unwrap()).unwrap();

    let error = store
      .compare_and_update("account-1", account.record_revision, precondition, patch)
      .unwrap_err();

    assert_eq!(
      fs::read(account_file(&temp, "account-1")).unwrap(),
      disk_before
    );
    assert_eq!(
      serde_json::to_value(store.get("account-1").unwrap()).unwrap(),
      memory_before
    );
    error
  }

  #[test]
  fn atomic_account_save_survives_store_reload() {
    let temp = TempDir::new().unwrap();
    let account = registration_result("account-1");
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();

    store.save(&account).unwrap();

    let reloaded = CredentialStore::with_base_dir(temp.path()).unwrap();
    let persisted = reloaded.get("account-1").unwrap();
    assert_eq!(
      serde_json::to_value(persisted).unwrap(),
      serde_json::to_value(account).unwrap()
    );
  }

  #[test]
  fn trimmed_account_id_key_supports_preview_cas_migration_and_all_mutations() {
    let temp = TempDir::new().unwrap();
    let mut account = registration_result("\u{00a0} account-1 \u{2003}");
    account.email = "\u{2009} account-1@example.com \u{00a0}".into();
    let legacy_file = account_file(&temp, "\u{00a0} account-1 \u{2003}");
    fs::write(&legacy_file, serde_json::to_vec_pretty(&account).unwrap()).unwrap();

    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();

    assert_eq!(store.list_all().len(), 1);
    assert_eq!(
      store.get("account-1").unwrap().account_id,
      "\u{00a0} account-1 \u{2003}"
    );
    assert!(store.get(" id ").is_none());
    assert!(store.get("\taccount-1\n").is_some());
    assert!(store.get("\u{00a0}account-1\u{2003}").is_some());
    assert!(legacy_file.exists());
    assert!(!account_file(&temp, "account-1").exists());

    let preview = evaluate_eligibility(
      &TwoFactorBackfillPreviewRequest {
        selected_account_keys: vec![" account-1 ".into()],
        allow_free_trial_no: false,
        acknowledge_legacy_access: false,
      },
      &store.list_all(),
    );
    assert_eq!(preview.accounts.len(), 1);
    assert_eq!(preview.accounts[0].account_key, "account-1");
    assert_eq!(
      preview.accounts[0].account_id,
      "\u{00a0} account-1 \u{2003}"
    );
    assert_eq!(
      preview.accounts[0].email,
      "\u{2009} account-1@example.com \u{00a0}"
    );
    assert!(preview.accounts[0].requires_provider_persistence);

    let inferred = store
      .persist_inferred_email_provider(
        &preview.accounts[0].account_key,
        preview.accounts[0].record_revision,
        EmailProvider::Gmail123452026,
      )
      .unwrap();
    assert!(!legacy_file.exists());
    assert!(account_file(&temp, "account-1").exists());

    let started = store
      .compare_and_update(
        &preview.accounts[0].account_key,
        inferred.record_revision,
        BackfillPatchPrecondition::start("operation-1", false, false),
        TwoFactorBackfillPatch::start(),
      )
      .unwrap();
    assert_eq!(
      store
        .update_status(
          &[" account-1 ".into()],
          AccountInventoryStatus::Available,
          Some("status-updated".into()),
        )
        .unwrap(),
      1
    );
    assert!(store
      .update_note("\u{00a0}account-1\u{2003}", "note-updated".into())
      .unwrap());

    let reloaded = CredentialStore::with_base_dir(temp.path()).unwrap();
    let persisted = reloaded.get("account-1").unwrap();
    assert_eq!(persisted.record_revision, started.record_revision + 2);
    assert_eq!(persisted.note, "note-updated");
    assert_eq!(persisted.account_id, "\u{00a0} account-1 \u{2003}");
    assert_eq!(
      persisted.email_provider,
      Some(EmailProvider::Gmail123452026)
    );

    let mut reloaded = reloaded;
    assert!(reloaded.delete(" account-1 ").unwrap());
    assert!(!account_file(&temp, "account-1").exists());
    assert!(CredentialStore::with_base_dir(temp.path())
      .unwrap()
      .get("account-1")
      .is_none());
  }

  #[test]
  fn trimmed_email_fallback_key_supports_every_store_mutation_and_reload() {
    let temp = TempDir::new().unwrap();
    let mut account = registration_result("   ");
    account.email = "  fallback@example.com  ".into();
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();

    store.save(&account).unwrap();

    assert_eq!(store.list_all().len(), 1);
    assert_eq!(store.get("fallback@example.com").unwrap().account_id, "   ");
    assert!(store.get("  fallback@example.com  ").is_some());
    assert!(account_file(&temp, "fallback@example.com").exists());

    let inferred = store
      .persist_inferred_email_provider("fallback@example.com", 7, EmailProvider::Gmail123452026)
      .unwrap();
    let started = store
      .compare_and_update(
        "fallback@example.com",
        inferred.record_revision,
        BackfillPatchPrecondition::start("operation-1", false, false),
        TwoFactorBackfillPatch::start(),
      )
      .unwrap();
    assert_eq!(
      store
        .update_status(
          &["fallback@example.com".into()],
          AccountInventoryStatus::Available,
          Some("status-updated".into()),
        )
        .unwrap(),
      1
    );

    let reloaded = CredentialStore::with_base_dir(temp.path()).unwrap();
    let persisted = reloaded.get("fallback@example.com").unwrap();
    assert_eq!(persisted.record_revision, started.record_revision + 1);
    assert_eq!(persisted.note, "status-updated");
    assert_eq!(persisted.email, "  fallback@example.com  ");

    let mut reloaded = reloaded;
    assert!(reloaded.delete("fallback@example.com").unwrap());
    assert!(!account_file(&temp, "fallback@example.com").exists());
    assert!(CredentialStore::with_base_dir(temp.path())
      .unwrap()
      .get("fallback@example.com")
      .is_none());
  }

  #[test]
  fn first_mutation_migrates_legacy_untrimmed_filename_without_orphan() {
    let temp = TempDir::new().unwrap();
    let account = registration_result("  account-1  ");
    let legacy_file = account_file(&temp, "  account-1  ");
    fs::write(&legacy_file, serde_json::to_vec_pretty(&account).unwrap()).unwrap();

    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    assert!(store.get("account-1").is_some());
    assert!(legacy_file.exists());
    assert!(!account_file(&temp, "account-1").exists());

    store.update_note("account-1", "migrated".into()).unwrap();

    assert!(!legacy_file.exists());
    assert!(account_file(&temp, "account-1").exists());
    let reloaded = CredentialStore::with_base_dir(temp.path()).unwrap();
    assert_eq!(reloaded.accounts.len(), 1);
    assert_eq!(reloaded.get("account-1").unwrap().note, "migrated");
  }

  #[test]
  fn colliding_legacy_whitespace_records_load_deterministically_and_fail_closed_on_mutation() {
    let temp = TempDir::new().unwrap();
    let mut older = registration_result("  account-1  ");
    older.record_revision = 7;
    older.password = "older-password".into();
    let mut newer = registration_result("\u{00a0}account-1\u{2003}");
    newer.record_revision = 8;
    newer.password = "newer-password".into();
    let older_file = account_file(&temp, "  account-1  ");
    let newer_file = account_file(&temp, "\u{00a0}account-1\u{2003}");
    fs::write(&older_file, serde_json::to_vec_pretty(&older).unwrap()).unwrap();
    fs::write(&newer_file, serde_json::to_vec_pretty(&newer).unwrap()).unwrap();
    let older_before = fs::read(&older_file).unwrap();
    let newer_before = fs::read(&newer_file).unwrap();

    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();

    assert_eq!(store.accounts.len(), 1);
    assert_eq!(store.get("account-1").unwrap().password, "newer-password");
    let error = store
      .update_note(" account-1 ", "must-not-persist".into())
      .unwrap_err();
    assert!(error.contains("migration collision"));
    assert_eq!(fs::read(&older_file).unwrap(), older_before);
    assert_eq!(fs::read(&newer_file).unwrap(), newer_before);
    assert!(!account_file(&temp, "account-1").exists());
    assert_eq!(store.get("account-1").unwrap().password, "newer-password");
    assert_eq!(store.get("account-1").unwrap().note, "original-note");
  }

  #[test]
  fn legacy_untrimmed_filename_migration_refuses_destination_collision() {
    let temp = TempDir::new().unwrap();
    let account = registration_result("  account-1  ");
    let legacy_file = account_file(&temp, "  account-1  ");
    fs::write(&legacy_file, serde_json::to_vec_pretty(&account).unwrap()).unwrap();
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();

    let mut collision = registration_result("account-1");
    collision.email = "different@example.com".into();
    let canonical_file = account_file(&temp, "account-1");
    fs::write(
      &canonical_file,
      serde_json::to_vec_pretty(&collision).unwrap(),
    )
    .unwrap();
    let legacy_before = fs::read(&legacy_file).unwrap();
    let collision_before = fs::read(&canonical_file).unwrap();

    let error = store
      .update_note("account-1", "must-not-persist".into())
      .unwrap_err();

    assert!(error.contains("migration collision"));
    assert_eq!(fs::read(&legacy_file).unwrap(), legacy_before);
    assert_eq!(fs::read(&canonical_file).unwrap(), collision_before);
    assert_eq!(store.get("account-1").unwrap().note, "original-note");
  }

  #[test]
  fn account_save_failure_leaves_memory_and_disk_unchanged() {
    for stage in [
      AtomicWriteFailureStage::Serialize,
      AtomicWriteFailureStage::Write,
      AtomicWriteFailureStage::Replace,
    ] {
      let temp = TempDir::new().unwrap();
      let original = registration_result("account-1");
      let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
      store.save(&original).unwrap();
      let disk_before = fs::read(account_file(&temp, "account-1")).unwrap();
      let memory_before = serde_json::to_value(store.get("account-1").unwrap()).unwrap();
      let mut changed = original.clone();
      changed.note = format!("changed-at-{stage:?}");

      store.fail_next_account_write(stage);
      assert!(store.save(&changed).is_err(), "stage {stage:?}");

      assert_eq!(
        serde_json::to_value(store.get("account-1").unwrap()).unwrap(),
        memory_before,
        "memory changed at {stage:?}"
      );
      assert_eq!(
        fs::read(account_file(&temp, "account-1")).unwrap(),
        disk_before,
        "disk changed at {stage:?}"
      );
    }
  }

  #[test]
  fn account_key_change_removes_only_the_stale_record_file() {
    let temp = TempDir::new().unwrap();
    let mut legacy = registration_result("");
    legacy.email = "legacy@example.com".into();
    let unrelated = registration_result("unrelated");
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&legacy).unwrap();
    store.save(&unrelated).unwrap();

    let mut migrated = legacy;
    migrated.account_id = "account-1".into();
    store.save(&migrated).unwrap();

    assert!(!temp.path().join("legacy@example.com.json").exists());
    assert!(account_file(&temp, "account-1").exists());
    assert!(account_file(&temp, "unrelated").exists());
    assert!(CredentialStore::with_base_dir(temp.path())
      .unwrap()
      .get("unrelated")
      .is_some());
  }

  #[test]
  fn legacy_key_migration_collision_preserves_both_records() {
    let temp = TempDir::new().unwrap();
    let mut legacy = registration_result("");
    legacy.email = "legacy@example.com".into();
    let mut destination = registration_result("account-1");
    destination.email = "destination@example.com".into();
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&legacy).unwrap();
    store.save(&destination).unwrap();
    let memory_before = serde_json::to_value(&store.accounts).unwrap();
    let legacy_disk_before = fs::read(temp.path().join("legacy@example.com.json")).unwrap();
    let destination_disk_before = fs::read(account_file(&temp, "account-1")).unwrap();

    let mut incoming = legacy.clone();
    incoming.account_id = "account-1".into();
    let error = store.save(&incoming).unwrap_err();

    assert!(error.contains("migration collision"));
    assert_eq!(
      fs::read(temp.path().join("legacy@example.com.json")).unwrap(),
      legacy_disk_before
    );
    assert_eq!(
      fs::read(account_file(&temp, "account-1")).unwrap(),
      destination_disk_before
    );
    assert_eq!(
      serde_json::to_value(&store.accounts).unwrap(),
      memory_before
    );
  }

  #[test]
  fn post_replace_sync_failure_rolls_back_canonical_and_migrating_saves() {
    let temp = TempDir::new().unwrap();
    let original = registration_result("account-1");
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&original).unwrap();
    let disk_before = fs::read(account_file(&temp, "account-1")).unwrap();
    let mut changed = original.clone();
    changed.note = "must-not-survive".into();

    store.fail_next_account_write(AtomicWriteFailureStage::SyncDestinationDirectory);
    assert!(store.save(&changed).is_err());
    assert_eq!(
      fs::read(account_file(&temp, "account-1")).unwrap(),
      disk_before
    );
    assert_eq!(store.get("account-1").unwrap().note, original.note);

    let mut legacy = registration_result("");
    legacy.email = "legacy@example.com".into();
    store.save(&legacy).unwrap();
    let legacy_disk_before = fs::read(temp.path().join("legacy@example.com.json")).unwrap();
    let mut migrated = legacy.clone();
    migrated.account_id = "account-2".into();

    store.fail_next_account_write(AtomicWriteFailureStage::SyncDestinationDirectory);
    assert!(store.save(&migrated).is_err());
    assert_eq!(
      fs::read(temp.path().join("legacy@example.com.json")).unwrap(),
      legacy_disk_before
    );
    assert!(!account_file(&temp, "account-2").exists());
    assert!(store.get("legacy@example.com").is_some());
    assert!(store.get("account-2").is_none());
  }

  #[test]
  fn post_replace_sync_and_rollback_failures_are_compounded() {
    let temp = TempDir::new().unwrap();
    let original = registration_result("account-1");
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&original).unwrap();
    let memory_before = serde_json::to_value(store.get("account-1").unwrap()).unwrap();
    let mut changed = original;
    changed.note = "must-not-reach-memory".into();

    store.fail_account_writes(&[
      AtomicWriteFailureStage::SyncDestinationDirectory,
      AtomicWriteFailureStage::RollbackDestination,
    ]);
    let error = store.save(&changed).unwrap_err();

    assert!(error.contains("destination-directory sync failure"));
    assert!(error.contains("destination rollback failure"));
    assert_eq!(
      serde_json::to_value(store.get("account-1").unwrap()).unwrap(),
      memory_before
    );

    let mut legacy = registration_result("");
    legacy.email = "legacy@example.com".into();
    store.save(&legacy).unwrap();
    let legacy_memory_before =
      serde_json::to_value(store.get("legacy@example.com").unwrap()).unwrap();
    let mut migrated = legacy;
    migrated.account_id = "account-2".into();
    store.fail_account_writes(&[
      AtomicWriteFailureStage::SyncDestinationDirectory,
      AtomicWriteFailureStage::RollbackDestination,
    ]);

    let migration_error = store.save(&migrated).unwrap_err();

    assert!(migration_error.contains("destination-directory sync failure"));
    assert!(migration_error.contains("destination rollback failure"));
    assert_eq!(
      serde_json::to_value(store.get("legacy@example.com").unwrap()).unwrap(),
      legacy_memory_before
    );
    assert!(store.get("account-2").is_none());
  }

  #[test]
  fn failed_stable_key_cleanup_rolls_back_new_file_and_memory() {
    for stage in [
      AtomicWriteFailureStage::DeleteStale,
      AtomicWriteFailureStage::SyncStaleDirectory,
    ] {
      let temp = TempDir::new().unwrap();
      let mut legacy = registration_result("");
      legacy.email = "legacy@example.com".into();
      let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
      store.save(&legacy).unwrap();

      let mut migrated = legacy.clone();
      migrated.account_id = "account-1".into();
      store.fail_next_account_write(stage);
      assert!(store.save(&migrated).is_err(), "stage {stage:?}");

      let legacy_file = temp.path().join("legacy@example.com.json");
      assert!(legacy_file.exists(), "stage {stage:?}");
      assert!(
        !account_file(&temp, "account-1").exists(),
        "stage {stage:?}"
      );
      assert_eq!(
        serde_json::from_slice::<RegistrationResult>(&fs::read(legacy_file).unwrap())
          .unwrap()
          .account_id,
        "",
        "stage {stage:?}"
      );
      assert_eq!(
        store.get("legacy@example.com").unwrap().account_id,
        "",
        "stage {stage:?}"
      );
      assert!(store.get("account-1").is_none(), "stage {stage:?}");
    }
  }

  #[test]
  fn reload_prefers_canonical_account_id_when_legacy_file_also_exists() {
    let temp = TempDir::new().unwrap();
    let mut legacy = registration_result("");
    legacy.email = "legacy@example.com".into();
    let mut migrated = legacy.clone();
    migrated.account_id = "account-1".into();
    migrated.record_revision = 8;
    fs::write(
      temp.path().join("legacy@example.com.json"),
      serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();
    fs::write(
      account_file(&temp, "account-1"),
      serde_json::to_vec_pretty(&migrated).unwrap(),
    )
    .unwrap();

    let reloaded = CredentialStore::with_base_dir(temp.path()).unwrap();

    assert_eq!(reloaded.accounts.len(), 1);
    assert_eq!(reloaded.get("account-1").unwrap().record_revision, 8);
    assert_eq!(
      reloaded.get("legacy@example.com").unwrap().account_id,
      "account-1"
    );
  }

  #[test]
  fn stable_key_change_never_deletes_a_distinct_account_with_the_same_email() {
    let temp = TempDir::new().unwrap();
    let mut first = registration_result("account-1");
    first.email = "shared@example.com".into();
    let mut second = registration_result("account-2");
    second.email = "shared@example.com".into();
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&first).unwrap();
    store.save(&second).unwrap();

    second.note = "updated".into();
    store.save(&second).unwrap();

    assert!(account_file(&temp, "account-1").exists());
    assert!(account_file(&temp, "account-2").exists());
    assert!(store.get("account-1").is_some());
    assert_eq!(store.get("account-2").unwrap().note, "updated");
  }

  #[test]
  fn stale_save_cannot_overwrite_newer_revision_in_memory_or_on_disk() {
    let temp = TempDir::new().unwrap();
    let original = registration_result("account-1");
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&original).unwrap();

    let mut current = original.clone();
    current.record_revision += 1;
    current.note = "newer-note".into();
    store.save(&current).unwrap();
    let disk_before = fs::read(account_file(&temp, "account-1")).unwrap();

    let mut stale = original;
    stale.note = "stale-note".into();
    let error = store.save(&stale).unwrap_err();

    assert!(error.contains("revision conflict"));
    assert_eq!(store.get("account-1").unwrap().note, "newer-note");
    assert_eq!(
      fs::read(account_file(&temp, "account-1")).unwrap(),
      disk_before
    );
  }

  #[test]
  fn status_and_note_updates_increment_revision_and_preserve_credentials() {
    let temp = TempDir::new().unwrap();
    let original = registration_result("account-1");
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&original).unwrap();

    assert_eq!(
      store
        .update_status(
          &["account-1".into()],
          AccountInventoryStatus::Exported,
          Some("exported-note".into()),
        )
        .unwrap(),
      1
    );
    let status_updated = store.get("account-1").unwrap();
    assert_eq!(status_updated.record_revision, 8);
    assert_eq!(status_updated.status, AccountInventoryStatus::Exported);
    assert_eq!(status_updated.note, "exported-note");
    assert!(status_updated.exported_at.is_some());
    assert_eq!(status_updated.password, ORIGINAL_PASSWORD);
    assert_eq!(status_updated.access_token, ORIGINAL_ACCESS_TOKEN);
    assert_eq!(status_updated.cdk, ORIGINAL_CDK);

    assert!(store
      .update_note("account-1", "second-note".into())
      .unwrap());
    let note_updated = store.get("account-1").unwrap();
    assert_eq!(note_updated.record_revision, 9);
    assert_eq!(note_updated.note, "second-note");
    assert_eq!(note_updated.status, AccountInventoryStatus::Exported);
    assert_eq!(note_updated.exported_at, status_updated.exported_at);
    assert_eq!(note_updated.password, ORIGINAL_PASSWORD);
    assert_eq!(note_updated.error_message, "original-error");
    assert_eq!(note_updated.step_logs, vec!["original-log"]);

    let reloaded = CredentialStore::with_base_dir(temp.path()).unwrap();
    assert_eq!(reloaded.get("account-1").unwrap().record_revision, 9);
  }

  #[test]
  fn failed_status_or_note_update_returns_error_without_counting_or_mutating() {
    let temp = TempDir::new().unwrap();
    let original = registration_result("account-1");
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&original).unwrap();
    let disk_before = fs::read(account_file(&temp, "account-1")).unwrap();

    store.fail_next_account_write(AtomicWriteFailureStage::Replace);
    assert!(store
      .update_status(&["account-1".into()], AccountInventoryStatus::Sold, None,)
      .is_err());
    assert_eq!(store.get("account-1").unwrap().record_revision, 7);
    assert_eq!(
      fs::read(account_file(&temp, "account-1")).unwrap(),
      disk_before
    );

    store.fail_next_account_write(AtomicWriteFailureStage::Write);
    assert!(store.update_note("account-1", "not-saved".into()).is_err());
    assert_eq!(store.get("account-1").unwrap().note, "original-note");
  }

  #[test]
  fn new_registration_finalize_promotes_owned_provisional_record() {
    let temp = TempDir::new().unwrap();
    let operation_id = "registration-operation";
    let original = new_registration_provisional("account-1", operation_id);
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&original).unwrap();

    let updated = store
      .compare_and_update(
        "account-1",
        original.record_revision,
        BackfillPatchPrecondition::finalize_new_registration_enabled(operation_id),
        TwoFactorBackfillPatch::finalize_new_registration_enabled("JBSWY3DPEHPK3PXP"),
      )
      .unwrap();

    assert!(updated.success);
    assert_eq!(updated.status, AccountInventoryStatus::Available);
    assert!(updated.two_fa_enabled);
    assert_eq!(updated.totp_secret, "JBSWY3DPEHPK3PXP");
    assert_eq!(updated.error_message, "");
    assert_eq!(updated.note, "");
    assert_eq!(
      updated.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      updated.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Enabled)
    );
    assert_eq!(
      updated.two_factor_backfill_operation_id.as_deref(),
      Some(operation_id)
    );
    assert_eq!(updated.record_revision, original.record_revision + 1);
  }

  #[test]
  fn new_registration_finalize_rejects_wrong_owner_revision_status_reason_and_empty_secret() {
    let operation_id = "registration-operation";

    let wrong_owner = new_registration_provisional("account-1", "other-operation");
    assert!(assert_cas_rejection_preserves(
      wrong_owner,
      BackfillPatchPrecondition::finalize_new_registration_enabled(operation_id),
      TwoFactorBackfillPatch::finalize_new_registration_enabled("JBSWY3DPEHPK3PXP"),
    )
    .contains("operation ownership"));

    let temp = TempDir::new().unwrap();
    let original = new_registration_provisional("account-1", operation_id);
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&original).unwrap();
    assert!(store
      .compare_and_update(
        "account-1",
        original.record_revision + 1,
        BackfillPatchPrecondition::finalize_new_registration_enabled(operation_id),
        TwoFactorBackfillPatch::finalize_new_registration_enabled("JBSWY3DPEHPK3PXP"),
      )
      .unwrap_err()
      .contains("revision conflict"));

    let mut wrong_status = new_registration_provisional("account-1", operation_id);
    wrong_status.status = AccountInventoryStatus::Invalid;
    assert!(assert_cas_rejection_preserves(
      wrong_status,
      BackfillPatchPrecondition::finalize_new_registration_enabled(operation_id),
      TwoFactorBackfillPatch::finalize_new_registration_enabled("JBSWY3DPEHPK3PXP"),
    )
    .contains("provisional record"));

    let mut wrong_reason = new_registration_provisional("account-1", operation_id);
    wrong_reason.registration_outcome_reason = Some(RegistrationOutcomeReason::FreeTrialNo);
    assert!(assert_cas_rejection_preserves(
      wrong_reason,
      BackfillPatchPrecondition::finalize_new_registration_enabled(operation_id),
      TwoFactorBackfillPatch::finalize_new_registration_enabled("JBSWY3DPEHPK3PXP"),
    )
    .contains("provisional record"));

    let empty_secret = new_registration_provisional("account-1", operation_id);
    assert!(assert_cas_rejection_preserves(
      empty_secret,
      BackfillPatchPrecondition::finalize_new_registration_enabled(operation_id),
      TwoFactorBackfillPatch::finalize_new_registration_enabled("  "),
    )
    .contains("non-empty TOTP secret"));
  }

  #[test]
  fn registration_terminal_patches_keep_owned_provisional_reserved_without_secret() {
    let operation_id = "registration-operation";
    for (operation, expected_outcome) in [
      (
        BackfillPatchOperation::FinalizeFailed,
        TwoFactorBackfillOutcome::Failed,
      ),
      (
        BackfillPatchOperation::FinalizeCancelled,
        TwoFactorBackfillOutcome::Cancelled,
      ),
      (
        BackfillPatchOperation::FinalizeReconciliationRequired,
        TwoFactorBackfillOutcome::ReconciliationRequired,
      ),
    ] {
      let temp = TempDir::new().unwrap();
      let original = new_registration_provisional("account-1", operation_id);
      let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
      store.save(&original).unwrap();

      let updated = store
        .compare_and_update(
          "account-1",
          original.record_revision,
          precondition_for(operation, operation_id, false),
          patch_for(operation),
        )
        .unwrap();

      assert!(!updated.success);
      assert_eq!(updated.status, AccountInventoryStatus::Reserved);
      assert!(!updated.two_fa_enabled);
      assert!(updated.totp_secret.is_empty());
      assert_eq!(updated.two_factor_backfill_outcome, Some(expected_outcome));
    }
  }

  #[test]
  fn available_status_transition_requires_complete_successful_two_factor_record() {
    let mutations: [fn(&mut RegistrationResult); 4] = [
      |account| account.success = false,
      |account| account.free_trial_eligible = false,
      |account| account.two_fa_enabled = false,
      |account| account.totp_secret.clear(),
    ];
    for mutation in mutations {
      let temp = TempDir::new().unwrap();
      let mut account = backfill_ready_result("account-1");
      account.status = AccountInventoryStatus::Reserved;
      account.two_fa_enabled = true;
      account.totp_secret = "JBSWY3DPEHPK3PXP".into();
      mutation(&mut account);
      let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
      store.save(&account).unwrap();

      let error = store
        .update_status(
          &["account-1".into()],
          AccountInventoryStatus::Available,
          None,
        )
        .unwrap_err();

      assert!(error.contains("cannot be marked available"));
      assert_eq!(
        store.get("account-1").unwrap().status,
        AccountInventoryStatus::Reserved
      );
    }
  }

  #[test]
  fn legacy_available_record_can_be_loaded_and_re_saved_without_migration() {
    let temp = TempDir::new().unwrap();
    let legacy = registration_result("account-1");
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&legacy).unwrap();

    assert!(store.get("account-1").is_some());
    assert_eq!(
      store
        .update_status(
          &["account-1".into()],
          AccountInventoryStatus::Available,
          Some("legacy-note".into()),
        )
        .unwrap(),
      1
    );
    assert_eq!(store.get("account-1").unwrap().note, "legacy-note");
  }

  #[test]
  fn safe_failed_registration_is_startable_and_backfill_success_promotes_it() {
    let temp = TempDir::new().unwrap();
    let failed = retryable_new_registration_failure("account-1");
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&failed).unwrap();

    let started = store
      .compare_and_update(
        "account-1",
        failed.record_revision,
        BackfillPatchPrecondition::start("backfill-operation", false, false),
        TwoFactorBackfillPatch::start(),
      )
      .unwrap();
    assert!(!started.success);
    assert_eq!(started.status, AccountInventoryStatus::Reserved);
    assert_eq!(
      started.two_factor_backfill_state,
      Some(TwoFactorBackfillState::InProgress)
    );

    let enabled = store
      .compare_and_update(
        "account-1",
        started.record_revision,
        BackfillPatchPrecondition::finalize_enabled("backfill-operation", false, false),
        TwoFactorBackfillPatch::finalize_enabled("JBSWY3DPEHPK3PXP"),
      )
      .unwrap();

    assert!(enabled.success);
    assert_eq!(enabled.status, AccountInventoryStatus::Available);
    assert!(enabled.two_fa_enabled);
    assert_eq!(enabled.totp_secret, "JBSWY3DPEHPK3PXP");
    assert_eq!(enabled.error_message, "");
    assert_eq!(enabled.note, "");
    assert_eq!(
      enabled.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Enabled)
    );
  }

  #[test]
  fn ordinary_unsuccessful_reserved_record_is_not_startable() {
    let mut account = retryable_new_registration_failure("account-1");
    account.registration_outcome_reason = Some(RegistrationOutcomeReason::RegistrationFailed);

    assert!(assert_cas_rejection_preserves(
      account,
      BackfillPatchPrecondition::start("backfill-operation", false, false),
      TwoFactorBackfillPatch::start(),
    )
    .contains("unsuccessful"));
  }

  #[test]
  fn cas_patch_changes_only_the_allowed_fields() {
    let temp = TempDir::new().unwrap();
    let operation_id = "operation-1";
    let original = account_for_operation(BackfillPatchOperation::FinalizeEnabled, operation_id);
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&original).unwrap();

    let updated = store
      .compare_and_update(
        "account-1",
        7,
        BackfillPatchPrecondition::finalize_enabled(operation_id, false, false),
        TwoFactorBackfillPatch::finalize_enabled("JBSWY3DPEHPK3PXP"),
      )
      .unwrap();

    assert_eq!(updated.record_revision, 8);
    assert_eq!(updated.email_provider, original.email_provider);
    assert_eq!(
      updated.email_provider_provenance,
      original.email_provider_provenance
    );
    assert!(updated.two_fa_enabled);
    assert_eq!(updated.totp_secret, "JBSWY3DPEHPK3PXP");
    assert_eq!(
      updated.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      updated.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Enabled)
    );

    let mut original_json = serde_json::to_value(original).unwrap();
    let mut updated_json = serde_json::to_value(updated).unwrap();
    for field in [
      "twoFactorBackfillState",
      "twoFactorBackfillOperationId",
      "twoFactorBackfillOutcome",
      "twoFaEnabled",
      "totpSecret",
      "recordRevision",
    ] {
      original_json.as_object_mut().unwrap().remove(field);
      updated_json.as_object_mut().unwrap().remove(field);
    }
    assert_eq!(updated_json, original_json);
  }

  #[test]
  fn locked_finalize_marks_access_locked_and_preserves_account_fields() {
    let temp = TempDir::new().unwrap();
    let operation_id = "operation-locked";
    let original = account_for_operation(BackfillPatchOperation::FinalizeFailed, operation_id);
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&original).unwrap();

    let updated = store
      .compare_and_update(
        "account-1",
        original.record_revision,
        BackfillPatchPrecondition::finalize_failed(operation_id, false, false),
        TwoFactorBackfillPatch::finalize_failed_locked(),
      )
      .unwrap();

    assert_eq!(
      updated.two_factor_backfill_access_state,
      Some(TwoFactorBackfillAccessState::Locked)
    );
    assert_eq!(
      updated.two_factor_backfill_state,
      Some(TwoFactorBackfillState::Completed)
    );
    assert_eq!(
      updated.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Failed)
    );
    assert_eq!(updated.record_revision, original.record_revision + 1);
    assert_eq!(updated.password, original.password);
    assert_eq!(updated.cdk, original.cdk);
    assert_eq!(updated.status, original.status);
    assert_eq!(updated.note, original.note);

    let reloaded = CredentialStore::with_base_dir(temp.path()).unwrap();
    assert_eq!(
      reloaded
        .get("account-1")
        .unwrap()
        .two_factor_backfill_access_state,
      Some(TwoFactorBackfillAccessState::Locked)
    );
  }

  #[test]
  fn terminal_cas_variants_preserve_fields_after_owned_account_becomes_ineligible() {
    let operation_id = "operation-1";
    for (operation, expected_outcome) in [
      (
        BackfillPatchOperation::FinalizeFailed,
        TwoFactorBackfillOutcome::Failed,
      ),
      (
        BackfillPatchOperation::FinalizeCancelled,
        TwoFactorBackfillOutcome::Cancelled,
      ),
      (
        BackfillPatchOperation::FinalizeReconciliationRequired,
        TwoFactorBackfillOutcome::ReconciliationRequired,
      ),
    ] {
      let temp = TempDir::new().unwrap();
      let mut original = account_for_operation(operation, operation_id);
      original.success = false;
      original.status = AccountInventoryStatus::Sold;
      original.email.clear();
      original.password.clear();
      original.cdk.clear();
      original.email_provider = None;
      original.email_provider_provenance = None;
      original.two_factor_backfill_access_state = Some(TwoFactorBackfillAccessState::Locked);
      original.two_factor_backfill_exclusion = Some(TwoFactorBackfillExclusion::ManualReview);
      let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
      store.save(&original).unwrap();

      let updated = store
        .compare_and_update(
          "account-1",
          original.record_revision,
          precondition_for(operation, operation_id, false),
          patch_for(operation),
        )
        .unwrap();

      assert_eq!(
        updated.two_factor_backfill_state,
        Some(TwoFactorBackfillState::Completed)
      );
      assert_eq!(updated.two_factor_backfill_outcome, Some(expected_outcome));
      assert_eq!(updated.record_revision, original.record_revision + 1);
      let mut original_json = serde_json::to_value(original).unwrap();
      let mut updated_json = serde_json::to_value(updated).unwrap();
      for field in [
        "twoFactorBackfillState",
        "twoFactorBackfillOperationId",
        "twoFactorBackfillOutcome",
        "recordRevision",
      ] {
        original_json.as_object_mut().unwrap().remove(field);
        updated_json.as_object_mut().unwrap().remove(field);
      }
      assert_eq!(updated_json, original_json, "operation={operation:?}");
    }
  }

  #[test]
  fn cas_rejects_provider_conflicts_and_existing_two_factor_credentials() {
    let temp = TempDir::new().unwrap();
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    let mut configured = registration_result("account-1");
    configured.email_provider = Some(EmailProvider::Gmail123452026);
    configured.email_provider_provenance = Some(EmailProviderProvenance::RegistrationConfig);
    store.save(&configured).unwrap();

    let provider_error = store
      .persist_inferred_email_provider("account-1", 7, EmailProvider::SmsIosmq)
      .unwrap_err();
    assert!(provider_error.contains("provider conflict"));

    let mut enabled = configured;
    enabled.two_fa_enabled = true;
    enabled.totp_secret = "existing-secret".into();
    enabled.record_revision = 8;
    enabled.two_factor_backfill_state = Some(TwoFactorBackfillState::InProgress);
    enabled.two_factor_backfill_operation_id = Some("operation-1".into());
    store.save(&enabled).unwrap();
    let disk_before = fs::read(account_file(&temp, "account-1")).unwrap();

    assert!(store
      .compare_and_update(
        "account-1",
        8,
        BackfillPatchPrecondition::finalize_enabled("operation-1", false, false),
        TwoFactorBackfillPatch::finalize_enabled("replacement-secret"),
      )
      .unwrap_err()
      .contains("local 2FA state"));
    assert_eq!(
      fs::read(account_file(&temp, "account-1")).unwrap(),
      disk_before
    );
    assert_eq!(
      store.get("account-1").unwrap().totp_secret,
      "existing-secret"
    );
  }

  #[test]
  fn stale_or_concurrently_invalidated_cas_is_rejected() {
    let temp = TempDir::new().unwrap();
    let original = backfill_ready_result("account-1");
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&original).unwrap();

    assert!(store
      .compare_and_update(
        "account-1",
        6,
        BackfillPatchPrecondition::start("operation-1", false, false),
        TwoFactorBackfillPatch::start(),
      )
      .unwrap_err()
      .contains("revision conflict"));

    store
      .update_note("account-1", "concurrent-note".into())
      .unwrap();
    assert!(store
      .compare_and_update(
        "account-1",
        7,
        BackfillPatchPrecondition::start("operation-1", false, false),
        TwoFactorBackfillPatch::start(),
      )
      .unwrap_err()
      .contains("revision conflict"));
    let current = store.get("account-1").unwrap();
    assert_eq!(current.note, "concurrent-note");
    assert_eq!(current.record_revision, 8);
  }

  #[test]
  fn cas_revalidates_every_policy_gate_for_start_and_finalize_under_lock() {
    let operation_id = "operation-1";

    for operation in [
      BackfillPatchOperation::Start,
      BackfillPatchOperation::FinalizeEnabled,
    ] {
      let valid = account_for_operation(operation, operation_id);
      let mut cases = Vec::new();

      let mut unsuccessful = valid.clone();
      unsuccessful.success = false;
      cases.push(unsuccessful);

      for status in [
        AccountInventoryStatus::Sold,
        AccountInventoryStatus::Exported,
        AccountInventoryStatus::Reserved,
      ] {
        let mut account = valid.clone();
        account.status = status;
        cases.push(account);
      }

      let mut invalid_unknown = valid.clone();
      invalid_unknown.status = AccountInventoryStatus::Invalid;
      invalid_unknown.registration_outcome_reason = None;
      cases.push(invalid_unknown);

      let mut locked = valid.clone();
      locked.two_factor_backfill_access_state = Some(TwoFactorBackfillAccessState::Locked);
      cases.push(locked);

      let mut excluded = valid.clone();
      excluded.two_factor_backfill_exclusion = Some(TwoFactorBackfillExclusion::ManualReview);
      cases.push(excluded);

      let mut missing_provider = valid.clone();
      missing_provider.email_provider = None;
      missing_provider.email_provider_provenance = None;
      cases.push(missing_provider);

      let mut provider_missing_only = valid.clone();
      provider_missing_only.email_provider = None;
      cases.push(provider_missing_only);

      let mut missing_provenance = valid.clone();
      missing_provenance.email_provider_provenance = None;
      cases.push(missing_provenance);

      let mut missing_email = valid.clone();
      missing_email.email = "  ".into();
      cases.push(missing_email);

      let mut missing_password = valid.clone();
      missing_password.password = "  ".into();
      cases.push(missing_password);

      let mut missing_cdk = valid.clone();
      missing_cdk.cdk = "  ".into();
      cases.push(missing_cdk);

      let mut enabled = valid.clone();
      enabled.two_fa_enabled = true;
      cases.push(enabled);

      let mut secret_without_flag = valid.clone();
      secret_without_flag.totp_secret = "existing-secret".into();
      cases.push(secret_without_flag);

      for current in cases {
        let error = assert_cas_rejection_preserves(
          current,
          precondition_for(operation, operation_id, false),
          patch_for(operation),
        );
        assert!(error.contains("precondition"), "{operation:?}: {error}");
      }
    }
  }

  #[test]
  fn cas_rejects_lifecycle_mismatches_and_operation_bypass_without_mutation() {
    let operation_id = "operation-1";

    let mut already_in_progress = backfill_ready_result("account-1");
    already_in_progress.two_factor_backfill_state = Some(TwoFactorBackfillState::InProgress);
    already_in_progress.two_factor_backfill_operation_id = Some(operation_id.into());
    assert!(assert_cas_rejection_preserves(
      already_in_progress,
      BackfillPatchPrecondition::start(operation_id, false, false),
      TwoFactorBackfillPatch::start(),
    )
    .contains("lifecycle"));

    let not_started = backfill_ready_result("account-1");
    assert!(assert_cas_rejection_preserves(
      not_started,
      BackfillPatchPrecondition::finalize_enabled(operation_id, false, false),
      TwoFactorBackfillPatch::finalize_enabled("JBSWY3DPEHPK3PXP"),
    )
    .contains("lifecycle"));

    let mut wrong_owner =
      account_for_operation(BackfillPatchOperation::FinalizeEnabled, operation_id);
    wrong_owner.two_factor_backfill_operation_id = Some("other-operation".into());
    assert!(assert_cas_rejection_preserves(
      wrong_owner,
      BackfillPatchPrecondition::finalize_enabled(operation_id, false, false),
      TwoFactorBackfillPatch::finalize_enabled("JBSWY3DPEHPK3PXP"),
    )
    .contains("operation ownership"));

    for (precondition, patch) in [
      (
        BackfillPatchPrecondition::start(operation_id, false, false),
        TwoFactorBackfillPatch::finalize_enabled("JBSWY3DPEHPK3PXP"),
      ),
      (
        BackfillPatchPrecondition::finalize_enabled(operation_id, false, false),
        TwoFactorBackfillPatch::start(),
      ),
    ] {
      let account = account_for_operation(precondition.operation, operation_id);
      assert!(
        assert_cas_rejection_preserves(account, precondition, patch).contains("does not match")
      );
    }
  }

  #[test]
  fn free_trial_no_override_must_be_explicit_for_start_and_finalize() {
    let operation_id = "operation-1";
    let mut account = backfill_ready_result("account-1");
    account.success = false;
    account.status = AccountInventoryStatus::Invalid;
    account.registration_outcome_reason = Some(RegistrationOutcomeReason::FreeTrialNo);

    assert!(assert_cas_rejection_preserves(
      account.clone(),
      BackfillPatchPrecondition::start(operation_id, false, false),
      TwoFactorBackfillPatch::start(),
    )
    .contains("unsuccessful"));

    let temp = TempDir::new().unwrap();
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&account).unwrap();
    let started = store
      .compare_and_update(
        "account-1",
        7,
        BackfillPatchPrecondition::start(operation_id, true, false),
        TwoFactorBackfillPatch::start(),
      )
      .unwrap();
    assert_eq!(
      started.two_factor_backfill_state,
      Some(TwoFactorBackfillState::InProgress)
    );

    assert!(assert_cas_rejection_preserves(
      started.clone(),
      BackfillPatchPrecondition::finalize_enabled(operation_id, false, false),
      TwoFactorBackfillPatch::finalize_enabled("JBSWY3DPEHPK3PXP"),
    )
    .contains("unsuccessful"));

    let finalized = store
      .compare_and_update(
        "account-1",
        started.record_revision,
        BackfillPatchPrecondition::finalize_enabled(operation_id, true, false),
        TwoFactorBackfillPatch::finalize_enabled("JBSWY3DPEHPK3PXP"),
      )
      .unwrap();
    assert!(finalized.two_fa_enabled);
    assert_eq!(
      finalized.two_factor_backfill_outcome,
      Some(TwoFactorBackfillOutcome::Enabled)
    );
  }

  #[test]
  fn provider_inference_is_one_time_idempotent_and_rejects_conflicts() {
    let temp = TempDir::new().unwrap();
    let original = registration_result("account-1");
    let mut store = CredentialStore::with_base_dir(temp.path()).unwrap();
    store.save(&original).unwrap();

    let inferred = store
      .persist_inferred_email_provider("account-1", 7, EmailProvider::Gmail123452026)
      .unwrap();
    assert_eq!(inferred.record_revision, 8);
    assert_eq!(
      inferred.email_provider_provenance,
      Some(EmailProviderProvenance::InferredFromCdk)
    );

    let idempotent = store
      .persist_inferred_email_provider("account-1", 8, EmailProvider::Gmail123452026)
      .unwrap();
    assert_eq!(idempotent.record_revision, 8);

    assert!(store
      .persist_inferred_email_provider("account-1", 8, EmailProvider::SmsIosmq)
      .unwrap_err()
      .contains("provider conflict"));

    let mut configured = registration_result("account-2");
    configured.email_provider = Some(EmailProvider::Gmail123452026);
    configured.email_provider_provenance = Some(EmailProviderProvenance::RegistrationConfig);
    store.save(&configured).unwrap();
    assert!(store
      .persist_inferred_email_provider("account-2", 7, EmailProvider::Gmail123452026,)
      .unwrap_err()
      .contains("provider conflict"));
  }

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
