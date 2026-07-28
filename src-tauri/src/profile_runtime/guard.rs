//! RAII guard that releases a profile lease exactly once on drop or explicit close.

use super::lease::{LeaseRegistry, RuntimeLease};

/// Releases the held lease when dropped. Safe to `close()` early (idempotent).
#[derive(Debug)]
pub struct LeaseCleanupGuard {
  registry: &'static LeaseRegistry,
  lease_id: Option<String>,
  source_profile_id: String,
  runtime_key: String,
  process_id: Option<u32>,
}

impl LeaseCleanupGuard {
  /// Wrap an acquired lease so Drop always releases it.
  pub fn new(lease: RuntimeLease) -> Self {
    Self::with_registry(LeaseRegistry::global(), lease)
  }

  /// Test/helper constructor with an explicit registry reference.
  ///
  /// The registry must outlive the guard. Production code uses [`Self::new`].
  pub fn with_registry(registry: &'static LeaseRegistry, lease: RuntimeLease) -> Self {
    Self {
      registry,
      lease_id: Some(lease.lease_id),
      source_profile_id: lease.source_profile_id,
      runtime_key: lease.runtime_key,
      process_id: lease.process_id,
    }
  }

  pub fn lease_id(&self) -> Option<&str> {
    self.lease_id.as_deref()
  }

  pub fn source_profile_id(&self) -> &str {
    &self.source_profile_id
  }

  pub fn runtime_key(&self) -> &str {
    &self.runtime_key
  }

  pub fn process_id(&self) -> Option<u32> {
    self.process_id
  }

  pub fn set_process_id(&mut self, process_id: Option<u32>) {
    self.process_id = process_id;
    if let Some(lease_id) = self.lease_id.as_deref() {
      let _ = self.registry.set_process_id(lease_id, process_id);
    }
  }

  /// Explicitly release the lease before Drop. Idempotent.
  pub fn close(&mut self) {
    if let Some(lease_id) = self.lease_id.take() {
      self.registry.release(&lease_id);
    }
  }

  /// Take the remaining lease id without releasing (for transfer). Prefer `close`.
  pub fn into_lease_id(mut self) -> Option<String> {
    self.lease_id.take()
  }
}

impl Drop for LeaseCleanupGuard {
  fn drop(&mut self) {
    self.close();
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::profile_runtime::lease::LeaseError;
  use serial_test::serial;
  use std::sync::OnceLock;

  fn test_registry() -> &'static LeaseRegistry {
    static REGISTRY: OnceLock<LeaseRegistry> = OnceLock::new();
    let registry = REGISTRY.get_or_init(LeaseRegistry::new);
    // Isolate tests that share this static: release everything by re-acquiring fresh ids
    // via a dedicated source namespace per test instead of clear (keeps production API clean).
    registry
  }

  fn unique_source(label: &str) -> String {
    format!("guard-test-{label}-{}", uuid::Uuid::new_v4())
  }

  #[test]
  #[serial]
  fn drop_releases_lease() {
    let registry = test_registry();
    let source = unique_source("drop");
    let lease = registry.try_acquire(&source).expect("acquire");
    {
      let _guard = LeaseCleanupGuard::with_registry(registry, lease);
      assert!(registry.is_leased(&source));
    }
    assert!(!registry.is_leased(&source));
    let again = registry.try_acquire(&source).expect("reacquire after drop");
    registry.release(&again.lease_id);
  }

  #[test]
  #[serial]
  fn close_then_drop_is_idempotent() {
    let registry = test_registry();
    let source = unique_source("close");
    let lease = registry.try_acquire(&source).expect("acquire");
    let mut guard = LeaseCleanupGuard::with_registry(registry, lease);
    guard.close();
    guard.close();
    assert!(!registry.is_leased(&source));
    drop(guard);
    assert!(!registry.is_leased(&source));
    let again = registry.try_acquire(&source).expect("reacquire");
    registry.release(&again.lease_id);
  }

  #[test]
  #[serial]
  fn busy_still_holds_while_guard_alive() {
    let registry = test_registry();
    let source = unique_source("busy");
    let lease = registry.try_acquire(&source).expect("acquire");
    let guard = LeaseCleanupGuard::with_registry(registry, lease);
    match registry.try_acquire(&source) {
      Err(LeaseError::Busy { .. }) => {}
      other => panic!("expected Busy while guard alive, got {other:?}"),
    }
    drop(guard);
    let again = registry.try_acquire(&source).expect("after drop");
    registry.release(&again.lease_id);
  }
}
