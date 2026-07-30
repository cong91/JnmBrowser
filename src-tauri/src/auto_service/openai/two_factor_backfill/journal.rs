use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app_dirs::data_dir;
use crate::auto_service::openai::register::store::{persist_temp_file, sync_parent_directory};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TwoFactorBackfillJournalState {
  SecretCaptured,
  RemoteConfirmed,
  ManualReview,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TwoFactorBackfillJournalEntry {
  pub operation_id: String,
  pub account_key: String,
  pub expected_account_revision: u64,
  #[serde(default)]
  pub final_account_revision: Option<u64>,
  pub journal_revision: u64,
  pub pending_totp_secret: String,
  pub state: TwoFactorBackfillJournalState,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PersistedBackfillAccountPatch<'a> {
  pub(crate) revision: u64,
  pub(crate) operation_id: Option<&'a str>,
  pub(crate) two_fa_enabled: bool,
  pub(crate) totp_secret: &'a str,
}

impl<'a> PersistedBackfillAccountPatch<'a> {
  pub(crate) fn new(
    revision: u64,
    operation_id: Option<&'a str>,
    two_fa_enabled: bool,
    totp_secret: &'a str,
  ) -> Self {
    Self {
      revision,
      operation_id,
      two_fa_enabled,
      totp_secret,
    }
  }
}

pub(crate) struct TwoFactorBackfillJournal {
  base_dir: PathBuf,
  #[cfg(test)]
  fail_next_operation: Option<AtomicJournalFailureStage>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtomicJournalFailureStage {
  Write,
  Replace,
  SyncDirectory,
  Delete,
}

impl TwoFactorBackfillJournal {
  pub(crate) fn new() -> Result<Self, String> {
    Self::with_base_dir(data_dir().join("two_factor_backfill").join("journal"))
  }

  pub(crate) fn with_base_dir(base_dir: impl AsRef<Path>) -> Result<Self, String> {
    let base_dir = base_dir.as_ref().to_path_buf();
    fs::create_dir_all(&base_dir).map_err(|error| {
      format!(
        "Failed to create 2FA backfill journal directory {}: {error}",
        base_dir.display()
      )
    })?;
    Ok(Self {
      base_dir,
      #[cfg(test)]
      fail_next_operation: None,
    })
  }

  pub(crate) fn create_secret_captured(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_account_revision: u64,
    pending_totp_secret: String,
  ) -> Result<TwoFactorBackfillJournalEntry, String> {
    validate_identity(operation_id, account_key)?;
    if pending_totp_secret.trim().is_empty() {
      return Err("2FA backfill journal secret cannot be empty".into());
    }
    if self.load(operation_id, account_key)?.is_some() {
      return Err(format!(
        "2FA backfill journal entry already exists for operation {operation_id} and account {account_key}"
      ));
    }
    let now = Utc::now();
    let entry = TwoFactorBackfillJournalEntry {
      operation_id: operation_id.to_string(),
      account_key: account_key.to_string(),
      expected_account_revision,
      final_account_revision: None,
      journal_revision: 1,
      pending_totp_secret,
      state: TwoFactorBackfillJournalState::SecretCaptured,
      created_at: now,
      updated_at: now,
    };
    self.persist(&entry)?;
    Ok(entry)
  }

  pub(crate) fn update_state(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_journal_revision: u64,
    state: TwoFactorBackfillJournalState,
  ) -> Result<TwoFactorBackfillJournalEntry, String> {
    let mut entry = self
      .load(operation_id, account_key)?
      .ok_or_else(|| journal_not_found(operation_id, account_key))?;
    if entry.journal_revision != expected_journal_revision {
      return Err(format!(
        "2FA backfill journal revision conflict for operation {operation_id} and account {account_key}: expected {expected_journal_revision}, current {}",
        entry.journal_revision
      ));
    }
    if !transition_allowed(entry.state, state) {
      return Err(format!(
        "Invalid 2FA backfill journal transition from {:?} to {:?}",
        entry.state, state
      ));
    }
    entry.journal_revision = entry
      .journal_revision
      .checked_add(1)
      .ok_or_else(|| "2FA backfill journal revision overflow".to_string())?;
    entry.state = state;
    entry.updated_at = Utc::now();
    self.persist(&entry)?;
    Ok(entry)
  }

  pub(crate) fn load(
    &self,
    operation_id: &str,
    account_key: &str,
  ) -> Result<Option<TwoFactorBackfillJournalEntry>, String> {
    validate_identity(operation_id, account_key)?;
    let path = self.entry_path(operation_id, account_key);
    let bytes = match fs::read(&path) {
      Ok(bytes) => bytes,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => {
        return Err(format!(
          "Failed to read 2FA backfill journal file {}: {error}",
          path.display()
        ));
      }
    };
    let entry: TwoFactorBackfillJournalEntry = serde_json::from_slice(&bytes).map_err(|error| {
      format!(
        "Failed to deserialize 2FA backfill journal file {}: {error}",
        path.display()
      )
    })?;
    if entry.operation_id != operation_id || entry.account_key != account_key {
      return Err(format!(
        "2FA backfill journal identity mismatch in {}",
        path.display()
      ));
    }
    Ok(Some(entry))
  }

  pub(crate) fn list(&self) -> Result<Vec<TwoFactorBackfillJournalEntry>, String> {
    let mut entries: Vec<TwoFactorBackfillJournalEntry> = Vec::new();
    let directory = fs::read_dir(&self.base_dir).map_err(|error| {
      format!(
        "Failed to read 2FA backfill journal directory {}: {error}",
        self.base_dir.display()
      )
    })?;
    for directory_entry in directory {
      let directory_entry = directory_entry.map_err(|error| {
        format!(
          "Failed to read an entry from 2FA backfill journal directory {}: {error}",
          self.base_dir.display()
        )
      })?;
      let path = directory_entry.path();
      if path.extension().is_none_or(|extension| extension != "json") {
        continue;
      }
      let bytes = fs::read(&path).map_err(|error| {
        format!(
          "Failed to read 2FA backfill journal file {}: {error}",
          path.display()
        )
      })?;
      entries.push(serde_json::from_slice(&bytes).map_err(|error| {
        format!(
          "Failed to deserialize 2FA backfill journal file {}: {error}",
          path.display()
        )
      })?);
    }
    entries.sort_by(|left, right| {
      left
        .operation_id
        .cmp(&right.operation_id)
        .then_with(|| left.account_key.cmp(&right.account_key))
    });
    Ok(entries)
  }

  pub(crate) fn transition_to_manual_review(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_journal_revision: u64,
  ) -> Result<TwoFactorBackfillJournalEntry, String> {
    self.update_state(
      operation_id,
      account_key,
      expected_journal_revision,
      TwoFactorBackfillJournalState::ManualReview,
    )
  }

  pub(crate) fn summarize(
    entry: &TwoFactorBackfillJournalEntry,
  ) -> crate::auto_service::openai::two_factor_backfill::types::TwoFactorBackfillRecoverySummary {
    use crate::auto_service::openai::two_factor_backfill::types::{
      TwoFactorBackfillRecoveryState, TwoFactorBackfillRecoverySummary,
    };

    TwoFactorBackfillRecoverySummary {
      operation_id: entry.operation_id.clone(),
      account_key: entry.account_key.clone(),
      state: match entry.state {
        TwoFactorBackfillJournalState::SecretCaptured => {
          TwoFactorBackfillRecoveryState::SecretCaptured
        }
        TwoFactorBackfillJournalState::RemoteConfirmed => {
          TwoFactorBackfillRecoveryState::RemoteConfirmed
        }
        TwoFactorBackfillJournalState::ManualReview => TwoFactorBackfillRecoveryState::ManualReview,
      },
      expected_account_revision: entry.expected_account_revision,
      final_account_revision: entry.final_account_revision,
      journal_revision: entry.journal_revision,
      created_at: entry.created_at,
      updated_at: entry.updated_at,
    }
  }
  pub(crate) fn record_final_account_revision(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_journal_revision: u64,
    final_account_revision: u64,
  ) -> Result<TwoFactorBackfillJournalEntry, String> {
    let mut entry = self
      .load(operation_id, account_key)?
      .ok_or_else(|| journal_not_found(operation_id, account_key))?;
    if entry.journal_revision != expected_journal_revision {
      return Err(format!(
        "2FA backfill journal revision conflict for operation {operation_id} and account {account_key}: expected {expected_journal_revision}, current {}",
        entry.journal_revision
      ));
    }
    if entry.state != TwoFactorBackfillJournalState::RemoteConfirmed {
      return Err("2FA backfill final account revision requires remote confirmation".into());
    }
    if final_account_revision <= entry.expected_account_revision {
      return Err("2FA backfill final account revision must advance the account record".into());
    }
    entry.journal_revision = entry
      .journal_revision
      .checked_add(1)
      .ok_or_else(|| "2FA backfill journal revision overflow".to_string())?;
    entry.final_account_revision = Some(final_account_revision);
    entry.updated_at = Utc::now();
    self.persist(&entry)?;
    Ok(entry)
  }

  pub(crate) fn delete_after_account_patch(
    &mut self,
    operation_id: &str,
    account_key: &str,
    expected_journal_revision: u64,
    persisted: PersistedBackfillAccountPatch<'_>,
  ) -> Result<bool, String> {
    let Some(entry) = self.load(operation_id, account_key)? else {
      return Ok(false);
    };
    if entry.journal_revision != expected_journal_revision {
      return Err(format!(
        "2FA backfill journal revision conflict for operation {operation_id} and account {account_key}: expected {expected_journal_revision}, current {}",
        entry.journal_revision
      ));
    }
    if entry.state != TwoFactorBackfillJournalState::RemoteConfirmed {
      return Err("2FA backfill journal cleanup requires remote confirmation".into());
    }
    if entry.final_account_revision != Some(persisted.revision) {
      return Err("2FA backfill journal cleanup requires the exact final account revision".into());
    }
    if persisted.operation_id != Some(operation_id) {
      return Err("2FA backfill journal cleanup operation ownership conflict".into());
    }
    if !persisted.two_fa_enabled || persisted.totp_secret != entry.pending_totp_secret {
      return Err("2FA backfill journal cleanup requires the final account patch".into());
    }
    #[cfg(test)]
    if self.take_failure(AtomicJournalFailureStage::Delete) {
      return Err("Injected 2FA backfill journal delete failure".into());
    }
    let path = self.entry_path(operation_id, account_key);
    match fs::remove_file(&path) {
      Ok(()) => sync_parent_directory(&self.base_dir, "2FA backfill journal directory")?,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
      Err(error) => {
        return Err(format!(
          "Failed to delete 2FA backfill journal file {}: {error}",
          path.display()
        ));
      }
    }
    Ok(true)
  }

  fn persist(&mut self, entry: &TwoFactorBackfillJournalEntry) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(entry)
      .map_err(|error| format!("Failed to serialize 2FA backfill journal entry: {error}"))?;
    let path = self.entry_path(&entry.operation_id, &entry.account_key);
    let previous_bytes = match fs::read(&path) {
      Ok(bytes) => Some(bytes),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
      Err(error) => {
        return Err(format!(
          "Failed to read 2FA backfill journal file {} before replacement: {error}",
          path.display()
        ));
      }
    };
    let mut temp_file = tempfile::NamedTempFile::new_in(&self.base_dir).map_err(|error| {
      format!(
        "Failed to create 2FA backfill journal temp file in {}: {error}",
        self.base_dir.display()
      )
    })?;
    #[cfg(test)]
    if self.take_failure(AtomicJournalFailureStage::Write) {
      return Err("Injected 2FA backfill journal write failure".into());
    }
    temp_file
      .write_all(&json)
      .map_err(|error| format!("Failed to write 2FA backfill journal temp file: {error}"))?;
    temp_file
      .as_file()
      .sync_all()
      .map_err(|error| format!("Failed to sync 2FA backfill journal temp file: {error}"))?;
    #[cfg(test)]
    if self.take_failure(AtomicJournalFailureStage::Replace) {
      return Err("Injected 2FA backfill journal replacement failure".into());
    }
    persist_temp_file(temp_file, &path, "2FA backfill journal entry")?;
    #[cfg(test)]
    let sync_result = if self.take_failure(AtomicJournalFailureStage::SyncDirectory) {
      Err("Injected 2FA backfill journal directory sync failure".into())
    } else {
      sync_parent_directory(&self.base_dir, "2FA backfill journal directory")
    };
    #[cfg(not(test))]
    let sync_result = sync_parent_directory(&self.base_dir, "2FA backfill journal directory");
    if let Err(sync_error) = sync_result {
      let rollback_result = restore_journal_file(&self.base_dir, &path, previous_bytes);
      return match rollback_result {
        Ok(()) => Err(sync_error),
        Err(rollback_error) => Err(format!(
          "{sync_error}; failed to roll back 2FA backfill journal entry: {rollback_error}"
        )),
      };
    }
    Ok(())
  }

  fn entry_path(&self, operation_id: &str, account_key: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(operation_id.as_bytes());
    hasher.update([0]);
    hasher.update(account_key.as_bytes());
    let digest = hasher.finalize();
    let mut filename = String::with_capacity(69);
    for byte in digest {
      std::fmt::Write::write_fmt(&mut filename, format_args!("{byte:02x}"))
        .expect("writing SHA-256 digest to String cannot fail");
    }
    filename.push_str(".json");
    self.base_dir.join(filename)
  }

  #[cfg(test)]
  fn fail_next_write(&mut self, stage: AtomicJournalFailureStage) {
    self.fail_next_operation = Some(stage);
  }

  #[cfg(test)]
  fn take_failure(&mut self, stage: AtomicJournalFailureStage) -> bool {
    if self.fail_next_operation == Some(stage) {
      self.fail_next_operation = None;
      true
    } else {
      false
    }
  }
}

fn restore_journal_file(
  base_dir: &Path,
  path: &Path,
  previous_bytes: Option<Vec<u8>>,
) -> Result<(), String> {
  if let Some(previous_bytes) = previous_bytes {
    let mut temp_file = tempfile::NamedTempFile::new_in(base_dir).map_err(|error| {
      format!(
        "Failed to create 2FA backfill journal rollback temp file in {}: {error}",
        base_dir.display()
      )
    })?;
    temp_file
      .write_all(&previous_bytes)
      .map_err(|error| format!("Failed to write 2FA backfill journal rollback file: {error}"))?;
    temp_file
      .as_file()
      .sync_all()
      .map_err(|error| format!("Failed to sync 2FA backfill journal rollback file: {error}"))?;
    persist_temp_file(temp_file, path, "2FA backfill journal rollback")?;
  } else {
    match fs::remove_file(path) {
      Ok(()) => {}
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
      Err(error) => {
        return Err(format!(
          "Failed to remove 2FA backfill journal rollback file {}: {error}",
          path.display()
        ));
      }
    }
  }
  sync_parent_directory(base_dir, "2FA backfill journal rollback directory")
}

fn validate_identity(operation_id: &str, account_key: &str) -> Result<(), String> {
  if operation_id.trim().is_empty() {
    return Err("2FA backfill journal operation ID cannot be empty".into());
  }
  if account_key.trim().is_empty() {
    return Err("2FA backfill journal account key cannot be empty".into());
  }
  Ok(())
}

fn journal_not_found(operation_id: &str, account_key: &str) -> String {
  format!(
    "2FA backfill journal entry not found for operation {operation_id} and account {account_key}"
  )
}

fn transition_allowed(
  current: TwoFactorBackfillJournalState,
  next: TwoFactorBackfillJournalState,
) -> bool {
  matches!(
    (current, next),
    (
      TwoFactorBackfillJournalState::SecretCaptured,
      TwoFactorBackfillJournalState::RemoteConfirmed
    ) | (
      TwoFactorBackfillJournalState::SecretCaptured,
      TwoFactorBackfillJournalState::ManualReview
    ) | (
      TwoFactorBackfillJournalState::RemoteConfirmed,
      TwoFactorBackfillJournalState::ManualReview
    )
  )
}

#[cfg(test)]
mod tests {
  use std::fs;

  use tempfile::TempDir;

  use super::*;

  const PENDING_SECRET: &str = "JBSWY3DPEHPK3PXP";
  const PASSWORD: &str = "known-password";
  const RAW_CDK: &str = "GMAIL-known-cdk";
  const ACCESS_TOKEN: &str = "known-access-token";
  const EMAIL_OTP: &str = "654321";
  const GENERATED_TOTP: &str = "123456";

  fn open_journal(temp: &TempDir) -> TwoFactorBackfillJournal {
    TwoFactorBackfillJournal::with_base_dir(temp.path()).unwrap()
  }

  fn persisted_patch<'a>(
    revision: u64,
    operation_id: Option<&'a str>,
    two_fa_enabled: bool,
    totp_secret: &'a str,
  ) -> PersistedBackfillAccountPatch<'a> {
    PersistedBackfillAccountPatch::new(revision, operation_id, two_fa_enabled, totp_secret)
  }

  #[test]
  fn secret_captured_survives_reload_and_uses_hashed_filename() {
    let temp = TempDir::new().unwrap();
    let mut journal = open_journal(&temp);
    let operation_id = "../../operation:1";
    let account_key = "../account@example.com";

    let created = journal
      .create_secret_captured(operation_id, account_key, 7, PENDING_SECRET.into())
      .unwrap();

    assert_eq!(created.state, TwoFactorBackfillJournalState::SecretCaptured);
    assert_eq!(created.journal_revision, 1);
    assert_eq!(created.pending_totp_secret, PENDING_SECRET);
    let files: Vec<_> = fs::read_dir(temp.path())
      .unwrap()
      .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
      .collect();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].len(), 69);
    assert!(files[0].ends_with(".json"));
    assert!(files[0][..64].bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(!files[0].contains("operation"));
    assert!(!files[0].contains("account"));
    assert!(!temp
      .path()
      .parent()
      .unwrap()
      .join("account@example.com")
      .exists());

    let reloaded = open_journal(&temp)
      .load(operation_id, account_key)
      .unwrap()
      .unwrap();
    assert_eq!(reloaded, created);
  }

  #[test]
  fn remote_confirmed_and_manual_review_are_resumable() {
    for state in [
      TwoFactorBackfillJournalState::RemoteConfirmed,
      TwoFactorBackfillJournalState::ManualReview,
    ] {
      let temp = TempDir::new().unwrap();
      let mut journal = open_journal(&temp);
      journal
        .create_secret_captured("operation-1", "account-1", 9, PENDING_SECRET.into())
        .unwrap();

      let updated = journal
        .update_state("operation-1", "account-1", 1, state)
        .unwrap();

      assert_eq!(updated.state, state);
      assert_eq!(updated.journal_revision, 2);
      let reloaded = open_journal(&temp)
        .load("operation-1", "account-1")
        .unwrap()
        .unwrap();
      assert_eq!(reloaded, updated);
    }
  }

  #[test]
  fn stale_journal_update_is_rejected_without_overwriting_newer_state() {
    let temp = TempDir::new().unwrap();
    let mut journal = open_journal(&temp);
    journal
      .create_secret_captured("operation-1", "account-1", 7, PENDING_SECRET.into())
      .unwrap();
    journal
      .update_state(
        "operation-1",
        "account-1",
        1,
        TwoFactorBackfillJournalState::RemoteConfirmed,
      )
      .unwrap();

    let error = journal
      .update_state(
        "operation-1",
        "account-1",
        1,
        TwoFactorBackfillJournalState::ManualReview,
      )
      .unwrap_err();

    assert!(error.contains("revision conflict"));
    assert_eq!(
      journal
        .load("operation-1", "account-1")
        .unwrap()
        .unwrap()
        .state,
      TwoFactorBackfillJournalState::RemoteConfirmed
    );
  }

  #[test]
  fn cleanup_requires_remote_confirmation_and_final_account_patch_revision() {
    let temp = TempDir::new().unwrap();
    let mut journal = open_journal(&temp);
    let created = journal
      .create_secret_captured("operation-1", "account-1", 7, PENDING_SECRET.into())
      .unwrap();

    assert!(journal
      .delete_after_account_patch(
        "operation-1",
        "account-1",
        created.journal_revision,
        persisted_patch(8, Some("operation-1"), true, PENDING_SECRET),
      )
      .unwrap_err()
      .contains("remote confirmation"));
    let confirmed = journal
      .update_state(
        "operation-1",
        "account-1",
        created.journal_revision,
        TwoFactorBackfillJournalState::RemoteConfirmed,
      )
      .unwrap();
    assert!(journal
      .delete_after_account_patch(
        "operation-1",
        "account-1",
        confirmed.journal_revision,
        persisted_patch(8, Some("operation-1"), true, PENDING_SECRET),
      )
      .unwrap_err()
      .contains("final account revision"));
    let finalized = journal
      .record_final_account_revision("operation-1", "account-1", confirmed.journal_revision, 8)
      .unwrap();
    for (enabled, secret) in [(false, PENDING_SECRET), (true, "different-secret")] {
      assert!(journal
        .delete_after_account_patch(
          "operation-1",
          "account-1",
          finalized.journal_revision,
          persisted_patch(8, Some("operation-1"), enabled, secret),
        )
        .unwrap_err()
        .contains("account patch"));
    }
    assert!(journal.load("operation-1", "account-1").unwrap().is_some());

    assert!(journal
      .delete_after_account_patch(
        "operation-1",
        "account-1",
        finalized.journal_revision,
        persisted_patch(8, Some("operation-1"), true, PENDING_SECRET),
      )
      .unwrap());
    assert!(journal.load("operation-1", "account-1").unwrap().is_none());
  }

  #[test]
  fn cleanup_binds_exact_final_revision_journal_revision_and_operation() {
    let temp = TempDir::new().unwrap();
    let mut journal = open_journal(&temp);
    journal
      .create_secret_captured("operation-1", "account-1", 7, PENDING_SECRET.into())
      .unwrap();
    journal
      .update_state(
        "operation-1",
        "account-1",
        1,
        TwoFactorBackfillJournalState::RemoteConfirmed,
      )
      .unwrap();
    let finalized = journal
      .record_final_account_revision("operation-1", "account-1", 2, 8)
      .unwrap();

    assert!(journal
      .delete_after_account_patch(
        "operation-1",
        "account-1",
        finalized.journal_revision - 1,
        persisted_patch(8, Some("operation-1"), true, PENDING_SECRET),
      )
      .unwrap_err()
      .contains("revision conflict"));
    assert!(journal
      .delete_after_account_patch(
        "operation-1",
        "account-1",
        finalized.journal_revision,
        persisted_patch(9, Some("operation-1"), true, PENDING_SECRET),
      )
      .unwrap_err()
      .contains("final account revision"));
    assert!(journal
      .delete_after_account_patch(
        "operation-1",
        "account-1",
        finalized.journal_revision,
        persisted_patch(8, Some("operation-2"), true, PENDING_SECRET),
      )
      .unwrap_err()
      .contains("operation ownership"));
    assert!(journal.load("operation-1", "account-1").unwrap().is_some());

    assert!(journal
      .delete_after_account_patch(
        "operation-1",
        "account-1",
        finalized.journal_revision,
        persisted_patch(8, Some("operation-1"), true, PENDING_SECRET),
      )
      .unwrap());
  }

  #[test]
  fn failed_write_or_delete_preserves_recoverable_entry() {
    let temp = TempDir::new().unwrap();
    let mut journal = open_journal(&temp);
    let original = journal
      .create_secret_captured("operation-1", "account-1", 7, PENDING_SECRET.into())
      .unwrap();

    for stage in [
      AtomicJournalFailureStage::Write,
      AtomicJournalFailureStage::Replace,
    ] {
      journal.fail_next_write(stage);
      assert!(journal
        .update_state(
          "operation-1",
          "account-1",
          1,
          TwoFactorBackfillJournalState::RemoteConfirmed,
        )
        .is_err());
      assert_eq!(
        journal.load("operation-1", "account-1").unwrap().unwrap(),
        original,
        "journal changed at {stage:?}"
      );
    }

    let confirmed = journal
      .update_state(
        "operation-1",
        "account-1",
        1,
        TwoFactorBackfillJournalState::RemoteConfirmed,
      )
      .unwrap();
    let finalized = journal
      .record_final_account_revision("operation-1", "account-1", confirmed.journal_revision, 8)
      .unwrap();
    journal.fail_next_write(AtomicJournalFailureStage::Delete);
    assert!(journal
      .delete_after_account_patch(
        "operation-1",
        "account-1",
        finalized.journal_revision,
        persisted_patch(8, Some("operation-1"), true, PENDING_SECRET),
      )
      .is_err());
    assert_eq!(
      journal.load("operation-1", "account-1").unwrap().unwrap(),
      finalized
    );
  }

  #[test]
  fn post_replace_sync_failure_restores_previous_journal_entry() {
    let temp = TempDir::new().unwrap();
    let mut journal = open_journal(&temp);
    let original = journal
      .create_secret_captured("operation-1", "account-1", 7, PENDING_SECRET.into())
      .unwrap();

    journal.fail_next_write(AtomicJournalFailureStage::SyncDirectory);
    assert!(journal
      .update_state(
        "operation-1",
        "account-1",
        1,
        TwoFactorBackfillJournalState::RemoteConfirmed,
      )
      .is_err());

    assert_eq!(
      journal.load("operation-1", "account-1").unwrap().unwrap(),
      original
    );
  }

  #[test]
  fn journal_serialization_contains_secret_but_excludes_other_credentials() {
    let temp = TempDir::new().unwrap();
    let mut journal = open_journal(&temp);
    journal
      .create_secret_captured("operation-1", "account-1", 7, PENDING_SECRET.into())
      .unwrap();
    let bytes = fs::read(
      fs::read_dir(temp.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path(),
    )
    .unwrap();
    let serialized = String::from_utf8(bytes).unwrap();

    assert!(serialized.contains(PENDING_SECRET));
    for forbidden in [PASSWORD, RAW_CDK, ACCESS_TOKEN, EMAIL_OTP, GENERATED_TOTP] {
      assert!(!serialized.contains(forbidden));
    }
  }

  #[test]
  fn list_returns_all_entries_in_stable_order() {
    let temp = TempDir::new().unwrap();
    let mut journal = open_journal(&temp);
    journal
      .create_secret_captured("operation-2", "account-b", 3, "secret-b".into())
      .unwrap();
    journal
      .create_secret_captured("operation-1", "account-a", 2, "secret-a".into())
      .unwrap();

    let entries = journal.list().unwrap();

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].operation_id, "operation-1");
    assert_eq!(entries[1].operation_id, "operation-2");
  }
}
