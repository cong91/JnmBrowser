use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app_dirs::data_dir;
use crate::auto_service::openai::register::store::{persist_temp_file, sync_parent_directory};
use crate::email::EmailProvider;

use super::commands::{BackfillBrowser, BackfillMode, BackfillNetworkConfig};

/// Hardcoded canary gate versioning. Bump when the orchestrator state machine
/// or selector strings change; bulk mode is then rejected until a fresh canary
/// produces matching evidence.
pub(crate) const TWO_FACTOR_BACKFILL_WORKFLOW_VERSION: &str = "1";
pub(crate) const TWO_FACTOR_BACKFILL_SELECTOR_VERSION: &str = "1";

/// Structured reason a start request was rejected by the canary gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CanaryRejection {
  InvalidSelection(String),
  CanaryMustSelectExactlyOneAccount,
  ModeRequiresSelection,
  BulkRequiresCanaryEvidence {
    missing_provider: EmailProvider,
    missing_browser: String,
    missing_network_config_hash: String,
  },
}

/// Durable, non-secret proof that a canary workflow run once succeeded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CanaryEvidence {
  pub workflow_version: String,
  pub selector_version: String,
  pub browser: String,
  pub email_provider: EmailProvider,
  pub network_config_hash: String,
  /// SHA-256 of the stable account key (account id, or email fallback). Never
  /// the secret — only enough to prove a specific account executed the gate.
  pub account_key_hash: String,
  pub recorded_at: DateTime<Utc>,
}

impl CanaryEvidence {
  fn key_fields_match(
    &self,
    workflow_version: &str,
    selector_version: &str,
    browser: &str,
    provider: EmailProvider,
    network_config_hash: &str,
  ) -> bool {
    self.workflow_version == workflow_version
      && self.selector_version == selector_version
      && self.browser == browser
      && self.email_provider == provider
      && self.network_config_hash == network_config_hash
  }
}

/// Backend-only canary gate. Canary mode opens after one durable success; bulk
/// mode is rejected until matching evidence exists per browser/provider/network
/// combination present in the selection.
pub(crate) struct CanaryGate {
  base_dir: PathBuf,
}

impl CanaryGate {
  pub(crate) fn new() -> Result<Self, String> {
    Self::with_base_dir(
      data_dir()
        .join("two_factor_backfill")
        .join("canary_evidence"),
    )
  }

  pub(crate) fn with_base_dir(base_dir: impl AsRef<Path>) -> Result<Self, String> {
    let base_dir = base_dir.as_ref().to_path_buf();
    fs::create_dir_all(&base_dir).map_err(|error| {
      format!(
        "Failed to create 2FA backfill canary evidence directory {}: {error}",
        base_dir.display()
      )
    })?;
    Ok(Self { base_dir })
  }

  /// Validate the request before scheduling a workflow. Returns the network
  /// config hash so the caller can pass it straight to `record_canary_success`
  /// once the canary account reaches a durable success.
  pub(crate) fn validate(
    &self,
    mode: BackfillMode,
    account_keys: &[String],
    browser: &BackfillBrowser,
    providers: &[EmailProvider],
    network: &BackfillNetworkConfig,
  ) -> Result<String, CanaryRejection> {
    let selected: Vec<&str> = account_keys
      .iter()
      .map(|key| key.trim())
      .filter(|key| !key.is_empty())
      .collect();
    if selected.is_empty() {
      return Err(CanaryRejection::ModeRequiresSelection);
    }
    if providers.is_empty() {
      return Err(CanaryRejection::InvalidSelection(
        "no email provider resolved for the selection".into(),
      ));
    }
    let browser_str = match browser {
      BackfillBrowser::Chromium => "chromium",
      BackfillBrowser::Camoufox => "camoufox",
    }
    .to_string();
    let network_hash = network_config_hash(network);

    match mode {
      BackfillMode::Canary => {
        if selected.len() != 1 {
          return Err(CanaryRejection::CanaryMustSelectExactlyOneAccount);
        }
        // Canary is always accepted (the gate exists to authorize canary itself);
        // the persisted evidence is what unlocks future bulk requests for this
        // browser/provider/network combination.
        Ok(network_hash)
      }
      BackfillMode::Bulk => {
        for provider in providers {
          if !self.has_matching_evidence(&browser_str, *provider, &network_hash) {
            return Err(CanaryRejection::BulkRequiresCanaryEvidence {
              missing_provider: *provider,
              missing_browser: browser_str.clone(),
              missing_network_config_hash: network_hash.clone(),
            });
          }
        }
        Ok(network_hash)
      }
    }
  }

  /// Report whether matching canary evidence unlocks Bulk for this exact
  /// browser/provider/network combination. The response contains no evidence
  /// details or account material; the start path still revalidates the gate.
  pub(crate) fn bulk_available(
    &self,
    account_keys: &[String],
    browser: &BackfillBrowser,
    providers: &[EmailProvider],
    network: &BackfillNetworkConfig,
  ) -> bool {
    self
      .validate(
        BackfillMode::Bulk,
        account_keys,
        browser,
        providers,
        network,
      )
      .is_ok()
  }

  /// Record a durable canary success. Stored evidence contains only the hashed
  /// account key, never the TOTP secret or any credential material.
  pub(crate) fn record_canary_success(
    &self,
    browser: &BackfillBrowser,
    provider: EmailProvider,
    network: &BackfillNetworkConfig,
    account_key: &str,
  ) -> Result<(), String> {
    let account_key = account_key.trim();
    if account_key.is_empty() {
      return Err("canary evidence requires a non-empty account key".into());
    }
    let browser_str = match browser {
      BackfillBrowser::Chromium => "chromium",
      BackfillBrowser::Camoufox => "camoufox",
    };
    let network_hash = network_config_hash(network);
    let evidence = CanaryEvidence {
      workflow_version: TWO_FACTOR_BACKFILL_WORKFLOW_VERSION.into(),
      selector_version: TWO_FACTOR_BACKFILL_SELECTOR_VERSION.into(),
      browser: browser_str.into(),
      email_provider: provider,
      network_config_hash: network_hash.clone(),
      account_key_hash: sha256_hex(account_key),
      recorded_at: Utc::now(),
    };
    let json = serde_json::to_vec_pretty(&evidence)
      .map_err(|error| format!("Failed to serialize canary evidence: {error}"))?;
    let path = self.evidence_path(&evidence.browser, provider, &network_hash);

    let mut temp_file = tempfile::NamedTempFile::new_in(&self.base_dir)
      .map_err(|error| format!("Failed to create canary evidence temp file: {error}"))?;
    temp_file
      .write_all(&json)
      .map_err(|error| format!("Failed to write canary evidence temp file: {error}"))?;
    temp_file
      .as_file()
      .sync_all()
      .map_err(|error| format!("Failed to sync canary evidence temp file: {error}"))?;
    persist_temp_file(temp_file, &path, "canary evidence")?;
    sync_parent_directory(&self.base_dir, "canary evidence directory")?;
    Ok(())
  }

  fn has_matching_evidence(
    &self,
    browser: &str,
    provider: EmailProvider,
    network_config_hash: &str,
  ) -> bool {
    let path = self.evidence_path(browser, provider, network_config_hash);
    let Ok(bytes) = fs::read(&path) else {
      return false;
    };
    let Ok(evidence) = serde_json::from_slice::<CanaryEvidence>(&bytes) else {
      return false;
    };
    evidence.key_fields_match(
      TWO_FACTOR_BACKFILL_WORKFLOW_VERSION,
      TWO_FACTOR_BACKFILL_SELECTOR_VERSION,
      browser,
      provider,
      network_config_hash,
    )
  }

  fn evidence_path(&self, browser: &str, provider: EmailProvider, network_hash: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(TWO_FACTOR_BACKFILL_WORKFLOW_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(TWO_FACTOR_BACKFILL_SELECTOR_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(browser.as_bytes());
    hasher.update([0]);
    hasher.update(provider.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(network_hash.as_bytes());
    let digest = hasher.finalize();
    let mut name = String::with_capacity(65);
    for byte in digest {
      use std::fmt::Write;
      write!(&mut name, "{byte:02x}").expect("writing SHA-256 digest to String cannot fail");
    }
    name.push_str(".json");
    self.base_dir.join(name)
  }
}

pub(crate) fn network_config_hash(network: &BackfillNetworkConfig) -> String {
  match network {
    BackfillNetworkConfig::None => sha256_hex("none"),
    BackfillNetworkConfig::Proxy { proxy_id } => sha256_hex(&format!("proxy:{}", proxy_id.trim())),
    BackfillNetworkConfig::Vpn { vpn_id } => sha256_hex(&format!("vpn:{}", vpn_id.trim())),
  }
}

fn sha256_hex(value: &str) -> String {
  let digest = Sha256::digest(value.as_bytes());
  let mut hex = String::with_capacity(64);
  for byte in digest {
    use std::fmt::Write;
    write!(&mut hex, "{byte:02x}").expect("writing SHA-256 digest to String cannot fail");
  }
  hex
}

#[cfg(test)]
mod tests {
  use tempfile::TempDir;

  use super::*;

  const ACCOUNT_KEY: &str = "account-1";
  const SECRET: &str = "JBSWY3DPEHPK3PXP";
  const PASSWORD: &str = "known-password";
  const RAW_CDK: &str = "GMAIL-raw-cdk";
  const ACCESS_TOKEN: &str = "known-access-token";
  const EMAIL_OTP: &str = "123456";

  fn gate() -> (TempDir, CanaryGate) {
    let temp = TempDir::new().unwrap();
    let gate = CanaryGate::with_base_dir(temp.path()).unwrap();
    (temp, gate)
  }

  #[test]
  fn canary_accepts_exactly_one_selected_account() {
    let (_temp, gate) = gate();
    let hash = gate
      .validate(
        BackfillMode::Canary,
        &["account-1".into()],
        &BackfillBrowser::Chromium,
        &[EmailProvider::Gmail123452026],
        &BackfillNetworkConfig::None,
      )
      .unwrap();
    assert_eq!(hash, network_config_hash(&BackfillNetworkConfig::None));
  }

  #[test]
  fn canary_rejects_multiple_selected_accounts() {
    let (_temp, gate) = gate();
    let error = gate
      .validate(
        BackfillMode::Canary,
        &["a".into(), "b".into()],
        &BackfillBrowser::Chromium,
        &[EmailProvider::Gmail123452026],
        &BackfillNetworkConfig::None,
      )
      .unwrap_err();
    assert_eq!(error, CanaryRejection::CanaryMustSelectExactlyOneAccount);
  }

  #[test]
  fn gate_requires_at_least_one_selected_account() {
    let (_temp, gate) = gate();
    let error = gate
      .validate(
        BackfillMode::Canary,
        &["  ".into()],
        &BackfillBrowser::Chromium,
        &[EmailProvider::Gmail123452026],
        &BackfillNetworkConfig::None,
      )
      .unwrap_err();
    assert_eq!(error, CanaryRejection::ModeRequiresSelection);
  }

  #[test]
  fn bulk_is_rejected_without_any_evidence() {
    let (_temp, gate) = gate();
    let error = gate
      .validate(
        BackfillMode::Bulk,
        &["account-1".into(), "account-2".into()],
        &BackfillBrowser::Camoufox,
        &[EmailProvider::SmsIosmq],
        &BackfillNetworkConfig::Proxy {
          proxy_id: "p1".into(),
        },
      )
      .unwrap_err();
    match error {
      CanaryRejection::BulkRequiresCanaryEvidence {
        missing_provider,
        missing_browser,
        missing_network_config_hash,
      } => {
        assert_eq!(missing_provider, EmailProvider::SmsIosmq);
        assert_eq!(missing_browser, "camoufox");
        assert_eq!(
          missing_network_config_hash,
          network_config_hash(&BackfillNetworkConfig::Proxy {
            proxy_id: "p1".into()
          })
        );
      }
      other => panic!("expected BulkRequiresCanaryEvidence, got {other:?}"),
    }
  }

  #[test]
  fn bulk_passes_after_a_matching_canary_evidence_is_recorded() {
    let (_temp, gate) = gate();
    gate
      .record_canary_success(
        &BackfillBrowser::Chromium,
        EmailProvider::Gmail123452026,
        &BackfillNetworkConfig::None,
        ACCOUNT_KEY,
      )
      .unwrap();
    gate
      .validate(
        BackfillMode::Bulk,
        &["account-1".into(), "account-2".into()],
        &BackfillBrowser::Chromium,
        &[EmailProvider::Gmail123452026],
        &BackfillNetworkConfig::None,
      )
      .unwrap();
  }

  #[test]
  fn evidence_for_one_combination_does_not_unlock_another() {
    let (_temp, gate) = gate();
    gate
      .record_canary_success(
        &BackfillBrowser::Chromium,
        EmailProvider::Gmail123452026,
        &BackfillNetworkConfig::None,
        ACCOUNT_KEY,
      )
      .unwrap();
    // Different browser.
    assert!(matches!(
      gate.validate(
        BackfillMode::Bulk,
        &["account-2".into()],
        &BackfillBrowser::Camoufox,
        &[EmailProvider::Gmail123452026],
        &BackfillNetworkConfig::None,
      ),
      Err(CanaryRejection::BulkRequiresCanaryEvidence { .. })
    ));
    // Different provider.
    assert!(matches!(
      gate.validate(
        BackfillMode::Bulk,
        &["account-2".into()],
        &BackfillBrowser::Chromium,
        &[EmailProvider::SmsIosmq],
        &BackfillNetworkConfig::None,
      ),
      Err(CanaryRejection::BulkRequiresCanaryEvidence { .. })
    ));
    // Different network.
    assert!(matches!(
      gate.validate(
        BackfillMode::Bulk,
        &["account-2".into()],
        &BackfillBrowser::Chromium,
        &[EmailProvider::Gmail123452026],
        &BackfillNetworkConfig::Vpn {
          vpn_id: "v1".into()
        },
      ),
      Err(CanaryRejection::BulkRequiresCanaryEvidence { .. })
    ));
  }

  #[test]
  fn stale_workflow_or_selector_version_rejects_bulk() {
    let temp = TempDir::new().unwrap();
    let base_dir = temp.path().to_path_buf();
    // Hand-write an evidence file that matches today's selector/network but an
    // outdated workflow_version — the gate must treat it as stale and reject bulk.
    let browser = "chromium";
    let provider = EmailProvider::Gmail123452026;
    let network = BackfillNetworkConfig::None;
    let network_hash = network_config_hash(&network);
    let evidence = CanaryEvidence {
      workflow_version: "0".into(),
      selector_version: TWO_FACTOR_BACKFILL_SELECTOR_VERSION.into(),
      browser: browser.into(),
      email_provider: provider,
      network_config_hash: network_hash.clone(),
      account_key_hash: sha256_hex(ACCOUNT_KEY),
      recorded_at: Utc::now(),
    };
    let json = serde_json::to_vec_pretty(&evidence).unwrap();

    let gate = CanaryGate::with_base_dir(&base_dir).unwrap();
    let mut hasher = Sha256::new();
    hasher.update("0".as_bytes());
    hasher.update([0]);
    hasher.update(TWO_FACTOR_BACKFILL_SELECTOR_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(browser.as_bytes());
    hasher.update([0]);
    hasher.update(provider.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(network_hash.as_bytes());
    let digest = hasher.finalize();
    let mut name = String::new();
    for byte in digest {
      use std::fmt::Write;
      write!(&mut name, "{byte:02x}").unwrap();
    }
    name.push_str(".json");
    std::fs::write(base_dir.join(&name), json).unwrap();

    match gate.validate(
      BackfillMode::Bulk,
      &["account-2".into()],
      &BackfillBrowser::Chromium,
      &[provider],
      &network,
    ) {
      Err(CanaryRejection::BulkRequiresCanaryEvidence { .. }) => {}
      other => panic!("expected rejection for stale workflow evidence, got {other:?}"),
    }
  }

  #[test]
  fn stored_evidence_contains_no_secret_or_credential_material() {
    let temp = TempDir::new().unwrap();
    let gate_dir = temp.path().to_path_buf();
    let gate = CanaryGate::with_base_dir(&gate_dir).unwrap();

    // Pass a secret-shaped account key to prove even that material is hashed
    // before persistence; the evidence file must not retain any secret.
    let suspicious_account_key =
      format!("{SECRET}-{PASSWORD}-{RAW_CDK}-{ACCESS_TOKEN}-{EMAIL_OTP}");
    gate
      .record_canary_success(
        &BackfillBrowser::Chromium,
        EmailProvider::Gmail123452026,
        &BackfillNetworkConfig::None,
        &suspicious_account_key,
      )
      .unwrap();

    let path = std::fs::read_dir(&gate_dir)
      .unwrap()
      .next()
      .unwrap()
      .unwrap()
      .path();
    let serialized = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
    for forbidden in [
      SECRET,
      PASSWORD,
      RAW_CDK,
      ACCESS_TOKEN,
      EMAIL_OTP,
      "totp_secret",
      "totp_",
    ] {
      assert!(
        !serialized.contains(forbidden),
        "canary evidence leaked forbidden material '{forbidden}' in {serialized}"
      );
    }
    // The evidence must retain the hashed account key, not the secret material.
    assert!(serialized.contains("accountKeyHash"));
    assert!(serialized.contains(&sha256_hex(&suspicious_account_key)));
  }
}
