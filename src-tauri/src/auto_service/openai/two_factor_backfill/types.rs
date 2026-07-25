use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub fn safe_progress_account_key(account_key: &str) -> String {
  let account_key = account_key.trim();
  if !account_key.contains('@') {
    return account_key.to_string();
  }
  let mut hasher = Sha256::new();
  hasher.update(account_key.as_bytes());
  let digest = hasher.finalize();
  let mut value = String::with_capacity(20);
  value.push_str("account-");
  for byte in digest.iter().take(6) {
    std::fmt::Write::write_fmt(&mut value, format_args!("{byte:02x}"))
      .expect("writing progress account key cannot fail");
  }
  value
}

use crate::auto_service::openai::register::types::{
  AccountInventoryStatus, EmailProviderProvenance, RegistrationOutcomeReason,
  TwoFactorBackfillExclusion,
};
use crate::email::EmailProvider;

/// Selection-only request for a backend-authoritative eligibility preview.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TwoFactorBackfillPreviewRequest {
  pub selected_account_keys: Vec<String>,
  #[serde(default)]
  pub allow_free_trial_no: bool,
  #[serde(default)]
  pub acknowledge_legacy_access: bool,
}

/// Safe preview response. Credential and recovery material is intentionally absent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TwoFactorBackfillPreview {
  pub accounts: Vec<TwoFactorBackfillAccountPreview>,
  /// Whether a matching successful canary unlocks Bulk for the requested
  /// browser/provider/network combination. Evidence details stay backend-only.
  #[serde(default)]
  pub bulk_available: bool,
}

/// Safe summary of an incomplete private recovery journal. The pending secret
/// is intentionally absent so this type can cross the Tauri boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TwoFactorBackfillRecoverySummary {
  pub operation_id: String,
  pub account_key: String,
  pub state: TwoFactorBackfillRecoveryState,
  pub expected_account_revision: u64,
  pub final_account_revision: Option<u64>,
  pub journal_revision: u64,
  pub created_at: chrono::DateTime<chrono::Utc>,
  pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TwoFactorBackfillRecoveryState {
  SecretCaptured,
  RemoteConfirmed,
  ManualReview,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TwoFactorBackfillRecoveryResult {
  pub summary: TwoFactorBackfillRecoverySummary,
  pub recovered: bool,
  pub requires_manual_review: bool,
}

/// Safe per-account eligibility details for operator review.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TwoFactorBackfillAccountPreview {
  pub account_key: String,
  pub account_id: String,
  pub email: String,
  pub eligible: bool,
  pub ineligibility_reasons: Vec<TwoFactorBackfillIneligibilityReason>,
  pub email_provider: Option<EmailProvider>,
  pub email_provider_provenance: Option<EmailProviderProvenance>,
  #[serde(default)]
  pub requires_provider_persistence: bool,
  pub record_revision: u64,
}

/// Structured provider resolution result used before any network operation exists.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EmailProviderResolution {
  Resolved {
    provider: EmailProvider,
    provenance: EmailProviderProvenance,
    requires_provider_persistence: bool,
  },
  MissingEmailProviderProvenance,
  MissingCdk,
  UnknownCdkPrefix,
}

/// Backend policy reasons that make a selected account ineligible.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TwoFactorBackfillIneligibilityReason {
  AccountNotFound,
  MissingAccountKey,
  RegistrationUnsuccessful,
  MissingEmail,
  MissingPassword,
  InventoryStatusNotAvailable(AccountInventoryStatus),
  FreeTrialNoOverrideRequired,
  InvalidOutcomeNotEligible(Option<RegistrationOutcomeReason>),
  TwoFactorAlreadyEnabled,
  InconsistentLocalTwoFactorState,
  MissingCdk,
  UnknownCdkPrefix,
  MissingEmailProviderProvenance,
  LegacyAccessAcknowledgementRequired,
  AccessLocked,
  ExplicitlyExcluded(TwoFactorBackfillExclusion),
  BackfillInProgress,
  BackfillCompleted,
  BackfillReconciliationRequiresManualReview,
  BackfillCompletedOutcomeMissing,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn progress_account_key_hashes_only_email_fallbacks() {
    assert_eq!(safe_progress_account_key(" account-1 "), "account-1");
    let email = "legacy@example.com";
    let safe = safe_progress_account_key(email);
    assert!(safe.starts_with("account-"));
    assert!(!safe.contains(email));
    assert!(!safe.contains('@'));
    assert_eq!(safe, safe_progress_account_key(email));
  }

  #[test]
  fn legacy_preview_request_defaults_legacy_access_acknowledgement_to_false() {
    let request: TwoFactorBackfillPreviewRequest =
      serde_json::from_str(r#"{"selectedAccountKeys":["account-1"],"allowFreeTrialNo":false}"#)
        .unwrap();

    assert!(!request.acknowledge_legacy_access);
  }
}
