use std::collections::HashSet;

use crate::auto_service::openai::register::types::{
  AccountInventoryStatus, EmailProviderProvenance, RegistrationOutcomeReason, RegistrationResult,
  TwoFactorBackfillAccessState, TwoFactorBackfillOutcome, TwoFactorBackfillState,
};
use crate::email::EmailProvider;

use super::types::{
  EmailProviderResolution, TwoFactorBackfillAccountPreview, TwoFactorBackfillIneligibilityReason,
  TwoFactorBackfillPreview, TwoFactorBackfillPreviewRequest,
};

/// Resolve provider metadata without invoking a provider or making a network request.
pub fn resolve_email_provider(account: &RegistrationResult) -> EmailProviderResolution {
  if let Some(provider) = account.email_provider {
    let Some(provenance) = account.email_provider_provenance else {
      return EmailProviderResolution::MissingEmailProviderProvenance;
    };
    return EmailProviderResolution::Resolved {
      provider,
      provenance,
      requires_provider_persistence: false,
    };
  }

  let cdk = account.cdk.trim();
  if cdk.is_empty() {
    return EmailProviderResolution::MissingCdk;
  }

  let normalized = cdk.to_ascii_uppercase();
  let provider = if normalized.starts_with("GMAIL-") {
    EmailProvider::Gmail123452026
  } else if normalized.starts_with("MAIL-") {
    EmailProvider::SmsIosmq
  } else {
    return EmailProviderResolution::UnknownCdkPrefix;
  };

  EmailProviderResolution::Resolved {
    provider,
    provenance: EmailProviderProvenance::InferredFromCdk,
    requires_provider_persistence: true,
  }
}

/// Evaluate selected account keys using backend policy and preserve first-selection order.
pub fn evaluate_eligibility(
  request: &TwoFactorBackfillPreviewRequest,
  accounts: &[RegistrationResult],
) -> TwoFactorBackfillPreview {
  let mut seen = HashSet::with_capacity(request.selected_account_keys.len());
  let mut previews = Vec::with_capacity(request.selected_account_keys.len());

  for selected_key in &request.selected_account_keys {
    let account_key = selected_key.trim().to_string();
    if !seen.insert(account_key.clone()) {
      continue;
    }

    if account_key.is_empty() {
      previews.push(missing_preview(
        account_key,
        TwoFactorBackfillIneligibilityReason::MissingAccountKey,
      ));
      continue;
    }

    let Some(account) = accounts
      .iter()
      .find(|account| stable_account_key(account).as_deref() == Some(account_key.as_str()))
    else {
      previews.push(missing_preview(
        account_key,
        TwoFactorBackfillIneligibilityReason::AccountNotFound,
      ));
      continue;
    };

    previews.push(evaluate_account(
      account,
      request.allow_free_trial_no,
      request.acknowledge_legacy_access,
    ));
  }

  TwoFactorBackfillPreview {
    accounts: previews,
    bulk_available: false,
  }
}

fn stable_account_key(account: &RegistrationResult) -> Option<String> {
  let account_id = account.account_id.trim();
  if !account_id.is_empty() {
    return Some(account_id.to_string());
  }

  let email = account.email.trim();
  if email.is_empty() {
    None
  } else {
    Some(email.to_string())
  }
}

fn evaluate_account(
  account: &RegistrationResult,
  allow_free_trial_no: bool,
  acknowledge_legacy_access: bool,
) -> TwoFactorBackfillAccountPreview {
  let account_key = stable_account_key(account).unwrap_or_default();
  let mut reasons = Vec::new();
  let invalid_free_trial_no = account.status == AccountInventoryStatus::Invalid
    && account.registration_outcome_reason == Some(RegistrationOutcomeReason::FreeTrialNo);
  let free_trial_no_override = invalid_free_trial_no && allow_free_trial_no;

  if !account.success && !invalid_free_trial_no {
    reasons.push(TwoFactorBackfillIneligibilityReason::RegistrationUnsuccessful);
  }
  if account.email.trim().is_empty() {
    reasons.push(TwoFactorBackfillIneligibilityReason::MissingEmail);
  }
  if account.password.trim().is_empty() {
    reasons.push(TwoFactorBackfillIneligibilityReason::MissingPassword);
  }

  match &account.status {
    AccountInventoryStatus::Available => {}
    AccountInventoryStatus::Invalid if invalid_free_trial_no => {
      if !allow_free_trial_no {
        reasons.push(TwoFactorBackfillIneligibilityReason::FreeTrialNoOverrideRequired);
      }
    }
    AccountInventoryStatus::Invalid => reasons.push(
      TwoFactorBackfillIneligibilityReason::InvalidOutcomeNotEligible(
        account.registration_outcome_reason,
      ),
    ),
    status => reasons
      .push(TwoFactorBackfillIneligibilityReason::InventoryStatusNotAvailable(status.clone())),
  }

  if account.two_fa_enabled {
    reasons.push(TwoFactorBackfillIneligibilityReason::TwoFactorAlreadyEnabled);
  } else if !account.totp_secret.trim().is_empty() {
    reasons.push(TwoFactorBackfillIneligibilityReason::InconsistentLocalTwoFactorState);
  }

  if account.cdk.trim().is_empty() {
    reasons.push(TwoFactorBackfillIneligibilityReason::MissingCdk);
  }

  let provider_resolution = resolve_email_provider(account);
  let (email_provider, email_provider_provenance, requires_provider_persistence) =
    match provider_resolution {
      EmailProviderResolution::Resolved {
        provider,
        provenance,
        requires_provider_persistence,
      } => (
        Some(provider),
        Some(provenance),
        requires_provider_persistence,
      ),
      EmailProviderResolution::MissingEmailProviderProvenance => {
        reasons.push(TwoFactorBackfillIneligibilityReason::MissingEmailProviderProvenance);
        (None, None, false)
      }
      EmailProviderResolution::MissingCdk => {
        if !reasons.contains(&TwoFactorBackfillIneligibilityReason::MissingCdk) {
          reasons.push(TwoFactorBackfillIneligibilityReason::MissingCdk);
        }
        (None, None, false)
      }
      EmailProviderResolution::UnknownCdkPrefix => {
        reasons.push(TwoFactorBackfillIneligibilityReason::UnknownCdkPrefix);
        (None, None, false)
      }
    };

  match account.two_factor_backfill_access_state {
    Some(TwoFactorBackfillAccessState::Accessible) => {}
    Some(TwoFactorBackfillAccessState::Locked) => {
      reasons.push(TwoFactorBackfillIneligibilityReason::AccessLocked);
    }
    None if !acknowledge_legacy_access => {
      reasons.push(TwoFactorBackfillIneligibilityReason::LegacyAccessAcknowledgementRequired);
    }
    None => {}
  }
  if let Some(exclusion) = account.two_factor_backfill_exclusion {
    reasons.push(TwoFactorBackfillIneligibilityReason::ExplicitlyExcluded(
      exclusion,
    ));
  }
  match (
    account.two_factor_backfill_state,
    account.two_factor_backfill_outcome,
  ) {
    (Some(TwoFactorBackfillState::InProgress), _) => {
      reasons.push(TwoFactorBackfillIneligibilityReason::BackfillInProgress);
    }
    (Some(TwoFactorBackfillState::Completed), Some(TwoFactorBackfillOutcome::Enabled)) => {
      reasons.push(TwoFactorBackfillIneligibilityReason::BackfillCompleted);
    }
    (
      Some(TwoFactorBackfillState::Completed),
      Some(TwoFactorBackfillOutcome::ReconciliationRequired),
    ) => {
      reasons
        .push(TwoFactorBackfillIneligibilityReason::BackfillReconciliationRequiresManualReview);
    }
    (
      Some(TwoFactorBackfillState::Completed),
      Some(TwoFactorBackfillOutcome::Failed | TwoFactorBackfillOutcome::Cancelled),
    )
    | (None, _) => {}
    (Some(TwoFactorBackfillState::Completed), None) => {
      reasons.push(TwoFactorBackfillIneligibilityReason::BackfillCompletedOutcomeMissing);
    }
  }

  TwoFactorBackfillAccountPreview {
    account_key,
    account_id: account.account_id.clone(),
    email: account.email.clone(),
    eligible: reasons.is_empty() && (account.success || free_trial_no_override),
    ineligibility_reasons: reasons,
    email_provider,
    email_provider_provenance,
    requires_provider_persistence,
    record_revision: account.record_revision,
  }
}

fn missing_preview(
  account_key: String,
  reason: TwoFactorBackfillIneligibilityReason,
) -> TwoFactorBackfillAccountPreview {
  TwoFactorBackfillAccountPreview {
    account_key,
    account_id: String::new(),
    email: String::new(),
    eligible: false,
    ineligibility_reasons: vec![reason],
    email_provider: None,
    email_provider_provenance: None,
    requires_provider_persistence: false,
    record_revision: 0,
  }
}

#[cfg(test)]
mod tests {
  use chrono::Utc;

  use super::*;
  use crate::auto_service::openai::register::types::{
    TwoFactorBackfillExclusion, TwoFactorBackfillOutcome,
  };

  const PASSWORD: &str = "known-password-value";
  const RAW_CDK: &str = "GMAIL-known-raw-cdk-value";
  const ACCESS_TOKEN: &str = "known-access-token-value";
  const TOTP_SECRET: &str = "known-totp-secret-value";

  fn account() -> RegistrationResult {
    RegistrationResult {
      success: true,
      email: "user@example.com".into(),
      password: PASSWORD.into(),
      account_id: "account-1".into(),
      access_token: ACCESS_TOKEN.into(),
      device_id: "device-1".into(),
      error_message: String::new(),
      step_logs: Vec::new(),
      created_at: Utc::now(),
      two_fa_enabled: false,
      totp_secret: String::new(),
      free_trial_eligible: true,
      plan_type: "trial".into(),
      cdk: RAW_CDK.into(),
      base_email: "base@example.com".into(),
      phone_number: String::new(),
      status: AccountInventoryStatus::Available,
      note: String::new(),
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

  fn preview_for(
    account: RegistrationResult,
    allow_free_trial_no: bool,
  ) -> TwoFactorBackfillAccountPreview {
    preview_for_with_legacy_access_ack(account, allow_free_trial_no, false)
  }

  fn preview_for_with_legacy_access_ack(
    account: RegistrationResult,
    allow_free_trial_no: bool,
    acknowledge_legacy_access: bool,
  ) -> TwoFactorBackfillAccountPreview {
    let key = stable_account_key(&account).unwrap_or_default();
    evaluate_eligibility(
      &TwoFactorBackfillPreviewRequest {
        selected_account_keys: vec![key],
        allow_free_trial_no,
        acknowledge_legacy_access,
      },
      &[account],
    )
    .accounts
    .into_iter()
    .next()
    .unwrap()
  }

  fn assert_reason(account: RegistrationResult, reason: TwoFactorBackfillIneligibilityReason) {
    let preview = preview_for(account, false);
    assert!(!preview.eligible);
    assert!(preview.ineligibility_reasons.contains(&reason));
  }

  #[test]
  fn eligible_account_uses_inferred_provider() {
    let preview = preview_for(account(), false);

    assert!(preview.eligible);
    assert!(preview.ineligibility_reasons.is_empty());
    assert_eq!(preview.email_provider, Some(EmailProvider::Gmail123452026));
    assert_eq!(
      preview.email_provider_provenance,
      Some(EmailProviderProvenance::InferredFromCdk)
    );
    assert!(preview.requires_provider_persistence);
    assert_eq!(preview.record_revision, 7);
  }

  #[test]
  fn provider_inference_is_trimmed_ascii_insensitive_and_exact_prefix_only() {
    for (cdk, expected) in [
      ("  gMaIl-card  ", EmailProvider::Gmail123452026),
      (" mAiL-card ", EmailProvider::SmsIosmq),
    ] {
      let mut value = account();
      value.cdk = cdk.into();
      assert_eq!(
        resolve_email_provider(&value),
        EmailProviderResolution::Resolved {
          provider: expected,
          provenance: EmailProviderProvenance::InferredFromCdk,
          requires_provider_persistence: true,
        }
      );
    }

    for cdk in ["GMAIL", "MAIL", "XGMAIL-card", "EMAIL-card"] {
      let mut value = account();
      value.cdk = cdk.into();
      assert_eq!(
        resolve_email_provider(&value),
        EmailProviderResolution::UnknownCdkPrefix
      );
    }

    let mut value = account();
    value.cdk = "   ".into();
    assert_eq!(
      resolve_email_provider(&value),
      EmailProviderResolution::MissingCdk
    );
  }

  #[test]
  fn stored_provider_wins_and_retains_provenance() {
    let mut value = account();
    value.cdk = "UNKNOWN-card".into();
    value.email_provider = Some(EmailProvider::SmsIosmq);
    value.email_provider_provenance = Some(EmailProviderProvenance::RegistrationConfig);

    assert_eq!(
      resolve_email_provider(&value),
      EmailProviderResolution::Resolved {
        provider: EmailProvider::SmsIosmq,
        provenance: EmailProviderProvenance::RegistrationConfig,
        requires_provider_persistence: false,
      }
    );
    let preview = preview_for(value, false);
    assert!(preview.eligible);
    assert_eq!(preview.email_provider, Some(EmailProvider::SmsIosmq));
    assert_eq!(
      preview.email_provider_provenance,
      Some(EmailProviderProvenance::RegistrationConfig)
    );
    assert!(!preview.requires_provider_persistence);
  }

  #[test]
  fn stored_provider_without_provenance_fails_closed() {
    let mut value = account();
    value.email_provider = Some(EmailProvider::SmsIosmq);
    value.email_provider_provenance = None;

    assert_eq!(
      resolve_email_provider(&value),
      EmailProviderResolution::MissingEmailProviderProvenance
    );
    let preview = preview_for(value, false);
    assert!(!preview.eligible);
    assert_eq!(preview.email_provider, None);
    assert_eq!(preview.email_provider_provenance, None);
    assert!(!preview.requires_provider_persistence);
    assert!(preview
      .ineligibility_reasons
      .contains(&TwoFactorBackfillIneligibilityReason::MissingEmailProviderProvenance));
  }

  #[test]
  fn rejects_each_core_account_gate() {
    let mut unsuccessful = account();
    unsuccessful.success = false;
    assert_reason(
      unsuccessful,
      TwoFactorBackfillIneligibilityReason::RegistrationUnsuccessful,
    );

    let mut missing_email = account();
    missing_email.email = "  ".into();
    assert_reason(
      missing_email,
      TwoFactorBackfillIneligibilityReason::MissingEmail,
    );

    let mut missing_password = account();
    missing_password.password = "  ".into();
    assert_reason(
      missing_password,
      TwoFactorBackfillIneligibilityReason::MissingPassword,
    );

    let mut enabled = account();
    enabled.two_fa_enabled = true;
    assert_reason(
      enabled,
      TwoFactorBackfillIneligibilityReason::TwoFactorAlreadyEnabled,
    );

    let mut inconsistent = account();
    inconsistent.totp_secret = TOTP_SECRET.into();
    assert_reason(
      inconsistent,
      TwoFactorBackfillIneligibilityReason::InconsistentLocalTwoFactorState,
    );

    let mut missing_cdk = account();
    missing_cdk.cdk = "  ".into();
    assert_reason(
      missing_cdk,
      TwoFactorBackfillIneligibilityReason::MissingCdk,
    );

    let mut unknown_provider = account();
    unknown_provider.cdk = "OTHER-card".into();
    assert_reason(
      unknown_provider,
      TwoFactorBackfillIneligibilityReason::UnknownCdkPrefix,
    );
  }

  #[test]
  fn rejects_every_non_available_status_without_valid_override() {
    for status in [
      AccountInventoryStatus::Exported,
      AccountInventoryStatus::Sold,
      AccountInventoryStatus::Reserved,
    ] {
      let mut value = account();
      value.status = status.clone();
      assert_reason(
        value,
        TwoFactorBackfillIneligibilityReason::InventoryStatusNotAvailable(status),
      );
    }

    let mut invalid = account();
    invalid.status = AccountInventoryStatus::Invalid;
    invalid.registration_outcome_reason = None;
    assert_reason(
      invalid,
      TwoFactorBackfillIneligibilityReason::InvalidOutcomeNotEligible(None),
    );
  }

  #[test]
  fn invalid_free_trial_no_requires_explicit_override() {
    let mut value = account();
    value.success = false;
    value.status = AccountInventoryStatus::Invalid;
    value.registration_outcome_reason = Some(RegistrationOutcomeReason::FreeTrialNo);

    let denied = preview_for(value.clone(), false);
    assert!(!denied.eligible);
    assert_eq!(
      denied.ineligibility_reasons,
      vec![TwoFactorBackfillIneligibilityReason::FreeTrialNoOverrideRequired]
    );

    let allowed = preview_for(value, true);
    assert!(allowed.eligible);
    assert!(allowed.ineligibility_reasons.is_empty());
  }

  #[test]
  fn invalid_unknown_or_other_outcome_cannot_be_overridden() {
    for outcome in [
      None,
      Some(RegistrationOutcomeReason::Registered),
      Some(RegistrationOutcomeReason::RegistrationFailed),
      Some(RegistrationOutcomeReason::BatchSummary),
    ] {
      let mut value = account();
      value.status = AccountInventoryStatus::Invalid;
      value.registration_outcome_reason = outcome;
      let preview = preview_for(value, true);
      assert!(!preview.eligible);
      assert!(preview
        .ineligibility_reasons
        .contains(&TwoFactorBackfillIneligibilityReason::InvalidOutcomeNotEligible(outcome)));
    }
  }

  #[test]
  fn legacy_access_requires_acknowledgement_and_locked_access_always_rejects() {
    let mut legacy = account();
    legacy.two_factor_backfill_access_state = None;
    let denied = preview_for(legacy.clone(), false);
    assert!(!denied.eligible);
    assert!(denied
      .ineligibility_reasons
      .contains(&TwoFactorBackfillIneligibilityReason::LegacyAccessAcknowledgementRequired));

    let allowed = preview_for_with_legacy_access_ack(legacy, false, true);
    assert!(allowed.eligible);

    let mut locked = account();
    locked.two_factor_backfill_access_state = Some(TwoFactorBackfillAccessState::Locked);
    let locked = preview_for_with_legacy_access_ack(locked, false, true);
    assert!(!locked.eligible);
    assert!(locked
      .ineligibility_reasons
      .contains(&TwoFactorBackfillIneligibilityReason::AccessLocked));
  }

  #[test]
  fn rejects_explicit_exclusions() {
    for exclusion in [
      TwoFactorBackfillExclusion::OperatorExcluded,
      TwoFactorBackfillExclusion::ManualReview,
    ] {
      let mut value = account();
      value.two_factor_backfill_exclusion = Some(exclusion);
      assert_reason(
        value,
        TwoFactorBackfillIneligibilityReason::ExplicitlyExcluded(exclusion),
      );
    }
  }

  #[test]
  fn lifecycle_policy_rejects_active_or_non_retryable_completed_attempts() {
    let mut in_progress = account();
    in_progress.two_factor_backfill_state = Some(TwoFactorBackfillState::InProgress);
    assert_reason(
      in_progress,
      TwoFactorBackfillIneligibilityReason::BackfillInProgress,
    );

    let mut completed = account();
    completed.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
    completed.two_factor_backfill_outcome = Some(TwoFactorBackfillOutcome::Enabled);
    assert_reason(
      completed,
      TwoFactorBackfillIneligibilityReason::BackfillCompleted,
    );

    let mut reconciliation = account();
    reconciliation.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
    reconciliation.two_factor_backfill_outcome =
      Some(TwoFactorBackfillOutcome::ReconciliationRequired);
    assert_reason(
      reconciliation,
      TwoFactorBackfillIneligibilityReason::BackfillReconciliationRequiresManualReview,
    );

    let mut missing_outcome = account();
    missing_outcome.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
    assert_reason(
      missing_outcome,
      TwoFactorBackfillIneligibilityReason::BackfillCompletedOutcomeMissing,
    );
  }

  #[test]
  fn completed_failed_or_cancelled_attempts_may_retry() {
    for outcome in [
      TwoFactorBackfillOutcome::Failed,
      TwoFactorBackfillOutcome::Cancelled,
    ] {
      let mut value = account();
      value.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
      value.two_factor_backfill_outcome = Some(outcome);

      assert!(preview_for(value, false).eligible);
    }
  }

  #[test]
  fn past_outcome_without_lifecycle_state_does_not_block() {
    let mut value = account();
    value.two_factor_backfill_access_state = Some(TwoFactorBackfillAccessState::Accessible);
    value.two_factor_backfill_outcome = Some(TwoFactorBackfillOutcome::Failed);

    assert!(preview_for(value, false).eligible);
  }

  #[test]
  fn stable_key_prefers_account_id_then_email_and_missing_key_rejects() {
    let mut email_keyed = account();
    email_keyed.account_id = "  ".into();
    assert_eq!(
      stable_account_key(&email_keyed).as_deref(),
      Some("user@example.com")
    );
    assert!(preview_for(email_keyed, false).eligible);

    let preview = evaluate_eligibility(
      &TwoFactorBackfillPreviewRequest {
        selected_account_keys: vec!["  ".into()],
        allow_free_trial_no: false,
        acknowledge_legacy_access: false,
      },
      &[],
    );
    assert_eq!(
      preview.accounts[0].ineligibility_reasons,
      vec![TwoFactorBackfillIneligibilityReason::MissingAccountKey]
    );
    assert!(!preview.accounts[0].requires_provider_persistence);
  }

  #[test]
  fn selected_keys_are_deduplicated_in_first_occurrence_order() {
    let first = account();
    let mut second = account();
    second.account_id = "account-2".into();
    second.email = "second@example.com".into();

    let preview = evaluate_eligibility(
      &TwoFactorBackfillPreviewRequest {
        selected_account_keys: vec![
          "account-2".into(),
          "account-1".into(),
          "account-2".into(),
          "missing-account".into(),
          "account-1".into(),
        ],
        allow_free_trial_no: false,
        acknowledge_legacy_access: false,
      },
      &[first, second],
    );

    assert_eq!(
      preview
        .accounts
        .iter()
        .map(|account| account.account_key.as_str())
        .collect::<Vec<_>>(),
      vec!["account-2", "account-1", "missing-account"]
    );
    assert_eq!(
      preview.accounts[2].ineligibility_reasons,
      vec![TwoFactorBackfillIneligibilityReason::AccountNotFound]
    );
    assert!(!preview.accounts[2].requires_provider_persistence);
  }

  #[test]
  fn preview_serialization_never_contains_credential_or_recovery_material() {
    let mut value = account();
    value.totp_secret = TOTP_SECRET.into();
    let preview = preview_for(value, false);
    let json = serde_json::to_string(&preview).unwrap();

    for secret in [PASSWORD, RAW_CDK, ACCESS_TOKEN, TOTP_SECRET] {
      assert!(
        !json.contains(secret),
        "preview leaked known secret: {secret}"
      );
    }
    for forbidden_field in [
      "\"password\":",
      "\"cdk\":",
      "\"accessToken\":",
      "\"totpSecret\":",
    ] {
      assert!(!json.contains(forbidden_field));
    }
  }
}
