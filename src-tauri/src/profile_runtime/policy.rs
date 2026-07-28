//! Launch policy types for automation runtime reuse of source profiles.
//!
//! Policy is task-local: it must not be written into source `BrowserProfile` metadata.

use serde::{Deserialize, Serialize};

/// Whether the launch uses disposable runtime data or the source profile's disk data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DataMode {
  /// Task-scoped disposable data directory (default for automation isolation).
  #[default]
  Ephemeral,
  /// Explicit opt-in to use the source profile's persistent data directory.
  Persistent,
}

/// Whether fingerprint configuration is preserved or regenerated per launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum FingerprintMode {
  /// Generate a fresh general fingerprint for this launch only (default).
  #[default]
  RandomPerLaunch,
  /// Preserve the source profile's stored fingerprint configuration.
  Stable,
}

/// Task-local launch policy layered over an immutable source profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LaunchPolicy {
  /// Selected existing profile id, or `None` for generated-worker fallback.
  pub source_profile_id: Option<String>,
  pub data_mode: DataMode,
  pub fingerprint_mode: FingerprintMode,
  /// Optional task-local proxy override.
  pub proxy_id: Option<String>,
  /// Optional task-local VPN override.
  pub vpn_id: Option<String>,
  /// When true, force no proxy/VPN for this launch.
  pub clear_network: bool,
  /// When true, cleanup may delete the generated worker profile.
  pub owns_generated_worker: bool,
  /// When false, do not write process/fingerprint fields to source metadata.
  pub persist_process_to_source: bool,
}

impl Default for LaunchPolicy {
  fn default() -> Self {
    Self {
      source_profile_id: None,
      data_mode: DataMode::Ephemeral,
      fingerprint_mode: FingerprintMode::RandomPerLaunch,
      proxy_id: None,
      vpn_id: None,
      clear_network: false,
      owns_generated_worker: true,
      persist_process_to_source: false,
    }
  }
}

impl LaunchPolicy {
  /// Policy for adopting an existing source profile without mutating its metadata.
  pub fn for_source_profile(source_profile_id: impl Into<String>) -> Self {
    Self {
      source_profile_id: Some(source_profile_id.into()),
      owns_generated_worker: false,
      persist_process_to_source: false,
      ..Self::default()
    }
  }

  /// Policy for a workflow-owned generated worker (existing isolation default).
  pub fn for_generated_worker() -> Self {
    Self {
      source_profile_id: None,
      owns_generated_worker: true,
      // Generated workers still use the normal process-tracking path today.
      persist_process_to_source: true,
      ..Self::default()
    }
  }

  /// Whether launch metadata (process id / fingerprint) may be written to source disk.
  pub fn should_persist_launch_metadata(&self) -> bool {
    self.persist_process_to_source
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn defaults_are_ephemeral_random_and_non_persisting() {
    let policy = LaunchPolicy::default();
    assert_eq!(policy.data_mode, DataMode::Ephemeral);
    assert_eq!(policy.fingerprint_mode, FingerprintMode::RandomPerLaunch);
    assert!(!policy.should_persist_launch_metadata());
    assert!(policy.owns_generated_worker);
    assert!(policy.source_profile_id.is_none());
  }

  #[test]
  fn source_profile_factory_disables_ownership_and_metadata_writes() {
    let policy = LaunchPolicy::for_source_profile("profile-a");
    assert_eq!(policy.source_profile_id.as_deref(), Some("profile-a"));
    assert!(!policy.owns_generated_worker);
    assert!(!policy.should_persist_launch_metadata());
  }

  #[test]
  fn serde_roundtrip_uses_camel_case() {
    let policy = LaunchPolicy {
      source_profile_id: Some("abc".into()),
      data_mode: DataMode::Persistent,
      fingerprint_mode: FingerprintMode::Stable,
      proxy_id: Some("proxy-1".into()),
      vpn_id: None,
      clear_network: true,
      owns_generated_worker: false,
      persist_process_to_source: false,
    };
    let json = serde_json::to_string(&policy).expect("serialize");
    assert!(json.contains("\"sourceProfileId\""));
    assert!(json.contains("\"dataMode\""));
    assert!(json.contains("\"fingerprintMode\""));
    assert!(json.contains("\"clearNetwork\""));
    let back: LaunchPolicy = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, policy);
  }
}
