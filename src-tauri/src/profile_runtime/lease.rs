//! Process-local exclusive lease over a selected source profile for one automation lifecycle.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use thiserror::Error;
use uuid::Uuid;

/// Resources owned by one runtime launch and retained until cleanup fully succeeds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeCleanupState {
  pub process_id: Option<u32>,
  pub browser_instance_id: Option<String>,
  pub owned_vpn_worker_id: Option<String>,
  pub ephemeral_runtime_key: Option<String>,
}

impl RuntimeCleanupState {
  pub fn is_empty(&self) -> bool {
    self.process_id.is_none()
      && self.browser_instance_id.is_none()
      && self.owned_vpn_worker_id.is_none()
      && self.ephemeral_runtime_key.is_none()
  }
}

/// Active runtime identity for one leased automation operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLease {
  pub lease_id: String,
  pub source_profile_id: String,
  /// Unique key for ephemeral directory maps (not bare source profile id).
  pub runtime_key: String,
  pub process_id: Option<u32>,
  pub browser_instance_id: Option<String>,
  pub owned_vpn_worker_id: Option<String>,
  pub ephemeral_runtime_key: Option<String>,
}

impl RuntimeLease {
  pub fn with_process_id(mut self, process_id: Option<u32>) -> Self {
    self.process_id = process_id;
    self
  }

  pub fn set_process_id(&mut self, process_id: Option<u32>) {
    self.process_id = process_id;
  }

  pub fn runtime_cleanup_state(&self) -> RuntimeCleanupState {
    RuntimeCleanupState {
      process_id: self.process_id,
      browser_instance_id: self.browser_instance_id.clone(),
      owned_vpn_worker_id: self.owned_vpn_worker_id.clone(),
      ephemeral_runtime_key: self.ephemeral_runtime_key.clone(),
    }
  }

  pub fn set_runtime_cleanup_state(&mut self, state: RuntimeCleanupState) {
    self.process_id = state.process_id;
    self.browser_instance_id = state.browser_instance_id;
    self.owned_vpn_worker_id = state.owned_vpn_worker_id;
    self.ephemeral_runtime_key = state.ephemeral_runtime_key;
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LeaseError {
  #[error("profile lease busy: source profile `{source_profile_id}` is already leased by `{holder_lease_id}`")]
  Busy {
    source_profile_id: String,
    holder_lease_id: String,
  },
  #[error("invalid source profile id")]
  InvalidSourceProfileId,
}

#[derive(Debug, Clone)]
struct LeaseEntry {
  lease_id: String,
  runtime_key: String,
  cleanup_state: RuntimeCleanupState,
}

/// Process-local registry of exclusive source-profile leases.
#[derive(Debug, Default)]
pub struct LeaseRegistry {
  /// source_profile_id -> active lease
  by_source: Mutex<HashMap<String, LeaseEntry>>,
  /// lease_id -> source_profile_id (for release by lease id)
  by_lease: Mutex<HashMap<String, String>>,
}

impl LeaseRegistry {
  pub fn new() -> Self {
    Self::default()
  }

  /// Global process-local registry for automation workflows.
  pub fn global() -> &'static LeaseRegistry {
    static REGISTRY: OnceLock<LeaseRegistry> = OnceLock::new();
    REGISTRY.get_or_init(LeaseRegistry::new)
  }

  /// Acquire an exclusive lease for `source_profile_id`.
  ///
  /// Concurrent acquire on the same source fails immediately with [`LeaseError::Busy`].
  pub fn try_acquire(&self, source_profile_id: &str) -> Result<RuntimeLease, LeaseError> {
    let source = source_profile_id.trim();
    if source.is_empty() {
      return Err(LeaseError::InvalidSourceProfileId);
    }

    let lease_id = Uuid::new_v4().to_string();
    let runtime_key = format!("{source}__{lease_id}");

    let mut by_source = self
      .by_source
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = by_source.get(source) {
      return Err(LeaseError::Busy {
        source_profile_id: source.to_string(),
        holder_lease_id: existing.lease_id.clone(),
      });
    }

    by_source.insert(
      source.to_string(),
      LeaseEntry {
        lease_id: lease_id.clone(),
        runtime_key: runtime_key.clone(),
        cleanup_state: RuntimeCleanupState::default(),
      },
    );
    drop(by_source);

    let mut by_lease = self
      .by_lease
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    by_lease.insert(lease_id.clone(), source.to_string());

    Ok(RuntimeLease {
      lease_id,
      source_profile_id: source.to_string(),
      runtime_key,
      process_id: None,
      browser_instance_id: None,
      owned_vpn_worker_id: None,
      ephemeral_runtime_key: None,
    })
  }

  /// Release a lease by id. Missing or already-released ids are no-ops (idempotent).
  pub fn release(&self, lease_id: &str) {
    if lease_id.trim().is_empty() {
      return;
    }

    let source = {
      let mut by_lease = self
        .by_lease
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      by_lease.remove(lease_id)
    };

    let Some(source) = source else {
      return;
    };

    let mut by_source = self
      .by_source
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(entry) = by_source.get(&source) {
      if entry.lease_id == lease_id {
        by_source.remove(&source);
      }
    }
  }

  /// Replace all retryable cleanup identity for an active lease.
  pub fn set_runtime_cleanup_state(&self, lease_id: &str, state: RuntimeCleanupState) -> bool {
    let source = {
      let by_lease = self
        .by_lease
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      by_lease.get(lease_id).cloned()
    };
    let Some(source) = source else {
      return false;
    };

    let mut by_source = self
      .by_source
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(entry) = by_source.get_mut(&source) {
      if entry.lease_id == lease_id {
        entry.cleanup_state = state;
        return true;
      }
    }
    false
  }

  /// Snapshot retryable cleanup identity for an active lease.
  pub fn runtime_cleanup_state(&self, lease_id: &str) -> Option<RuntimeCleanupState> {
    let source = {
      let by_lease = self
        .by_lease
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      by_lease.get(lease_id).cloned()
    }?;
    let by_source = self
      .by_source
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    by_source
      .get(&source)
      .filter(|entry| entry.lease_id == lease_id)
      .map(|entry| entry.cleanup_state.clone())
  }

  /// Update the tracked process id for an active lease (in-memory only).
  pub fn set_process_id(&self, lease_id: &str, process_id: Option<u32>) -> bool {
    let Some(mut state) = self.runtime_cleanup_state(lease_id) else {
      return false;
    };
    state.process_id = process_id;
    self.set_runtime_cleanup_state(lease_id, state)
  }

  /// Whether `source_profile_id` currently holds an active lease.
  pub fn is_leased(&self, source_profile_id: &str) -> bool {
    let by_source = self
      .by_source
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    by_source.contains_key(source_profile_id.trim())
  }

  /// Active holder lease id for a source profile, if any.
  pub fn holder_lease_id(&self, source_profile_id: &str) -> Option<String> {
    let by_source = self
      .by_source
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    by_source
      .get(source_profile_id.trim())
      .map(|entry| entry.lease_id.clone())
  }

  /// Runtime key for the active lease on `source_profile_id`, if any.
  pub fn runtime_key_for_source(&self, source_profile_id: &str) -> Option<String> {
    let by_source = self
      .by_source
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    by_source
      .get(source_profile_id.trim())
      .map(|entry| entry.runtime_key.clone())
  }

  /// Process id tracked on the active lease for `lease_id`, if any.
  pub fn process_id_for_lease(&self, lease_id: &str) -> Option<u32> {
    self
      .runtime_cleanup_state(lease_id)
      .and_then(|state| state.process_id)
  }

  #[cfg(test)]
  fn clear_for_tests(&self) {
    self
      .by_source
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clear();
    self
      .by_lease
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clear();
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serial_test::serial;

  fn fresh_registry() -> LeaseRegistry {
    LeaseRegistry::new()
  }

  #[test]
  #[serial]
  fn acquire_same_source_twice_returns_busy() {
    let registry = fresh_registry();
    let first = registry
      .try_acquire("profile-a")
      .expect("first acquire should succeed");
    let second = registry.try_acquire("profile-a");
    match second {
      Err(LeaseError::Busy {
        source_profile_id,
        holder_lease_id,
      }) => {
        assert_eq!(source_profile_id, "profile-a");
        assert_eq!(holder_lease_id, first.lease_id);
      }
      other => panic!("expected Busy, got {other:?}"),
    }
    assert!(registry.is_leased("profile-a"));
    registry.release(&first.lease_id);
  }

  #[test]
  #[serial]
  fn release_then_reacquire_succeeds() {
    let registry = fresh_registry();
    let first = registry.try_acquire("profile-b").expect("acquire");
    registry.release(&first.lease_id);
    assert!(!registry.is_leased("profile-b"));
    let second = registry
      .try_acquire("profile-b")
      .expect("reacquire after release");
    assert_ne!(first.lease_id, second.lease_id);
    assert_ne!(first.runtime_key, second.runtime_key);
    assert!(second.runtime_key.starts_with("profile-b__"));
    registry.release(&second.lease_id);
  }

  #[test]
  #[serial]
  fn double_release_is_idempotent() {
    let registry = fresh_registry();
    let lease = registry.try_acquire("profile-c").expect("acquire");
    registry.release(&lease.lease_id);
    registry.release(&lease.lease_id);
    registry.release("missing-lease-id");
    assert!(!registry.is_leased("profile-c"));
    let again = registry.try_acquire("profile-c").expect("reacquire");
    registry.release(&again.lease_id);
  }

  #[test]
  #[serial]
  fn different_sources_can_lease_concurrently() {
    let registry = fresh_registry();
    let a = registry.try_acquire("src-a").expect("a");
    let b = registry.try_acquire("src-b").expect("b");
    assert!(registry.is_leased("src-a"));
    assert!(registry.is_leased("src-b"));
    registry.release(&a.lease_id);
    registry.release(&b.lease_id);
  }

  #[test]
  #[serial]
  fn empty_source_id_is_rejected() {
    let registry = fresh_registry();
    assert!(matches!(
      registry.try_acquire("  "),
      Err(LeaseError::InvalidSourceProfileId)
    ));
  }

  #[test]
  #[serial]
  fn set_process_id_updates_active_lease() {
    let registry = fresh_registry();
    let lease = registry.try_acquire("profile-pid").expect("acquire");
    assert!(registry.set_process_id(&lease.lease_id, Some(4242)));
    assert_eq!(registry.process_id_for_lease(&lease.lease_id), Some(4242));
    assert_eq!(
      registry.runtime_key_for_source("profile-pid").as_deref(),
      Some(lease.runtime_key.as_str())
    );
    assert!(!registry.set_process_id("missing", Some(1)));
    registry.release(&lease.lease_id);
  }

  #[test]
  fn runtime_cleanup_state_is_retained_until_explicitly_cleared() {
    let registry = fresh_registry();
    let mut lease = registry.try_acquire("profile-cleanup").expect("acquire");
    let state = RuntimeCleanupState {
      process_id: Some(6161),
      browser_instance_id: Some("kernel-6161".to_string()),
      owned_vpn_worker_id: Some("vpn-owned-6161".to_string()),
      ephemeral_runtime_key: Some(lease.runtime_key.clone()),
    };

    assert!(registry.set_runtime_cleanup_state(&lease.lease_id, state.clone()));
    lease.set_runtime_cleanup_state(state.clone());
    assert_eq!(
      registry.runtime_cleanup_state(&lease.lease_id),
      Some(state.clone())
    );
    assert_eq!(lease.runtime_cleanup_state(), state);

    let cleared = RuntimeCleanupState::default();
    assert!(registry.set_runtime_cleanup_state(&lease.lease_id, cleared.clone()));
    lease.set_runtime_cleanup_state(cleared.clone());
    assert_eq!(
      registry.runtime_cleanup_state(&lease.lease_id),
      Some(cleared)
    );
    assert!(lease.runtime_cleanup_state().is_empty());
    registry.release(&lease.lease_id);
  }

  #[test]
  #[serial]
  fn global_registry_survives_clear_isolation_via_local_only_in_unit_tests() {
    // Ensure global() constructs without panic; unit tests use local registries.
    let _ = LeaseRegistry::global();
    LeaseRegistry::global().clear_for_tests();
  }
}
