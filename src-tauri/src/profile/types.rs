use crate::camoufox_manager::CamoufoxConfig;
use crate::chromium_manager::ChromiumConfig;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum SyncStatus {
  #[default]
  Disabled,
  Syncing,
  Synced,
  Error,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
  #[default]
  Disabled,
  Regular,
  Encrypted,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BrowserProfile {
  pub id: uuid::Uuid,
  pub name: String,
  #[serde(
    serialize_with = "serialize_browser_name",
    deserialize_with = "deserialize_browser_name"
  )]
  pub browser: String,
  pub version: String,
  #[serde(default)]
  pub proxy_id: Option<String>, // Reference to stored proxy
  #[serde(default)]
  pub vpn_id: Option<String>, // Reference to stored VPN config
  #[serde(default)]
  pub launch_hook: Option<String>,
  #[serde(default)]
  pub process_id: Option<u32>,
  #[serde(default)]
  pub last_launch: Option<u64>,
  #[serde(default = "default_release_type")]
  pub release_type: String, // "stable" or "nightly"
  #[serde(default)]
  pub camoufox_config: Option<CamoufoxConfig>, // Camoufox configuration
  #[serde(default)]
  pub chromium_config: Option<ChromiumConfig>, // Chromium configuration
  #[serde(default)]
  pub group_id: Option<String>, // Reference to profile group
  #[serde(default)]
  pub tags: Vec<String>, // Free-form tags
  #[serde(default)]
  pub note: Option<String>, // User note
  #[serde(default)]
  pub sync_mode: SyncMode,
  #[serde(default)]
  pub encryption_salt: Option<String>,
  #[serde(default)]
  pub last_sync: Option<u64>, // Timestamp of last successful sync (epoch seconds)
  #[serde(default)]
  pub host_os: Option<String>, // OS where profile was created ("macos", "windows", "linux")
  #[serde(default)]
  pub ephemeral: bool,
  #[serde(default)]
  pub extension_group_id: Option<String>,
  #[serde(default)]
  pub proxy_bypass_rules: Vec<String>,
  #[serde(default)]
  pub created_by_id: Option<String>,
  #[serde(default)]
  pub created_by_email: Option<String>,
  #[serde(default)]
  pub dns_blocklist: Option<String>,
}

impl Default for BrowserProfile {
  fn default() -> Self {
    Self {
      id: uuid::Uuid::nil(),
      name: String::new(),
      browser: "chromium".to_string(),
      version: String::new(),
      proxy_id: None,
      vpn_id: None,
      launch_hook: None,
      process_id: None,
      last_launch: None,
      release_type: default_release_type(),
      camoufox_config: None,
      chromium_config: None,
      group_id: None,
      tags: Vec::new(),
      note: None,
      sync_mode: SyncMode::Disabled,
      encryption_salt: None,
      last_sync: None,
      host_os: None,
      ephemeral: false,
      extension_group_id: None,
      proxy_bypass_rules: Vec::new(),
      created_by_id: None,
      created_by_email: None,
      dns_blocklist: None,
    }
  }
}

pub fn default_release_type() -> String {
  "stable".to_string()
}

pub fn get_host_os() -> String {
  if cfg!(target_os = "macos") {
    "macos".to_string()
  } else if cfg!(target_os = "windows") {
    "windows".to_string()
  } else {
    "linux".to_string()
  }
}

fn serialize_browser_name<S>(browser: &str, serializer: S) -> Result<S::Ok, S::Error>
where
  S: Serializer,
{
  serializer.serialize_str(crate::browser::canonical_browser_name(browser))
}

fn deserialize_browser_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
  D: Deserializer<'de>,
{
  let browser = String::deserialize(deserializer)?;
  let browser = crate::browser::canonical_browser_name(&browser);
  match browser {
    "chromium" | "camoufox" => Ok(browser.to_string()),
    _ => Err(serde::de::Error::custom(format!(
      "Unsupported browser in profile metadata: {browser}"
    ))),
  }
}

pub fn chromium_profile_data_dir_name(use_fingerprint_runtime: bool) -> &'static str {
  if use_fingerprint_runtime {
    "fingerprint-chromium-profile"
  } else {
    "profile"
  }
}

impl BrowserProfile {
  pub fn get_profile_data_path_for_runtime(
    &self,
    profiles_dir: &Path,
    use_fingerprint_runtime: bool,
  ) -> PathBuf {
    let data_dir_name = if crate::browser::is_chromium_browser_name(&self.browser) {
      chromium_profile_data_dir_name(use_fingerprint_runtime)
    } else {
      "profile"
    };
    profiles_dir.join(self.id.to_string()).join(data_dir_name)
  }

  /// Get the path to the profile data directory.
  ///
  /// Legacy Chromium-compatible profiles now run on fingerprint-chromium 142. Keep them
  /// in an engine-specific directory so an older Chromium engine never opens a
  /// profile previously created by a newer Chromium-compatible build.
  pub fn get_profile_data_path(&self, profiles_dir: &Path) -> PathBuf {
    self.get_profile_data_path_for_runtime(
      profiles_dir,
      crate::browser::use_fingerprint_chromium_runtime(),
    )
  }

  /// Resolve the OS this profile was created on. Checks `host_os` first,
  /// then falls back to the fingerprint config's `os` field (for profiles
  /// created before `host_os` was introduced or synced without it).
  pub fn resolved_os(&self) -> Option<&str> {
    self
      .host_os
      .as_deref()
      .or_else(|| self.camoufox_config.as_ref().and_then(|c| c.os.as_deref()))
      .or_else(|| self.chromium_config.as_ref().and_then(|c| c.os.as_deref()))
  }

  /// Returns true when the profile was created on a different OS than the current host.
  /// Checks `host_os` first, then falls back to the browser config's `os` field.
  pub fn is_cross_os(&self) -> bool {
    match self.resolved_os() {
      Some(os) => os != get_host_os(),
      None => false,
    }
  }

  /// Returns true if sync is enabled (either Regular or Encrypted mode).
  pub fn is_sync_enabled(&self) -> bool {
    self.sync_mode != SyncMode::Disabled
  }

  /// Returns true if sync uses E2E encryption.
  pub fn is_encrypted_sync(&self) -> bool {
    self.sync_mode == SyncMode::Encrypted
  }
}

#[cfg(test)]
mod tests {
  use super::{chromium_profile_data_dir_name, BrowserProfile, SyncMode};
  use crate::chromium_manager::ChromiumConfig;
  use std::path::PathBuf;

  fn sample_legacy_chromium_profile() -> BrowserProfile {
    BrowserProfile {
      id: uuid::Uuid::parse_str("12345678-1234-1234-1234-123456789abc").unwrap(),
      name: "test".to_string(),
      browser: "chromium".to_string(),
      version: "1.0.0".to_string(),
      proxy_id: None,
      vpn_id: None,
      launch_hook: None,
      process_id: None,
      last_launch: None,
      release_type: "stable".to_string(),
      camoufox_config: None,
      chromium_config: None,
      group_id: None,
      tags: Vec::new(),
      note: None,
      sync_mode: SyncMode::Disabled,
      encryption_salt: None,
      last_sync: None,
      host_os: None,
      ephemeral: false,
      extension_group_id: None,
      proxy_bypass_rules: Vec::new(),
      created_by_id: None,
      created_by_email: None,
      dns_blocklist: None,
    }
  }

  #[test]
  fn test_legacy_chromium_profile_data_dir_name_switches_with_runtime() {
    assert_eq!(
      chromium_profile_data_dir_name(true),
      "fingerprint-chromium-profile"
    );
    assert_eq!(chromium_profile_data_dir_name(false), "profile");
  }

  #[test]
  fn test_get_profile_data_path_for_runtime_is_isolated_between_engines() {
    let profiles_dir = PathBuf::from("/fake/profiles");
    let profile = sample_legacy_chromium_profile();

    let fingerprint_path = profile.get_profile_data_path_for_runtime(&profiles_dir, true);
    let legacy_path = profile.get_profile_data_path_for_runtime(&profiles_dir, false);

    assert_eq!(
      fingerprint_path,
      profiles_dir
        .join("12345678-1234-1234-1234-123456789abc")
        .join("fingerprint-chromium-profile")
    );
    assert_eq!(
      legacy_path,
      profiles_dir
        .join("12345678-1234-1234-1234-123456789abc")
        .join("profile")
    );
    assert_ne!(fingerprint_path, legacy_path);
  }

  #[test]
  fn test_chromium_browser_uses_legacy_compatible_profile_dir_layout() {
    let profiles_dir = PathBuf::from("/fake/profiles");
    let mut profile = sample_legacy_chromium_profile();
    profile.browser = "chromium".to_string();

    let fingerprint_path = profile.get_profile_data_path_for_runtime(&profiles_dir, true);

    assert_eq!(
      fingerprint_path,
      profiles_dir
        .join("12345678-1234-1234-1234-123456789abc")
        .join("fingerprint-chromium-profile")
    );
  }

  #[test]
  fn test_browser_profile_serializes_legacy_config_as_chromium_config() {
    let mut profile = sample_legacy_chromium_profile();
    profile.chromium_config = Some(ChromiumConfig {
      os: Some("windows".to_string()),
      ..ChromiumConfig::default()
    });

    let serialized = serde_json::to_value(&profile).expect("profile should serialize");

    assert!(serialized.get("wayfern_config").is_none());
    assert_eq!(serialized["browser"], "chromium");
    assert_eq!(serialized["chromium_config"]["os"], "windows");
  }

  #[test]
  fn test_browser_profile_rejects_legacy_wayfern_shape() {
    let result = serde_json::from_value::<BrowserProfile>(serde_json::json!({
      "id": "12345678-1234-1234-1234-123456789abc",
      "name": "legacy",
      "browser": "wayfern",
      "version": "1.0.0",
      "release_type": "stable",
      "wayfern_config": {
        "os": "linux"
      }
    }));

    assert!(result.is_err(), "legacy wayfern profile should be rejected");
  }
}
