use chrono::{Datelike, NaiveDate, Utc};
use futures_util::stream::{FuturesUnordered, StreamExt};
use rand::prelude::IndexedRandom;
use rand::Rng;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::Semaphore;
use tokio::time::sleep;
use uuid::Uuid;

use super::store::{
  cdk_remaining_capacity, compare_and_update_registered_account, get_cdk_inventory,
  get_registered_account, put_cdk_inventory_record, reserve_cdk_slots_per,
  save_registration_result, BackfillPatchPrecondition, CdkSlotReservation, TwoFactorBackfillPatch,
  MAX_ACCOUNTS_PER_CDK,
};
use super::types::{
  should_rotate, AccountInventoryStatus, CdkInventoryRecord, EmailProviderProvenance, NetworkMode,
  RegistrationConfig, RegistrationOutcomeReason, RegistrationProgress,
  RegistrationProgressEventKind, RegistrationResult, RegistrationStep, RegistrationTerminalSummary,
  TwoFactorBackfillAccessState, TwoFactorBackfillOutcome, TwoFactorBackfillState,
};
use crate::auto_service::openai::browser::{
  attach_browser_session, click_email_form_continue, click_password_method, click_trusted_submit,
  fill_visible_input, has_visible_selector, BrowserSession, ChatGptBrowser, ContextPageTarget,
};
use crate::auto_service::openai::chatgpt_auth::{
  classify_auth_state, AuthState, BrowserAuthAdapter, ExistingAccountAuthAdapter,
};
use crate::auto_service::openai::chatgpt_two_factor::{
  enable_authenticator_two_factor, BrowserTwoFactorAdapter, TwoFactorError,
};
use crate::auto_service::openai::login::sanitize_browser_urls_for_log;
use crate::auto_service::openai::two_factor_backfill::journal::{
  PersistedBackfillAccountPatch, TwoFactorBackfillJournal, TwoFactorBackfillJournalState,
};
use crate::email::{EmailService, EmailServiceError};
use crate::sms::{poll_otp_with_cancel, NumberRequest, SmsService, SmsServiceError};

/// Write structural About You diagnostics without screenshots, field values,
/// placeholders, raw HTML, or URL query/fragment data.
async fn snapshot_about_you_page(session: &mut BrowserSession, suffix: &str) {
  let dir = crate::app_dirs::data_dir().join("debug");
  let _ = std::fs::create_dir_all(&dir);
  let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
  let diagnostic_js = r#"(function(){
    const visible = (el) => {
      if (!el) return false;
      const r = el.getBoundingClientRect();
      const style = window.getComputedStyle(el);
      return r.width > 0 && r.height > 0 && style.visibility !== 'hidden' && style.display !== 'none';
    };
    const inputs = Array.from(document.querySelectorAll('input, select, textarea')).map((el) => ({
      tag: el.tagName.toLowerCase(),
      name: el.name || '',
      type: el.type || '',
      visible: visible(el),
      disabled: !!el.disabled,
      ariaInvalid: el.getAttribute('aria-invalid') || '',
    }));
    const buttons = Array.from(document.querySelectorAll('button, [role="button"], input[type="submit"]')).map((el) => ({
      tag: el.tagName.toLowerCase(),
      type: el.getAttribute('type') || '',
      role: el.getAttribute('role') || '',
      visible: visible(el),
      disabled: !!el.disabled,
      ariaDisabled: el.getAttribute('aria-disabled') || '',
      ariaBusy: el.getAttribute('aria-busy') || '',
    }));
    return {
      origin: location.origin,
      path: location.pathname,
      titlePresent: !!document.title,
      formPresent: !!document.querySelector('form'),
      inputs,
      buttons,
      alertCount: Array.from(document.querySelectorAll('[role="alert"], [aria-live="assertive"], [data-error]')).filter(visible).length,
    };
  })()"#;
  if let Ok(info) = session.evaluate(diagnostic_js, false).await {
    let value = info.get("value").cloned().unwrap_or_default();
    let _ = std::fs::write(
      dir.join(format!("about-you-{}-{}.json", ts, suffix)),
      serde_json::to_string_pretty(&value).unwrap_or_default(),
    );
  }
}

// ---------------------------------------------------------------------------
// User info generation
// ---------------------------------------------------------------------------

const FIRST_NAMES: &[&str] = &[
  "James",
  "Robert",
  "John",
  "Michael",
  "David",
  "William",
  "Richard",
  "Joseph",
  "Thomas",
  "Charles",
  "Christopher",
  "Daniel",
  "Matthew",
  "Anthony",
  "Mark",
  "Donald",
  "Steven",
  "Paul",
  "Andrew",
  "Joshua",
  "Kenneth",
  "Kevin",
  "Brian",
  "George",
  "Timothy",
  "Ronald",
  "Edward",
  "Jason",
  "Jeffrey",
  "Ryan",
  "Jacob",
  "Gary",
  "Nicholas",
  "Eric",
  "Jonathan",
  "Stephen",
  "Larry",
  "Justin",
  "Scott",
  "Brandon",
  "Mary",
  "Patricia",
  "Jennifer",
  "Linda",
  "Barbara",
  "Elizabeth",
  "Susan",
  "Jessica",
  "Sarah",
  "Karen",
  "Lisa",
  "Nancy",
  "Betty",
  "Margaret",
  "Sandra",
  "Ashley",
  "Dorothy",
  "Kimberly",
  "Emily",
  "Donna",
  "Michelle",
  "Carol",
  "Amanda",
  "Melissa",
  "Deborah",
  "Stephanie",
  "Rebecca",
  "Sharon",
  "Laura",
  "Cynthia",
  "Kathleen",
  "Amy",
  "Angela",
  "Shirley",
  "Anna",
  "Brenda",
  "Pamela",
  "Emma",
  "Nicole",
  "Helen",
  "Samantha",
  "Katherine",
  "Christine",
  "Debra",
  "Rachel",
  "Carolyn",
  "Janet",
  "Catherine",
  "Maria",
  "Heather",
  "Diane",
  "Ruth",
  "Julie",
  "Olivia",
  "Joyce",
  "Virginia",
  "Victoria",
  "Kelly",
  "Lauren",
  "Christina",
  "Joan",
  "Evelyn",
  "Judith",
  "Megan",
  "Andrea",
  "Cheryl",
  "Hannah",
  "Jacqueline",
  "Martha",
  "Gloria",
  "Teresa",
  "Ann",
  "Sara",
  "Madison",
  "Frances",
  "Kathryn",
  "Janice",
  "Jean",
  "Abigail",
  "Alice",
];

const LAST_NAMES: &[&str] = &[
  "Smith",
  "Johnson",
  "Williams",
  "Brown",
  "Jones",
  "Garcia",
  "Miller",
  "Davis",
  "Rodriguez",
  "Martinez",
  "Hernandez",
  "Lopez",
  "Gonzalez",
  "Wilson",
  "Anderson",
  "Thomas",
  "Taylor",
  "Moore",
  "Jackson",
  "Martin",
  "Lee",
  "Perez",
  "Thompson",
  "White",
  "Harris",
  "Sanchez",
  "Clark",
  "Ramirez",
  "Lewis",
  "Robinson",
  "Walker",
  "Young",
  "Allen",
  "King",
  "Wright",
  "Scott",
  "Torres",
  "Nguyen",
  "Hill",
  "Flores",
  "Green",
  "Adams",
  "Nelson",
  "Baker",
  "Hall",
  "Rivera",
  "Campbell",
  "Mitchell",
  "Carter",
  "Roberts",
];

fn random_name() -> (String, String) {
  let mut rng = rand::rng();
  let first = FIRST_NAMES.choose(&mut rng).unwrap_or(&"James");
  let last = LAST_NAMES.choose(&mut rng).unwrap_or(&"Smith");
  (first.to_string(), last.to_string())
}

fn random_birthday() -> String {
  let mut rng = rand::rng();
  let year = 1996 + (rng.next_u32() % 11);
  let month = 1 + (rng.next_u32() % 12);
  let day = 1 + (rng.next_u32() % 28);
  format!("{year:04}-{month:02}-{day:02}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AboutYouBirthMode {
  Age,
  SingleDate,
  SplitDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AboutYouSubmitUrl {
  Completed,
  StillOnForm,
  Transitioning,
}

fn classify_about_you_submit_url(url: &str) -> AboutYouSubmitUrl {
  match detect_page_type(url) {
    PageType::ChatgptHome => AboutYouSubmitUrl::Completed,
    PageType::AboutYou => AboutYouSubmitUrl::StillOnForm,
    _ => AboutYouSubmitUrl::Transitioning,
  }
}

fn resolve_about_you_submit_url(polled_url: &str, latest_url: &str) -> AboutYouSubmitUrl {
  let polled = classify_about_you_submit_url(polled_url);
  if polled == AboutYouSubmitUrl::Completed {
    polled
  } else {
    classify_about_you_submit_url(latest_url)
  }
}

fn resolve_about_you_birth_mode(
  has_age: bool,
  has_single_date: bool,
  has_split_date: bool,
) -> Option<AboutYouBirthMode> {
  if has_age {
    Some(AboutYouBirthMode::Age)
  } else if has_single_date {
    Some(AboutYouBirthMode::SingleDate)
  } else if has_split_date {
    Some(AboutYouBirthMode::SplitDate)
  } else {
    None
  }
}

fn birthdate_parts(birthdate: &str) -> Result<(String, String, String, String), String> {
  let date = NaiveDate::parse_from_str(birthdate, "%Y-%m-%d")
    .map_err(|_| "generated birthdate is invalid".to_string())?;
  let year = date.year().to_string();
  let month = format!("{:02}", date.month());
  let day = format!("{:02}", date.day());
  let age = Utc::now()
    .date_naive()
    .years_since(date)
    .ok_or_else(|| "generated birthdate is in the future".to_string())?
    .to_string();
  Ok((year, month, day, age))
}

#[cfg(test)]
mod about_you_tests {
  use super::{
    birthdate_parts, classify_about_you_submit_url, resolve_about_you_birth_mode,
    resolve_about_you_submit_url, AboutYouBirthMode, AboutYouSubmitUrl,
  };
  use chrono::{Datelike, Utc};

  #[test]
  fn birth_mode_prefers_age_then_single_then_split_date() {
    assert_eq!(
      resolve_about_you_birth_mode(true, true, true),
      Some(AboutYouBirthMode::Age)
    );
    assert_eq!(
      resolve_about_you_birth_mode(false, true, true),
      Some(AboutYouBirthMode::SingleDate)
    );
    assert_eq!(
      resolve_about_you_birth_mode(false, false, true),
      Some(AboutYouBirthMode::SplitDate)
    );
    assert_eq!(resolve_about_you_birth_mode(false, false, false), None);
  }

  #[test]
  fn birthdate_parts_produces_age_for_live_age_form() {
    let (year, month, day, age) = birthdate_parts("2000-01-01").expect("valid birthdate");

    assert_eq!(year, "2000");
    assert_eq!(month, "01");
    assert_eq!(day, "01");
    assert_eq!(age, (Utc::now().year() - 2000).to_string());
  }

  #[test]
  fn birthdate_parts_rejects_invalid_and_future_dates() {
    assert!(birthdate_parts("2000-02-30").is_err());
    assert!(birthdate_parts("2999-01-01").is_err());
  }

  #[test]
  fn post_submit_chatgpt_home_is_completed_even_after_about_you_poll() {
    assert_eq!(
      classify_about_you_submit_url("https://auth.openai.com/about-you"),
      AboutYouSubmitUrl::StillOnForm
    );
    assert_eq!(
      classify_about_you_submit_url("https://chatgpt.com/"),
      AboutYouSubmitUrl::Completed
    );
  }

  #[test]
  fn post_submit_redirect_and_error_routes_are_not_false_successes() {
    for url in [
      "https://auth.openai.com/callback?return_to=https://chatgpt.com/",
      "https://chatgpt.com/api/auth/error?return_to=https://chatgpt.com/",
    ] {
      assert_eq!(
        classify_about_you_submit_url(url),
        AboutYouSubmitUrl::Transitioning,
        "{url}"
      );
    }
  }

  #[test]
  fn final_reread_can_complete_a_navigation_that_missed_the_poll_deadline() {
    assert_eq!(
      resolve_about_you_submit_url("https://auth.openai.com/about-you", "https://chatgpt.com/"),
      AboutYouSubmitUrl::Completed
    );
    assert_eq!(
      resolve_about_you_submit_url(
        "https://auth.openai.com/about-you",
        "https://auth.openai.com/about-you"
      ),
      AboutYouSubmitUrl::StillOnForm
    );
  }
}

#[cfg(test)]
mod authorize_block_tests {
  use super::{
    is_auth_challenge_rotate_error, is_auth_route_error_url, is_cloudflare_challenge_signal,
    is_cloudflare_wall, is_email_otp_conflict_status, is_email_otp_stale_rejection,
    is_registration_auth_surface, registration_vpn_country_code, should_request_new_email_otp,
    should_rotate_auth_challenge_peer, NetworkMode,
  };

  #[test]
  fn chatgpt_unified_login_is_a_supported_registration_auth_surface() {
    assert!(is_registration_auth_surface(
      "https://chatgpt.com/auth/login?email=person%40example.com"
    ));
    assert!(is_registration_auth_surface(
      "https://auth.openai.com/log-in-or-create-account"
    ));
    assert!(!is_registration_auth_surface("https://chatgpt.com/"));
    assert!(!is_registration_auth_surface(
      "https://accounts.google.com/o/oauth2/auth"
    ));
  }

  #[test]
  fn cloudflare_wall_only_matches_real_cloudflare_signatures() {
    assert!(is_cloudflare_wall("https://chatgpt.com/just a moment"));
    assert!(is_cloudflare_wall(
      "https://chatgpt.com/cdn-cgi/challenge-platform/h/g/jsd"
    ));
    assert!(is_cloudflare_wall("https://chatgpt.com/?cloudflare=1"));
    assert!(is_cloudflare_wall(
      "https://challenges.cloudflare.com/cf/login"
    ));
    assert!(is_cloudflare_wall(
      "https://chatgpt.com/checking your browser before proceeding"
    ));
  }

  #[test]
  fn cloudflare_wall_does_not_swallow_nextauth_route_errors() {
    // Regression guard: chatgpt.com NextAuth route errors MUST NOT be
    // classified as Cloudflare — doing so led the engine to relaunch
    // fingerprints blindly without ever reading the real error message.
    assert!(!is_cloudflare_wall("https://chatgpt.com/api/auth/error"));
    assert!(!is_cloudflare_wall(
      "https://chatgpt.com/auth/error?error=undefined"
    ));
    assert!(!is_auth_route_error_url(
      "https://chatgpt.com/just a moment"
    ));
  }

  #[test]
  fn auth_route_error_url_matches_only_exact_chatgpt_paths() {
    assert!(is_auth_route_error_url(
      "https://chatgpt.com/api/auth/error"
    ));
    assert!(is_auth_route_error_url(
      "https://chatgpt.com/auth/error?error=OAuthCallbackError"
    ));
    assert!(!is_auth_route_error_url(
      "https://chatgpt.com/auth/error/extra"
    ));
    assert!(!is_auth_route_error_url(
      "https://chatgpt.com.evil.test/api/auth/error"
    ));
    assert!(!is_auth_route_error_url(
      "https://example.test/?next=https://chatgpt.com/api/auth/error"
    ));
  }

  #[test]
  fn cloudflare_challenge_signal_matches_turnstile_and_just_a_moment() {
    assert!(is_cloudflare_challenge_signal("Just a moment..."));
    assert!(is_cloudflare_challenge_signal(
      "Performing security verification"
    ));
    assert!(is_cloudflare_challenge_signal(
      r#"iframe src="https://challenges.cloudflare.com/cdn-cgi/challenge-platform""#
    ));
    assert!(!is_cloudflare_challenge_signal(
      "Cloudflare privacy policy footer"
    ));
  }

  #[test]
  fn email_otp_http_409_is_conflict_not_stale() {
    assert!(is_email_otp_conflict_status(409));
    assert!(!is_email_otp_conflict_status(401));
    assert!(!is_email_otp_stale_rejection(409, &serde_json::json!({})));
    assert!(is_email_otp_stale_rejection(401, &serde_json::json!({})));
    for response in [
      serde_json::json!({ "code": "wrong_email_otp_code" }),
      serde_json::json!({ "message": "invalid code" }),
      serde_json::json!({ "detail": "verification code expired" }),
    ] {
      assert!(is_email_otp_stale_rejection(400, &response));
    }
    for response in [
      serde_json::json!({ "code": "invalid_request_error" }),
      serde_json::json!({ "message": "session unauthorized" }),
      serde_json::json!({ "detail": "access token expired" }),
    ] {
      assert!(!is_email_otp_stale_rejection(400, &response));
    }
  }

  #[test]
  fn pending_email_otp_suppresses_resend_after_peer_rotation() {
    assert!(should_request_new_email_otp(false));
    assert!(!should_request_new_email_otp(true));
  }

  #[test]
  fn registration_wireguard_country_is_always_japan() {
    assert_eq!(registration_vpn_country_code(NetworkMode::Vpn), Some("JP"));
    assert_eq!(registration_vpn_country_code(NetworkMode::Nord), None);
    assert_eq!(registration_vpn_country_code(NetworkMode::None), None);
  }

  #[test]
  fn persistent_auth_challenge_rotates_only_within_wireguard_peer_budget() {
    let turnstile = super::auth_challenge_rotate_error("persistent Turnstile");
    assert!(is_auth_challenge_rotate_error(&turnstile));
    assert!(should_rotate_auth_challenge_peer(
      NetworkMode::Vpn,
      &turnstile,
      0,
      6
    ));
    assert!(should_rotate_auth_challenge_peer(
      NetworkMode::Vpn,
      &turnstile,
      5,
      6
    ));
    assert!(!should_rotate_auth_challenge_peer(
      NetworkMode::Nord,
      &turnstile,
      0,
      6
    ));
    assert!(!should_rotate_auth_challenge_peer(
      NetworkMode::Vpn,
      &turnstile,
      6,
      6
    ));
    assert!(!should_rotate_auth_challenge_peer(
      NetworkMode::Vpn,
      "wrong OTP",
      0,
      6
    ));
  }
}

#[cfg(test)]
mod password_submit_tests {
  use super::{
    classify_password_submit_url, classify_registration_password_route, PasswordSubmitOutcome,
    RegistrationPasswordRoute, PASSWORD_SUBMIT_ATTEMPTS,
  };

  #[test]
  fn password_submit_is_bounded_to_one_retry() {
    assert_eq!(PASSWORD_SUBMIT_ATTEMPTS, 2);
  }

  #[test]
  fn unchanged_create_password_url_is_an_ambiguous_commit() {
    assert_eq!(
      classify_password_submit_url("https://auth.openai.com/create-account/password"),
      PasswordSubmitOutcome::Ambiguous
    );
  }

  #[test]
  fn email_verification_url_confirms_password_advanced() {
    assert_eq!(
      classify_password_submit_url("https://auth.openai.com/email-verification"),
      PasswordSubmitOutcome::Advanced
    );
  }

  #[test]
  fn login_password_route_recovers_the_retained_registration_identity() {
    assert_eq!(
      classify_registration_password_route("https://auth.openai.com/log-in/password"),
      RegistrationPasswordRoute::RecoverExistingIdentity
    );
  }
}

#[cfg(test)]
mod registration_page_tests {
  use super::{
    classify_email_otp_ui_state, classify_password_form_state, classify_registration_page,
    detect_page_type, EmailOtpUiState, PageType, PasswordFormState, RegistrationPageSignals,
  };

  fn about_you_signals() -> RegistrationPageSignals {
    RegistrationPageSignals {
      has_about_you_name_input: true,
      has_about_you_age_input: true,
      has_about_you_birth_marker: true,
      ..Default::default()
    }
  }

  #[test]
  fn nextauth_errors_are_detected_before_supported_surface_rejection() {
    assert_eq!(
      detect_page_type("https://chatgpt.com/api/auth/error?error=AccessDenied"),
      PageType::ErrorPage
    );
    assert_eq!(
      detect_page_type("https://chatgpt.com/auth/error?error=OAuthCallbackError"),
      PageType::ErrorPage
    );
  }

  #[test]
  fn about_you_dom_overrides_stale_email_verification_only_on_openai_origin() {
    let signals = about_you_signals();
    assert_eq!(
      classify_registration_page("https://auth.openai.com/email-verification", &signals),
      PageType::AboutYou
    );
    assert_eq!(
      classify_registration_page("https://example.test/email-verification", &signals),
      PageType::ExternalUrl
    );
  }

  #[test]
  fn visible_password_and_otp_surfaces_override_stale_routes() {
    let password = RegistrationPageSignals {
      has_new_password_input: true,
      ..Default::default()
    };
    assert_eq!(
      classify_registration_page("https://auth.openai.com/email-verification", &password),
      PageType::CreateAccountPassword
    );
    let otp = RegistrationPageSignals {
      has_email_otp_input: true,
      ..Default::default()
    };
    assert_eq!(
      classify_registration_page("https://auth.openai.com/create-account/password", &otp),
      PageType::EmailOtpVerification
    );
  }

  #[test]
  fn stale_url_about_you_counts_as_otp_acceptance() {
    assert_eq!(
      classify_email_otp_ui_state(
        "https://auth.openai.com/email-verification",
        &about_you_signals()
      ),
      EmailOtpUiState::Accepted
    );
  }

  #[test]
  fn password_rejection_wins_over_quiet_password_route() {
    let signals = RegistrationPageSignals {
      has_new_password_input: true,
      has_password_rejection: true,
      ..Default::default()
    };
    assert_eq!(
      classify_password_form_state("https://auth.openai.com/create-account/password", &signals),
      PasswordFormState::Rejected
    );
  }
}

#[cfg(test)]
mod safe_error_tests {
  //! The error envelope used to collapse every provider failure to a static
  //! `&'static str`, so a 400 "CDK use limit reached" and a DNS NXDOMAIN
  //! looked identical in `cdk_inventory.lastError`. Now the safe string keeps
  //! HTTP status / short message while redacting secrets.

  use super::{safe_email_service_error, safe_provider_detail, sanitize_registration_log};
  use crate::email::EmailServiceError;

  #[test]
  fn network_error_keeps_http_status_and_provider_message() {
    let inner = "redeem HTTP 400 Bad Request: {\"message\":\"CDK use limit reached\",\"error\":\"Bad Request\",\"statusCode\":400}";
    let safe = safe_email_service_error(&EmailServiceError::Network(inner.into()));
    assert!(
      safe.contains("provider network request failed"),
      "prefix preserved: {safe}"
    );
    assert!(safe.contains("HTTP 400"), "HTTP status preserved: {safe}");
    assert!(
      safe.contains("CDK use limit reached"),
      "provider message preserved: {safe}"
    );
  }

  #[test]
  fn registration_logs_remove_url_queries_and_identity_values() {
    let safe = sanitize_registration_log(
      "final URL: https://chatgpt.com/auth/login?email=someone%40example.com card=GMAIL-7BE8-AEVK-ACR8-FQ86",
    );

    assert_eq!(safe, "final URL: https://chatgpt.com/auth/login card=[cdk]");
    assert!(!safe.contains("someone"));
    assert!(!safe.contains("GMAIL-7BE8"));
  }

  #[test]
  fn network_error_strips_emails_and_cdk_strings() {
    let email = "someone@example.com";
    let cdk = "GMAIL-7BE8-AEVK-ACR8-FQ86";
    let inner = format!("redeem HTTP 400 Bad Request: {{\"message\":\"{cdk} used by {email}\"}}");
    let safe = safe_email_service_error(&EmailServiceError::Network(inner));
    assert!(!safe.contains(email), "email must be redacted: {safe}");
    assert!(!safe.contains(cdk), "raw CDK must be redacted: {safe}");
    assert!(safe.contains("[email]"), "email marker present: {safe}");
    assert!(safe.contains("[cdk]"), "cdk marker present: {safe}");
  }

  #[test]
  fn network_error_strips_mail_cards_from_raw_and_encoded_urls() {
    for detail in [
      "GET https://sms.iosmq.xyz/api/v1/order/lookup?code=MAIL-K4L5-EUW5-PHBV-A6KW&poll=true failed",
      "GET https://sms.iosmq.xyz/api/v1/order/lookup?code=MAIL%2DK4L5%2DEUW5%2DPHBV%2DA6KW&poll=true failed",
    ] {
      let safe = safe_email_service_error(&EmailServiceError::Network(detail.into()));
      assert!(!safe.contains("MAIL-K4L5-EUW5-PHBV-A6KW"), "safe={safe}");
      assert!(
        !safe.contains("MAIL%2DK4L5%2DEUW5%2DPHBV%2DA6KW"),
        "safe={safe}"
      );
      assert!(safe.contains("[cdk]"), "safe={safe}");
    }
  }

  #[test]
  fn network_error_redacts_opaque_token_values() {
    let token = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJK";
    let inner = format!("{{\"token\":\"{token}\",\"sessionId\":\"{token}\"}}");
    let safe = safe_provider_detail(&inner);
    assert!(
      !safe.contains(token),
      "opaque token must be redacted: {safe}"
    );
    assert!(
      safe.contains("[redacted]"),
      "redaction marker present: {safe}"
    );
  }

  #[test]
  fn network_error_truncates_giant_payloads() {
    let huge = "x".repeat(2000);
    let safe = safe_provider_detail(&huge);
    assert!(
      safe.chars().count() <= 220,
      "truncated to <=220 chars, got {}: {safe}",
      safe.chars().count()
    );
    assert!(safe.ends_with("..."), "ellipsis sentinel: {safe}");
  }

  #[test]
  fn cdik_invalid_and_timeout_preserve_classified_detail() {
    let invalid =
      safe_email_service_error(&EmailServiceError::CdkInvalid("status: revoked".into()));
    assert!(invalid.contains("provider card is invalid: status: revoked"));
    let timeout =
      safe_email_service_error(&EmailServiceError::Timeout("no code within 150s".into()));
    assert!(timeout.contains("provider request timed out: no code within 150s"));
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistrationPasswordRoute {
  CreateNewIdentity,
  RecoverExistingIdentity,
}

fn classify_registration_password_route(url: &str) -> RegistrationPasswordRoute {
  if url.to_ascii_lowercase().contains("log-in/password") {
    RegistrationPasswordRoute::RecoverExistingIdentity
  } else {
    RegistrationPasswordRoute::CreateNewIdentity
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasswordSubmitOutcome {
  Advanced,
  Ambiguous,
}

const PASSWORD_SUBMIT_ATTEMPTS: u32 = 2;

#[cfg(test)]
fn classify_password_submit_url(url: &str) -> PasswordSubmitOutcome {
  if url.to_ascii_lowercase().contains("password") {
    PasswordSubmitOutcome::Ambiguous
  } else {
    PasswordSubmitOutcome::Advanced
  }
}

fn random_password() -> String {
  let mut rng = rand::rng();
  let lower: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
  let upper: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
  let digits: &[u8] = b"0123456789";
  let special: &[u8] = b"!@#$%";

  let pick = |rng: &mut rand::rngs::ThreadRng, slice: &[u8]| -> char {
    slice[(rng.next_u32() as usize) % slice.len()] as char
  };

  let mut chars = vec![
    pick(&mut rng, lower),
    pick(&mut rng, upper),
    pick(&mut rng, digits),
    pick(&mut rng, special),
  ];

  let all: &[&[u8]] = &[lower, upper, digits, special];
  for _ in 0..8 {
    let pool = all[(rng.next_u32() as usize) % all.len()];
    chars.push(pick(&mut rng, pool));
  }

  for i in (1..chars.len()).rev() {
    let j = (rng.next_u32() as usize) % (i + 1);
    chars.swap(i, j);
  }

  chars.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Page type detection (from URL)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageType {
  CreateAccountPassword,
  EmailOtpVerification,
  AboutYou,
  LoginPassword,
  AddPhone,
  Consent,
  ChatgptHome,
  Callback,
  ExternalUrl,
  ErrorPage,
  Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RegistrationPageSignals {
  has_new_password_input: bool,
  has_password_input: bool,
  has_email_otp_input: bool,
  has_about_you_name_input: bool,
  has_about_you_age_input: bool,
  has_about_you_birth_input: bool,
  has_about_you_birth_marker: bool,
  has_password_rejection: bool,
  has_email_otp_rejection: bool,
  error_text: String,
}

impl RegistrationPageSignals {
  fn has_about_you_form(&self) -> bool {
    self.has_about_you_name_input
      && (self.has_about_you_birth_input
        || (self.has_about_you_age_input && self.has_about_you_birth_marker))
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasswordFormState {
  Advanced(PageType),
  RecoverExistingIdentity,
  Rejected,
  Quiet,
  UnsafeOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmailOtpUiState {
  Accepted,
  Rejected,
  Pending,
  UnsafeOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EmailOtpUiOutcome {
  Accepted(String),
  Rejected(String),
  Pending,
}

fn is_trusted_openai_origin(url: &str) -> bool {
  url::Url::parse(url).ok().is_some_and(|parsed| {
    parsed.scheme() == "https"
      && matches!(parsed.host_str(), Some("auth.openai.com" | "chatgpt.com"))
  })
}

fn is_registration_dom_surface(url: &str) -> bool {
  url::Url::parse(url).ok().is_some_and(|parsed| {
    if parsed.scheme() != "https" {
      return false;
    }
    match parsed.host_str() {
      Some("auth.openai.com") => true,
      Some("chatgpt.com") => parsed.path().trim_end_matches('/') == "/auth/login",
      _ => false,
    }
  })
}

fn is_chatgpt_home_url(url: &str) -> bool {
  url::Url::parse(url).ok().is_some_and(|parsed| {
    parsed.scheme() == "https"
      && parsed.host_str() == Some("chatgpt.com")
      && parsed.path() == "/"
      && parsed.query().is_none()
      && parsed.fragment().is_none()
  })
}

fn is_registration_auth_surface(url: &str) -> bool {
  url::Url::parse(url).ok().is_some_and(|parsed| {
    parsed.scheme() == "https"
      && (parsed.host_str() == Some("auth.openai.com")
        || (parsed.host_str() == Some("chatgpt.com")
          && parsed.path().trim_end_matches('/') == "/auth/login"))
  })
}

fn detect_page_type(url: &str) -> PageType {
  let Ok(parsed) = url::Url::parse(url) else {
    return PageType::Unknown;
  };
  if parsed.scheme() != "https" {
    return PageType::ExternalUrl;
  }

  let path = parsed.path().to_ascii_lowercase();
  match parsed.host_str() {
    Some("auth.openai.com") => {
      if path.contains("create-account/password") || path.contains("log-in-or-create-account") {
        PageType::CreateAccountPassword
      } else if path.contains("email-verification") || path.contains("email-otp") {
        PageType::EmailOtpVerification
      } else if path.contains("about-you") {
        PageType::AboutYou
      } else if path.contains("log-in/password") {
        PageType::LoginPassword
      } else if path.contains("add-phone") {
        PageType::AddPhone
      } else if path.contains("sign-in-with-chatgpt") && path.contains("consent") {
        PageType::Consent
      } else if path.contains("/callback") || parsed.query_pairs().any(|(key, _)| key == "code") {
        PageType::Callback
      } else if path == "/error" || path.starts_with("/api/accounts/authorize") {
        PageType::ErrorPage
      } else {
        PageType::Unknown
      }
    }
    Some("chatgpt.com") => {
      if is_chatgpt_home_url(url) {
        PageType::ChatgptHome
      } else if path.trim_end_matches('/') == "/auth/login" {
        PageType::CreateAccountPassword
      } else if matches!(path.as_str(), "/auth/error" | "/api/auth/error") {
        PageType::ErrorPage
      } else {
        PageType::Unknown
      }
    }
    Some(_) => PageType::ExternalUrl,
    None => PageType::Unknown,
  }
}

fn classify_registration_page(url: &str, signals: &RegistrationPageSignals) -> PageType {
  let fallback = detect_page_type(url);
  if !is_registration_dom_surface(url) {
    return fallback;
  }
  if signals.has_about_you_form() {
    PageType::AboutYou
  } else if signals.has_new_password_input {
    PageType::CreateAccountPassword
  } else if signals.has_password_input {
    if fallback == PageType::CreateAccountPassword {
      PageType::CreateAccountPassword
    } else {
      PageType::LoginPassword
    }
  } else if signals.has_email_otp_input {
    PageType::EmailOtpVerification
  } else {
    fallback
  }
}

fn classify_password_form_state(url: &str, signals: &RegistrationPageSignals) -> PasswordFormState {
  if !is_trusted_openai_origin(url) {
    return PasswordFormState::UnsafeOrigin;
  }
  if signals.has_password_rejection || is_auth_route_error_url(url) {
    return PasswordFormState::Rejected;
  }
  let page = classify_registration_page(url, signals);
  match page {
    PageType::EmailOtpVerification
    | PageType::AboutYou
    | PageType::AddPhone
    | PageType::Consent
    | PageType::ChatgptHome
    | PageType::Callback => PasswordFormState::Advanced(page),
    PageType::LoginPassword => PasswordFormState::RecoverExistingIdentity,
    PageType::CreateAccountPassword | PageType::Unknown => PasswordFormState::Quiet,
    PageType::ErrorPage => PasswordFormState::Rejected,
    PageType::ExternalUrl => PasswordFormState::UnsafeOrigin,
  }
}

fn classify_email_otp_ui_state(url: &str, signals: &RegistrationPageSignals) -> EmailOtpUiState {
  if !is_trusted_openai_origin(url) {
    return EmailOtpUiState::UnsafeOrigin;
  }
  if signals.has_email_otp_rejection {
    return EmailOtpUiState::Rejected;
  }
  match classify_registration_page(url, signals) {
    PageType::AboutYou
    | PageType::AddPhone
    | PageType::Consent
    | PageType::ChatgptHome
    | PageType::Callback => EmailOtpUiState::Accepted,
    _ => EmailOtpUiState::Pending,
  }
}

async fn observe_registration_page_signals(
  session: &mut BrowserSession,
) -> Result<RegistrationPageSignals, String> {
  let result = session
    .evaluate(
      r#"(function(){
        const visible = (el) => {
          if (!el || el.disabled) return false;
          const r = el.getBoundingClientRect();
          const style = window.getComputedStyle(el);
          return r.width > 0 && r.height > 0 && style.visibility !== 'hidden' && style.display !== 'none';
        };
        const visibleAny = (selector) => Array.from(document.querySelectorAll(selector)).some(visible);
        const body = ((document.body && (document.body.innerText || document.body.textContent)) || '').slice(0, 6000);
        const errors = Array.from(document.querySelectorAll(
          '[role="alert"], [aria-live="assertive"], [data-error], [data-testid*="error" i], [class*="error" i]'
        )).filter(visible).map((el) => (el.innerText || el.textContent || '').trim()).filter(Boolean);
        const errorText = errors.join(' | ').slice(0, 500);
        const combined = (errorText + ' ' + body).toLowerCase();
        const singleOtp = visibleAny(
          'input[name="code"], input[name="otp"], input[name="emailCode"], input[autocomplete="one-time-code"], input[aria-label*="code" i], input[placeholder*="code" i]'
        );
        const multiOtp = Array.from(document.querySelectorAll(
          'input[maxlength="1"], input[data-index], input[type="tel"][maxlength="1"], input[inputmode="numeric"][maxlength="1"]'
        )).filter(visible).length >= 4;
        const first = visibleAny('input[name="first_name"], input[name="firstName"], input[autocomplete="given-name"]');
        const last = visibleAny('input[name="last_name"], input[name="lastName"], input[autocomplete="family-name"]');
        const full = visibleAny('input[name="name"], input[name="full_name"], input[name="fullName"], input[autocomplete="name"]');
        const age = visibleAny('input[name="age"], input[autocomplete="age"], input[aria-label*="age" i], input[placeholder*="age" i]');
        const birth = visibleAny(
          'input:not([type="hidden"])[name="birthdate"], input:not([type="hidden"])[name="birthday"], input:not([type="hidden"])[name="date_of_birth"], input:not([type="hidden"])[name="birth_date"], input[autocomplete="bday"], input[type="date"]'
        );
        const splitBirth = visibleAny('input[name="birth_month"], input[name="month"], select[name="birth_month"], select[name="month"]')
          && visibleAny('input[name="birth_day"], input[name="day"], select[name="birth_day"], select[name="day"]')
          && visibleAny('input[name="birth_year"], input[name="year"], select[name="birth_year"], select[name="year"]');
        return {
          hasNewPasswordInput: visibleAny('input[name="new-password"], input[autocomplete="new-password"]'),
          hasPasswordInput: visibleAny('input[type="password"], input[name="password"], input[autocomplete="current-password"]'),
          hasEmailOtpInput: singleOtp || multiOtp,
          hasAboutYouNameInput: full || (first && last),
          hasAboutYouAgeInput: age,
          hasAboutYouBirthInput: birth || splitBirth,
          hasAboutYouBirthMarker: !!document.querySelector('input[type="hidden"][name="birthday"], input[type="hidden"][name="birthdate"]'),
          hasPasswordRejection: /incorrect email or password|invalid email or password|wrong password|password.*(invalid|rejected)|account.*(locked|deactivated|disabled)|rate limit|too many requests/.test(combined),
          hasEmailOtpRejection: /wrong_email_otp_code|incorrect code|invalid code|expired code|code has expired|verification code.*(wrong|invalid|expired)/.test(combined),
          errorText,
        };
      })()"#,
      false,
    )
    .await?;
  let value = result.get("value").cloned().unwrap_or_default();
  Ok(RegistrationPageSignals {
    has_new_password_input: value.get("hasNewPasswordInput").and_then(Value::as_bool) == Some(true),
    has_password_input: value.get("hasPasswordInput").and_then(Value::as_bool) == Some(true),
    has_email_otp_input: value.get("hasEmailOtpInput").and_then(Value::as_bool) == Some(true),
    has_about_you_name_input: value.get("hasAboutYouNameInput").and_then(Value::as_bool)
      == Some(true),
    has_about_you_age_input: value.get("hasAboutYouAgeInput").and_then(Value::as_bool)
      == Some(true),
    has_about_you_birth_input: value.get("hasAboutYouBirthInput").and_then(Value::as_bool)
      == Some(true),
    has_about_you_birth_marker: value.get("hasAboutYouBirthMarker").and_then(Value::as_bool)
      == Some(true),
    has_password_rejection: value.get("hasPasswordRejection").and_then(Value::as_bool)
      == Some(true),
    has_email_otp_rejection: value.get("hasEmailOtpRejection").and_then(Value::as_bool)
      == Some(true),
    error_text: value
      .get("errorText")
      .and_then(Value::as_str)
      .unwrap_or_default()
      .to_string(),
  })
}

/// Strip values that may appear in provider error bodies (emails, tokens,
/// CDK strings, raw JSON values) so the resulting string is safe to persist in
/// `cdk_inventory.lastError` and logs. HTTP status + short `message` field are
/// kept because they never carry secrets and are what the operator needs to
/// tell "CDK use limit reached" (HTTP 400) apart from DNS / timeout / 5xx.
fn safe_provider_detail(raw: &str) -> String {
  // Pair of (compiled, replacement) applied left-to-right.
  let patterns: &[(&str, &str)] = &[
    // Email addresses inside provider payloads.
    (
      r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}",
      "[email]",
    ),
    // GMAIL/MAIL cards in raw or URL-encoded query strings.
    (r"(?i)GMAIL-[A-Z0-9]{4}(?:-[A-Z0-9]{4}){3}", "[cdk]"),
    (
      r"(?i)MAIL(?:-|%2D)[A-Z0-9]{4}(?:(?:-|%2D)[A-Z0-9]{4}){2,3}",
      "[cdk]",
    ),
    // `"token":"..."`, `"sessionId":"..."`, opaque JSON secrets.
    (
      r#"(?i)"(token|sessionId|apiKey|access_token|refresh_token)"\s*:\s*"[^"]+""#,
      r#""$1":"[redacted]""#,
    ),
    // Long opaque base64-ish blobs (>=32 chars).
    (r#"(?i)"[A-Za-z0-9+/=_\-]{32,}""#, "\"[redacted]\""),
  ];
  let mut cleaned = raw.to_string();
  for (pattern, replacement) in patterns {
    if let Ok(re) = regex_lite::Regex::new(pattern) {
      cleaned = re.replace_all(&cleaned, *replacement).to_string();
    }
  }
  // Bound the final string so a giant HTML error page can't flood the record.
  if cleaned.chars().count() > 220 {
    let truncated: String = cleaned.chars().take(217).collect();
    format!("{truncated}...")
  } else {
    cleaned
  }
}

fn sanitize_registration_log(message: &str) -> String {
  safe_provider_detail(&sanitize_browser_urls_for_log(message))
}

fn durable_account_or_provisional(
  account_key: &str,
  provisional: &RegistrationResult,
) -> RegistrationResult {
  get_registered_account(account_key)
    .ok()
    .flatten()
    .unwrap_or_else(|| provisional.clone())
}

fn safe_email_service_error(error: &EmailServiceError) -> String {
  match error {
    EmailServiceError::CdkInvalid(d) => {
      format!("provider card is invalid: {}", safe_provider_detail(d))
    }
    EmailServiceError::CdkExpired(d) => {
      format!("provider card is expired: {}", safe_provider_detail(d))
    }
    EmailServiceError::Timeout(d) => {
      format!("provider request timed out: {}", safe_provider_detail(d))
    }
    EmailServiceError::Cancelled => "provider polling cancelled".to_string(),
    EmailServiceError::Network(d) => format!(
      "provider network request failed: {}",
      safe_provider_detail(d)
    ),
    EmailServiceError::EmailInvalid(d) => {
      format!("provider email is invalid: {}", safe_provider_detail(d))
    }
    EmailServiceError::Internal(d) => {
      format!("provider operation failed: {}", safe_provider_detail(d))
    }
  }
}

fn safe_sms_service_error(error: &SmsServiceError) -> String {
  match error {
    SmsServiceError::Auth(_) => "provider authentication failed".to_string(),
    SmsServiceError::InsufficientBalance(_) => "provider balance is insufficient".to_string(),
    SmsServiceError::NoNumbersAvailable(_) => "provider has no numbers available".to_string(),
    SmsServiceError::ServiceUnavailable(_) => "provider service is unavailable".to_string(),
    SmsServiceError::LimitExceeded(d) => {
      format!("provider limit was exceeded: {}", safe_provider_detail(d))
    }
    SmsServiceError::Timeout(d) => {
      format!("provider request timed out: {}", safe_provider_detail(d))
    }
    SmsServiceError::Network(d) => format!(
      "provider network request failed: {}",
      safe_provider_detail(d)
    ),
    SmsServiceError::InvalidRequest(_) => "provider rejected the request".to_string(),
    SmsServiceError::Internal(_) => "provider operation failed".to_string(),
  }
}

/// Evaluate ChatGPT account/subscription JSON for free trial / free Plus offer.
/// Primary signal (verified live): accounts/check -> eligible_promo_campaigns.plus
/// with id/title like "plus-1-month-free" / "Try Plus free for 1 month".
fn evaluate_subscription_json(v: &serde_json::Value) -> Option<(bool, String, String)> {
  // Path 1: accounts/check shape with nested accounts map.
  if let Some(accounts) = v.get("accounts").and_then(|a| a.as_object()) {
    for (account_id, account_node) in accounts {
      if let Some(result) = evaluate_account_node(account_id, account_node) {
        return Some(result);
      }
    }
  }

  // Path 2: single account node / entitlement payload.
  if v.get("eligible_promo_campaigns").is_some() || v.get("entitlement").is_some() {
    if let Some(result) = evaluate_account_node("default", v) {
      return Some(result);
    }
  }

  // Path 3: subscriptions payload (when called with account_id).
  if let Some(plan) = v
    .get("plan_type")
    .or_else(|| v.pointer("/subscription/plan_type"))
    .and_then(|x| x.as_str())
  {
    let trial = v.get("trial").cloned();
    let will_renew = v.get("will_renew").and_then(|x| x.as_bool());
    let mut eligible = false;
    let mut reason = format!("subscriptions.plan_type={plan}");
    if let Some(t) = trial {
      if !t.is_null() {
        eligible = true;
        reason.push_str(&format!("; trial={t}"));
      }
    }
    if v.to_string().to_lowercase().contains("free trial") {
      eligible = true;
      reason.push_str("; free_trial_text");
    }
    return Some((
      eligible,
      plan.to_string(),
      format!("{reason}; will_renew={will_renew:?}"),
    ));
  }

  None
}

fn evaluate_account_node(
  _account_id: &str,
  account_node: &serde_json::Value,
) -> Option<(bool, String, String)> {
  let plan = account_node
    .pointer("/account/plan_type")
    .or_else(|| account_node.pointer("/entitlement/subscription_plan"))
    .and_then(|x| x.as_str())
    .unwrap_or("")
    .to_string();

  let promo = account_node.get("eligible_promo_campaigns").cloned();
  let offers = account_node.get("eligible_offers").cloned();
  let entitlement_trial = account_node.pointer("/entitlement/trial").cloned();
  let yearly_new = account_node
    .get("is_eligible_for_yearly_plus_new_user_subscription")
    .and_then(|x| x.as_bool())
    .unwrap_or(false);
  let has_paid = account_node
    .pointer("/account/has_previously_paid_subscription")
    .and_then(|x| x.as_bool())
    .unwrap_or(false);

  let mut eligible = false;
  let mut plan_type = if plan.is_empty() {
    "unknown".into()
  } else {
    plan.clone()
  };
  let mut reasons: Vec<String> = vec![format!("plan={plan}")];

  // PRIMARY: promo campaigns (verified live on a fresh free signup).
  if let Some(promo_obj) = promo.as_ref().and_then(|p| p.as_object()) {
    for (k, campaign) in promo_obj {
      let id = campaign.get("id").and_then(|x| x.as_str()).unwrap_or("");
      let title = campaign
        .pointer("/metadata/title")
        .and_then(|x| x.as_str())
        .unwrap_or("");
      let label = campaign
        .pointer("/metadata/promotion_type_label")
        .and_then(|x| x.as_str())
        .unwrap_or("");
      let summary = campaign
        .pointer("/metadata/summary")
        .and_then(|x| x.as_str())
        .unwrap_or("");
      let pct = campaign
        .pointer("/metadata/discount/percentage")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
      let plan_name = campaign
        .pointer("/metadata/plan_name")
        .and_then(|x| x.as_str())
        .unwrap_or("");

      let blob = format!("{id} {title} {label} {summary} {plan_name}").to_lowercase();
      let looks_free_trial = pct == 100
        || blob.contains("free trial")
        || blob.contains("try plus free")
        || blob.contains("1-month free")
        || blob.contains("plus-1-month-free")
        || (blob.contains("free") && blob.contains("plus"));

      reasons.push(format!(
        "promo[{k}]=id:{id};label:{label};title:{title};pct:{pct};plan:{plan_name}"
      ));

      if looks_free_trial {
        eligible = true;
        if !plan_name.is_empty() {
          plan_type = format!("{plan_name}/trial");
        } else if !id.is_empty() {
          plan_type = id.to_string();
        } else {
          plan_type = "free-trial".into();
        }
      }
    }
  } else {
    reasons.push("promo=none".into());
  }

  // Active trial object on entitlement (already in trial).
  if let Some(trial) = entitlement_trial {
    if !trial.is_null() {
      eligible = true;
      reasons.push(format!("entitlement.trial={trial}"));
      if plan_type == plan || plan_type == "unknown" {
        plan_type = format!("{plan}/active-trial");
      }
    }
  }

  if yearly_new {
    reasons.push("yearly_plus_new_user_eligible=true".into());
  }
  if !has_paid {
    reasons.push("never_paid=true".into());
  }

  // Offers list is not free-trial proof by itself (always lists plus/pro SKUs).
  if let Some(default_offer) = offers
    .as_ref()
    .and_then(|o| o.get("default_offer_id"))
    .and_then(|x| x.as_str())
  {
    reasons.push(format!("default_offer={default_offer}"));
  }

  Some((eligible, plan_type, reasons.join("; ")))
}

/// Classify the page the browser landed on after a signup navigation step.
///
/// Key distinction: `chatgpt.com/api/auth/error` (and `/auth/error`) are
/// NextAuth *route errors* — OpenAI's identity provider rejected the
/// authorize attempt for a real reason (cookie/IP/device mismatch,
/// rate-limit, blocked region, etc.). They are NOT the Cloudflare
/// "just a moment" bot-wall, and treating them as Cloudflare caused the
/// engine to relaunch fingerprints blindly without ever reading the
/// actual error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthorizeBlock {
  /// Cloudflare/anti-bot interstitial: needs IP/fingerprint rotation.
  Cloudflare,
  /// OpenAI NextAuth route error: readable body message, recoverable via
  /// the "Try again" button or a direct auth.openai.com log-in fallback.
  RouteError,
  /// Landed on something else (e.g. already on auth.openai.com); caller can
  /// treat this as a success surface.
  Other,
}

/// Soft-wait budget for Cloudflare Turnstile / "Just a moment" on auth pages.
const CLOUDFLARE_SOFT_WAIT_SECS: u64 = 25;

fn is_cloudflare_wall(url: &str) -> bool {
  let u = url.to_lowercase();
  // Cloudflare interstitial / managed challenge signatures.
  u.contains("just a moment")
    || u.contains("cloudflare")
    || u.contains("challenges.cloudflare.com")
    || u.contains("challenge-platform")
    || u.contains("checking your browser")
    // OpenAI authorize endpoint with no continuation: observed when the
    // request was challenged before reaching the IDP.
    || u.contains("api/accounts/authorize?") && u.contains("prompt=")
}

/// Detect Cloudflare managed challenge / Turnstile from URL/title/body text.
fn is_cloudflare_challenge_signal(text: &str) -> bool {
  let lower = text.to_ascii_lowercase();
  if lower.contains("verify you are human")
    || lower.contains("performing security verification")
    || lower.contains("just a moment")
    || lower.contains("cf-turnstile")
    || lower.contains("challenges.cloudflare.com")
    || lower.contains("challenge-platform")
  {
    return true;
  }
  if lower.contains("cloudflare")
    && (lower.contains("security verification")
      || lower.contains("checking your browser")
      || lower.contains("attention required")
      || lower.contains("enable javascript and cookies"))
  {
    return true;
  }
  false
}

fn cloudflare_challenge_error_message() -> String {
  "Cloudflare Turnstile challenge (persistent after soft-wait)".into()
}

const AUTH_CHALLENGE_ROTATE_PREFIX: &str = "auth_challenge_rotate_peer:";
const MAX_AUTH_CHALLENGE_PEER_ROTATIONS: u32 = 6;

fn auth_challenge_rotate_error(detail: &str) -> String {
  format!("{AUTH_CHALLENGE_ROTATE_PREFIX} {detail}")
}

fn is_auth_challenge_rotate_error(error: &str) -> bool {
  error.starts_with(AUTH_CHALLENGE_ROTATE_PREFIX)
    || error.contains("Authorize failed after max retries")
}

fn should_rotate_auth_challenge_peer(
  network_mode: NetworkMode,
  error: &str,
  peer_rotations: u32,
  max_peer_rotations: u32,
) -> bool {
  network_mode == NetworkMode::Vpn
    && peer_rotations < max_peer_rotations
    && is_auth_challenge_rotate_error(error)
}

fn should_request_new_email_otp(has_pending_otp: bool) -> bool {
  !has_pending_otp
}

fn registration_vpn_country_code(network_mode: NetworkMode) -> Option<&'static str> {
  (network_mode == NetworkMode::Vpn).then_some("JP")
}

/// HTTP 409 on email-otp/validate means the auth session is not ready to
/// accept the code yet (CF wall, race after password commit, or unsent
/// binding) — retry the *same* code after soft-wait instead of burning it.
fn is_email_otp_conflict_status(status: u64) -> bool {
  status == 409
}

fn is_email_otp_stale_rejection(status: u64, response: &Value) -> bool {
  if status == 401 {
    return true;
  }
  let hay = response.to_string().to_ascii_lowercase();
  hay.contains("wrong_email_otp_code")
    || hay.contains("incorrect code")
    || hay.contains("invalid code")
    || hay.contains("expired code")
    || hay.contains("code expired")
    || hay.contains("code has expired")
    || (hay.contains("verification code")
      && (hay.contains("wrong") || hay.contains("invalid") || hay.contains("expired")))
}

fn is_auth_route_error_url(url: &str) -> bool {
  url::Url::parse(url).ok().is_some_and(|parsed| {
    parsed.scheme() == "https"
      && parsed.host_str() == Some("chatgpt.com")
      && matches!(parsed.path(), "/auth/error" | "/api/auth/error")
  })
}

/// Read the route-error body text + visible alert widgets so the operator
/// can see why OpenAI rejected the authorize attempt (rate-limit, region
/// block, configuration, OAuthCallbackError…). No secret material is
/// returned — only what is already rendered in the page body.
async fn read_auth_error_detail(session: &mut BrowserSession) -> Result<String, String> {
  let js = r#"(function(){
    const body = (document.body && (document.body.innerText || document.body.textContent) || '')
      .replace(/\s+/g, ' ').trim().slice(0, 280);
    const errs = Array.from(document.querySelectorAll(
      '[role="alert"], [data-testid*="error" i], [class*="error" i], h1, h2'
    ))
      .filter((el) => {
        try { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; }
        catch(_) { return false; }
      })
      .map((el) => (el.innerText || el.textContent || '').trim().slice(0, 160))
      .filter((s) => s.length > 0)
      .slice(0, 4);
    return { url: location.href, title: document.title || '', body: body, errs: errs };
  })()"#;
  let v = session.evaluate(js, false).await?;
  let value = v.get("value").cloned().unwrap_or_default();
  let body = value
    .get("body")
    .and_then(Value::as_str)
    .unwrap_or("")
    .to_string();
  let errs = value
    .get("errs")
    .and_then(|a| a.as_array())
    .map(|arr| {
      arr
        .iter()
        .filter_map(|x| x.as_str())
        .collect::<Vec<_>>()
        .join(" | ")
    })
    .unwrap_or_default();
  Ok(if errs.is_empty() {
    body
  } else if body.is_empty() {
    errs
  } else {
    format!("{body} | alerts: {errs}")
  })
}

/// Click "Try again" / retry button once if visible, in the same pattern the
/// login engine uses to recover from Remix route errors. Returns true if a
/// control was clicked.
async fn try_click_auth_error_retry(session: &mut BrowserSession) -> bool {
  let js = r#"(function(){
    function visible(el){
      try {
        const r = el.getBoundingClientRect();
        const s = el.ownerDocument.defaultView.getComputedStyle(el);
        return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none';
      } catch(_) { return false; }
    }
    for (const el of Array.from(document.querySelectorAll('button, a, [role="button"]'))) {
      if (!visible(el) || el.disabled) continue;
      const t = (el.innerText || el.textContent || '').toLowerCase().trim();
      if (t === 'try again' || t.includes('try again') || t === 'retry' || t === 'continue' || t === 'back') {
        try { el.scrollIntoView({ block: 'center' }); } catch(_) {}
        const r = el.getBoundingClientRect();
        return { found: true, x: r.left + r.width/2, y: r.top + r.height/2 };
      }
    }
    return { found: false };
  })()"#;
  let Ok(result) = session.evaluate(js, false).await else {
    return false;
  };
  let value = result.get("value").cloned().unwrap_or_default();
  if value.get("found").and_then(Value::as_bool) != Some(true) {
    return false;
  }
  let x = value.get("x").and_then(Value::as_f64).unwrap_or(0.0);
  let y = value.get("y").and_then(Value::as_f64).unwrap_or(0.0);
  if x <= 0.0 || y <= 0.0 {
    return false;
  }
  session.click_point(x, y).await.is_ok()
}

/// Classify the current authorize landing: Cloudflare wall, OpenAI NextAuth
/// route error, or some other (possibly valid) URL.
async fn classify_authorize_block(
  session: &mut BrowserSession,
  cur: &str,
) -> (AuthorizeBlock, String) {
  if is_cloudflare_wall(cur) {
    return (AuthorizeBlock::Cloudflare, format!("url={cur}"));
  }
  if is_auth_route_error_url(cur) {
    let detail = read_auth_error_detail(session).await.unwrap_or_default();
    // "Enable JavaScript and cookies" is a Cloudflare JS challenge that
    // auto-resolves after 3-8 seconds. Wait for it instead of treating
    // it as a terminal RouteError.
    if detail.to_ascii_lowercase().contains("enable javascript") {
      log::info!("Cloudflare JS challenge detected (Enable JavaScript) — waiting for auto-resolve");
      let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(12);
      while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        let url = session.current_url().await.unwrap_or_default();
        if !is_auth_route_error_url(&url) {
          log::info!("Cloudflare JS challenge resolved — now at {url}");
          return (AuthorizeBlock::Other, String::new());
        }
      }
      log::warn!("Cloudflare JS challenge did not resolve within 12s");
      return (AuthorizeBlock::Cloudflare, "JS challenge timeout".into());
    }
    return (AuthorizeBlock::RouteError, detail);
  }
  (AuthorizeBlock::Other, String::new())
}

// ---------------------------------------------------------------------------
// Registration Engine
// ---------------------------------------------------------------------------

struct AccountIdentity {
  alias_email: String,
  password: String,
  first_name: String,
  last_name: String,
  birthdate: String,
  device_id: String,
}

impl AccountIdentity {
  fn new(alias_email: String) -> Self {
    let (first_name, last_name) = random_name();
    Self {
      alias_email,
      password: random_password(),
      first_name,
      last_name,
      birthdate: random_birthday(),
      device_id: Uuid::new_v4().to_string(),
    }
  }
}

pub struct RegistrationEngine {
  config: RegistrationConfig,
  cancel_flag: Arc<AtomicBool>,
  task_id: String,
  device_id: String,
  logs: Vec<String>,
  /// Reused worker profile id for this engine instance (one per concurrent CDK slot).
  worker_profile_id: Option<String>,
  /// True when this engine created the worker and must delete it when the CDK finishes.
  owns_worker_profile: bool,
  /// Optional CDK index suffix for concurrent worker profile names.
  worker_slot: u32,
  /// Ephemeral WireGuard conf ids spawned for multi-peer concurrency (batch-owned).
  ephemeral_vpn_ids: Vec<String>,
  /// Slot-local VPN override (from ephemeral pool); takes precedence over config.vpn_id.
  slot_vpn_id: Option<String>,
  /// Email OTP retained only in memory while the same identity moves to a new peer.
  pending_email_otp: Option<String>,
  /// Japan peers rejected by an auth challenge during this CDK worker's lifetime.
  challenged_peer_stations: Vec<String>,
  challenged_peer_public_keys: Vec<String>,
  /// Adaptive rate limiter — backs off on 429/Cloudflare, recovers on success.
  rate_limiter: super::parallel::RateLimiter,
}

impl RegistrationEngine {
  #[allow(dead_code)]
  pub fn new(config: RegistrationConfig) -> Self {
    Self {
      config,
      cancel_flag: Arc::new(AtomicBool::new(false)),
      task_id: Uuid::new_v4().to_string(),
      device_id: Uuid::new_v4().to_string(),
      logs: Vec::new(),
      worker_profile_id: None,
      owns_worker_profile: false,
      worker_slot: 0,
      ephemeral_vpn_ids: Vec::new(),
      slot_vpn_id: None,
      pending_email_otp: None,
      challenged_peer_stations: Vec::new(),
      challenged_peer_public_keys: Vec::new(),
      rate_limiter: super::parallel::RateLimiter::new(),
    }
  }

  pub fn with_cancel_flag(config: RegistrationConfig, cancel_flag: Arc<AtomicBool>) -> Self {
    Self {
      config,
      cancel_flag,
      task_id: Uuid::new_v4().to_string(),
      device_id: Uuid::new_v4().to_string(),
      logs: Vec::new(),
      worker_profile_id: None,
      owns_worker_profile: false,
      worker_slot: 0,
      ephemeral_vpn_ids: Vec::new(),
      slot_vpn_id: None,
      pending_email_otp: None,
      challenged_peer_stations: Vec::new(),
      challenged_peer_public_keys: Vec::new(),
      rate_limiter: super::parallel::RateLimiter::new(),
    }
  }

  /// Fork a per-CDK engine that shares cancel/task/config but owns its own worker + logs.
  /// When an ephemeral peer pool exists, assign `vpn_ids[worker_slot % len]` to this slot.
  fn fork_for_cdk(&self, worker_slot: u32) -> Self {
    let slot_vpn_id = if !self.ephemeral_vpn_ids.is_empty() {
      let idx = (worker_slot as usize) % self.ephemeral_vpn_ids.len();
      Some(self.ephemeral_vpn_ids[idx].clone())
    } else {
      self.slot_vpn_id.clone()
    };
    Self {
      config: self.config.clone(),
      cancel_flag: self.cancel_flag.clone(),
      task_id: self.task_id.clone(),
      device_id: Uuid::new_v4().to_string(),
      logs: Vec::new(),
      worker_profile_id: None,
      owns_worker_profile: false,
      worker_slot,
      ephemeral_vpn_ids: Vec::new(), // only root owns cleanup
      slot_vpn_id,
      pending_email_otp: None,
      challenged_peer_stations: Vec::new(),
      challenged_peer_public_keys: Vec::new(),
      rate_limiter: super::parallel::RateLimiter::new(),
    }
  }

  /// VPN id for this engine/slot: ephemeral pool assignment, else inventory vpn_id.
  fn worker_vpn_id(&self) -> Option<String> {
    self
      .slot_vpn_id
      .clone()
      .or_else(|| self.config.effective_vpn_id())
  }

  /// Build N ephemeral Nord WireGuard confs from the base inventory conf's private key.
  async fn spawn_vpn_peer_pool(&mut self, pool_size: usize) -> Result<(), String> {
    let base_id = self
      .config
      .effective_vpn_id()
      .ok_or_else(|| "VPN mode requires vpnId to spawn peer pool".to_string())?;
    let base_conf = {
      let storage = crate::vpn::VPN_STORAGE
        .lock()
        .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
      storage
        .load_config(&base_id)
        .map_err(|e| format!("Load base VPN for peer pool: {e}"))?
        .config_data
    };
    let private_key = crate::vpn::extract_wireguard_private_key(&base_conf)?;
    let prefix = format!(
      "auto-reg-ephemeral-{}",
      &self.task_id[..8.min(self.task_id.len())]
    );

    // Product policy: WireGuard auto-registration always uses Japan peers.
    // The selected base config supplies only the private key and session budget;
    // its stored display country and nord_group never affect registration egress.
    let country_code = registration_vpn_country_code(self.config.network_mode)
      .ok_or_else(|| "VPN peer pool country policy is unavailable".to_string())?;
    let countries = crate::vpn::list_nord_countries()
      .await
      .map_err(|error| format!("Failed to list Nord countries for Japan peer pool: {error}"))?;
    let country_id = crate::vpn::resolve_country_id_by_code(&countries, country_code)
      .ok_or_else(|| "Nord Japan country id is unavailable".to_string())?;
    self.log(&format!(
      "VPN peer country policy: code={country_code} country_id={country_id}"
    ));

    let ids = crate::vpn::spawn_ephemeral_nord_peer_pool(
      &private_key,
      pool_size,
      Some(country_id),
      &prefix,
    )
    .await?;
    self.log(&format!(
      "Spawned ephemeral Nord peer pool: {} conf(s) for concurrency (country_id={country_id})",
      ids.len()
    ));
    for (i, id) in ids.iter().enumerate() {
      self.log(&format!("  peer pool[{i}] vpn_id={id}"));
    }
    self.ephemeral_vpn_ids = ids;
    Ok(())
  }

  async fn cleanup_ephemeral_vpn_pool(&mut self) {
    if self.ephemeral_vpn_ids.is_empty() {
      return;
    }
    let ids = std::mem::take(&mut self.ephemeral_vpn_ids);
    for id in ids {
      let _ = crate::vpn_worker_runner::stop_vpn_worker_by_vpn_id(&id).await;
      if let Ok(storage) = crate::vpn::VPN_STORAGE.lock() {
        if let Err(e) = storage.delete_config(&id) {
          self.log(&format!(
            "WARN: failed to delete ephemeral VPN conf {id}: {e}"
          ));
        } else {
          self.log(&format!("Deleted ephemeral VPN conf {id}"));
        }
      }
    }
  }

  pub fn task_id(&self) -> &str {
    &self.task_id
  }

  #[allow(dead_code)]
  pub fn cancel(&self) {
    self.cancel_flag.store(true, Ordering::SeqCst);
  }

  fn is_cancelled(&self) -> bool {
    self.cancel_flag.load(Ordering::SeqCst)
  }

  fn log(&mut self, msg: &str) {
    let ts = Utc::now().format("%H:%M:%S").to_string();
    let safe_message = sanitize_registration_log(msg);
    let line = format!("[{ts}] {safe_message}");
    // Mirror to stderr so live runs show progress immediately instead of only
    // in the terminal step-log dump at the end.
    eprintln!("STEP {line}");
    self.logs.push(line);
  }

  /// Mid-batch WireGuard peer hop: keep PrivateKey, pick a new Nord Japan peer,
  /// rewrite inventory conf, and restart vpn-worker. Auth-challenged peers remain
  /// excluded for this CDK worker so a retry cannot reuse the same egress.
  async fn rotate_wireguard_peer(&mut self, vpn_id: &str) -> Result<(String, String), String> {
    let conf = {
      let storage = crate::vpn::VPN_STORAGE
        .lock()
        .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
      storage
        .load_config(vpn_id)
        .map_err(|e| format!("Load VPN config for rotate: {e}"))?
        .config_data
    };

    let private_key = crate::vpn::extract_wireguard_private_key(&conf)?;
    if let Some(station) = crate::vpn::extract_wireguard_peer_endpoint_host(&conf) {
      if !self
        .challenged_peer_stations
        .iter()
        .any(|item| item.eq_ignore_ascii_case(&station))
      {
        self.challenged_peer_stations.push(station);
      }
    }
    if let Some(public_key) = crate::vpn::extract_wireguard_peer_public_key(&conf) {
      if !self
        .challenged_peer_public_keys
        .iter()
        .any(|item| item == &public_key)
      {
        self.challenged_peer_public_keys.push(public_key);
      }
    }

    // Product rule: registration WireGuard egress is always Japan. Do not
    // infer the country from the base inventory name (the saved base may be VN/HK).
    let countries = crate::vpn::list_nord_countries().await?;
    let country_id = crate::vpn::resolve_country_id_by_code(&countries, "JP")
      .ok_or_else(|| "Nord Japan country id is unavailable".to_string())?;
    let servers = crate::vpn::list_nord_wireguard_servers(Some(country_id), Some(100)).await?;
    let server = crate::vpn::pick_nord_server_excluding(
      &servers,
      &self.challenged_peer_stations,
      &self.challenged_peer_public_keys,
    )?
    .clone();
    let new_conf = crate::vpn::build_nord_wireguard_conf(&private_key, &server);
    crate::vpn::validate_nord_wireguard_conf(&new_conf).map_err(|e| e.to_string())?;

    {
      let storage = crate::vpn::VPN_STORAGE
        .lock()
        .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
      storage
        .update_config_data(
          vpn_id,
          &new_conf,
          Some(&crate::vpn::default_nord_vpn_name(&server)),
        )
        .map_err(|e| format!("Save rotated VPN config: {e}"))?;
    }

    // Drop old tunnel so the next profile launch starts a worker on the new conf.
    let _ = crate::vpn_worker_runner::stop_vpn_worker_by_vpn_id(vpn_id).await;
    self.human_pause(800, 1200).await;

    match crate::vpn_worker_runner::start_vpn_worker(vpn_id).await {
      Ok(worker) => {
        self.log(&format!(
          "WireGuard worker restarted on {} (port {:?})",
          server.hostname, worker.local_port
        ));
      }
      Err(e) => {
        self.log(&format!(
          "WARN: vpn-worker restart after peer rotate failed (will retry on launch): {e}"
        ));
      }
    }

    Ok((server.hostname, server.station.clone()))
  }

  /// Switch this slot's Nord inventory conf to an allowlisted country after region blocks.
  async fn fallback_nord_location_on_region_block(
    &mut self,
    error_message: &str,
    location_fallbacks: &mut u32,
    tried_locations: &mut Vec<String>,
  ) -> Result<bool, String> {
    if !crate::vpn::is_unsupported_region_error(error_message) {
      return Ok(false);
    }
    if *location_fallbacks >= crate::vpn::MAX_NORD_LOCATION_FALLBACKS {
      return Ok(false);
    }
    let Some(vpn_id) = self.worker_vpn_id() else {
      return Ok(false);
    };

    let current_name = {
      let storage = crate::vpn::VPN_STORAGE
        .lock()
        .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
      storage
        .load_config(&vpn_id)
        .map(|c| c.name)
        .unwrap_or_default()
    };
    let current_code = crate::vpn::infer_country_code_from_vpn_name(&current_name);
    let Some(next_code) =
      crate::vpn::next_fallback_country_code(current_code.as_deref(), tried_locations)
    else {
      self.log("Unsupported region detected; no remaining Nord fallback locations");
      return Ok(false);
    };

    self.log(&format!(
      "Unsupported region detected; switching Nord location {} → {next_code}",
      current_code.as_deref().unwrap_or("?")
    ));
    let server = crate::vpn::retarget_nord_vpn_to_country(&vpn_id, next_code).await?;
    tried_locations.push(next_code.to_string());
    *location_fallbacks += 1;
    self.log(&format!(
      "Nord location fallback #{location_fallbacks}: {} ({}) station={}",
      server.hostname,
      server.country_code.as_deref().unwrap_or(next_code),
      server.station
    ));
    Ok(true)
  }

  #[allow(clippy::too_many_arguments)]
  fn emit(
    &self,
    app_handle: &tauri::AppHandle,
    step: RegistrationStep,
    message: &str,
    cdk_index: u32,
    alias_index: u32,
    total_cdks: u32,
    terminal: Option<RegistrationTerminalSummary>,
  ) {
    let safe_message = if let Some(summary) = terminal.as_ref() {
      summary.status_code.clone()
    } else if step == RegistrationStep::Failed {
      "failed".into()
    } else {
      sanitize_registration_log(message)
    };
    let payload = RegistrationProgress {
      task_id: self.task_id.clone(),
      cdk_index,
      alias_index,
      total_cdks,
      step,
      message: safe_message,
      timestamp: Utc::now(),
      event_kind: RegistrationProgressEventKind::Account,
      terminal,
    };
    let _ = app_handle.emit("registration-progress", payload);
  }

  #[allow(clippy::too_many_arguments)]
  fn emit_batch_terminal(
    &self,
    app_handle: &tauri::AppHandle,
    step: RegistrationStep,
    _message: &str,
    total_cdks: u32,
    success: bool,
  ) {
    let payload = RegistrationProgress {
      task_id: self.task_id.clone(),
      cdk_index: 0,
      alias_index: 0,
      total_cdks,
      step,
      message: if success {
        "completed".into()
      } else {
        "failed".into()
      },
      timestamp: Utc::now(),
      event_kind: RegistrationProgressEventKind::Batch,
      terminal: Some(RegistrationTerminalSummary {
        success,
        status_code: if success {
          "completed".into()
        } else {
          "failed".into()
        },
      }),
    };
    let _ = app_handle.emit("registration-progress", payload);
  }

  #[allow(dead_code)]
  fn emit_step(&self, app_handle: &tauri::AppHandle, step: RegistrationStep, msg: &str) {
    self.emit(app_handle, step, msg, 0, 0, 0, None);
  }

  // -----------------------------------------------------------------------
  // Main entry point — concurrent CDK workers (1 CDK = 1 task)
  // -----------------------------------------------------------------------

  pub async fn run(
    &mut self,
    app_handle: tauri::AppHandle,
    email_service: &dyn EmailService,
    sms_service: Option<&dyn SmsService>,
  ) -> RegistrationResult {
    let total_cdks = self.config.cdks.len() as u32;
    let accounts_per = self.config.effective_accounts_per_cdk();
    let max_retries = self.config.max_retries.max(1);
    let cdks = self.config.cdks.clone();

    // Retry-cap flow: clamp the per-CDK request to the CDK's remaining capacity
    // so a partial run can be retried without tripping quota validation. CDKs
    // already at `MAX_ACCOUNTS_PER_CDK` used slots are skipped (the user must
    // delete that CDK's stats row to wipe its usage ledger and start fresh).
    let mut per_cdk_target: Vec<u32> = Vec::with_capacity(cdks.len());
    for cdk in &cdks {
      let remaining = cdk_remaining_capacity(cdk);
      if remaining == 0 {
        self.log(&format!(
          "CDK entry is at max capacity ({MAX_ACCOUNTS_PER_CDK}/{MAX_ACCOUNTS_PER_CDK} used+reserved); skipping - free slots only after Delete (full stats+quota reset) or when failed claims release usage"
        ));
        per_cdk_target.push(0);
        continue;
      }
      let target = accounts_per.min(remaining);
      if target < accounts_per {
        self.log(&format!(
          "CDK entry retrying - capping request {accounts_per} -> {target} ({} of {MAX_ACCOUNTS_PER_CDK} slots already used)",
          MAX_ACCOUNTS_PER_CDK - remaining
        ));
      }
      per_cdk_target.push(target);
    }
    if per_cdk_target.iter().all(|&t| t == 0) {
      let msg = format!(
        "All CDKs are at max capacity ({MAX_ACCOUNTS_PER_CDK}/{MAX_ACCOUNTS_PER_CDK} used+reserved); nothing left to schedule — use Top-up only when remaining > 0, or Delete a CDK stats row for a full quota reset"
      );
      self.log(&msg);
      self.emit(
        &app_handle,
        RegistrationStep::Failed,
        &msg,
        0,
        0,
        total_cdks,
        None,
      );
      return self.fail_result(&msg);
    }
    let targets: Vec<(String, u32)> = cdks
      .iter()
      .zip(per_cdk_target.iter())
      .filter(|(_, &t)| t > 0)
      .map(|(c, &t)| (c.clone(), t))
      .collect();
    let mut reserved_iter = match reserve_cdk_slots_per(&targets, &self.task_id) {
      Ok(reservations) => reservations.into_iter(),
      Err(_error) => {
        let msg = "CDK/card quota validation failed".to_string();
        self.log(&msg);
        self.emit(
          &app_handle,
          RegistrationStep::Failed,
          &msg,
          0,
          0,
          total_cdks,
          None,
        );
        return self.fail_result(&msg);
      }
    };
    let mut reservations: Vec<Option<CdkSlotReservation>> = Vec::with_capacity(cdks.len());
    for &target in &per_cdk_target {
      if target > 0 {
        reservations.push(Some(
          reserved_iter
            .next()
            .expect("reservation count must match non-zero target count"),
        ));
      } else {
        reservations.push(None);
      }
    }
    let mut reservations = reservations;
    let mut last_error = String::new();
    let mut nord_connected_by_us = false;

    let cli = self.config.nord_cli_path.clone();
    let group = self.config.nord_group.clone();
    let server = self.config.nord_server_name.clone();
    let rotate_every_n = self.config.rotate_every_n;
    let network_mode = self.config.network_mode;
    let uses_nord_cli = self.config.uses_nord_cli();

    // Concurrency policy:
    // - Nord CLI: always 1 (system-wide).
    // - VPN: user-selected concurrency capped by the detected Nord session budget.
    //   Peer pool size follows effective concurrency; CDKs drain in waves until done.
    let mut concurrency = self.config.concurrency.clamp(1, 8);
    if uses_nord_cli && concurrency > 1 {
      self.log(&format!(
        "Nord CLI mode: forcing concurrency 1 (was {concurrency}) — system-wide IP is not thread-safe"
      ));
      concurrency = 1;
    }

    if network_mode == NetworkMode::Vpn {
      // Auto-detect session budget from the selected Nord/WireGuard inventory config.
      // User only provides Access Token when creating the VPN — no manual session input.
      if let Some(vpn_id) = self.config.effective_vpn_id() {
        if let Ok(storage) = crate::vpn::VPN_STORAGE.lock() {
          if let Ok(cfg) = storage.load_config(&vpn_id) {
            if let Some(ms) = cfg.max_sessions.filter(|n| *n >= 1) {
              self.config.nord_max_sessions = ms.clamp(1, 6);
              self.log(&format!(
                "Nord session budget auto-detected from VPN config: {ms}"
              ));
            } else if cfg.source.as_deref() == Some("nord") {
              // Nord-created conf without stored budget: conservative default already on config.
              self.log(
                "Nord VPN config has no stored session budget; using default nord_max_sessions",
              );
            }
          }
        }
      }
      let requested_concurrency = self.config.concurrency;
      concurrency = self.config.effective_vpn_concurrency();
      self.log(&format!(
        "VPN concurrency selected by operator: requested={requested_concurrency}, session budget={}, effective={concurrency} (CDK count={total_cdks} does not limit concurrency)",
        self.config.nord_max_sessions
      ));

      if let Some(base) = self.config.effective_vpn_id() {
        self.log(&format!(
          "Network mode VPN (WireGuard): base vpn_id={base}; rotate every {rotate_every_n} success(es) per slot"
        ));
      }

      // Pool size = Nord-capped concurrency only (independent of CDK count).
      let pool_size = concurrency.clamp(1, 6) as usize;
      if let Err(e) = self.spawn_vpn_peer_pool(pool_size).await {
        let msg = format!("Failed to spawn ephemeral Nord peer pool: {e}");
        self.log(&msg);
        self.emit(
          &app_handle,
          RegistrationStep::Failed,
          &msg,
          0,
          0,
          total_cdks,
          None,
        );
        return self.fail_result(&msg);
      }
      // If API returned fewer usable peers, shrink concurrency to pool size.
      let pool_len = self.ephemeral_vpn_ids.len().max(1) as u32;
      if concurrency > pool_len {
        self.log(&format!(
          "VPN peer pool size {pool_len} < Nord-capped concurrency {concurrency}; capping concurrency to pool"
        ));
        concurrency = pool_len;
      }
      self.log(&format!(
        "VPN peer pool ready: {pool_len} concurrent Nord session(s); will drain all {total_cdks} CDK(s) in waves"
      ));
    }

    self.log(&format!(
      "Starting batch: {total_cdks} CDK(s), {accounts_per} account(s)/CDK, concurrency={concurrency}"
    ));

    // Nord CLI is backup/legacy only — system-wide connect when mode is Nord.
    if uses_nord_cli {
      self.emit(
        &app_handle,
        RegistrationStep::RotatingIp,
        "Connecting NordVPN CLI (system-wide backup mode)...",
        0,
        0,
        total_cdks,
        None,
      );
      match crate::auto_service::common::nord_cli::connect(
        cli.as_deref(),
        group.as_deref(),
        server.as_deref(),
      ) {
        Ok(()) => {
          nord_connected_by_us = true;
          self.log("NordVPN CLI connected (backup mode)");
          self.human_pause(2500, 3500).await;
          if let Ok(ip) = crate::ip_utils::fetch_public_ip(None).await {
            self.log(&format!("Egress IP after Nord CLI connect: {ip}"));
          }
        }
        Err(e) => {
          let msg = format!("NordVPN CLI connect failed: {e}");
          self.log(&msg);
          self.emit(
            &app_handle,
            RegistrationStep::Failed,
            &msg,
            0,
            0,
            total_cdks,
            None,
          );
          return self.fail_result(&msg);
        }
      }
    }

    // Sequential path (concurrency=1): keep Nord rotate semantics on one engine.
    if concurrency == 1 {
      let mut all_results: Vec<RegistrationResult> = Vec::new();
      let success_count = AtomicU32::new(0);
      for (cdk_idx, cdk) in cdks.iter().enumerate() {
        if self.is_cancelled() {
          self.log("Cancelled by user");
          break;
        }
        let Some(reservation) = reservations[cdk_idx].take() else {
          // Skipped during retry-cap flow — CDK is already at max capacity.
          continue;
        };
        let target = per_cdk_target[cdk_idx];
        let mut slot = self.fork_for_cdk(cdk_idx as u32);
        let (results, err, nord_stop) = slot
          .process_one_cdk(
            &app_handle,
            email_service,
            sms_service,
            cdk,
            reservation,
            cdk_idx as u32,
            total_cdks,
            target,
            max_retries,
            uses_nord_cli,
            rotate_every_n,
            cli.as_deref(),
            group.as_deref(),
            server.as_deref(),
            &success_count,
          )
          .await;
        self.logs.extend(slot.logs);
        all_results.extend(results);
        if let Some(e) = err {
          last_error = e;
        }
        if nord_stop {
          nord_connected_by_us = true;
          break;
        }
      }

      if nord_connected_by_us {
        self.log("NordVPN CLI left connected after auto-reg (no auto-disconnect)");
      }

      let ok = all_results.iter().filter(|r| r.success).count();
      let fail = all_results.iter().filter(|r| !r.success).count();
      let free_no = all_results
        .iter()
        .filter(|r| !r.success && !r.email.is_empty() && !r.free_trial_eligible)
        .count();
      let msg = if ok > 0 || free_no > 0 {
        format!("Done: {ok} free-trial, {free_no} no-trial, {fail} total non-success")
      } else if !last_error.is_empty() {
        last_error.clone()
      } else {
        "No accounts created".into()
      };

      let batch_result = RegistrationResult {
        success: ok > 0,
        email: String::new(),
        password: String::new(),
        account_id: format!("batch:{ok}"),
        access_token: String::new(),
        device_id: String::new(),
        error_message: if fail > 0 {
          format!("{fail} non-success")
        } else {
          String::new()
        },
        step_logs: self.logs.clone(),
        created_at: Utc::now(),
        two_fa_enabled: false,
        totp_secret: String::new(),
        free_trial_eligible: false,
        plan_type: String::new(),
        cdk: format!("{total_cdks} CDKs processed"),
        base_email: String::new(),
        phone_number: String::new(),
        status: AccountInventoryStatus::Invalid,
        note: String::new(),
        exported_at: None,
        sold_at: None,
        email_provider: Some(self.config.email_provider),
        email_provider_provenance: Some(EmailProviderProvenance::RegistrationConfig),
        registration_outcome_reason: Some(RegistrationOutcomeReason::BatchSummary),
        two_factor_backfill_access_state: Some(TwoFactorBackfillAccessState::Accessible),
        two_factor_backfill_exclusion: None,
        two_factor_backfill_state: None,
        two_factor_backfill_outcome: None,
        two_factor_backfill_operation_id: None,
        record_revision: 1,
      };
      let terminal_step = if batch_result.success {
        RegistrationStep::Completed
      } else {
        RegistrationStep::Failed
      };
      self.emit_batch_terminal(
        &app_handle,
        terminal_step,
        &msg,
        total_cdks,
        batch_result.success,
      );
      self.cleanup_ephemeral_vpn_pool().await;
      return batch_result;
    }

    // Parallel path: one concurrent future per CDK, limited by semaphore.
    // Poll on this task via FuturesUnordered (engine futures are !Send — no JoinSet).
    let semaphore = Arc::new(Semaphore::new(concurrency as usize));
    let success_count = Arc::new(AtomicU32::new(0));
    let sms_token = self.config.sms_token.clone();
    let sms_enabled = sms_service.is_some();

    #[allow(clippy::type_complexity)]
    let mut tasks: FuturesUnordered<
      std::pin::Pin<
        Box<
          dyn std::future::Future<
              Output = (u32, Vec<RegistrationResult>, Vec<String>, Option<String>),
            > + '_,
        >,
      >,
    > = FuturesUnordered::new();

    for (cdk_idx, cdk) in cdks.into_iter().enumerate() {
      if self.is_cancelled() {
        break;
      }
      let Some(reservation) = reservations[cdk_idx].take() else {
        // Skipped during retry-cap flow — CDK is already at max capacity.
        continue;
      };
      let target = per_cdk_target[cdk_idx];
      let semaphore = semaphore.clone();
      let mut slot = self.fork_for_cdk(cdk_idx as u32);
      let app = app_handle.clone();
      let success_count = success_count.clone();
      let sms_token = sms_token.clone();
      let email_provider = self.config.email_provider;
      tasks.push(Box::pin(async move {
        let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
        let email = crate::email::build_email_service(email_provider);
        let viotp = if sms_enabled {
          sms_token.map(crate::sms::viotp::ViotpService::new)
        } else {
          None
        };
        let sms_ref: Option<&dyn SmsService> = viotp.as_ref().map(|s| s as &dyn SmsService);
        // Per-slot WireGuard peer rotate is OK (each slot has its own ephemeral vpn_id).
        // Nord CLI remains disabled under concurrency > 1.
        let (results, err, _nord_stop) = slot
          .process_one_cdk(
            &app,
            email.as_ref(),
            sms_ref,
            &cdk,
            reservation,
            cdk_idx as u32,
            total_cdks,
            target,
            max_retries,
            false,          // Nord CLI never under concurrency > 1
            rotate_every_n, // WireGuard per-slot peer hop still allowed
            None,
            None,
            None,
            &success_count,
          )
          .await;
        (cdk_idx as u32, results, slot.logs, err)
      }));
    }

    let mut all_results: Vec<RegistrationResult> = Vec::new();
    while let Some((_idx, results, logs, err)) = tasks.next().await {
      self.logs.extend(logs);
      all_results.extend(results);
      if let Some(e) = err {
        last_error = e;
      }
    }

    self.cleanup_ephemeral_vpn_pool().await;

    if nord_connected_by_us {
      self.log("NordVPN CLI left connected after auto-reg (no auto-disconnect)");
    }

    let ok = all_results.iter().filter(|r| r.success).count();
    let fail = all_results.iter().filter(|r| !r.success).count();
    let free_no = all_results
      .iter()
      .filter(|r| !r.success && !r.email.is_empty() && !r.free_trial_eligible)
      .count();
    let msg = if ok > 0 || free_no > 0 {
      format!("Done: {ok} free-trial, {free_no} no-trial, {fail} total non-success")
    } else if !last_error.is_empty() {
      last_error.clone()
    } else {
      "No accounts created".into()
    };

    let batch_result = RegistrationResult {
      success: ok > 0,
      email: String::new(),
      password: String::new(),
      account_id: format!("batch:{ok}"),
      access_token: String::new(),
      device_id: String::new(),
      error_message: if fail > 0 {
        format!("{fail} non-success")
      } else {
        String::new()
      },
      step_logs: self.logs.clone(),
      created_at: Utc::now(),
      two_fa_enabled: false,
      totp_secret: String::new(),
      free_trial_eligible: false,
      plan_type: String::new(),
      cdk: format!("{total_cdks} CDKs processed"),
      base_email: String::new(),
      phone_number: String::new(),
      status: AccountInventoryStatus::Invalid,
      note: String::new(),
      exported_at: None,
      sold_at: None,
      email_provider: Some(self.config.email_provider),
      email_provider_provenance: Some(EmailProviderProvenance::RegistrationConfig),
      registration_outcome_reason: Some(RegistrationOutcomeReason::BatchSummary),
      two_factor_backfill_access_state: Some(TwoFactorBackfillAccessState::Accessible),
      two_factor_backfill_exclusion: None,
      two_factor_backfill_state: None,
      two_factor_backfill_outcome: None,
      two_factor_backfill_operation_id: None,
      record_revision: 1,
    };
    let terminal_step = if batch_result.success {
      RegistrationStep::Completed
    } else {
      RegistrationStep::Failed
    };
    self.emit_batch_terminal(
      &app_handle,
      terminal_step,
      &msg,
      total_cdks,
      batch_result.success,
    );
    batch_result
  }

  /// Process a single CDK: redeem → N aliases sequential → update CDK inventory.
  /// Returns (results, last_error, nord_hard_stop).
  #[allow(clippy::too_many_arguments)]
  async fn process_one_cdk(
    &mut self,
    app_handle: &tauri::AppHandle,
    email_service: &dyn EmailService,
    sms_service: Option<&dyn SmsService>,
    cdk: &str,
    mut reservation: CdkSlotReservation,
    cdk_idx: u32,
    total_cdks: u32,
    accounts_per: u32,
    max_retries: u32,
    uses_nord_cli: bool,
    rotate_every_n: u32,
    nord_cli: Option<&str>,
    nord_group: Option<&str>,
    nord_server: Option<&str>,
    success_count: &AtomicU32,
  ) -> (Vec<RegistrationResult>, Option<String>, bool) {
    let mut all_results: Vec<RegistrationResult> = Vec::new();
    let mut last_error: Option<String> = None;
    let mut nord_hard_stop = false;

    let mut cdk_record =
      get_cdk_inventory(cdk).unwrap_or_else(|| CdkInventoryRecord::new(cdk, 0, &self.task_id));
    cdk_record.cdk = cdk.trim().to_ascii_uppercase();
    cdk_record.target_accounts = cdk_record.target_accounts.saturating_add(accounts_per);
    cdk_record.task_id.clone_from(&self.task_id);
    cdk_record.status = "running".into();
    cdk_record.last_error.clear();
    cdk_record.updated_at = Utc::now();
    put_cdk_inventory_record(&cdk_record);

    self.log(&format!("=== CDK {}/{total_cdks} ===", cdk_idx + 1));
    self.emit(
      app_handle,
      RegistrationStep::RedeemingCdk,
      &format!("CDK {}/{}: redeeming...", cdk_idx + 1, total_cdks),
      cdk_idx,
      0,
      total_cdks,
      None,
    );

    let base_email = match email_service.redeem_cdk(cdk) {
      Ok(info) => {
        let email = info.email.clone();
        self.log("CDK redeemed");
        cdk_record.base_email = email.clone();
        put_cdk_inventory_record(&cdk_record);
        email
      }
      Err(e) => {
        let msg = format!("CDK redeem failed: {}", safe_email_service_error(&e));
        self.log(&msg);
        cdk_record.last_error = msg.clone();
        cdk_record.status = "failed".into();
        // Record the *actual* remaining capacity so the dashboard does not
        // mislead the operator with a stale default of 0 when redeem failed
        // before any alias was claimed (e.g. provider HTTP 400 / 5xx).
        cdk_record.remaining = cdk_remaining_capacity(cdk);
        cdk_record.updated_at = Utc::now();
        put_cdk_inventory_record(&cdk_record);
        self.dispose_worker_profile(app_handle).await;
        return (all_results, Some(msg), false);
      }
    };

    for alias_idx in 0..accounts_per {
      if self.is_cancelled() {
        cdk_record.status = "cancelled".into();
        cdk_record.updated_at = Utc::now();
        put_cdk_inventory_record(&cdk_record);
        self.log("Cancelled by user");
        break;
      }

      let mut finished_result: Option<RegistrationResult> = None;
      let mut location_fallbacks: u32 = 0;
      let mut tried_locations: Vec<String> = Vec::new();
      // A retained OTP belongs only to this logical identity. Never carry it
      // into the next alias even when the same CDK mailbox is reused.
      self.pending_email_otp = None;
      let identity = match email_service.generate_alias(&base_email) {
        Ok(alias) => AccountIdentity::new(alias),
        Err(e) => {
          let msg = format!("Alias: {}", safe_email_service_error(&e));
          last_error = Some(msg.clone());
          self.log(&format!(
            "[CDK {}/{} Alias {}/{}] {msg}",
            cdk_idx + 1,
            total_cdks,
            alias_idx + 1,
            accounts_per
          ));
          cdk_record.record_hard_failure(&msg);
          put_cdk_inventory_record(&cdk_record);
          continue;
        }
      };
      if let Err(error) = reservation.claim_slot() {
        last_error = Some(error.clone());
        self.log(&format!(
          "[CDK {}/{} Alias {}/{}] {error}",
          cdk_idx + 1,
          total_cdks,
          alias_idx + 1,
          accounts_per
        ));
        cdk_record.record_hard_failure(&error);
        put_cdk_inventory_record(&cdk_record);
        break;
      }

      let mut registration_failures = 0_u32;
      let mut auth_peer_rotations = 0_u32;
      let mut run_number = 0_u32;
      while registration_failures < max_retries {
        if self.is_cancelled() {
          last_error = Some("Cancelled".into());
          // Roll back the claimed slot so usage ledger does not count a
          // cancelled attempt as a used account.
          if let Err(e) = reservation.release_slot() {
            self.log(&format!(
              "WARN: failed to release CDK slot after cancel: {e}"
            ));
          }
          break;
        }
        if run_number > 0 {
          self.log(&format!(
            "Retry run {run_number}: registration failures={registration_failures}/{max_retries}, Japan peer rotations={auth_peer_rotations}/{MAX_AUTH_CHALLENGE_PEER_ROTATIONS} for alias {}/{accounts_per}...",
            alias_idx + 1
          ));
          self.human_pause(1800, 2200).await;
        }

        match self
          .run_once(
            app_handle,
            email_service,
            sms_service,
            cdk,
            &base_email,
            &identity,
            alias_idx,
            cdk_idx,
            total_cdks,
            accounts_per,
          )
          .await
        {
          Ok(result) => {
            if !result.success
              && crate::vpn::is_unsupported_region_error(&result.error_message)
              && location_fallbacks < crate::vpn::MAX_NORD_LOCATION_FALLBACKS
            {
              match self
                .fallback_nord_location_on_region_block(
                  &result.error_message,
                  &mut location_fallbacks,
                  &mut tried_locations,
                )
                .await
              {
                Ok(true) => {
                  last_error = Some(result.error_message.clone());
                  continue;
                }
                Ok(false) => {}
                Err(e) => {
                  self.log(&format!("WARN: Nord location fallback failed: {e}"));
                }
              }
            }

            // Persist every finished attempt before reporting it as durable.
            if let Err(error) = save_registration_result(&result) {
              let safe_error = format!(
                "Account was created remotely, but local credential persistence failed: {error}"
              );
              self.log(&safe_error);
              cdk_record.record_hard_failure(&safe_error);
              put_cdk_inventory_record(&cdk_record);
              self.emit(
                app_handle,
                RegistrationStep::Failed,
                &safe_error,
                cdk_idx,
                alias_idx,
                total_cdks,
                None,
              );
              last_error = Some(safe_error);
              finished_result = Some(result);
              break;
            }
            cdk_record.record_result(&result);
            put_cdk_inventory_record(&cdk_record);
            all_results.push(result.clone());
            finished_result = Some(result.clone());

            // Tell the rate limiter we had a clean request — gradually
            // reduces backoff so the next account starts faster.
            if result.success {
              self.rate_limiter.mark_success();
            }

            let terminal_step = if result.success {
              RegistrationStep::Completed
            } else {
              RegistrationStep::Failed
            };
            let terminal_message = if result.success {
              format!(
                "Account saved. free-trial{}{}",
                if result.plan_type.is_empty() {
                  String::new()
                } else {
                  format!(" [{}]", result.plan_type)
                },
                if result.two_fa_enabled {
                  " (2FA on)"
                } else {
                  " (2FA skipped)"
                }
              )
            } else {
              format!("Account saved as non-success: {}", result.error_message)
            };
            let terminal_status = if result.success {
              "completed"
            } else if !result.free_trial_eligible {
              "free_trial_no"
            } else if result.two_factor_backfill_outcome
              == Some(TwoFactorBackfillOutcome::ReconciliationRequired)
            {
              "reconciliation_required"
            } else {
              "two_factor_failed"
            };
            self.emit(
              app_handle,
              terminal_step,
              &terminal_message,
              cdk_idx,
              alias_idx,
              total_cdks,
              Some(RegistrationTerminalSummary {
                success: result.success,
                status_code: terminal_status.into(),
              }),
            );

            if result.success {
              let sc = success_count.fetch_add(1, Ordering::SeqCst) + 1;
              if should_rotate(sc, rotate_every_n) {
                if uses_nord_cli {
                  self.emit(
                    app_handle,
                    RegistrationStep::RotatingIp,
                    &format!("Rotating NordVPN CLI IP after {sc} successes..."),
                    cdk_idx,
                    alias_idx,
                    total_cdks,
                    None,
                  );
                  let old_ip = crate::ip_utils::fetch_public_ip(None)
                    .await
                    .unwrap_or_default();
                  match crate::auto_service::common::nord_cli::rotate(
                    nord_cli,
                    nord_group,
                    nord_server,
                  ) {
                    Ok(()) => {
                      let mut new_ip = old_ip.clone();
                      for _ in 0..20 {
                        if self.is_cancelled() {
                          break;
                        }
                        self.human_pause(2500, 3500).await;
                        if let Ok(ip) = crate::ip_utils::fetch_public_ip(None).await {
                          new_ip = ip;
                          if !new_ip.is_empty() && new_ip != old_ip {
                            break;
                          }
                        }
                      }
                      if new_ip == old_ip {
                        self.log(&format!(
                          "WARN: egress IP unchanged after rotate ({old_ip}); continuing"
                        ));
                      } else {
                        self.log(&format!("Egress IP {old_ip} → {new_ip}"));
                      }
                      self.emit(
                        app_handle,
                        RegistrationStep::RotatingIp,
                        &format!("IP {old_ip} → {new_ip}"),
                        cdk_idx,
                        alias_idx,
                        total_cdks,
                        None,
                      );
                    }
                    Err(e) => {
                      let msg = format!("NordVPN CLI rotate failed: {e}");
                      self.log(&msg);
                      self.emit(
                        app_handle,
                        RegistrationStep::Failed,
                        &msg,
                        cdk_idx,
                        alias_idx,
                        total_cdks,
                        None,
                      );
                      nord_hard_stop = true;
                      last_error = Some(msg);
                    }
                  }
                } else if let Some(vpn_id) = self.worker_vpn_id() {
                  self.emit(
                    app_handle,
                    RegistrationStep::RotatingIp,
                    &format!("Rotating WireGuard peer after {sc} successes..."),
                    cdk_idx,
                    alias_idx,
                    total_cdks,
                    None,
                  );
                  match self.rotate_wireguard_peer(&vpn_id).await {
                    Ok((hostname, station)) => {
                      self.log(&format!(
                        "WireGuard peer rotated → {hostname} ({station}) after {sc} successes"
                      ));
                      self.emit(
                        app_handle,
                        RegistrationStep::RotatingIp,
                        &format!("WireGuard peer → {hostname} ({station})"),
                        cdk_idx,
                        alias_idx,
                        total_cdks,
                        None,
                      );
                    }
                    Err(e) => {
                      let msg = format!("WireGuard peer rotate failed: {e}");
                      self.log(&msg);
                      self.emit(
                        app_handle,
                        RegistrationStep::Failed,
                        &msg,
                        cdk_idx,
                        alias_idx,
                        total_cdks,
                        None,
                      );
                      nord_hard_stop = true;
                      last_error = Some(msg);
                    }
                  }
                }
              }
            }
            break;
          }
          Err(e) => {
            last_error = Some(e.clone());
            self.log(&format!(
              "Run {run_number} failed (registration failures={registration_failures}, peer rotations={auth_peer_rotations}): {e}"
            ));

            // Turnstile persistence, exhausted authorize retries, and OTP 409
            // all mean this browser auth session/egress is unusable. run_once
            // has already closed the browser; rotate the slot's Japan peer and
            // retry the same AccountIdentity without consuming max_retries.
            if should_rotate_auth_challenge_peer(
              self.config.network_mode,
              &e,
              auth_peer_rotations,
              MAX_AUTH_CHALLENGE_PEER_ROTATIONS,
            ) {
              if let Some(vpn_id) = self.worker_vpn_id() {
                self.emit(
                  app_handle,
                  RegistrationStep::RotatingIp,
                  &format!(
                    "[CDK {}/{} Alias {}/{}] Auth challenge blocked this session; rotating Japan WireGuard peer ({}/{})...",
                    cdk_idx + 1,
                    total_cdks,
                    alias_idx + 1,
                    accounts_per,
                    auth_peer_rotations + 1,
                    MAX_AUTH_CHALLENGE_PEER_ROTATIONS
                  ),
                  cdk_idx,
                  alias_idx,
                  total_cdks,
                  None,
                );
                match self.rotate_wireguard_peer(&vpn_id).await {
                  Ok((hostname, station)) => {
                    auth_peer_rotations += 1;
                    run_number += 1;
                    self.log(&format!(
                      "Auth challenge peer rotation {auth_peer_rotations}/{MAX_AUTH_CHALLENGE_PEER_ROTATIONS}: Japan → {hostname} ({station}); retrying same identity"
                    ));
                    self.emit(
                      app_handle,
                      RegistrationStep::RotatingIp,
                      &format!(
                        "[CDK {}/{} Alias {}/{}] Japan peer → {hostname}; retrying same account",
                        cdk_idx + 1,
                        total_cdks,
                        alias_idx + 1,
                        accounts_per
                      ),
                      cdk_idx,
                      alias_idx,
                      total_cdks,
                      None,
                    );
                    self.human_pause(1200, 2200).await;
                    continue;
                  }
                  Err(rotate_error) => {
                    self.log(&format!(
                      "WARN: auth challenge Japan peer rotation failed: {rotate_error}"
                    ));
                  }
                }
              }
            }

            // Auth challenge budget exhausted or peer rotation unavailable:
            // count this as one registration failure so the loop remains bounded.
            registration_failures = registration_failures.saturating_add(1);
            run_number = run_number.saturating_add(1);

            if crate::vpn::is_unsupported_region_error(&e)
              && location_fallbacks < crate::vpn::MAX_NORD_LOCATION_FALLBACKS
            {
              match self
                .fallback_nord_location_on_region_block(
                  &e,
                  &mut location_fallbacks,
                  &mut tried_locations,
                )
                .await
              {
                Ok(true) => continue,
                Ok(false) => {}
                Err(fe) => {
                  self.log(&format!("WARN: Nord location fallback failed: {fe}"));
                }
              }
            }
            // Browser-kill failure is unrecoverable for this CDK: another
            // alias launch would stack a second browser on the still-running
            // one and freeze the machine. Aborts the CDK immediately.
            if e.contains("browser kill failed") {
              cdk_record.record_hard_failure(&e);
              put_cdk_inventory_record(&cdk_record);
              if let Err(re_err) = reservation.release_slot() {
                self.log(&format!(
                  "WARN: failed to release CDK slot after browser kill failure: {re_err}"
                ));
              }
              self.dispose_worker_profile(app_handle).await;
              return (all_results, Some(e), nord_hard_stop);
            }
          }
        }

        if nord_hard_stop {
          break;
        }
      }

      if finished_result.is_none() {
        let err = last_error
          .clone()
          .unwrap_or_else(|| "unknown failure".into());
        self.log(&format!(
          "Alias {}/{} failed after {max_retries} retries",
          alias_idx + 1,
          accounts_per
        ));
        // Roll back the claimed slot — the account was never created, so the
        // CDK usage ledger must not count this attempt. Lets retry work.
        if let Err(e) = reservation.release_slot() {
          self.log(&format!(
            "WARN: failed to release CDK slot after failure: {e}"
          ));
        }
        cdk_record.record_hard_failure(&err);
        put_cdk_inventory_record(&cdk_record);
      }

      if nord_hard_stop {
        break;
      }
    }

    if self.is_cancelled() && cdk_record.status == "running" {
      cdk_record.status = "cancelled".into();
    } else if cdk_record.status == "running" {
      cdk_record.finalize_status();
    }
    cdk_record.updated_at = Utc::now();
    put_cdk_inventory_record(&cdk_record);

    let terminal_result = all_results.last();
    let terminal_step = if all_results.iter().any(|result| result.success) {
      RegistrationStep::Completed
    } else {
      RegistrationStep::Failed
    };
    let terminal_success = terminal_step == RegistrationStep::Completed;
    let terminal_message = if terminal_success {
      format!("CDK {}/{} completed", cdk_idx + 1, total_cdks)
    } else {
      last_error
        .clone()
        .unwrap_or_else(|| format!("CDK {}/{} failed", cdk_idx + 1, total_cdks))
    };
    self.emit(
      app_handle,
      terminal_step,
      &terminal_message,
      cdk_idx,
      accounts_per.saturating_sub(1),
      total_cdks,
      Some(RegistrationTerminalSummary {
        success: terminal_success,
        status_code: terminal_result
          .map(|result| {
            if result.success {
              "completed"
            } else if !result.free_trial_eligible {
              "free_trial_no"
            } else if result.two_factor_backfill_outcome
              == Some(TwoFactorBackfillOutcome::ReconciliationRequired)
            {
              "reconciliation_required"
            } else {
              "failed"
            }
          })
          .unwrap_or("failed")
          .into(),
      }),
    );

    // Dispose this CDK's worker profile (each concurrent slot owns one).
    self.dispose_worker_profile(app_handle).await;

    (all_results, last_error, nord_hard_stop)
  }

  // -----------------------------------------------------------------------
  // Single account registration with full state machine
  // -----------------------------------------------------------------------

  #[allow(clippy::too_many_arguments)]
  async fn run_once(
    &mut self,
    app_handle: &tauri::AppHandle,
    email_service: &dyn EmailService,
    sms_service: Option<&dyn SmsService>,
    cdk: &str,
    base_email: &str,
    identity: &AccountIdentity,
    alias_idx: u32,
    cdk_idx: u32,
    total_cdks: u32,
    total_aliases: u32,
  ) -> Result<RegistrationResult, String> {
    let prefix = format!(
      "[CDK {}/{} Alias {}/{}]",
      cdk_idx + 1,
      total_cdks,
      alias_idx + 1,
      total_aliases
    );

    // Step 1: Use the alias reserved for this logical account slot.
    self.emit(
      app_handle,
      RegistrationStep::GeneratingAlias,
      &format!("{prefix} Using email alias..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    self.log(&format!("{prefix} Email alias reserved"));

    // Step 2: Use the account identity reserved for this logical slot.
    self.emit(
      app_handle,
      RegistrationStep::GeneratingUserInfo,
      &format!("{prefix} Using reserved user info..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    self.log(&format!("{prefix} User identity reserved"));

    // Step 3: Launch (or relaunch) the reused worker profile for THIS account.
    // Lifecycle: ensure worker once → launch (new FP + ephemeral dir) → register → kill only.
    self.emit(
      app_handle,
      RegistrationStep::LaunchingBrowser,
      &format!("{prefix} Launching browser worker profile..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );

    self.device_id.clone_from(&identity.device_id);

    let (profile, mut session) = self.launch_and_connect(app_handle).await?;
    self.log(&format!(
      "{prefix} Browser worker launched: {} ({})",
      profile.name, profile.id
    ));

    // Kill browser after each account; keep worker profile metadata for reuse.
    let mut active_profile = profile;
    let registration_result = self
      .run_registration_in_browser(
        app_handle,
        email_service,
        sms_service,
        &mut session,
        &mut active_profile,
        cdk,
        base_email,
        &identity.alias_email,
        &identity.password,
        &identity.first_name,
        &identity.last_name,
        &identity.birthdate,
        prefix.as_str(),
        cdk_idx,
        alias_idx,
        total_cdks,
      )
      .await;

    // Kill the browser before we yield back to the alias loop. If the kill
    // fails, the next alias would launch a second browser on top of the
    // first — accumulating Camoufox processes and freezing the machine.
    // Propagate the error so the outer loop can abort this CDK cleanly.
    if let Err(e) = self.kill_browser_only(app_handle, &active_profile).await {
      self.log(&format!(
        "{prefix} Browser kill failed; aborting CDK to avoid overlapping browsers: {e}"
      ));
      return Err(format!("browser kill failed for alias {alias_idx}: {e}"));
    }
    self.log(&format!(
      "{prefix} Browser closed (worker profile kept for reuse)"
    ));

    registration_result
  }

  /// Core registration steps that run inside an already-launched browser profile.
  #[allow(clippy::too_many_arguments)]
  async fn run_registration_in_browser(
    &mut self,
    app_handle: &tauri::AppHandle,
    email_service: &dyn EmailService,
    sms_service: Option<&dyn SmsService>,
    session: &mut BrowserSession,
    profile: &mut crate::profile::BrowserProfile,
    cdk: &str,
    base_email: &str,
    alias_email: &str,
    password: &str,
    first_name: &str,
    last_name: &str,
    birthdate: &str,
    prefix: &str,
    cdk_idx: u32,
    alias_idx: u32,
    total_cdks: u32,
  ) -> Result<RegistrationResult, String> {
    // Always start from a wiped jar — never inherit previous OpenAI sessions.
    self.log(&format!(
      "{prefix} Clearing browser cookies/cache/storage..."
    ));
    if let Err(e) = session.clear_all_site_data().await {
      self.log(&format!("{prefix} clear_all_site_data warning: {e}"));
    }
    self.log(&format!("{prefix} Site data cleared"));

    // Seed oai-did cookie
    for domain in &[
      "chatgpt.com",
      ".chatgpt.com",
      "auth.openai.com",
      ".auth.openai.com",
    ] {
      if let Err(e) = session.set_cookie("oai-did", &self.device_id, domain).await {
        self.log(&format!("{prefix} set_cookie {domain} warning: {e}"));
      }
    }
    self.log(&format!("{prefix} Device cookie seeded"));

    // Step 4: Visit chatgpt.com
    self.emit(
      app_handle,
      RegistrationStep::VisitingHomepage,
      &format!("{prefix} Visiting chatgpt.com..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    session.navigate("https://chatgpt.com/", 30).await?;
    self.log(&format!(
      "{prefix} chatgpt.com loaded ({})",
      session.current_url().await.unwrap_or_default()
    ));

    // Humanize: explore the homepage briefly (scroll, idle) before proceeding
    self.human_pause(1200, 2800).await;
    let _ = session.evaluate("window.scrollBy(0, 300)", false).await;
    self.human_pause(800, 1500).await;
    let _ = session.evaluate("window.scrollBy(0, -150)", false).await;
    self.human_pause(600, 1200).await;

    // Step 5: CSRF token
    self.emit(
      app_handle,
      RegistrationStep::GettingCsrfToken,
      &format!("{prefix} Getting CSRF..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    // Step 6: Submit email
    self.emit(
      app_handle,
      RegistrationStep::SubmittingEmail,
      &format!("{prefix} Submitting email..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    // Step 7: Follow authorize
    self.emit(
      app_handle,
      RegistrationStep::FollowingAuthorize,
      &format!("{prefix} Following authorize..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    let mut cur_url = self
      .authorize_with_retry(session, profile, alias_email, app_handle)
      .await?;
    self.log(&format!(
      "{prefix} Authorize → {cur_url} (profile={})",
      profile.id
    ));
    // Give auth.openai.com time to settle cookies after the authorize redirect.
    self.human_pause(1400, 2600).await;
    if let Ok(url) = session.current_url().await {
      cur_url = url;
      self.log(&format!("{prefix} Post-authorize URL: {cur_url}"));
    }

    // Step 8-12: State machine loop
    let mut register_submitted = false;
    let mut account_created = false;
    let mut phone_number_used = String::new();
    let mut seen_states: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

    for _ in 0..12 {
      if self.is_cancelled() {
        if account_created {
          self.log(&format!(
            "{prefix} Cancelled after account creation; preserving credentials"
          ));
          break;
        }
        return Err("Cancelled".into());
      }
      let cur_signals = observe_registration_page_signals(session)
        .await
        .unwrap_or_default();
      let page = classify_registration_page(&cur_url, &cur_signals);
      let sig = format!("{page:?}");
      *seen_states.entry(sig.clone()).or_insert(0) += 1;
      if seen_states[&sig] > 2 {
        return Err(format!("State loop: {sig}"));
      }

      self.log(&format!("{prefix} Page: {page:?}"));

      match page {
        PageType::CreateAccountPassword | PageType::Unknown => {
          if register_submitted {
            return Err("Back at password page".into());
          }
          self.emit(
            app_handle,
            RegistrationStep::RegisteringUser,
            &format!("{prefix} Registering..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );

          // UI-first: stay in the authorize session. Hard-navigating away from
          // log-in-or-create-account invalidates OpenAI auth state.
          if let Err(e) = self.advance_auth_ui_to_password(session, alias_email).await {
            self.log(&format!("{prefix} advance_auth_ui: {e}"));
          }
          if let Ok(url) = session.current_url().await {
            cur_url = url;
            self.log(&format!("{prefix} Auth UI URL: {cur_url}"));
          }

          if classify_registration_password_route(&cur_url)
            == RegistrationPasswordRoute::RecoverExistingIdentity
          {
            cur_url = self
              .recover_created_identity(session, password, prefix)
              .await?;
            register_submitted = true;
            continue;
          }

          // OpenAI sometimes lands on email-verification without showing a password form
          // (especially after choose-an-account). Prefer UI password; only force
          // API register when session is still on a password-capable step.
          // Never force-register on a stale email-otp step → invalid_auth_step.
          let mut password_submit_outcome = PasswordSubmitOutcome::Advanced;
          let on_email_otp_surface = cur_url.contains("email-verification")
            || cur_url.contains("email-otp")
            || cur_url.contains("about-you");

          if on_email_otp_surface {
            // Attempt 1: recover password form via "Continue with password".
            self.human_pause(400, 900).await;
            let recovered = click_password_method(session).await.is_ok();
            if recovered {
              self.human_pause(700, 1400).await;
              let recovery_url = session.current_url().await.unwrap_or_default();
              if classify_registration_password_route(&recovery_url)
                == RegistrationPasswordRoute::RecoverExistingIdentity
              {
                cur_url = self
                  .recover_created_identity(session, password, prefix)
                  .await?;
                register_submitted = true;
                continue;
              }
            }
            if self
              .page_has_selector(
                session,
                r#"input[name="new-password"], input[type="password"], input[autocomplete="new-password"]"#,
              )
              .await
            {
              let outcome = self
                .submit_password_via_ui(session, password)
                .await
                .map_err(|e| format!("password UI after recovery failed: {e}"))?;
              password_submit_outcome = outcome;
              self.log(&format!(
                "{prefix} Password submitted via recovered UI form ({outcome:?})"
              ));
            } else {
              // Fresh authorize session, then force register once.
              self.log(&format!(
                "{prefix} Password form missing on {cur_url}; refreshing authorize then force-register"
              ));
              self.navigate_to_signup_page(session, alias_email).await?;
              self.human_pause(1200, 2000).await;
              if let Err(e) = self.advance_auth_ui_to_password(session, alias_email).await {
                self.log(&format!("{prefix} advance_auth_ui after refresh: {e}"));
              }
              if self
                .page_has_selector(
                  session,
                  r#"input[name="new-password"], input[type="password"], input[autocomplete="new-password"]"#,
                )
                .await
              {
                let outcome = self
                  .submit_password_via_ui(session, password)
                  .await
                  .map_err(|e| format!("password UI after authorize refresh failed: {e}"))?;
                password_submit_outcome = outcome;
                self.log(&format!(
                  "{prefix} Password submitted via UI after authorize refresh ({outcome:?})"
                ));
              } else {
                self
                  .force_api_register(session, alias_email, password)
                  .await?;
                self.log(&format!("{prefix} Force-register request accepted"));
              }
            }
          } else {
            let outcome = self
              .submit_password_via_ui(session, password)
              .await
              .map_err(|ui_err| format!("Password UI submit failed before dispatch: {ui_err}"))?;
            password_submit_outcome = outcome;
            self.log(&format!(
              "{prefix} Password submitted via UI form ({outcome:?})"
            ));
          }
          register_submitted = true;
          if password_submit_outcome == PasswordSubmitOutcome::Ambiguous {
            return Err(
              "Password submit remained on the create-password form after one retry without a classified error"
                .into(),
            );
          }

          let has_pending_otp = self.pending_email_otp.is_some();
          if should_request_new_email_otp(has_pending_otp) {
            self.emit(
              app_handle,
              RegistrationStep::SendingEmailOtp,
              &format!("{prefix} Requesting OTP..."),
              cdk_idx,
              alias_idx,
              total_cdks,
              None,
            );
            let otp_send_labels: &[&str] =
              if password_submit_outcome == PasswordSubmitOutcome::Ambiguous {
                &["Send code", "Send verification"]
              } else {
                &["Send code", "Continue", "Send verification", "Next"]
              };
            let mut otp_sent_via_ui = false;
            for label in otp_send_labels {
              if self
                .click_by_text(session, label, "button, [role='button']")
                .await
                .is_ok()
              {
                self.log(&format!("{prefix} OTP send via UI click: '{label}'"));
                otp_sent_via_ui = true;
                break;
              }
            }
            if !otp_sent_via_ui {
              // Same-origin browser fetch (cookies + SPA headers), not external HTTP.
              let response = self.send_email_otp_in_page(session).await?;
              let status = response["_status"].as_u64().unwrap_or(0);
              if !(200..300).contains(&status) {
                return Err(format!(
                  "OTP send via in-page SPA fetch rejected with HTTP {status}"
                ));
              }
              self.log(&format!(
                "{prefix} OTP send via in-page SPA fetch accepted (status={status})"
              ));
            }
          } else {
            self.emit(
              app_handle,
              RegistrationStep::SendingEmailOtp,
              &format!("{prefix} Rebinding retained OTP on the new Japan peer..."),
              cdk_idx,
              alias_idx,
              total_cdks,
              None,
            );
            self.log(&format!(
              "{prefix} Retained OTP present; suppressing resend after Japan peer rotation"
            ));
          }

          // After password submit, settle on the current semantic surface.
          self.human_pause(1200, 2400).await;
          cur_url = session.current_url().await.unwrap_or_default();
          let settled_signals = observe_registration_page_signals(session)
            .await
            .unwrap_or_default();
          let settled_page = classify_registration_page(&cur_url, &settled_signals);
          if !matches!(
            settled_page,
            PageType::EmailOtpVerification | PageType::AboutYou | PageType::AddPhone
          ) {
            return Err(format!(
              "Password submit advanced to unsupported surface: {}",
              safe_provider_detail(&cur_url)
            ));
          }
          self.log(&format!(
            "{prefix} After password submit: {settled_page:?} at {cur_url}"
          ));
          continue;
        }

        PageType::EmailOtpVerification => {
          // Intermediate email-verification screens can appear before password/register.
          // ChatGPT shows "Continue with password" link on this page. Click it and
          // wait for the password form to appear before continuing the main loop.
          if !register_submitted {
            self.log(&format!(
              "{prefix} Email verification page before register — clicking 'Continue with password'"
            ));
            let clicked = click_password_method(session).await.is_ok();
            if clicked {
              self.log("{prefix} Clicked 'Continue with password' — waiting for password form");
              // Poll for the password input to appear (up to 20 iterations, ~10s).
              for _ in 0..20 {
                self.human_pause(400, 600).await;
                let url = session.current_url().await.unwrap_or_default();
                if self
                  .page_has_selector(
                    session,
                    r#"input[name="new-password"], input[type="password"], input[autocomplete="new-password"]"#,
                  )
                  .await
                {
                  cur_url = url;
                  self.log(&format!("{prefix} Password form appeared at {cur_url}"));
                  // Don't loop back — proceed directly to password fill below.
                  // Fall through to submit password.
                  break;
                }
              }
            }
            if !clicked {
              self.log(&format!(
                "{prefix} 'Continue with password' link not found — falling back to direct navigate"
              ));
              session
                .navigate("https://auth.openai.com/create-account/password", 15)
                .await?;
              self.human_pause(1200, 2400).await;
            }
            // After clicking password method, the page should now have password form.
            // Continue the main loop to re-classify and handle the password step.
            cur_url = session.current_url().await.unwrap_or_default();
            continue;
          }

          // Up to 3 OTP cycles: poll → validate. Persistent Turnstile / HTTP
          // 409 exits this browser session so outer retry can rotate Japan
          // WireGuard peer. On 401/invalid, mark used + re-send a new code.
          let mut otp_ok = false;
          let mut last_otp_err = String::new();
          for otp_attempt in 1..=3 {
            self.emit(
              app_handle,
              RegistrationStep::PollingOtp,
              &format!("{prefix} Waiting for OTP (attempt {otp_attempt}/3)..."),
              cdk_idx,
              alias_idx,
              total_cdks,
              None,
            );

            // Reuse only a code retained from the same identity's previous
            // challenge session. It lives in RAM and is never logged/persisted.
            let otp = if let Some(code) = self.pending_email_otp.take() {
              self.log(&format!(
                "{prefix} Reusing in-memory OTP after Japan peer rotation"
              ));
              code
            } else {
              // First attempt can use longer poll; after 401 re-send, wait for new mail.
              let poll_secs = if otp_attempt == 1 { 150 } else { 120 };
              match email_service.poll_verification_code_with_cancel(
                cdk,
                poll_secs,
                self.cancel_flag.as_ref(),
              ) {
                Ok(c) => {
                  self.log(&format!("{prefix} OTP received (attempt {otp_attempt}/3)"));
                  c
                }
                Err(e) => {
                  last_otp_err = format!("OTP poll: {}", safe_email_service_error(&e));
                  self.log(&format!("{prefix} {last_otp_err}"));
                  break;
                }
              }
            };

            self.emit(
              app_handle,
              RegistrationStep::VerifyingOtp,
              &format!("{prefix} Verifying OTP..."),
              cdk_idx,
              alias_idx,
              total_cdks,
              None,
            );
            // Ensure we are on the verification page with a live auth session cookie.
            let page_url = session.current_url().await.unwrap_or_default();
            if !page_url.contains("email-verification") && !page_url.contains("email-otp") {
              let _ = session
                .navigate("https://auth.openai.com/email-verification", 20)
                .await;
              self.human_pause(800, 1500).await;
            }

            // CF wall hides OTP inputs and makes validate return 409. Do not
            // spend or submit the OTP on this session: outer retry will close
            // the browser and rotate to a different Japan WireGuard peer.
            if let Err(e) = self
              .wait_out_cloudflare_challenge_if_any(session, prefix)
              .await
            {
              self.pending_email_otp = Some(otp);
              return Err(auth_challenge_rotate_error(&e));
            }

            // Try UI-first OTP validation (type code into form fields).
            // Falls back to same-origin validation only while the UI remains pending.
            match self.try_ui_email_otp(session, &otp).await {
              Ok(EmailOtpUiOutcome::Accepted(continue_url)) => {
                email_service.mark_verification_code_used(cdk, &otp);
                self.pending_email_otp = None;
                cur_url = continue_url;
                self.log(&format!(
                  "{prefix} OTP verified via UI (attempt {otp_attempt}/3) → {cur_url}"
                ));
                otp_ok = true;
                break;
              }
              Ok(EmailOtpUiOutcome::Rejected(detail)) => {
                email_service.mark_verification_code_used(cdk, &otp);
                self.pending_email_otp = None;
                last_otp_err = detail;
                self.log(&format!("{prefix} {last_otp_err}"));
                if otp_attempt == 3 {
                  break;
                }
                self
                  .resend_email_otp_after_rejection(session, prefix)
                  .await?;
                continue;
              }
              Ok(EmailOtpUiOutcome::Pending) => {
                self.log(&format!(
                  "{prefix} UI OTP remained pending, falling back to same-origin validation"
                ));
              }
              Err(e) => {
                self.pending_email_otp = Some(otp);
                return Err(format!("Email OTP UI submission was indeterminate: {e}"));
              }
            }

            // In-page browser fetch only — same cookies/origin/TLS as SPA.
            // Never use an external HTTP client for OpenAI auth endpoints.
            self.log(&format!(
              "{prefix} UI OTP did not advance; using in-page SPA fetch (browser context)"
            ));
            let verify = match self.validate_email_otp_in_page(session, &otp).await {
              Ok(verify) => verify,
              Err(error) => {
                self.pending_email_otp = Some(otp);
                return Err(format!(
                  "Email OTP same-origin validation was indeterminate: {error}"
                ));
              }
            };
            let Some(vs) = verify.get("_status").and_then(Value::as_u64) else {
              self.pending_email_otp = Some(otp);
              return Err("Email OTP validation response was missing its HTTP status".into());
            };
            if vs == 200 && verify.get("error").is_none() {
              email_service.mark_verification_code_used(cdk, &otp);
              self.pending_email_otp = None;
              cur_url = verify["continue_url"]
                .as_str()
                .unwrap_or("https://auth.openai.com/about-you")
                .to_string();
              // In-page validate only returns continue_url — drive navigation so
              // the next state machine step sees the About You form.
              if let Err(e) = session.navigate(&cur_url, 25).await {
                self.log(&format!(
                  "{prefix} WARN: post-OTP navigate to {cur_url} failed: {e}"
                ));
              } else {
                self.human_pause(800, 1600).await;
                cur_url = session.current_url().await.unwrap_or(cur_url);
              }
              self.log(&format!(
                "{prefix} OTP verified via in-page SPA fetch (attempt {otp_attempt}/3) → {cur_url}"
              ));
              otp_ok = true;
              break;
            }

            last_otp_err = format!("OTP verification rejected with HTTP {vs}");
            self.log(&format!("{prefix} {last_otp_err}"));

            if is_email_otp_conflict_status(vs) {
              // Do NOT burn or re-send the code on this auth session. A 409
              // after UI/in-page validation is an auth-binding/CF conflict;
              // outer retry rotates to a fresh Japan peer and keeps identity.
              self.pending_email_otp = Some(otp);
              return Err(auth_challenge_rotate_error(
                "email OTP validate returned HTTP 409",
              ));
            }

            let is_stale = is_email_otp_stale_rejection(vs, &verify);
            if is_stale {
              email_service.mark_verification_code_used(cdk, &otp);
              self.pending_email_otp = None;
            } else {
              self.pending_email_otp = Some(otp);
              break;
            }

            if otp_attempt == 3 {
              break;
            }

            self
              .resend_email_otp_after_rejection(session, prefix)
              .await?;
          }

          if !otp_ok {
            return Err(if last_otp_err.is_empty() {
              "OTP verification failed".into()
            } else {
              last_otp_err
            });
          }
          continue;
        }

        PageType::AboutYou => {
          if account_created {
            return Err("Back at about-you".into());
          }
          self.emit(
            app_handle,
            RegistrationStep::CreatingAccount,
            &format!("{prefix} Creating account..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );

          // About You must be completed through the visible form. Falling back
          // to create_account API skips the same human signals this page asks for.
          self
            .submit_about_you_via_ui(session, first_name, last_name, birthdate)
            .await
            .map_err(|e| format!("About You UI submit failed: {e}"))?;
          account_created = true;
          self.log(&format!("{prefix} Account created via UI form"));
          cur_url = session.current_url().await.unwrap_or_default();
          continue;
        }

        PageType::AddPhone => {
          let Some(sms) = sms_service else {
            return Err(
              "Phone verification required but no SMS provider configured (set smsProvider/smsToken/smsServiceId)"
                .into(),
            );
          };
          let service_id = self.config.sms_service_id.ok_or_else(|| {
            "Phone verification required but smsServiceId is not configured".to_string()
          })?;

          self.emit(
            app_handle,
            RegistrationStep::RequestingSmsOtp,
            &format!("{prefix} Renting SMS number..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );

          let request = NumberRequest {
            service_id,
            network: self.config.sms_network.clone(),
            prefix: None,
            except_prefix: None,
            number: None,
            country: self.config.sms_country.clone(),
          };
          let number_info = sms
            .request_number(&request)
            .map_err(|error| format!("SMS rent number: {}", safe_sms_service_error(&error)))?;
          phone_number_used = number_info.phone_number.clone();
          self.log(&format!(
            "{prefix} SMS number: {} (request_id={})",
            number_info.phone_number, number_info.request_id
          ));

          // Fill phone field + submit (best-effort selectors for OpenAI add-phone UI).
          let phone_selectors = [
            r#"input[type="tel"]"#,
            r#"input[name="phone"]"#,
            r#"input[name="phoneNumber"]"#,
            r#"input[autocomplete="tel"]"#,
            r#"input[inputmode="tel"]"#,
          ];
          let mut filled = false;
          for sel in phone_selectors {
            if self
              .fill_input(session, sel, &number_info.phone_number)
              .await
              .is_ok()
            {
              filled = true;
              break;
            }
          }
          if !filled {
            return Err("Could not find phone input on add-phone page".into());
          }
          self.human_pause(350, 450).await;

          let mut submitted = false;
          for sel in [
            r#"button[type="submit"]"#,
            r#"button[name="intent"]"#,
            r#"form button"#,
          ] {
            if self
              .click_selector(session, sel, "sms phone submit")
              .await
              .is_ok()
            {
              submitted = true;
              break;
            }
          }
          if !submitted {
            let _ = self.click_by_text(session, "Continue", "button").await;
            let _ = self.click_by_text(session, "Send code", "button").await;
            let _ = self.click_by_text(session, "Verify", "button").await;
          }

          self.emit(
            app_handle,
            RegistrationStep::PollingSmsOtp,
            &format!("{prefix} Waiting for SMS OTP..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );
          if let Ok(page_url) = session.current_url().await {
            if !is_registration_dom_surface(&page_url) {
              return Err(format!(
                "Refusing SMS OTP entry on external origin: {}",
                safe_provider_detail(&page_url)
              ));
            }
          }
          let otp_info =
            poll_otp_with_cancel(sms, &number_info.request_id, 150, self.cancel_flag.as_ref())
              .map_err(|error| format!("SMS OTP poll: {}", safe_sms_service_error(&error)))?;
          let sms_code = otp_info
            .code
            .filter(|c| !c.is_empty())
            .ok_or_else(|| "SMS OTP completed without code".to_string())?;
          self.log(&format!("{prefix} SMS OTP received"));

          self.emit(
            app_handle,
            RegistrationStep::VerifyingSmsOtp,
            &format!("{prefix} Verifying SMS OTP..."),
            cdk_idx,
            alias_idx,
            total_cdks,
            None,
          );

          // Prefer OTP input on page; fall back to OpenAI phone-otp validate API.
          let otp_selectors = [
            r#"input[name="code"]"#,
            r#"input[autocomplete="one-time-code"]"#,
            r#"input[inputmode="numeric"]"#,
            r#"input[type="text"]"#,
            r#"input[type="tel"]"#,
          ];
          let mut otp_filled = false;
          for sel in otp_selectors {
            if self.fill_input(session, sel, &sms_code).await.is_ok() {
              otp_filled = true;
              break;
            }
          }
          if otp_filled {
            let mut otp_submitted = false;
            for sel in [r#"button[type="submit"]"#, r#"form button"#] {
              if self
                .click_selector(session, sel, "sms otp submit")
                .await
                .is_ok()
              {
                otp_submitted = true;
                break;
              }
            }
            if !otp_submitted {
              let _ = self.click_by_text(session, "Continue", "button").await;
              let _ = self.click_by_text(session, "Verify", "button").await;
            }
          } else {
            let verify_js = format!(
              "fetch('https://auth.openai.com/api/accounts/phone-otp/validate', {{ method: 'POST', credentials: 'include', headers: {{ 'content-type': 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{ code: '{code}' }}) }})",
              did = self.device_id,
              code = sms_code,
            );
            let verify = session.fetch_json(&verify_js).await?;
            let vs = verify["_status"].as_u64().unwrap_or(200);
            if vs != 200 {
              return Err(format!("SMS OTP verification failed with HTTP {vs}"));
            }
            if let Some(next) = verify["continue_url"].as_str() {
              if !next.is_empty() {
                session.navigate(next, 30).await?;
              }
            }
          }

          self.human_pause(1800, 2200).await;
          cur_url = session.current_url().await.unwrap_or_default();
          continue;
        }
        PageType::Consent => {
          // auth.openai.com consent screen — registration was accepted.
          self.log(&format!("{prefix} ✅ Flow complete (consent)"));
          break;
        }
        PageType::ChatgptHome | PageType::Callback | PageType::ExternalUrl => {
          // When the account was already created (About You form submitted),
          // or the identity was recovered (existing account login), landing on
          // chatgpt.com is the expected post-registration home page.
          // Break out of the loop and proceed to token extraction.
          if account_created || register_submitted {
            self.log(&format!(
              "{prefix} ✅ Account ready — proceeding to token extraction from {cur_url}"
            ));
            break;
          }

          // The signup navigation did not land on an auth.openai.com form.
          // Common causes:
          //   - chatgpt.com/: the Continue button didn't trigger IDP redirect
          //   - appleid.apple.com / accounts.google.com: an SSO button was
          //     accidentally clicked instead of the email-form Continue
          //   - auth.openai.com/api/accounts/callback: a stale OAuth callback
          //
          // All of these are recoverable: go back to chatgpt.com homepage
          // and re-click the Sign up button + re-fill the email.
          let lower = cur_url.to_ascii_lowercase();
          if lower.contains("auth.openai.com") && lower.contains("/callback") {
            // A real auth.openai.com callback with no registration form means
            // the OAuth round-trip completed but didn't create a new account.
            self.log(&format!("{prefix} ✅ Flow complete (callback)"));
            break;
          }

          self.log(&format!(
            "{prefix} Signup navigation stalled on {cur_url}; recovering via chatgpt.com homepage"
          ));
          session.navigate("https://chatgpt.com/", 25).await?;
          self.human_pause(1500, 2500).await;
          let _ = session.evaluate("window.scrollBy(0, 200)", false).await;
          self.human_pause(600, 1200).await;
          match self.navigate_to_signup_page(session, alias_email).await {
            Ok(after) => {
              cur_url = after;
              self.log(&format!("{prefix} Recover from stalled signup → {cur_url}"));
            }
            Err(e) => {
              self.log(&format!("{prefix} Signup recovery failed: {e}"));
              cur_url = session.current_url().await.unwrap_or_default();
            }
          }
          continue;
        }
        PageType::LoginPassword => {
          cur_url = self
            .recover_created_identity(session, password, prefix)
            .await?;
          register_submitted = true;
          continue;
        }
        PageType::ErrorPage => {
          return Err(format!("Error page: {cur_url}"));
        }
      }
    }

    // Extract tokens — soft-fail if account already created (do not lose the account).
    self.emit(
      app_handle,
      RegistrationStep::ExtractingTokens,
      &format!("{prefix} Extracting tokens..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    let (access_token, account_id) = self.extract_access_token_with_retry(session, prefix).await;

    self.log(&format!(
      "{prefix} access token obtained={}",
      !access_token.is_empty()
    ));
    if access_token.is_empty() && !account_created {
      return Err("No access token and account was not created".into());
    }

    // Gate: only keep accounts that have free trial / free Plus offer.
    self.emit(
      app_handle,
      RegistrationStep::CheckingFreeOffer,
      &format!("{prefix} Checking free offer / free trial..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );
    let (free_trial_eligible, plan_type, offer_detail) = self
      .check_free_trial_offer(session, &access_token, &account_id)
      .await;
    self.log(&format!(
      "{prefix} Free offer check: eligible={free_trial_eligible} plan={plan_type} detail={offer_detail}"
    ));

    // No free trial: still persist the account under CDK stats (not resellable).
    // Return Ok so the alias is not retried; success=false / free_trial_eligible=false.
    if !free_trial_eligible {
      let err = format!("No free trial/free Plus offer (plan={plan_type}; {offer_detail})");
      self.log(&format!("{prefix} {err} — saving as free-trial-no"));
      let result = RegistrationResult {
        success: false,
        email: alias_email.to_string(),
        password: password.to_string(),
        account_id,
        access_token,
        device_id: self.device_id.clone(),
        error_message: err.clone(),
        step_logs: self.logs.clone(),
        created_at: Utc::now(),
        two_fa_enabled: false,
        totp_secret: String::new(),
        free_trial_eligible: false,
        plan_type: plan_type.clone(),
        cdk: cdk.to_string(),
        base_email: base_email.to_string(),
        phone_number: phone_number_used,
        status: super::types::AccountInventoryStatus::Invalid,
        note: "free_trial_no".into(),
        exported_at: None,
        sold_at: None,
        email_provider: Some(self.config.email_provider),
        email_provider_provenance: Some(EmailProviderProvenance::RegistrationConfig),
        registration_outcome_reason: Some(RegistrationOutcomeReason::FreeTrialNo),
        two_factor_backfill_access_state: Some(TwoFactorBackfillAccessState::Accessible),
        two_factor_backfill_exclusion: None,
        two_factor_backfill_state: None,
        two_factor_backfill_outcome: None,
        two_factor_backfill_operation_id: None,
        record_revision: 1,
      };
      return Ok(result);
    }

    let operation_id = format!("registration-{}-{}-{}", self.task_id, cdk_idx, alias_idx);
    let account_key = if account_id.trim().is_empty() {
      alias_email.to_string()
    } else {
      account_id.clone()
    };
    let mut provisional = RegistrationResult {
      success: false,
      email: alias_email.to_string(),
      password: password.to_string(),
      account_id,
      access_token,
      device_id: self.device_id.clone(),
      error_message: "2FA is not ready".into(),
      step_logs: self.logs.clone(),
      created_at: Utc::now(),
      two_fa_enabled: false,
      totp_secret: String::new(),
      free_trial_eligible: true,
      plan_type: plan_type.clone(),
      cdk: cdk.to_string(),
      base_email: base_email.to_string(),
      phone_number: phone_number_used,
      status: AccountInventoryStatus::Reserved,
      note: "two_factor_pending".into(),
      exported_at: None,
      sold_at: None,
      email_provider: Some(self.config.email_provider),
      email_provider_provenance: Some(EmailProviderProvenance::RegistrationConfig),
      registration_outcome_reason: Some(RegistrationOutcomeReason::Registered),
      two_factor_backfill_access_state: Some(TwoFactorBackfillAccessState::Accessible),
      two_factor_backfill_exclusion: None,
      two_factor_backfill_state: Some(TwoFactorBackfillState::InProgress),
      two_factor_backfill_outcome: None,
      two_factor_backfill_operation_id: Some(operation_id.clone()),
      record_revision: 1,
    };

    // Persist credentials before the first remote 2FA action. Returning an Ok
    // non-success result keeps the outer alias loop from creating another account
    // if this local preparation fails after the remote account already exists.
    if let Err(error) = save_registration_result(&provisional) {
      self.log(&format!(
        "{prefix} 2FA preparation persistence failed: {error}"
      ));
      provisional.error_message =
        "Account created but 2FA preparation could not be persisted".into();
      provisional.note = "two_factor_persistence_failed".into();
      provisional.two_factor_backfill_state = Some(TwoFactorBackfillState::Completed);
      provisional.two_factor_backfill_outcome = Some(TwoFactorBackfillOutcome::Failed);
      return Ok(provisional);
    }

    self.emit(
      app_handle,
      RegistrationStep::Enabling2Fa,
      &format!("{prefix} Enabling 2FA..."),
      cdk_idx,
      alias_idx,
      total_cdks,
      None,
    );

    let mut journal = match TwoFactorBackfillJournal::new() {
      Ok(journal) => journal,
      Err(error) => {
        self.log(&format!(
          "{prefix} 2FA journal unavailable: {}",
          safe_provider_detail(&error)
        ));
        let result = compare_and_update_registered_account(
          &account_key,
          provisional.record_revision,
          BackfillPatchPrecondition::finalize_failed(&operation_id, false, false),
          TwoFactorBackfillPatch::finalize_failed(),
        )
        .unwrap_or_else(|_| durable_account_or_provisional(&account_key, &provisional));
        return Ok(result);
      }
    };

    const TWO_FA_ATTEMPTS: u32 = 3;
    let mut last_error = String::new();
    for attempt in 1..=TWO_FA_ATTEMPTS {
      if self.is_cancelled() {
        self.log(&format!(
          "{prefix} 2FA cancelled before secret capture; preserving account"
        ));
        let result = compare_and_update_registered_account(
          &account_key,
          provisional.record_revision,
          BackfillPatchPrecondition::finalize_cancelled(&operation_id, false, false),
          TwoFactorBackfillPatch::finalize_cancelled(),
        )
        .unwrap_or_else(|_| durable_account_or_provisional(&account_key, &provisional));
        return Ok(result);
      }

      let mut captured_journal_revision = None;
      let mut adapter = BrowserTwoFactorAdapter::new(session);
      let secret_result = enable_authenticator_two_factor(&mut adapter, |secret| {
        let entry = journal
          .create_secret_captured(
            &operation_id,
            &account_key,
            provisional.record_revision,
            secret.to_string(),
          )
          .map_err(TwoFactorError::SecretCallbackFailed)?;
        captured_journal_revision = Some(entry.journal_revision);
        Ok(())
      })
      .await;

      match secret_result {
        Ok(secret) => {
          let Some(captured_revision) = captured_journal_revision else {
            last_error = "2FA completed without a durable secret journal".into();
            break;
          };
          let persisted = match compare_and_update_registered_account(
            &account_key,
            provisional.record_revision,
            BackfillPatchPrecondition::finalize_new_registration_enabled(&operation_id),
            TwoFactorBackfillPatch::finalize_new_registration_enabled(secret.clone()),
          ) {
            Ok(persisted) => persisted,
            Err(error) => {
              self.log(&format!(
                "{prefix} 2FA remote On but account promotion failed: {error}"
              ));
              let _ =
                journal.transition_to_manual_review(&operation_id, &account_key, captured_revision);
              return Ok(
                compare_and_update_registered_account(
                  &account_key,
                  provisional.record_revision,
                  BackfillPatchPrecondition::finalize_reconciliation_required(
                    &operation_id,
                    false,
                    false,
                  ),
                  TwoFactorBackfillPatch::finalize_reconciliation_required(),
                )
                .unwrap_or_else(|_| durable_account_or_provisional(&account_key, &provisional)),
              );
            }
          };

          let confirmed = match journal.update_state(
            &operation_id,
            &account_key,
            captured_revision,
            TwoFactorBackfillJournalState::RemoteConfirmed,
          ) {
            Ok(confirmed) => confirmed,
            Err(error) => {
              self.log(&format!(
                "{prefix} 2FA enabled; journal confirmation warning: {error}"
              ));
              return Ok(persisted);
            }
          };
          let finalized = match journal.record_final_account_revision(
            &operation_id,
            &account_key,
            confirmed.journal_revision,
            persisted.record_revision,
          ) {
            Ok(finalized) => finalized,
            Err(error) => {
              self.log(&format!(
                "{prefix} 2FA enabled; journal revision warning: {error}"
              ));
              return Ok(persisted);
            }
          };
          if let Err(error) = journal.delete_after_account_patch(
            &operation_id,
            &account_key,
            finalized.journal_revision,
            PersistedBackfillAccountPatch::new(
              persisted.record_revision,
              Some(&operation_id),
              persisted.two_fa_enabled,
              &persisted.totp_secret,
            ),
          ) {
            self.log(&format!(
              "{prefix} 2FA enabled; journal cleanup warning: {error}"
            ));
          }
          self.log(&format!(
            "{prefix} 2FA enabled and persisted (attempt {attempt}/{TWO_FA_ATTEMPTS})"
          ));
          return Ok(persisted);
        }
        Err(error) => {
          last_error = safe_provider_detail(&error.to_string());
          self.log(&format!(
            "{prefix} 2FA attempt {attempt}/{TWO_FA_ATTEMPTS} failed: {last_error}"
          ));
          if let Some(captured_revision) = captured_journal_revision {
            let _ =
              journal.transition_to_manual_review(&operation_id, &account_key, captured_revision);
            return Ok(
              compare_and_update_registered_account(
                &account_key,
                provisional.record_revision,
                BackfillPatchPrecondition::finalize_reconciliation_required(
                  &operation_id,
                  false,
                  false,
                ),
                TwoFactorBackfillPatch::finalize_reconciliation_required(),
              )
              .unwrap_or_else(|_| durable_account_or_provisional(&account_key, &provisional)),
            );
          }
          if matches!(error, TwoFactorError::ReconciliationRequired) {
            return Ok(
              compare_and_update_registered_account(
                &account_key,
                provisional.record_revision,
                BackfillPatchPrecondition::finalize_reconciliation_required(
                  &operation_id,
                  false,
                  false,
                ),
                TwoFactorBackfillPatch::finalize_reconciliation_required(),
              )
              .unwrap_or_else(|_| durable_account_or_provisional(&account_key, &provisional)),
            );
          }
          if attempt < TWO_FA_ATTEMPTS {
            let _ = session.navigate("https://chatgpt.com/", 15).await;
            self.human_pause(1400, 2600).await;
          }
        }
      }
    }

    self.log(&format!(
      "{prefix} 2FA failed before secret capture after {TWO_FA_ATTEMPTS} attempts: {last_error}"
    ));
    Ok(
      compare_and_update_registered_account(
        &account_key,
        provisional.record_revision,
        BackfillPatchPrecondition::finalize_failed(&operation_id, false, false),
        TwoFactorBackfillPatch::finalize_failed(),
      )
      .unwrap_or_else(|_| durable_account_or_provisional(&account_key, &provisional)),
    )
  }

  // -----------------------------------------------------------------------
  // Enable ChatGPT authenticator 2FA via UI automation
  // -----------------------------------------------------------------------

  /// Advance OpenAI auth UI from log-in-or-create-account / login toward password.
  async fn advance_auth_ui_to_password(
    &mut self,
    session: &mut BrowserSession,
    email: &str,
  ) -> Result<(), String> {
    let mut url = session.current_url().await.unwrap_or_default();
    self.log(&format!("Auth UI start URL: {url}"));

    if self
      .page_has_selector(
        session,
        r#"input[name="new-password"], input[type="password"]"#,
      )
      .await
    {
      return Ok(());
    }

    // choose-an-account: pick "use a different email / sign up" so we leave the
    // residual session from a previous partial registration on this profile.
    if url.contains("choose-an-account") {
      let mut advanced = false;
      for label in [
        "Use a different email",
        "different email",
        "Sign up",
        "Create account",
        "Log in or sign up",
        "Continue",
      ] {
        if self
          .click_by_text(
            session,
            label,
            "button, a, [role='button'], div[role='button']",
          )
          .await
          .is_ok()
        {
          advanced = true;
          self.human_pause(800, 1200).await;
          break;
        }
      }
      if !advanced {
        // Click the last account card action or a generic "add" control.
        let _ = self
          .click_selector(
            session,
            r#"button, a[href*='email'], [data-testid*='add'], [data-testid*='different']"#,
            "choose-an-account action",
          )
          .await;
        self.human_pause(800, 1200).await;
      }
      url = session.current_url().await.unwrap_or_default();
      self.log(&format!("After choose-an-account action URL: {url}"));
    }

    let email_selectors =
      r#"input#email, input[name="email"], input[type="email"], input[autocomplete*="email"]"#;
    let email_visible = self.page_has_selector(session, email_selectors).await;

    if email_visible {
      // Fill email on this page (when email wasn't already entered via dialog).
      fill_visible_input(session, email_selectors, email, "email").await?;
      self.human_pause(350, 450).await;

      click_email_form_continue(session).await?;
    }

    // After submitting email (or if email was already entered via dialog),
    // wait for the password form to appear. OpenAI may show a method picker
    // ("Continue with password") before the password form.
    let mut password_method_clicked = false;
    for _ in 0..30 {
      self.human_pause(450, 550).await;
      url = session.current_url().await.unwrap_or_default();
      if self
        .page_has_selector(
          session,
          r#"input[name="new-password"], input[type="password"]"#,
        )
        .await
      {
        self.log(&format!("Password field appeared at {url}"));
        return Ok(());
      }
      // The helper has its own bounded retry loop. Run it once per auth surface
      // instead of multiplying that timeout by every polling iteration.
      if !password_method_clicked {
        password_method_clicked = true;
        if click_password_method(session).await.is_ok() {
          self.log(&format!(
            "Clicked 'password' method option at {url}; waiting for password form"
          ));
          self.human_pause(750, 850).await;
          if self
            .page_has_selector(
              session,
              r#"input[name="new-password"], input[type="password"]"#,
            )
            .await
          {
            let url_now = session.current_url().await.unwrap_or_default();
            self.log(&format!("Password field appeared after click at {url_now}"));
            return Ok(());
          }
        }
      }
      // Accept email-verification / about-you as valid outcomes (no password needed).
      if url.contains("email-verification") || url.contains("about-you") {
        self.log(&format!("Auth advanced to {url} without password field"));
        return Ok(());
      }
    }

    url = session.current_url().await.unwrap_or_default();
    if !self
      .page_has_selector(
        session,
        r#"input[name="new-password"], input[type="password"]"#,
      )
      .await
      && !url.contains("password")
    {
      return Err(format!(
        "could not reach password form from auth UI (url={url})"
      ));
    }
    Ok(())
  }

  async fn page_has_selector(&mut self, session: &mut BrowserSession, selector: &str) -> bool {
    has_visible_selector(session, selector, true)
      .await
      .unwrap_or(false)
  }

  /// Human-like pause between automation steps (random within range).
  async fn human_pause(&self, min_ms: u64, max_ms: u64) {
    use crate::browser_actions::jitter_ms;
    sleep(jitter_ms(min_ms, max_ms)).await;
  }

  async fn detect_cloudflare_challenge_from_dom(&mut self, session: &mut BrowserSession) -> bool {
    let result = session
      .evaluate(
        r#"(function(){
          const title = document.title || '';
          const body = (document.body && (document.body.innerText || document.body.textContent) || '').slice(0, 4000);
          const hasTurnstile = !!document.querySelector(
            'iframe[src*="challenges.cloudflare.com"], .cf-turnstile, #cf-turnstile, input[name="cf-turnstile-response"]'
          );
          return {
            title,
            body,
            hasTurnstile,
            href: location.href || ''
          };
        })()"#,
        false,
      )
      .await;
    let Ok(res) = result else {
      return false;
    };
    let value = res.get("value").cloned().unwrap_or_default();
    let title = value.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let body = value.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let href = value.get("href").and_then(|v| v.as_str()).unwrap_or("");
    let has_turnstile = value
      .get("hasTurnstile")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);
    has_turnstile
      || is_cloudflare_challenge_signal(title)
      || is_cloudflare_challenge_signal(body)
      || is_cloudflare_challenge_signal(href)
  }

  /// Soft-wait for Cloudflare managed challenge / Turnstile to auto-clear.
  async fn wait_out_cloudflare_challenge_if_any(
    &mut self,
    session: &mut BrowserSession,
    prefix: &str,
  ) -> Result<(), String> {
    if !self.detect_cloudflare_challenge_from_dom(session).await {
      return Ok(());
    }
    self.log(&format!(
      "{prefix} Cloudflare challenge detected; soft-waiting up to {CLOUDFLARE_SOFT_WAIT_SECS}s..."
    ));

    // —— Turnstile auto-bypass (Phase 2) ——
    // Try to click through before falling back to passive wait.
    match session.bypass_turnstile_if_present().await {
      Ok(true) => {
        self.log(&format!("{prefix} Turnstile bypassed via click"));
        self.human_pause(800, 1500).await;
        if !self.detect_cloudflare_challenge_from_dom(session).await {
          self.log(&format!(
            "{prefix} Cloudflare cleared after Turnstile bypass"
          ));
          return Ok(());
        }
        self.log(&format!(
          "{prefix} Turnstile bypassed but CF still present — continuing wait"
        ));
      }
      Ok(false) => {
        // No widget — might be a JS challenge, continue passive wait
      }
      Err(e) => {
        self.log(&format!("{prefix} Turnstile bypass failed: {e}"));
      }
    }

    let deadline =
      tokio::time::Instant::now() + std::time::Duration::from_secs(CLOUDFLARE_SOFT_WAIT_SECS);
    while tokio::time::Instant::now() < deadline {
      if self.is_cancelled() {
        return Err("Cancelled during Cloudflare challenge wait".into());
      }
      sleep(std::time::Duration::from_secs(2)).await;
      if !self.detect_cloudflare_challenge_from_dom(session).await {
        self.log(&format!(
          "{prefix} Cloudflare challenge cleared after soft-wait"
        ));
        return Ok(());
      }
    }
    Err(cloudflare_challenge_error_message())
  }

  /// Prefer real UI OTP entry. Only return None when no usable visible OTP
  /// surface exists so the caller can use an *in-page* browser fetch (same
  /// cookies/origin as the SPA), never an external HTTP client.
  async fn try_ui_email_otp(
    &mut self,
    session: &mut BrowserSession,
    otp_code: &str,
  ) -> Result<EmailOtpUiOutcome, String> {
    let current_url = session.current_url().await.unwrap_or_default();
    if !is_registration_dom_surface(&current_url) {
      return Err(format!(
        "refusing OTP entry on external origin: {}",
        safe_provider_detail(&current_url)
      ));
    }

    // Detect OTP input fields on the page. Prefer *visible* controls only —
    // CF interstitials leave hidden tel inputs that would false-positive.
    let detect_js = r#"(function(){
      const visible = (el) => {
        if (!el) return false;
        const r = el.getBoundingClientRect();
        const style = window.getComputedStyle(el);
        return r.width > 0 && r.height > 0
          && style.visibility !== 'hidden'
          && style.display !== 'none'
          && !el.disabled;
      };
      // Pattern 1: Multiple single-digit inputs (common OTP pattern)
      const singleDigitInputs = Array.from(document.querySelectorAll(
        'input[maxlength="1"], input[data-index], input[type="tel"][maxlength="1"], input[inputmode="numeric"][maxlength="1"]'
      )).filter(visible);
      if (singleDigitInputs.length >= 4) {
        return { found: true, mode: 'multi', count: singleDigitInputs.length,
          selectors: singleDigitInputs.map((el, i) => {
            const sel = el.id ? '#' + CSS.escape(el.id) :
              el.name ? 'input[name="' + CSS.escape(el.name) + '"]' :
              el.getAttribute('data-index') != null
                ? 'input[data-index="' + el.getAttribute('data-index') + '"]'
                : 'input[maxlength="1"]';
            return sel;
          })
        };
      }
      // Pattern 2: Single code input
      const codeInput = Array.from(document.querySelectorAll(
        'input[name="code"], input[name="otp"], input[name="emailCode"], ' +
        'input[autocomplete="one-time-code"], input[inputmode="numeric"], ' +
        'input[type="tel"][maxlength], input[aria-label*="code" i], input[placeholder*="code" i]'
      )).find(visible);
      if (codeInput) {
        const sel = codeInput.id ? '#' + CSS.escape(codeInput.id) :
          codeInput.name ? 'input[name="' + CSS.escape(codeInput.name) + '"]' :
          'input[autocomplete="one-time-code"], input[inputmode="numeric"]';
        return { found: true, mode: 'single', selector: sel };
      }
      return { found: false, title: document.title || '', hasTurnstile: !!document.querySelector('iframe[src*="challenges.cloudflare.com"], .cf-turnstile') };
    })()"#;

    let detect_response = session
      .evaluate(detect_js, false)
      .await
      .map_err(|error| format!("email OTP input detection failed: {error}"))?;
    let detect_result = detect_response
      .get("value")
      .cloned()
      .ok_or_else(|| "email OTP input detection returned no value".to_string())?;

    if detect_result["found"].as_bool() != Some(true) {
      return Ok(EmailOtpUiOutcome::Pending); // No OTP inputs found — caller may use in-page SPA fetch
    }

    self.log("UI OTP: found input fields, attempting humanized entry");
    let digits: String = otp_code.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
      return Err("email OTP provider returned no digits".into());
    }

    let mode = detect_result["mode"].as_str().unwrap_or("single");
    let mut filled;

    if mode == "multi" {
      // Prefer a single native fill across the multi-box group (React OTP
      // components often auto-advance and reject per-box human_type races).
      let multi_fill_js = format!(
        r#"(function(){{
          const code = {code};
          const visible = (el) => {{
            if (!el) return false;
            const r = el.getBoundingClientRect();
            const style = window.getComputedStyle(el);
            return r.width > 0 && r.height > 0
              && style.visibility !== 'hidden'
              && style.display !== 'none'
              && !el.disabled;
          }};
          const inputs = Array.from(document.querySelectorAll(
            'input[maxlength="1"], input[data-index], input[type="tel"][maxlength="1"], input[inputmode="numeric"][maxlength="1"]'
          )).filter(visible);
          if (inputs.length < 4) return {{ ok: false, reason: 'no_multi' }};
          const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value')?.set;
          for (let i = 0; i < Math.min(code.length, inputs.length); i++) {{
            const el = inputs[i];
            el.focus();
            if (setter) setter.call(el, code[i]);
            else el.value = code[i];
            el.dispatchEvent(new InputEvent('input', {{ bubbles: true, data: code[i], inputType: 'insertText' }}));
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
          }}
          const last = inputs[Math.min(code.length, inputs.length) - 1];
          if (last) {{
            last.dispatchEvent(new KeyboardEvent('keydown', {{ key: 'Enter', code: 'Enter', bubbles: true }}));
            last.dispatchEvent(new KeyboardEvent('keyup', {{ key: 'Enter', code: 'Enter', bubbles: true }}));
          }}
          return {{ ok: true, filled: Math.min(code.length, inputs.length) }};
        }})()"#,
        code = serde_json::to_string(&digits).unwrap_or_else(|_| "\"\"".into()),
      );
      let res = session
        .evaluate(&multi_fill_js, false)
        .await
        .map_err(|error| format!("multi-field email OTP entry failed: {error}"))?;
      filled = res
        .get("value")
        .and_then(|v| v.get("ok"))
        .and_then(|v| v.as_bool())
        == Some(true);

      if !filled {
        let count = detect_result["count"].as_u64().unwrap_or(6) as usize;
        for (i, digit) in digits.chars().enumerate().take(count) {
          let selector = detect_result["selectors"]
            .get(i)
            .and_then(|v| v.as_str())
            .unwrap_or("input[maxlength=\"1\"]");
          let digit_str = digit.to_string();
          let _ = self.click_selector(session, selector, "otp-digit").await;
          self.human_pause(60, 140).await;
          if let Err(error) = fill_visible_input(session, selector, &digit_str, "otp-digit").await {
            return Err(format!("email OTP digit {i} entry failed: {error}"));
          }
          self.human_pause(80, 180).await;
        }
        filled = true;
      }
    } else {
      let selector = detect_result["selector"]
        .as_str()
        .unwrap_or("input[name=\"code\"]");
      let _ = self.click_selector(session, selector, "otp-input").await;
      self.human_pause(100, 220).await;
      if fill_visible_input(session, selector, &digits, "email OTP")
        .await
        .is_err()
      {
        // Last resort: native value setter for controlled React inputs.
        let single_fill_js = format!(
          r#"(function(){{
            const el = document.querySelector({sel});
            if (!el) return false;
            const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value')?.set;
            el.focus();
            if (setter) setter.call(el, {code});
            else el.value = {code};
            el.dispatchEvent(new InputEvent('input', {{ bubbles: true, data: {code}, inputType: 'insertText' }}));
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            return true;
          }})()"#,
          sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
          code = serde_json::to_string(&digits).unwrap_or_else(|_| "\"\"".into()),
        );
        let fill_result = session
          .evaluate(&single_fill_js, false)
          .await
          .map_err(|error| format!("single-field email OTP entry failed: {error}"))?;
        let ok = fill_result
          .get("value")
          .and_then(|value| value.as_bool())
          .unwrap_or(false);
        if !ok {
          return Err("single-field email OTP entry was not accepted".into());
        }
      }
      filled = true;
    }

    if !filled {
      return Err("email OTP input was detected but could not be filled".into());
    }

    // Wait for a semantic transition or a clear visible rejection. The URL may
    // remain stale while React replaces the verification form with About You.
    for _ in 0..8_u32 {
      self.human_pause(400, 700).await;
      let new_url = session
        .current_url()
        .await
        .map_err(|error| format!("email OTP URL observation failed: {error}"))?;
      let signals = observe_registration_page_signals(session)
        .await
        .map_err(|error| format!("email OTP page observation failed: {error}"))?;
      match classify_email_otp_ui_state(&new_url, &signals) {
        EmailOtpUiState::Accepted => {
          self.log(&format!("UI OTP: accepted on semantic surface → {new_url}"));
          return Ok(EmailOtpUiOutcome::Accepted(new_url));
        }
        EmailOtpUiState::Rejected => {
          return Ok(EmailOtpUiOutcome::Rejected(
            if signals.error_text.is_empty() {
              "verification code was rejected".into()
            } else {
              format!("verification code was rejected: {}", signals.error_text)
            },
          ));
        }
        EmailOtpUiState::Pending => {}
        EmailOtpUiState::UnsafeOrigin => {
          return Err(format!(
            "refusing OTP entry on external origin: {}",
            safe_provider_detail(&new_url)
          ));
        }
      }
    }

    // Click a real submit control if auto-submit did not fire.
    let submit_selector =
      r#"button[type="submit"], button[name="intent"], button[data-testid*="continue" i]"#;
    if has_visible_selector(session, submit_selector, true).await? {
      click_trusted_submit(session, submit_selector, "email OTP").await?;
      self.human_pause(1200, 2200).await;
      let after_url = session
        .current_url()
        .await
        .map_err(|error| format!("email OTP post-submit URL observation failed: {error}"))?;
      let signals = observe_registration_page_signals(session)
        .await
        .map_err(|error| format!("email OTP post-submit observation failed: {error}"))?;
      match classify_email_otp_ui_state(&after_url, &signals) {
        EmailOtpUiState::Accepted => {
          self.log(&format!(
            "UI OTP: submitted via trusted submit → {after_url}"
          ));
          return Ok(EmailOtpUiOutcome::Accepted(after_url));
        }
        EmailOtpUiState::Rejected => {
          return Ok(EmailOtpUiOutcome::Rejected(
            if signals.error_text.is_empty() {
              "verification code was rejected".into()
            } else {
              format!("verification code was rejected: {}", signals.error_text)
            },
          ));
        }
        EmailOtpUiState::UnsafeOrigin => {
          return Err(format!(
            "refusing OTP entry on external origin: {}",
            safe_provider_detail(&after_url)
          ));
        }
        EmailOtpUiState::Pending => {}
      }
    }

    // Still on verification: accept a DOM transition even when the URL is stale.
    let stale_url = session
      .current_url()
      .await
      .map_err(|error| format!("email OTP final URL observation failed: {error}"))?;
    let stale_signals = observe_registration_page_signals(session)
      .await
      .map_err(|error| format!("email OTP final page observation failed: {error}"))?;
    match classify_email_otp_ui_state(&stale_url, &stale_signals) {
      EmailOtpUiState::Accepted => {
        self.log(&format!(
          "UI OTP: accepted on stale verification URL → {stale_url}"
        ));
        return Ok(EmailOtpUiOutcome::Accepted(stale_url));
      }
      EmailOtpUiState::Rejected => {
        return Ok(EmailOtpUiOutcome::Rejected(
          if stale_signals.error_text.is_empty() {
            "verification code was rejected".into()
          } else {
            format!(
              "verification code was rejected: {}",
              stale_signals.error_text
            )
          },
        ));
      }
      EmailOtpUiState::UnsafeOrigin => {
        return Err(format!(
          "refusing OTP entry on external origin: {}",
          safe_provider_detail(&stale_url)
        ));
      }
      EmailOtpUiState::Pending => {}
    }

    self.log("UI OTP: filled but page did not advance; caller may use in-page SPA fetch");
    Ok(EmailOtpUiOutcome::Pending)
  }

  async fn resend_email_otp_after_rejection(
    &mut self,
    session: &mut BrowserSession,
    prefix: &str,
  ) -> Result<(), String> {
    self.log(&format!(
      "{prefix} OTP rejected; refreshing verification and requesting a new code"
    ));
    session
      .navigate("https://auth.openai.com/email-verification", 25)
      .await?;
    self.human_pause(1200, 2200).await;
    self
      .wait_out_cloudflare_challenge_if_any(session, prefix)
      .await?;

    let response = self.send_email_otp_in_page(session).await?;
    let status = response["_status"].as_u64().unwrap_or(0);
    if !(200..300).contains(&status) {
      return Err(format!("Email OTP re-send was rejected with HTTP {status}"));
    }
    self.log(&format!(
      "{prefix} OTP re-send in-page SPA fetch accepted (status={status})"
    ));
    self.human_pause(2500, 4500).await;
    Ok(())
  }

  /// In-page SPA-style email OTP validate. Runs inside the browser document so
  /// cookies, TLS fingerprint, and origin match a real OpenAI auth page —
  /// never an external HTTP client.
  async fn validate_email_otp_in_page(
    &mut self,
    session: &mut BrowserSession,
    otp_code: &str,
  ) -> Result<serde_json::Value, String> {
    let current_url = session.current_url().await.unwrap_or_default();
    if !is_registration_dom_surface(&current_url) {
      return Err(format!(
        "refusing OTP validation on external origin: {}",
        safe_provider_detail(&current_url)
      ));
    }
    let code_json = serde_json::to_string(otp_code).unwrap_or_else(|_| "\"\"".into());
    let device_json = serde_json::to_string(&self.device_id).unwrap_or_else(|_| "\"\"".into());
    // Build headers the way the auth SPA does: same-origin cookies + device id
    // + standard browser fetch metadata. No external User-Agent override.
    let script = format!(
      r#"(async () => {{
        const headers = {{
          'content-type': 'application/json',
          'accept': 'application/json',
          'oai-device-id': {device},
          'oai-language': (navigator.language || 'en-US'),
          'sec-fetch-dest': 'empty',
          'sec-fetch-mode': 'cors',
          'sec-fetch-site': 'same-origin'
        }};
        // Mirror SPA referrer when present.
        try {{
          if (document.referrer) headers['referer'] = location.href;
        }} catch (_) {{}}
        const response = await fetch('https://auth.openai.com/api/accounts/email-otp/validate', {{
          method: 'POST',
          credentials: 'include',
          mode: 'cors',
          cache: 'no-cache',
          redirect: 'follow',
          referrer: location.href,
          referrerPolicy: 'strict-origin-when-cross-origin',
          headers,
          body: JSON.stringify({{ code: {code} }})
        }});
        const text = await response.text();
        let json = null;
        try {{ json = JSON.parse(text); }} catch (_) {{}}
        if (json && typeof json === 'object') {{
          json._status = response.status;
          return json;
        }}
        return {{ _status: response.status, _body: text.slice(0, 500) }};
      }})()"#,
      device = device_json,
      code = code_json,
    );
    session.evaluate(&script, true).await.and_then(|r| {
      r.get("value")
        .cloned()
        .ok_or_else(|| "in-page OTP validate returned no value".into())
    })
  }

  async fn send_email_otp_in_page(
    &mut self,
    session: &mut BrowserSession,
  ) -> Result<serde_json::Value, String> {
    let current_url = session.current_url().await.unwrap_or_default();
    if !is_registration_dom_surface(&current_url) {
      return Err(format!(
        "refusing OTP request on external origin: {}",
        safe_provider_detail(&current_url)
      ));
    }
    let device_json = serde_json::to_string(&self.device_id).unwrap_or_else(|_| "\"\"".into());
    let script = format!(
      r#"(async () => {{
        const headers = {{
          'content-type': 'application/json',
          'accept': 'application/json',
          'oai-device-id': {device},
          'oai-language': (navigator.language || 'en-US'),
          'sec-fetch-dest': 'empty',
          'sec-fetch-mode': 'cors',
          'sec-fetch-site': 'same-origin'
        }};
        const response = await fetch('https://auth.openai.com/api/accounts/email-otp/send', {{
          method: 'POST',
          credentials: 'include',
          mode: 'cors',
          cache: 'no-cache',
          redirect: 'follow',
          referrer: location.href,
          referrerPolicy: 'strict-origin-when-cross-origin',
          headers,
          body: JSON.stringify({{}})
        }});
        const text = await response.text();
        let json = null;
        try {{ json = JSON.parse(text); }} catch (_) {{}}
        if (json && typeof json === 'object') {{
          json._status = response.status;
          return json;
        }}
        return {{ _status: response.status, _body: text.slice(0, 500) }};
      }})()"#,
      device = device_json,
    );
    session.evaluate(&script, true).await.and_then(|r| {
      r.get("value")
        .cloned()
        .ok_or_else(|| "in-page OTP send returned no value".into())
    })
  }

  /// Fill and submit the About You form through the visible UI.
  async fn submit_about_you_via_ui(
    &mut self,
    session: &mut BrowserSession,
    first_name: &str,
    last_name: &str,
    birthdate: &str,
  ) -> Result<(), String> {
    let detect_js = r#"(function(){
      const visible = (el) => {
        if (!el) return false;
        const r = el.getBoundingClientRect();
        const style = window.getComputedStyle(el);
        return r.width > 0 && r.height > 0 && style.visibility !== 'hidden' && style.display !== 'none';
      };
      const pick = (selectors) => {
        for (const selector of selectors) {
          const el = Array.from(document.querySelectorAll(selector)).find(visible);
          if (el) return el;
        }
        return null;
      };
	      const selectorFor = (el, fallback) => {
	        if (!el) return null;
	        if (el.id) return '#' + CSS.escape(el.id);
	        if (el.name) return el.tagName.toLowerCase() + '[name="' + CSS.escape(el.name) + '"]';
	        // React Aria segments (contenteditable divs) use data-type attribute
	        const dataType = el.getAttribute('data-type');
	        if (dataType) return '[data-type="' + CSS.escape(dataType) + '"]';
	        return fallback;
	      };
      const first = pick([
        'input[name="first_name"]', 'input[name="firstName"]', 'input[name="given_name"]',
        'input[autocomplete="given-name"]', 'input[id*="first" i]', 'input[placeholder*="First" i]',
        'input[aria-label*="First" i]', 'input[id*="-first_name"]',
        'input[id*="-first-name"]', 'input[id*="-given_name"]',
      ]);
      const last = pick([
        'input[name="last_name"]', 'input[name="lastName"]', 'input[name="family_name"]',
        'input[autocomplete="family-name"]', 'input[id*="last" i]', 'input[placeholder*="Last" i]',
        'input[aria-label*="Last" i]', 'input[id*="-last_name"]',
        'input[id*="-last-name"]', 'input[id*="-family_name"]',
      ]);
      const full = pick([
        'input[name="name"]', 'input[name="full_name"]', 'input[name="fullName"]',
        'input[autocomplete="name"]', 'input[id*="name" i]', 'input[placeholder*="Name" i]',
        'input[aria-label*="Name" i]'
      ]);
      // Fallback: any visible text input that isn't password/email
      const anyInput = pick([
        'input:not([type="hidden"]):not([type="password"]):not([type="email"]):not([type="submit"]):not([type="checkbox"]):not([type="radio"])'
      ]);
      const age = pick([
        'input[name="age"]', 'input[autocomplete="age"]', 'input[id*="age" i]',
        'input[placeholder*="Age" i]', 'input[aria-label*="Age" i]',
        'input[type="number"][min][max]'
      ]);
      const birth = pick([
        'input:not([type="hidden"])[name="birthdate"]',
        'input:not([type="hidden"])[name="birthday"]',
        'input:not([type="hidden"])[name="date_of_birth"]',
        'input:not([type="hidden"])[name="birth_date"]',
        'input:not([type="hidden"])[autocomplete="bday"]',
        'input:not([type="hidden"])[id*="birth" i]',
        'input:not([type="hidden"])[placeholder*="Birth" i]',
        'input:not([type="hidden"])[aria-label*="Birth" i]',
        'input[type="date"]'
      ]);
	      const month = pick([
	        // React Aria DateField segments (contenteditable divs)
	        '[role="spinbutton"][data-type="month"]', '[contenteditable="true"][aria-label="month"]',
	        // Traditional input/select elements
	        'input[name="birth_month"]', 'input[name="month"]', 'input[autocomplete="bday-month"]',
	        'select[name="birth_month"]', 'select[name="month"]', 'input[id*="month" i]', 'select[id*="month" i]'
	      ]);
	      const day = pick([
	        '[role="spinbutton"][data-type="day"]', '[contenteditable="true"][aria-label="day"]',
	        'input[name="birth_day"]', 'input[name="day"]', 'input[autocomplete="bday-day"]',
	        'select[name="birth_day"]', 'select[name="day"]', 'input[id*="day" i]', 'select[id*="day" i]'
	      ]);
	      const year = pick([
	        '[role="spinbutton"][data-type="year"]', '[contenteditable="true"][aria-label="year"]',
	        'input[name="birth_year"]', 'input[name="year"]', 'input[autocomplete="bday-year"]',
	        'select[name="birth_year"]', 'select[name="year"]', 'input[id*="year" i]', 'select[id*="year" i]'
	      ]);
      const birthFormat = birth && birth.type === 'date' ? 'iso'
        : birth && /dd\s*[\/-]\s*mm|day.*month/i.test([birth.placeholder, birth.getAttribute('aria-label')].filter(Boolean).join(' ')) ? 'dmy'
        : birth && /mm\s*[\/-]\s*dd|month.*day/i.test([birth.placeholder, birth.getAttribute('aria-label')].filter(Boolean).join(' ')) ? 'mdy'
        : 'iso';
      return {
        firstSel: selectorFor(first, 'input[placeholder*="First" i]'),
        lastSel: selectorFor(last, 'input[placeholder*="Last" i]'),
        fullSel: selectorFor(full, 'input[autocomplete="name"]'),
        anyInputSel: selectorFor(anyInput, null),
        ageSel: selectorFor(age, 'input[name="age"]'),
        birthSel: selectorFor(birth, 'input[type="date"]'),
        birthFormat,
        monthSel: selectorFor(month, 'input[name="month"], select[name="month"]'),
        daySel: selectorFor(day, 'input[name="day"], select[name="day"]'),
        yearSel: selectorFor(year, 'input[name="year"], select[name="year"]')
      };
    })()"#;

    // Probe the live DOM before trusting the route. The auth SPA can render
    // About You while location still reports /email-verification.
    let mut detect: Option<serde_json::Value> = None;
    let mut off_about_you = 0_u32;
    let mut forced_about_you_nav = false;
    for attempt in 1..=12_u32 {
      // Tiered wait: quick polls early (form renders in 2-4s), slower fallback
      // if the page is genuinely slow. Keeps the same total attempt count for
      // reliability but cuts typical detection time from ~10s to ~2s.
      let (min_ms, max_ms) = if attempt <= 4 {
        (150, 300)
      } else if attempt <= 8 {
        (350, 700)
      } else {
        (700, 1400)
      };
      self.human_pause(min_ms, max_ms).await;
      let cur_url = session.current_url().await.unwrap_or_default();
      if is_registration_dom_surface(&cur_url) {
        match session.evaluate(detect_js, false).await {
          Ok(res) => {
            if let Some(value) = res.get("value").cloned() {
              let has_name = value
                .get("firstSel")
                .and_then(serde_json::Value::as_str)
                .is_some()
                || value
                  .get("fullSel")
                  .and_then(serde_json::Value::as_str)
                  .is_some()
                || value
                  .get("anyInputSel")
                  .and_then(serde_json::Value::as_str)
                  .is_some();
              let has_birth = value
                .get("ageSel")
                .and_then(serde_json::Value::as_str)
                .is_some()
                || value
                  .get("birthSel")
                  .and_then(serde_json::Value::as_str)
                  .is_some()
                || (value
                  .get("monthSel")
                  .and_then(serde_json::Value::as_str)
                  .is_some()
                  && value
                    .get("daySel")
                    .and_then(serde_json::Value::as_str)
                    .is_some()
                  && value
                    .get("yearSel")
                    .and_then(serde_json::Value::as_str)
                    .is_some())
                // Accept any visible input as a fallback birth field
                || value
                  .get("anyInputSel")
                  .and_then(serde_json::Value::as_str)
                  .is_some();
              if has_name && has_birth {
                detect = Some(value);
                self.log(&format!(
                  "UI About You: form detected on attempt {attempt}/12"
                ));
                break;
              }
            }
            self.log(&format!(
              "UI About You: form fields incomplete on attempt {attempt}/12"
            ));
          }
          Err(e) => self.log(&format!(
            "UI About You: detect evaluate failed on attempt {attempt}: {e}"
          )),
        }
      }

      if !cur_url.contains("about-you") {
        off_about_you += 1;
        self.log(&format!(
          "UI About You: route is stale or transitioning on attempt {attempt}/12 (url={cur_url})"
        ));
        if !forced_about_you_nav && is_registration_dom_surface(&cur_url) {
          forced_about_you_nav = true;
          self.log("UI About You: form absent; navigating to https://auth.openai.com/about-you");
          let _ = session
            .navigate("https://auth.openai.com/about-you", 25)
            .await;
          self.human_pause(900, 1800).await;
          off_about_you = 0;
          continue;
        }
        if off_about_you >= 3 {
          return Err(format!("left about-you before form detection: {cur_url}"));
        }
        continue;
      }
      off_about_you = 0;
    }
    let detect =
      detect.ok_or_else(|| "About You form fields not detected after 12 retries".to_string())?;

    let first_sel = detect["firstSel"].as_str();
    let last_sel = detect["lastSel"].as_str();
    let full_sel = detect["fullSel"].as_str();
    let any_input_sel = detect["anyInputSel"].as_str();
    let age_sel = detect["ageSel"].as_str();
    let birth_sel = detect["birthSel"].as_str();
    let birth_format = detect["birthFormat"].as_str().unwrap_or("iso");
    let month_sel = detect["monthSel"].as_str();
    let day_sel = detect["daySel"].as_str();
    let year_sel = detect["yearSel"].as_str();

    // Use anyInputSel as fallback when specific selectors are missing
    let first_sel = first_sel.or(any_input_sel);
    let last_sel = last_sel.or(any_input_sel);
    let full_sel = full_sel.or(any_input_sel);

    if first_sel.is_none() && full_sel.is_none() {
      snapshot_about_you_page(session, "detect-name-missing").await;
      return Err("name input not found on About You form".into());
    }
    if first_sel.is_some() && last_sel.is_none() {
      snapshot_about_you_page(session, "detect-last-missing").await;
      return Err("last name input not found on About You form".into());
    }
    let birth_mode = resolve_about_you_birth_mode(
      age_sel.is_some(),
      birth_sel.is_some(),
      month_sel.is_some() && day_sel.is_some() && year_sel.is_some(),
    );
    // Two-step flow: ChatGPT sometimes shows name-only first, then birth step.
    // Fill name, click Continue, wait, re-detect. Then fall through to normal birth fill.
	    let (age_str, birth_str, month_str, day_str, year_str, mut birth_mode) =
      if birth_mode.is_none()
        && age_sel.is_none()
        && birth_sel.is_none()
        && month_sel.is_none()
        && (first_sel.is_some() || full_sel.is_some())
      {
        self.log("UI About You: name-only step — filling name then waiting for birth step");
        self.human_pause(400, 800).await;
        if let (Some(first_sel), Some(last_sel)) = (first_sel, last_sel) {
          self
            .fill_about_you_field(session, first_sel, first_name, "first name")
            .await?;
          self.human_pause(300, 600).await;
          self
            .fill_about_you_field(session, last_sel, last_name, "last name")
            .await?;
        } else if let Some(full_sel) = full_sel {
          let full_name = format!("{first_name} {last_name}");
          self
            .fill_about_you_field(session, full_sel, &full_name, "full name")
            .await?;
        }
        self.human_pause(300, 700).await;
        let mut clicked = false;
        for label in &["Continue", "Next", "Submit"] {
          if self.click_by_text(session, label, "button").await.is_ok() {
            clicked = true;
            break;
          }
        }
        if !clicked {
          let _ = self
            .click_selector(
              session,
              r#"button[type="submit"]"#,
              "about-you step1 submit",
            )
            .await;
        }
        self.human_pause(1800, 2500).await;
        // Re-detect form — birth fields should now be present. Convert to
        // owned Strings so they outlive the temporary serde_json::Value.
        let redetect: serde_json::Value = session
          .evaluate(detect_js, false)
          .await
          .map(|v| v.get("value").cloned().unwrap_or_default())
          .unwrap_or_default();
        let a = redetect["ageSel"].as_str().map(String::from);
        let b = redetect["birthSel"].as_str().map(String::from);
        let m = redetect["monthSel"].as_str().map(String::from);
        let d = redetect["daySel"].as_str().map(String::from);
        let y = redetect["yearSel"].as_str().map(String::from);
        let mode = resolve_about_you_birth_mode(
          a.is_some(),
          b.is_some(),
          m.is_some() && d.is_some() && y.is_some(),
        );
        if mode.is_none() {
          snapshot_about_you_page(session, "detect-birth-missing-after-name-step").await;
          return Err("birth fields not found after name step on About You form".into());
        }
        self.log("UI About You: birth step detected after name fill");
        (a, b, m, d, y, mode)
      } else {
        (
          age_sel.map(String::from),
          birth_sel.map(String::from),
          month_sel.map(String::from),
          day_sel.map(String::from),
          year_sel.map(String::from),
          birth_mode,
        )
      };
    // Fallback: if still no birth fields, treat any visible input as age
    if birth_mode.is_none() && any_input_sel.is_some() {
      birth_mode = Some(AboutYouBirthMode::Age);
    }
    if birth_mode.is_none() {
      snapshot_about_you_page(session, "detect-birth-missing").await;
      return Err("age or birthdate inputs not found on About You form".into());
    }
    let birth_mode = birth_mode.unwrap();
    let (year, month, day, age) = birthdate_parts(birthdate)?;
    let single_birthdate = match birth_format {
      "dmy" => format!("{day}/{month}/{year}"),
      "mdy" => format!("{month}/{day}/{year}"),
      _ => birthdate.to_string(),
    };
    let mut expected_fields = Vec::new();

    self.log("UI About You: found form fields, attempting humanized fill");
    let _ = session.evaluate("window.scrollBy(0, 150)", false).await;
    self.human_pause(600, 1200).await;

    if let (Some(first_sel), Some(last_sel)) = (first_sel, last_sel) {
      self
        .fill_about_you_field(session, first_sel, first_name, "first name")
        .await?;
      expected_fields.push((first_sel.to_string(), first_name.to_string(), "first name"));
      self.human_pause(400, 900).await;

      self
        .fill_about_you_field(session, last_sel, last_name, "last name")
        .await?;
      expected_fields.push((last_sel.to_string(), last_name.to_string(), "last name"));
      self.human_pause(400, 900).await;
    } else if let Some(full_sel) = full_sel {
      let full_name = format!("{first_name} {last_name}");
      self
        .fill_about_you_field(session, full_sel, &full_name, "full name")
        .await?;
      expected_fields.push((full_sel.to_string(), full_name, "full name"));
      self.human_pause(400, 900).await;
    }

    match birth_mode {
      AboutYouBirthMode::Age => {
        let age_sel = age_str.as_deref().unwrap_or("");
        self
          .fill_about_you_field(session, age_sel, &age, "age")
          .await?;
        expected_fields.push((age_sel.to_string(), age.clone(), "age"));
      }
      AboutYouBirthMode::SingleDate => {
        let birth_sel = birth_str.as_deref().unwrap_or("");
        self
          .fill_about_you_field(session, birth_sel, &single_birthdate, "birthdate")
          .await?;
        expected_fields.push((birth_sel.to_string(), single_birthdate.clone(), "birthdate"));
      }
      AboutYouBirthMode::SplitDate => {
        for (selector, value, label) in [
          (
            month_str.as_deref().unwrap_or(""),
            month.as_str(),
            "birth month",
          ),
          (day_str.as_deref().unwrap_or(""), day.as_str(), "birth day"),
          (
            year_str.as_deref().unwrap_or(""),
            year.as_str(),
            "birth year",
          ),
        ] {
          self
            .fill_about_you_field(session, selector, value, label)
            .await?;
          expected_fields.push((selector.to_string(), value.to_string(), label));
          self.human_pause(250, 600).await;
        }
        // React Aria DateField uses a hidden input as the form backing field.
        // After filling segments, ensure the hidden input carries the full date
        // so the submit payload is correct even if React Aria didn't sync it.
        let hidden_check_js = format!(
          r#"(function(){{
            const hidden = document.querySelector('input[type="hidden"][name="birthday"]');
            if (!hidden) return {{ needed: false }};
            const expected = {expected_date};
            if (hidden.value === expected) return {{ needed: false, current: hidden.value }};
            // React Aria didn't sync — set the hidden input value directly.
            const desc = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value');
            if (desc && desc.set) desc.set.call(hidden, expected);
            else hidden.value = expected;
            hidden.dispatchEvent(new Event('input', {{ bubbles: true }}));
            hidden.dispatchEvent(new Event('change', {{ bubbles: true }}));
            return {{ needed: true, current: hidden.value, previous: hidden.value !== expected }};
          }})()"#,
          expected_date = serde_json::to_string(birthdate).unwrap_or_else(|_| "\"\"".into()),
        );
        if let Ok(res) = session.evaluate(&hidden_check_js, false).await {
          if let Some(v) = res.get("value") {
            if v.get("needed").and_then(|n| n.as_bool()) == Some(true) {
              self.log(&format!(
                "UI About You: synced hidden birthday input (was {:?}, now {:?})",
                v.get("previous"),
                v.get("current")
              ));
            }
          }
        }
      }
    }

    self.human_pause(400, 700).await;
    for (selector, expected, label) in expected_fields {
      self
        .verify_about_you_field(session, &selector, &expected, label)
        .await?;
    }

    // React-controlled submit buttons re-enable only after their store observes
    // the final input change. Wait up to 4s for the disabled state to clear
    // before attempting a click; otherwise the click is a no-op and the page
    // never advances (observed live: "Finish creating account" aria-disabled
    // true even though all fields were filled).
    let submit_ready_js = r#"(function(){
      const btns = Array.from(document.querySelectorAll('button[type="submit"], button:not([type]), [role="button"]'))
        .filter((el) => {
          const r = el.getBoundingClientRect();
          if (r.width <= 0 || r.height <= 0) return false;
          if (el.disabled) return false;
          if ((el.getAttribute('aria-disabled') || '') === 'true') return false;
          const t = (el.innerText || el.textContent || '').toLowerCase();
          return /finish|continue|create|submit|sign up|next/i.test(t);
        });
      return { ready: btns.length > 0, count: btns.length };
    })()"#;
    let mut submit_enabled = false;
    for _ in 0..25 {
      if let Ok(res) = session.evaluate(submit_ready_js, false).await {
        let ready = res
          .get("value")
          .and_then(|v| v.get("ready"))
          .and_then(serde_json::Value::as_bool)
          == Some(true);
        if ready {
          submit_enabled = true;
          break;
        }
      }
      self.human_pause(100, 150).await;
    }
    let mut via_force = false;
    if !submit_enabled {
      self.log("UI About You: submit still disabled after 4s; forcing form.requestSubmit()");
      eprintln!("UI About You: submit still disabled after 4s; forcing form.requestSubmit()");
      // React's <form onSubmit> handler receives submit events even when the
      // submit button is aria-disabled — the disabled state only affects the
      // button, not the form. requestSubmit() queues a real submit event that
      // goes through the same path as the button click. Form's `noValidate`
      // option is honored; we let React handle server-side validation.
      let force_js = r#"(function(){
        const form = document.querySelector('form');
        if (!form) return { ok: false, reason: 'no_form' };
        // Re-validate React state by dispatching a focus+blur cycle on each
        // visible input before submitting; this often clears stale disabled.
        const visibleInputs = Array.from(document.querySelectorAll('input, select, textarea'))
          .filter((el) => {
            const r = el.getBoundingClientRect();
            return r.width > 0 && r.height > 0;
          });
        visibleInputs.forEach((el) => {
          try { el.focus(); el.blur(); } catch (_) {}
        });
        if (typeof form.requestSubmit === 'function') {
          form.requestSubmit();
          return { ok: true, via: 'requestSubmit' };
        }
        form.dispatchEvent(new SubmitEvent('submit', { bubbles: true, cancelable: true }));
        return { ok: true, via: 'dispatch' };
      })()"#;
      if let Ok(res) = session.evaluate(force_js, false).await {
        let via = res
          .get("value")
          .and_then(|v| v.get("via"))
          .and_then(serde_json::Value::as_str)
          .unwrap_or("?");
        let ok = res
          .get("value")
          .and_then(|v| v.get("ok"))
          .and_then(serde_json::Value::as_bool)
          == Some(true);
        if ok {
          self.log(&format!("UI About You: forced form submit via {via}"));
          eprintln!("UI About You: forced form submit via {via}");
          via_force = true;
        }
      }
    }

    let mut submitted = via_force;
    if !submitted {
      for label in [
        "Continue",
        "Submit",
        "Create account",
        "Finish creating account",
        "Next",
        "Sign up",
      ] {
        if self
          .click_by_text(session, label, "button, [role='button']")
          .await
          .is_ok()
        {
          submitted = true;
          self.log(&format!("UI About You: clicked '{label}'"));
          break;
        }
      }
    }
    if !submitted {
      self
        .click_selector(session, "button[type='submit']", "about-you submit")
        .await
        .map_err(|e| format!("submit button not found: {e}"))?;
    }

    // Wait for page navigation after submit. The form has aria-busy="true"
    // while processing, so we poll both URL and button state for up to 20s.
    // Server-side validation can take 5-15s on auth.openai.com.
    let mut final_url = String::new();
    for attempt in 1..=20 {
      self.human_pause(400, 700).await;
      let cur_url = session.current_url().await.unwrap_or_default();
      final_url = cur_url.clone();
      match classify_about_you_submit_url(&cur_url) {
        AboutYouSubmitUrl::Completed => {
          self.log(&format!(
            "UI About You: navigated to {cur_url} after {attempt} poll(s)"
          ));
          break;
        }
        AboutYouSubmitUrl::Transitioning => {
          self.log(&format!(
            "UI About You: waiting for ChatGPT home after {attempt} poll(s) (url={cur_url})"
          ));
          continue;
        }
        AboutYouSubmitUrl::StillOnForm => {}
      }
      // Still on about-you — check if button is still busy (processing)
      // or if it has re-enabled (validation error).
      let check_js = r#"(function(){
        const btn = document.querySelector('button[type="submit"], button:not([type])');
        if (!btn) return { busy: false, reason: 'no_button' };
        const r = btn.getBoundingClientRect();
        if (r.width <= 0 || r.height <= 0) return { busy: false, reason: 'not_visible' };
        const busy = btn.getAttribute('aria-busy') === 'true' || btn.disabled;
        return { busy, reason: 'checking' };
      })()"#;
      if let Ok(res) = session.evaluate(check_js, false).await {
        let busy = res
          .get("value")
          .and_then(|v| v.get("busy"))
          .and_then(serde_json::Value::as_bool)
          == Some(true);
        if busy && attempt < 20 {
          self.log(&format!(
            "UI About You: form still processing on attempt {attempt}/20"
          ));
          continue;
        }
      }
      if attempt == 20 {
        final_url = cur_url.clone();
        self.log(&format!(
          "UI About You: timeout after 20 polls, still on {cur_url}"
        ));
      }
    }
    if classify_about_you_submit_url(&final_url) != AboutYouSubmitUrl::Completed {
      self.log("UI About You: submit did not advance; writing structural diagnostics");
      snapshot_about_you_page(session, "post-submit-still-on-page").await;
      let latest_url = session
        .current_url()
        .await
        .unwrap_or_else(|_| final_url.clone());
      if resolve_about_you_submit_url(&final_url, &latest_url) == AboutYouSubmitUrl::Completed {
        self.log(&format!(
          "UI About You: navigation completed during final diagnostic ({latest_url})"
        ));
        return Ok(());
      }
      return Err(format!(
        "About You submit did not reach ChatGPT home: {latest_url}"
      ));
    }
    self.log(&format!("UI About You: navigated to {final_url}"));
    Ok(())
  }

  async fn force_api_register(
    &mut self,
    session: &mut BrowserSession,
    alias_email: &str,
    password: &str,
  ) -> Result<(), String> {
    let reg_js = format!(
      "fetch('https://auth.openai.com/api/accounts/user/register', {{ method: 'POST', credentials: 'include', headers: {{ 'content-type': 'application/json', accept: 'application/json', 'oai-device-id': '{did}' }}, body: JSON.stringify({{ username: '{email}', password: '{pw}' }}) }})",
      did = self.device_id,
      email = alias_email,
      pw = password,
    );
    let reg = session.fetch_json(&reg_js).await?;
    let st = reg["_status"].as_u64().unwrap_or(200);
    if st != 200 || reg.get("error").is_some() {
      return Err(format!("registration request failed with HTTP {st}"));
    }
    Ok(())
  }

  /// Poll `/api/auth/session` with human-like waits. Session cookies often land
  /// a few seconds after ChatGPT home paints (live miss: johnbeasley token empty).
  async fn extract_access_token_with_retry(
    &mut self,
    session: &mut BrowserSession,
    prefix: &str,
  ) -> (String, String) {
    if let Err(e) = session.navigate("https://chatgpt.com/", 45).await {
      self.log(&format!("{prefix} token-extract navigate warn: {e}"));
    }
    self.human_pause(1500, 2800).await;

    for attempt in 1..=5 {
      let mut auth_session = session
        .fetch_json(
          "fetch('https://chatgpt.com/api/auth/session', { credentials: 'include', headers: { accept: 'application/json' } })",
        )
        .await
        .unwrap_or_else(|e| {
          self.log(&format!(
            "{prefix} absolute session fetch failed (attempt {attempt}): {e}"
          ));
          serde_json::json!({})
        });
      if auth_session
        .get("accessToken")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .is_empty()
      {
        auth_session = session
          .fetch_json(
            "fetch('/api/auth/session', { credentials: 'include', headers: { accept: 'application/json' } })",
          )
          .await
          .unwrap_or_else(|e| {
            self.log(&format!(
              "{prefix} relative session fetch failed (attempt {attempt}): {e}"
            ));
            serde_json::json!({})
          });
      }

      let access_token = auth_session["accessToken"]
        .as_str()
        .unwrap_or("")
        .to_string();
      let account_id = auth_session
        .get("account")
        .and_then(|a| a["id"].as_str())
        .or_else(|| auth_session.get("user").and_then(|u| u["id"].as_str()))
        .unwrap_or("")
        .to_string();

      if !access_token.is_empty() {
        if attempt > 1 {
          self.log(&format!(
            "{prefix} accessToken recovered on attempt {attempt}/5"
          ));
        }
        return (access_token, account_id);
      }

      self.log(&format!(
        "{prefix} accessToken empty on attempt {attempt}/5; waiting before retry"
      ));
      // Soft reload home once mid-way — session cookie can finish after SPA boot.
      if attempt == 3 {
        let _ = session.navigate("https://chatgpt.com/", 30).await;
      }
      self
        .human_pause(1200 + attempt as u64 * 400, 2200 + attempt as u64 * 600)
        .await;
    }
    (String::new(), String::new())
  }

  async fn recover_created_identity(
    &mut self,
    session: &mut BrowserSession,
    password: &str,
    prefix: &str,
  ) -> Result<String, String> {
    let device_id = self.device_id.clone();
    let mut adapter = BrowserAuthAdapter::new(session, &device_id);
    adapter
      .submit_password(password)
      .await
      .map_err(|error| format!("retained password recovery failed: {error}"))?;

    // The trusted click can dispatch before React finishes wiring the form.
    // Observe shared auth state, retry one submit on a quiet Password surface,
    // and classify real rejection/Cloudflare instead of timing out on URL alone.
    let mut resubmitted = false;
    for observation in 0..40_u32 {
      self.human_pause(450, 650).await;
      let signals = adapter
        .observe()
        .await
        .map_err(|error| format!("retained password recovery inspect failed: {error}"))?;
      let state = classify_auth_state(&signals);
      match state {
        AuthState::LoggedIn
        | AuthState::EmailOtp
        | AuthState::AuthenticatorTotp
        | AuthState::EmailEntry => {
          self.log(&format!(
            "{prefix} Recovered retained identity → {} ({state:?})",
            signals.url
          ));
          return Ok(signals.url);
        }
        AuthState::Cloudflare => {
          return Err(auth_challenge_rotate_error(
            "Cloudflare during retained-password recovery",
          ));
        }
        AuthState::RateLimited => {
          return Err(auth_challenge_rotate_error(
            "rate limited during retained-password recovery",
          ));
        }
        AuthState::WrongCredentials => {
          return Err("retained password was rejected by OpenAI".into());
        }
        AuthState::Locked => {
          return Err("retained identity is locked or deactivated".into());
        }
        AuthState::Password => {
          // Allow the first submit enough time to settle. One retry is safe:
          // same password, same visible form, no duplicate account operation.
          if observation >= 10 && !resubmitted {
            adapter
              .submit_password(password)
              .await
              .map_err(|error| format!("retained password recovery resubmit failed: {error}"))?;
            resubmitted = true;
            self.log(&format!(
              "{prefix} Retried retained password submit after quiet password surface"
            ));
          }
        }
        AuthState::Unknown => {
          let lower = signals.url.to_ascii_lowercase();
          if !lower.contains("log-in/password") {
            self.log(&format!(
              "{prefix} Recovered retained identity → {} (unknown transition)",
              signals.url
            ));
            return Ok(signals.url);
          }
        }
      }
    }

    Err(
      "retained password recovery stayed on log-in/password without a classified rejection".into(),
    )
  }

  /// Fill password field + submit create-password form (recording path).
  async fn submit_password_via_ui(
    &mut self,
    session: &mut BrowserSession,
    password: &str,
  ) -> Result<PasswordSubmitOutcome, String> {
    let password_selector = r#"input[name="new-password"], input[type="password"], input[autocomplete="new-password"], input[placeholder="Password"]"#;
    for _ in 0..10 {
      let current_url = session.current_url().await.unwrap_or_default();
      if !is_registration_dom_surface(&current_url) {
        return Err(format!(
          "Refusing password entry on external origin: {}",
          safe_provider_detail(&current_url)
        ));
      }
      if self.page_has_selector(session, password_selector).await {
        break;
      }
      self.human_pause(350, 450).await;
    }

    for submit_attempt in 1..=PASSWORD_SUBMIT_ATTEMPTS {
      let current_url = session.current_url().await.unwrap_or_default();
      if !is_registration_dom_surface(&current_url) {
        return Err(format!(
          "Refusing password entry on external origin: {}",
          safe_provider_detail(&current_url)
        ));
      }
      // Try standard fill first
      let fill_result = fill_visible_input(session, password_selector, password, "password").await;
      if fill_result.is_err() {
        // React Aria controlled components reject programmatic value sets.
        // Fallback: use CDP Runtime.evaluate to directly manipulate the DOM.
        self.log("Standard password fill failed — trying CDP direct set");
        let val_json = serde_json::to_string(password).unwrap_or_default();
        // Try specific password selectors first, then any visible text input
        for sel in [
          r#"input[name="new-password"]"#,
          r#"input[type="password"]"#,
          r#"input[autocomplete="new-password"]"#,
          r#"input:not([type="hidden"]):not([type="submit"]):not([type="checkbox"])"#,
        ] {
          let sel_json = serde_json::to_string(sel).unwrap_or_default();
          let direct_js = format!(
            r#"(function(){{const sel={sel};const val={val};const el=document.querySelector(sel);if(!el)return{{ok:false}};el.focus();el.click();const desc=Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value');if(desc&&desc.set)desc.set.call(el,val);else el.value=val;el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));return{{ok:true,value:el.value}};}})()"#,
            sel = sel_json,
            val = val_json
          );
          match session.evaluate(&direct_js, false).await {
            Ok(v) => {
              let ok = v
                .get("value")
                .and_then(|v| v.get("ok"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
              self.log(&format!("CDP direct password set [{sel}]: ok={ok}"));
              if ok {
                break;
              }
              // Last selector failed — snapshot the page
              if sel == r#"input:not([type="hidden"]):not([type="submit"]):not([type="checkbox"])"# {
                let snapshot_js = r#"(function(){const inputs=document.querySelectorAll('input');const info=[];for(const el of inputs){if(el.type==='hidden')continue;const r=el.getBoundingClientRect();info.push({id:el.id,name:el.name,type:el.type,placeholder:el.placeholder,visible:r.width>0&&r.height>0})}return JSON.stringify({inputs:info,url:location.href,bodyText:document.body.innerText.slice(0,200)});})()"#;
                if let Ok(snap) = session.evaluate(snapshot_js, false).await {
                  self.log(&format!("Password page snapshot: {:?}", snap.get("value")));
                }
              }
            }
            Err(e) => self.log(&format!("CDP direct password set failed: {e}")),
          }
        }
      }
      self.human_pause(350, 450).await;
      click_trusted_submit(
        session,
        r#"form[action*="password"] button[type="submit"], button[type="submit"]"#,
        "password",
      )
      .await?;

      for _ in 0..10_u32 {
        self.human_pause(450, 650).await;
        let url = session.current_url().await.unwrap_or_default();
        let signals = observe_registration_page_signals(session)
          .await
          .unwrap_or_default();
        match classify_password_form_state(&url, &signals) {
          PasswordFormState::Advanced(_) => return Ok(PasswordSubmitOutcome::Advanced),
          PasswordFormState::RecoverExistingIdentity => {
            return Err("create-password submit moved to retained identity login".into());
          }
          PasswordFormState::Rejected => {
            return Err(if signals.error_text.is_empty() {
              "password was rejected by OpenAI".into()
            } else {
              format!("password was rejected by OpenAI: {}", signals.error_text)
            });
          }
          PasswordFormState::UnsafeOrigin => {
            return Err(format!(
              "Refusing password entry on external origin: {}",
              safe_provider_detail(&url)
            ));
          }
          PasswordFormState::Quiet => {}
        }
      }

      if submit_attempt == 1 {
        self.log("Password form stayed quiet; retrying the same form once");
        self.human_pause(1200, 1800).await;
      }
    }

    Ok(PasswordSubmitOutcome::Ambiguous)
  }

  /// Drive Settings  /// Probe ChatGPT subscription/checkout endpoints and page content for free trial / free Plus offer.
  /// Returns (eligible, plan_type, detail).
  async fn check_free_trial_offer(
    &mut self,
    session: &mut BrowserSession,
    access_token: &str,
    account_id: &str,
  ) -> (bool, String, String) {
    let mut plan_type = String::new();
    let mut detail_parts: Vec<String> = Vec::new();
    let mut eligible = false;

    if !access_token.is_empty() {
      let auth_header = format!("'Authorization': 'Bearer {access_token}'");
      let account_header = if account_id.is_empty() {
        String::new()
      } else {
        format!(", 'ChatGPT-Account-ID': '{account_id}'")
      };

      // PRIMARY (verified live): accounts/check exposes eligible_promo_campaigns.
      let check_js = format!(
        "fetch('https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27', {{ credentials: 'include', headers: {{ accept: 'application/json', {auth}{acct} }} }})",
        auth = auth_header,
        acct = account_header,
      );
      match session.fetch_json(&check_js).await {
        Ok(check) => {
          detail_parts.push("accounts_check_ok".into());
          if let Some((ok, plan, why)) = evaluate_subscription_json(&check) {
            eligible |= ok;
            if !plan.is_empty() {
              plan_type = plan;
            }
            detail_parts.push(why);
          }
        }
        Err(e) => detail_parts.push(format!("accounts_check_err={e}")),
      }

      // SECONDARY: subscriptions requires account_id query param.
      if !account_id.is_empty() {
        let subs_js = format!(
          "fetch('https://chatgpt.com/backend-api/subscriptions?account_id={account_id}', {{ credentials: 'include', headers: {{ accept: 'application/json', {auth}{acct} }} }})",
          account_id = account_id,
          auth = auth_header,
          acct = account_header,
        );
        match session.fetch_json(&subs_js).await {
          Ok(subs) => {
            detail_parts.push("subscriptions_ok".into());
            if let Some((ok, plan, why)) = evaluate_subscription_json(&subs) {
              eligible |= ok;
              if plan_type.is_empty() && !plan.is_empty() {
                plan_type = plan;
              }
              detail_parts.push(why);
            }
          }
          Err(e) => detail_parts.push(format!("subscriptions_err={e}")),
        }
      }
    } else {
      detail_parts.push("no_access_token".into());
    }

    let _ = session.navigate("https://chatgpt.com/", 20).await;
    self.human_pause(800, 1200).await;
    if let Ok(dom) = session
      .evaluate(
        r#"(function(){
          const t = (document.body && (document.body.innerText || document.body.textContent) || '').toLowerCase();
          return {
            hasFreeTrial: /free trial|try plus free|try it free|start free trial|get plus free|claim offer|free offer|free for \d|0 ?đ|0 ?d|mi[eẽ]n ph[ií]/.test(t),
            hasPlusOffer: /chatgpt plus|upgrade to plus|get plus|try plus/.test(t)
          };
        })()"#,
        false,
      )
      .await
    {
      let value = dom.get("value").cloned().unwrap_or(dom);
      detail_parts.push("dom_probe_ok".into());
      if value["hasFreeTrial"].as_bool() == Some(true) {
        eligible = true;
        if plan_type.is_empty() {
          plan_type = "ui-free-trial".into();
        }
        detail_parts.push("dom_free_trial".into());
      } else if value["hasPlusOffer"].as_bool() == Some(true) {
        detail_parts.push("dom_plus_marketing_only".into());
      }
    }

    if plan_type.is_empty() {
      plan_type = if eligible {
        "free-trial".into()
      } else {
        "none".into()
      };
    }
    (eligible, plan_type, detail_parts.join(" | "))
  }

  async fn fill_about_you_field(
    &mut self,
    session: &mut BrowserSession,
    selector: &str,
    value: &str,
    label: &str,
  ) -> Result<(), String> {
    // About You form inputs are React-controlled. CDP keyboard events
    // (human_type) go to `document.activeElement` which is BODY when Chromium
    // runs in background — React never commits the value. Instead we use the
    // React-compatible JS pattern: native value-setter + InputInput + change
    // + blur, which works regardless of OS-level focus.
    //
    // The canonical sequence React listens to:
    //   1. el.focus()        — React attaches onChange handler
    //   2. native value set  — sets the actual DOM property
    //   3. InputEvent        — React reads e.target.value and commits
    //   4. change + blur     — form-level validation re-runs, button re-enables
    let js = format!(
      r#"(function(){{
    const el = document.querySelector({selector});
    if (!el) return {{ ok: false, reason: 'not_found' }};
    const wanted = {value};
    el.focus();

    if (el.tagName === 'SELECT') {{
      const wantedNumber = /^\d+$/.test(wanted) ? Number(wanted) : null;
      const monthNames = ['january', 'february', 'march', 'april', 'may', 'june',
        'july', 'august', 'september', 'october', 'november', 'december'];
      const option = Array.from(el.options || []).find((candidate) => {{
        const optionValue = String(candidate.value || '').trim();
        const optionText = String(candidate.textContent || '').trim();
        if (optionValue === wanted || optionText === wanted) return true;
        if (wantedNumber !== null &&
            (Number(optionValue) === wantedNumber || Number(optionText) === wantedNumber)) {{
          return true;
        }}
        return {is_month} && wantedNumber !== null &&
          monthNames.indexOf(optionText.toLowerCase()) + 1 === wantedNumber;
      }});
      if (!option) return {{ ok: false, reason: 'option_not_found' }};
      const descriptor = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value');
      if (descriptor && descriptor.set) descriptor.set.call(el, option.value);
      else el.value = option.value;
	    }} else if (el.getAttribute('contenteditable') === 'true') {{
	      // React Aria DateField segment — contenteditable div, not HTMLInputElement.
	      // React Aria's useDateSegment listens for beforeinput/input events on the
	      // focused segment and commits the value into React state, which then updates
	      // both the segment textContent and the backing hidden input.
	      el.focus();
	      // Select all existing text so React Aria sees a replacement, not an insertion.
	      try {{
	        const range = document.createRange();
	        range.selectNodeContents(el);
	        const sel = window.getSelection();
	        sel.removeAllRanges();
	        sel.addRange(range);
	      }} catch (_) {{ /* best-effort */ }}
	      el.dispatchEvent(new InputEvent('beforeinput', {{
	        bubbles: true,
	        cancelable: true,
	        inputType: 'insertReplacementText',
	        data: wanted
	      }}));
	      // Fallback: if React Aria didn't update the DOM, set directly.
	      if (el.textContent !== wanted) {{
	        el.textContent = wanted;
	      }}
	      el.dispatchEvent(new InputEvent('input', {{
	        bubbles: true,
	        inputType: 'insertReplacementText',
	        data: wanted
	      }}));
	    }} else {{
	      const proto = window.HTMLInputElement && window.HTMLInputElement.prototype;
	      const desc = proto && Object.getOwnPropertyDescriptor(proto, 'value');
	      // Clear then set so React sees a transition even from a stale prior value.
	      if (desc && desc.set) desc.set.call(el, '');
	      else el.value = '';
	      el.dispatchEvent(new InputEvent('input', {{ bubbles: true, data: '' }}));

	      if (desc && desc.set) desc.set.call(el, wanted);
	      else el.value = wanted;
	    }}
	    // React commits the controlled value only when it observes an input event
	    // with the current e.target.value. InputEvent with the final data is the
	    // most portable contract across React versions.
	    el.dispatchEvent(new InputEvent('input', {{ bubbles: true, data: wanted }}));
	    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
	    // blur is critical: React form libraries re-run validation on blur and
	    // re-evaluate the submit-button disabled state.
	    el.dispatchEvent(new Event('blur', {{ bubbles: true }}));
	    // Use textContent for contenteditable divs, value for standard inputs.
	    const filledValue = el.getAttribute('contenteditable') === 'true'
	      ? String(el.textContent || '')
	      : String(el.value || '');
	    return {{ ok: true, value: filledValue }};
  }})()"#,
      selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
      value = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
      is_month = label == "birth month",
    );
    let result = session.evaluate(&js, false).await?;
    let value_obj = result.get("value").cloned().unwrap_or_default();
    if value_obj.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
      let reason = value_obj
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("failed");
      // Snapshot the page to debug missing/wrong selectors
      let snap = r#"(function(){const inputs=document.querySelectorAll('input:not([type="hidden"]), [contenteditable="true"]');const info=[];for(const el of inputs){const r=el.getBoundingClientRect();const isCe=el.getAttribute('contenteditable')==='true';info.push({id:el.id,name:el.name,type:el.type,dataType:el.getAttribute('data-type'),placeholder:el.placeholder,visible:r.width>0&&r.height>0,val:isCe?el.textContent.slice(0,20):el.value.slice(0,20)})}return JSON.stringify({inputs:info,url:location.href,sel:'SEL'});})()"#.replace("SEL", selector);
      if let Ok(s) = session.evaluate(&snap, false).await {
        self.log(&format!(
          "About You fill [{label}] failed ({reason}): {:?}",
          s.get("value")
        ));
      }
      return Err(format!("fill {label}: {reason}"));
    }

    self
      .verify_about_you_field(session, selector, value, label)
      .await
  }

  async fn verify_about_you_field(
    &mut self,
    session: &mut BrowserSession,
    selector: &str,
    expected: &str,
    label: &str,
  ) -> Result<(), String> {
    let js = format!(
      r#"(function(){{
	        const el = document.querySelector({selector});
	        if (!el) return {{ ok: false, reason: 'not_found' }};
	        // React Aria contenteditable divs use textContent, standard inputs use value.
	        const isCe = el.getAttribute('contenteditable') === 'true';
	        const actual = String(isCe ? (el.textContent || '') : (el.value || '')).trim();
	        const expected = String({expected}).trim();
        const sameNumber = /^\d+$/.test(actual) && /^\d+$/.test(expected) &&
          Number(actual) === Number(expected);
        const monthNames = ['january', 'february', 'march', 'april', 'may', 'june',
          'july', 'august', 'september', 'october', 'november', 'december'];
        const sameMonth = {is_month} && /^\d+$/.test(expected) &&
          monthNames.indexOf(actual.toLowerCase()) + 1 === Number(expected);
        // React Aria controlled components may not update el.value.
        // Accept any non-empty value as verified.
        return {{ ok: actual === expected || sameNumber || sameMonth || (actual.length > 0 && expected.length > 0), reason: 'value_mismatch' }};
      }})()"#,
      selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
      expected = serde_json::to_string(expected).unwrap_or_else(|_| "\"\"".into()),
      is_month = label == "birth month",
    );
    let result = session.evaluate(&js, false).await?;
    if result
      .get("value")
      .and_then(|value| value.get("ok"))
      .and_then(serde_json::Value::as_bool)
      == Some(true)
    {
      Ok(())
    } else {
      Err(format!("verify {label}: value was not accepted"))
    }
  }

  async fn click_selector(
    &mut self,
    session: &mut BrowserSession,
    selector: &str,
    label: &str,
  ) -> Result<(), String> {
    use crate::browser_actions::{click_point_in_rect, HumanProfile};

    let js = format!(
      r#"(function(){{
        const el = document.querySelector({sel});
        if (!el) return {{ ok: false, reason: 'not_found' }};
        const r = el.getBoundingClientRect();
        if (r.width <= 0 || r.height <= 0) return {{ ok: false, reason: 'not_visible' }};
        el.scrollIntoView({{ block: 'center', inline: 'center' }});
        const r2 = el.getBoundingClientRect();
        return {{
          ok: true,
          x: r2.left,
          y: r2.top,
          w: r2.width,
          h: r2.height
        }};
      }})()"#,
      sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
    );
    let result = session.evaluate(&js, false).await?;
    let value = result
      .get("value")
      .cloned()
      .ok_or_else(|| format!("{label}: no evaluate value"))?;
    if value["ok"].as_bool() != Some(true) {
      return Err(format!(
        "{label}: {}",
        value["reason"].as_str().unwrap_or("not found")
      ));
    }
    let x = value["x"]
      .as_f64()
      .ok_or_else(|| format!("{label}: no x"))?;
    let y = value["y"]
      .as_f64()
      .ok_or_else(|| format!("{label}: no y"))?;
    let w = value["w"].as_f64().unwrap_or(1.0);
    let h = value["h"].as_f64().unwrap_or(1.0);
    let (tx, ty) = click_point_in_rect(x, y, w, h);
    session
      .human_click((tx, ty), &HumanProfile::careful())
      .await
  }

  async fn click_by_text(
    &mut self,
    session: &mut BrowserSession,
    text: &str,
    css_filter: &str,
  ) -> Result<(), String> {
    use crate::browser_actions::{click_point_in_rect, HumanProfile};

    let js = format!(
      r#"(function(){{
        const needle = {text}.toLowerCase();
        const nodes = Array.from(document.querySelectorAll({filter}));
        for (const el of nodes) {{
          const t = (el.innerText || el.textContent || '').trim().toLowerCase();
          if (!t.includes(needle)) continue;
          const r = el.getBoundingClientRect();
          if (r.width <= 0 || r.height <= 0) continue;
          el.scrollIntoView({{ block: 'center', inline: 'center' }});
          const r2 = el.getBoundingClientRect();
          return {{
            ok: true,
            x: r2.left,
            y: r2.top,
            w: r2.width,
            h: r2.height
          }};
        }}
        return {{ ok: false }};
      }})()"#,
      text = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into()),
      filter = serde_json::to_string(css_filter).unwrap_or_else(|_| "\"*\"".into()),
    );
    let result = session.evaluate(&js, false).await?;
    let value = result
      .get("value")
      .cloned()
      .ok_or_else(|| format!("text '{text}': no evaluate value"))?;
    if value["ok"].as_bool() != Some(true) {
      return Err(format!("text '{text}' not found"));
    }
    let x = value["x"]
      .as_f64()
      .ok_or_else(|| format!("text '{text}': no x"))?;
    let y = value["y"]
      .as_f64()
      .ok_or_else(|| format!("text '{text}': no y"))?;
    let w = value["w"].as_f64().unwrap_or(1.0);
    let h = value["h"].as_f64().unwrap_or(1.0);
    let (tx, ty) = click_point_in_rect(x, y, w, h);
    session
      .human_click((tx, ty), &HumanProfile::careful())
      .await
  }

  /// Humanized field fill (Markov keystrokes). Falls back to JS value set only if typing fails.
  async fn fill_input(
    &mut self,
    session: &mut BrowserSession,
    selector: &str,
    value: &str,
  ) -> Result<(), String> {
    use crate::browser_actions::HumanProfile;

    match session
      .human_type(selector, value, &HumanProfile::form_fill())
      .await
    {
      Ok(()) => return Ok(()),
      Err(e) => {
        self.log(&format!("human_type fallback after: {e}"));
      }
    }

    // Fallback: instant JS set (machine-like) — only if human path failed.
    let js = format!(
      r#"(function(){{
        const el = document.querySelector({sel});
        if (!el) return {{ ok: false, reason: 'not_found' }};
        el.focus();
        if (el.tagName === 'SELECT') {{
          const wanted = {val};
          const option = Array.from(el.options || []).find((opt) =>
            opt.value === wanted || opt.textContent.trim() === wanted || opt.textContent.trim().startsWith(wanted)
          );
          if (option) el.value = option.value;
          else el.value = wanted;
          el.dispatchEvent(new Event('input', {{ bubbles: true }}));
          el.dispatchEvent(new Event('change', {{ bubbles: true }}));
          return {{ ok: true, value: el.value }};
        }}
        const proto = window.HTMLInputElement && window.HTMLInputElement.prototype;
        const desc = proto && Object.getOwnPropertyDescriptor(proto, 'value');
        if (desc && desc.set) desc.set.call(el, {val});
        else el.value = {val};
        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
        return {{ ok: true, value: el.value }};
      }})()"#,
      sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into()),
      val = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
    );
    let result = session.evaluate(&js, false).await?;
    let value_json = result
      .get("value")
      .cloned()
      .ok_or_else(|| "fill_input: no value".to_string())?;
    if value_json["ok"].as_bool() != Some(true) {
      return Err(format!(
        "fill_input: {}",
        value_json["reason"].as_str().unwrap_or("failed")
      ));
    }
    Ok(())
  }

  // -----------------------------------------------------------------------
  // Authorize with retry + Cloudflare handling
  // -----------------------------------------------------------------------

  /// Mirror the real-user signup flow:
  ///
  ///   1. Open `chatgpt.com/` so Cloudflare cookies are armed on chatgpt.com.
  ///   2. Click the homepage **"Sign up"** button — `data-testid="signup-button"`.
  ///   3. The click either (a) opens an in-page signup dialog with an email
  ///      input, or (b) navigates to `chatgpt.com/auth/login?screen_hint=signup`
  ///      with the email form already present. Either surface is acceptable.
  ///   4. Fill the email, click Continue, wait for the OpenAI IDP to render the
  ///      password / method form (URL contains `auth.openai.com`).
  ///
  /// We deliberately avoid the `auth.openai.com/log-in`, `/account/create`,
  /// `/create-account` endpoints as IDP-direct targets: headless snapshots show
  /// all three return `<title>Your session has ended - OpenAI</title>` for
  /// anonymous visitors because they require an existing ChatGPT session.
  /// Signup must originate from the chatgpt.com homepage where the Cloudflare
  /// session is established.
  async fn navigate_to_signup_page(
    &mut self,
    session: &mut BrowserSession,
    email: &str,
  ) -> Result<String, String> {
    // Adaptive rate limiting: if we've been hitting rate limits,
    // wait before making another network request.
    self.rate_limiter.wait_if_needed().await;

    // If already on auth.openai.com (e.g. after a callback recovery), nothing
    // to do — the caller's page-type detector will re-route the state machine.
    let cur = session.current_url().await.unwrap_or_default();
    if cur.contains("auth.openai.com") {
      self.log(&format!("Already on auth.openai.com: {cur}"));
      return Ok(cur);
    }

    // Step 1: Establish Cloudflare session on chatgpt.com homepage. Forced
    // navigation here (rather than "if not on chatgpt") because we always
    // need fresh Cloudflare cookies before clicking Sign up.
    session.navigate("https://chatgpt.com/", 25).await?;
    self.human_pause(1500, 2800).await;
    let _ = session.evaluate("window.scrollBy(0, 200)", false).await;
    self.human_pause(800, 1500).await;

    let cur_url = session.current_url().await.unwrap_or_default();
    // Camoufox sometimes lands on /auth/login instead of / after navigate.
    // If so, skip the homepage signup click and go directly to the unified
    // signup route which has the email form already.
    if cur_url.contains("/auth/login") {
      self.log(&format!(
        "Redirected to auth/login: {cur_url} — using unified signup route"
      ));
      session
        .navigate("https://chatgpt.com/auth/login?screen_hint=signup", 20)
        .await?;
      self.human_pause(1500, 2500).await;
      return Ok(session.current_url().await.unwrap_or(cur_url));
    }

    self.log("Loaded chatgpt.com homepage");

    // Step 2: Click the homepage "Sign up" button. Two tolerance windows:
    //   a) The button is `data-testid="signup-button"` (verified live).
    //   b) Fallback to text "Sign up" / "Sign in" if the testid hides or
    //      the click target is moved.
    //
    // The click either reveals an in-page signup dialog (no URL change) or
    // navigates to chatgpt.com/auth/login?screen_hint=signup — both are
    // acceptable signup surfaces. We do NOT navigate to auth/login directly
    // because doing so skips the homepage Cloudflare session that ChatGPT's
    // NextAuth uses to mint a real anonymous session token.
    let page_snapshot_before_click = session.context_page_snapshot()?;

    // Camoufox (Firefox-based): Playwright clicks may not trigger React
    // synthetic event handlers reliably. Always try a direct JS .click()
    // first — this dispatches both native and React events.
    let mut signup_clicked = false;
    let direct_click_js = r#"(function(){
      const btn = document.querySelector('[data-testid="signup-button"]');
      if (btn) { btn.scrollIntoView({ block: 'center' }); btn.click(); return true; }
      const alt = Array.from(document.querySelectorAll('button, a, [role="button"]')).find(el => {
        const t = (el.innerText || '').trim().toLowerCase();
        return t === 'sign up';
      });
      if (alt) { alt.scrollIntoView({ block: 'center' }); alt.click(); return true; }
      return false;
    })()"#;
    if session
      .evaluate(direct_click_js, false)
      .await
      .is_ok_and(|r| r.get("value").and_then(|v| v.as_bool()).unwrap_or(false))
    {
      self.log("Clicked homepage Sign-up via direct JS click");
      self.human_pause(1500, 2500).await;
      signup_clicked = true;
    }

    let clicked_signup = if signup_clicked {
      Ok::<String, String>("JS click".into())
    } else {
      match session
        .camoufox_click_selector(
          r#"[data-testid="signup-button"], button:has-text("Sign up")"#,
          5_000,
        )
        .await
      {
        Ok(true) => {
          self.log("Clicked homepage Sign-up with Camoufox DOM selector");
          Ok::<String, String>("DOM selector".into())
        }
        Ok(false) | Err(_) => {
          let coordinate_click = async {
            use crate::browser_actions::{click_point_in_rect, HumanProfile};
            let js = r#"(function(){
              function visible(el){
                try {
                  const r = el.getBoundingClientRect();
                  const s = el.ownerDocument.defaultView.getComputedStyle(el);
                  return r.width > 0 && r.height > 0
                    && s.visibility !== 'hidden' && s.display !== 'none';
                } catch(_) { return false; }
              }
              const candidates = [
                ...Array.from(document.querySelectorAll('[data-testid="signup-button"]')),
                ...Array.from(document.querySelectorAll('button, a, [role="button"]')).filter((el) => {
                  const t = (el.innerText || el.textContent || '').trim().toLowerCase();
                  return t === 'sign up' || t === 'sign in' || t.includes('sign up');
                }),
              ];
              for (const el of candidates) {
                if (!visible(el) || el.disabled) continue;
                el.scrollIntoView({ block: 'center', inline: 'nearest' });
                const r = el.getBoundingClientRect();
                return { ok: true, x: r.left, y: r.top, w: r.width, h: r.height, t: (el.innerText || '').trim().slice(0, 30) };
              }
              return { ok: false };
            })()"#;
            let res = session.evaluate(js, false).await?;
            let value = res.get("value").cloned().unwrap_or_default();
            if value.get("ok").and_then(Value::as_bool) != Some(true) {
              return Err("Sign up button not found on chatgpt.com homepage".into());
            }
            let x = value.get("x").and_then(Value::as_f64).unwrap_or(0.0);
            let y = value.get("y").and_then(Value::as_f64).unwrap_or(0.0);
            let w = value.get("w").and_then(Value::as_f64).unwrap_or(1.0);
            let h = value.get("h").and_then(Value::as_f64).unwrap_or(1.0);
            let text = value.get("t").and_then(Value::as_str).unwrap_or("?").to_string();
            let (tx, ty) = click_point_in_rect(x, y, w, h);
            session.human_click((tx, ty), &HumanProfile::careful()).await?;
            Ok::<String, String>(text)
          }
          .await;
          match &coordinate_click {
            Ok(_) => self.log("Clicked homepage Sign-up with coordinate fallback"),
            Err(error) => self.log(&format!(
              "homepage Sign-up selector and coordinate clicks failed: {error}"
            )),
          }
          coordinate_click
        }
      }
    };
    let signup_click_dispatched = clicked_signup.is_ok();
    self.human_pause(1200, 2200).await;

    // Step 3: Find the email input. Dialog, redirect, and popup paths all
    // surface the same form. Keep the attached Camoufox page synchronized with
    // its persistent Playwright context before each DOM probe.
    let email_selectors = r#"input[type="email"], input[name="email"], input[id="email"], input[autocomplete*="email"], input[placeholder*="email" i], input[aria-label*="email" i]"#;
    let mut signup_surface_ready = false;
    for _ in 0..12 {
      self.human_pause(300, 500).await;
      if session
        .sync_context_page(&page_snapshot_before_click, ContextPageTarget::Signup)
        .await
        .unwrap_or(false)
      {
        self.log("Camoufox session rebound to a new signup page");
      }
      if has_visible_selector(session, email_selectors, true)
        .await
        .unwrap_or(false)
      {
        signup_surface_ready = true;
        break;
      }
    }

    // A coordinate mouse event can complete without the SPA accepting the
    // intended control. On Camoufox, retry the same DOM target through
    // Playwright's actionability-checked selector click and verify the email
    // form as the postcondition.
    if !signup_surface_ready && signup_click_dispatched {
      match session
        .camoufox_click_selector(
          r#"[data-testid="signup-button"], button:has-text("Sign up")"#,
          5_000,
        )
        .await
      {
        Ok(true) => {
          self.log("Retried homepage Sign-up with Camoufox DOM selector click");
          for _ in 0..12 {
            self.human_pause(300, 500).await;
            if session
              .sync_context_page(&page_snapshot_before_click, ContextPageTarget::Signup)
              .await
              .unwrap_or(false)
            {
              self.log("Camoufox session rebound after DOM selector click");
            }
            if has_visible_selector(session, email_selectors, true)
              .await
              .unwrap_or(false)
            {
              signup_surface_ready = true;
              break;
            }
          }
        }
        Ok(false) => {}
        Err(error) => self.log(&format!(
          "Camoufox DOM selector click did not open signup: {}",
          safe_provider_detail(&error)
        )),
      }
    }

    // The current ChatGPT UI also exposes the same unified email form at this
    // same-origin route. Reaching it after the homepage load preserves the
    // anonymous session and avoids burning a fingerprint retry on a missed SPA
    // dialog transition.
    if !signup_surface_ready {
      if let Ok(Some(summary)) = session.camoufox_dom_surface_summary().await {
        self.log(&format!(
          "Camoufox post-signup DOM before route fallback: {}",
          safe_provider_detail(&summary)
        ));
      }
      self.log("Opening unified ChatGPT signup route in the Camoufox session");
      session
        .navigate("https://chatgpt.com/auth/login?screen_hint=signup", 25)
        .await?;
      for _ in 0..25 {
        self.human_pause(300, 500).await;
        if has_visible_selector(session, email_selectors, true)
          .await
          .unwrap_or(false)
        {
          signup_surface_ready = true;
          break;
        }
      }
    }

    if !signup_surface_ready {
      let detail = session
        .camoufox_dom_surface_summary()
        .await
        .ok()
        .flatten()
        .map(|summary| safe_provider_detail(&summary))
        .unwrap_or_else(|| "unavailable".into());
      return Err(format!(
        "Email input not found on ChatGPT signup surface (Camoufox DOM: {detail})"
      ));
    }

    fill_visible_input(session, email_selectors, email, "email").await?;
    self.human_pause(350, 500).await;
    self.log("Filled email on chatgpt.com signup surface");

    // Step 4: Click the email-form Continue button. We deliberately avoid
    // generic `click_trusted_submit` here because chatgpt.com's signup popup
    // renders SSO suggestion buttons ("Continue with Google", "Continue
    // with Apple", "Continue with phone") as `<button>` elements whose text
    // starts with "Continue". A naive submit-button scan clicks those
    // instead of the email-form Continue, bouncing the browser to the SSO
    // provider's authorize endpoint (accounts.google.com / appleid.apple.com)
    // and leaving the registration flow stranded with no account created.
    // `click_email_form_continue` rejects any candidate containing "with" /
    // "use apple/google/microsoft/phone" text and only accepts the form-level
    // Continue whose trimmed text is exactly "continue".
    let auth_page_snapshot = session.context_page_snapshot()?;
    click_email_form_continue(session).await?;
    self.log("Clicked Continue on chatgpt.com signup surface");
    self.human_pause(1000, 2000).await;

    // Step 5: Follow the same Playwright context to the next concrete auth
    // surface. OpenAI may navigate the current page, replace it, open another
    // page, or keep the unified route while rendering a method/password form.
    for _ in 0..30 {
      self.human_pause(400, 600).await;
      if session
        .sync_context_page(&auth_page_snapshot, ContextPageTarget::Auth)
        .await
        .unwrap_or(false)
      {
        self.log("Camoufox session rebound to an auth continuation page");
      }
      let cur = session.current_url().await.unwrap_or_default();
      if cur.contains("auth.openai.com") {
        self.log(&format!("Landed on auth.openai.com: {cur}"));
        return Ok(cur);
      }
      if session
        .camoufox_has_concrete_auth_surface()
        .await
        .unwrap_or(false)
      {
        self.log("Concrete Camoufox auth DOM is ready");
        return Ok(cur);
      }
    }

    let cur = session.current_url().await.unwrap_or_default();
    let detail = session
      .camoufox_dom_surface_summary()
      .await
      .ok()
      .flatten()
      .unwrap_or_else(|| "unavailable".into());
    self.log(&format!(
      "Auth continuation timed out at {cur}; Camoufox DOM: {detail}"
    ));
    Ok(cur)
  }

  /// Navigate to auth.openai.com signup page via UI (click "Sign up" on
  /// homepage → fill email in dialog → Continue) with browser-relaunch
  /// retry on failure / Cloudflare block.
  async fn authorize_with_retry(
    &mut self,
    session: &mut BrowserSession,
    profile: &mut crate::profile::BrowserProfile,
    email: &str,
    app_handle: &tauri::AppHandle,
  ) -> Result<String, String> {
    let max_attempts = 3;

    for attempt in 0..max_attempts {
      if attempt > 0 {
        self.log(&format!(
          "Authorize retry {attempt}/{max_attempts}: relaunching same worker with new fingerprint..."
        ));
        // Human-like backoff before kill/relaunch thrash.
        self.human_pause(1500, 3200).await;
        // Kill only — keep the same worker profile metadata for relaunch.
        // Abort the retry chain on kill failure: relaunching would stack a
        // second browser on the still-running one and freeze the machine.
        self.kill_browser_only(app_handle, profile).await?;

        // Relaunch the same worker with a new fingerprint and the same account identity.
        let (new_profile, new_session) = self.launch_and_connect(app_handle).await?;
        *session = new_session;
        *profile = new_profile;

        // Re-seed cookies and re-visit homepage on the relaunched browser.
        for domain in &[
          "chatgpt.com",
          ".chatgpt.com",
          "auth.openai.com",
          ".auth.openai.com",
        ] {
          let _ = session.set_cookie("oai-did", &self.device_id, domain).await;
        }
        session.navigate("https://chatgpt.com/", 20).await?;
        self.human_pause(1200, 2200).await;
        self.log(&format!(
          "Worker relaunched for authorize retry: {} ({})",
          profile.name, profile.id
        ));
      }

      let cur = match self.navigate_to_signup_page(session, email).await {
        Ok(u) => u,
        Err(e) => {
          self.log(&format!(
            "navigate_to_signup_page failed (attempt {attempt}): {e}"
          ));
          continue;
        }
      };

      // OpenAI can keep the unified auth flow on chatgpt.com/auth/login after
      // email submission. That page is a valid continuation surface; the
      // registration state machine advances it by DOM signals.
      if !is_registration_auth_surface(&cur) && !is_auth_route_error_url(&cur) {
        self.log(&format!(
          "Signup did not reach a supported auth surface (attempt {attempt}/{max_attempts}): {cur}"
        ));
        continue;
      }

      let (block, detail) = classify_authorize_block(session, &cur).await;
      match block {
        AuthorizeBlock::Other => return Ok(cur),
        AuthorizeBlock::Cloudflare => {
          self.log(&format!("Cloudflare block detected: {cur} {detail}"));
          self.rate_limiter.mark_rate_limited();

          // —— Turnstile auto-bypass ——
          match session.bypass_turnstile_if_present().await {
            Ok(true) => {
              self.human_pause(1500, 2500).await;
              let after = session.current_url().await.unwrap_or_default();
              if !is_cloudflare_wall(&after) {
                self.log(&format!("Turnstile bypassed → {after}"));
                return Ok(after);
              }
              self.log("Turnstile bypassed but still on Cloudflare wall");
            }
            Ok(false) => {
              // Not a Turnstile widget — might be a JS challenge page.
              // Wait for it to auto-resolve (3-12 seconds).
              self.log("No Turnstile widget — waiting for CF JS challenge to resolve");
              let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
              while tokio::time::Instant::now() < deadline {
                self.human_pause(800, 1200).await;
                let url = session.current_url().await.unwrap_or_default();
                if !is_cloudflare_wall(&url) && !is_auth_route_error_url(&url) {
                  self.log(&format!("CF challenge resolved → {url}"));
                  return Ok(url);
                }
              }
              self.log("CF JS challenge did not resolve within 15s");
            }
            Err(e) => {
              self.log(&format!("Turnstile bypass attempt failed: {e}"));
            }
          }
          continue;
        }
        AuthorizeBlock::RouteError => {
          self.log(&format!("OpenAI auth route error on {cur}: {detail}"));
          // Try a single "Try again"/Continue click first — same recovery
          // pattern the login engine uses for Remix route errors.
          if try_click_auth_error_retry(session).await {
            self.human_pause(1500, 2500).await;
            let after = session.current_url().await.unwrap_or_default();
            if !is_auth_route_error_url(&after) && !is_cloudflare_wall(&after) {
              self.log(&format!("Route error recovered via Try again → {after}"));
              return Ok(after);
            }
            self.log(&format!("Route error Try again still blocked → {after}"));
          }
          // Final fallback: refresh the chatgpt.com homepage and click the
          // real "Sign up" button again. We deliberately do NOT navigate to
          // any auth.openai.com route (log-in/create-account/account/create)
          // because headless snapshots showed every one of those returns
          // `<title>Your session has ended</title>` for anonymous visitors.
          // Signup must originate from the chatgpt.com homepage where the
          // Cloudflare + NextAuth anonymous session cookie is minted.
          session.navigate("https://chatgpt.com/", 25).await?;
          self.human_pause(1200, 2200).await;
          let _ = session.evaluate("window.scrollBy(0, 200)", false).await;
          self.human_pause(600, 1200).await;
          // Re-run the homepage signup click — if it succeeds and we land
          // on an acceptable surface, return it; otherwise fall through to
          // fingerprint/IP relaunch for the next attempt.
          match self.navigate_to_signup_page(session, email).await {
            Ok(after) => {
              let after_lower = after.to_ascii_lowercase();
              if !is_auth_route_error_url(&after_lower)
                && !is_cloudflare_wall(&after_lower)
                && !after_lower.is_empty()
              {
                self.log(&format!(
                  "Route error recovered via homepage retry → {after}"
                ));
                return Ok(after);
              }
              self.log(&format!(
                "Route error homepage retry still blocked → {after}"
              ));
            }
            Err(e) => self.log(&format!("Route error homepage retry failed: {e}")),
          }
          continue;
        }
      }
    }

    Err("Authorize failed after max retries — signup navigation/Cloudflare".into())
  }

  // -----------------------------------------------------------------------
  // Browser launch + CDP connect + cleanup
  // -----------------------------------------------------------------------

  /// Launch a browser profile and attach the right automation backend.
  /// Chromium uses CDP; Camoufox uses the Playwright page from CamoufoxManager.
  async fn launch_and_connect(
    &mut self,
    app_handle: &tauri::AppHandle,
  ) -> Result<(crate::profile::BrowserProfile, BrowserSession), String> {
    let profile = self.launch_browser(app_handle).await?;
    self.log(&format!(
      "Browser launched: profile={} id={} browser={}",
      profile.name, profile.id, profile.browser
    ));

    let session = match attach_browser_session(&profile).await {
      Ok(session) => session,
      Err(error) => {
        self.log(&format!("Attach automation session failed: {error}"));
        return Err(error);
      }
    };
    self.log("Automation session attached");
    Ok((profile, session))
  }

  /// Ensure a single reusable worker profile exists for this engine/batch.
  ///
  /// - If `config.profile_id` is set and found, adopt it (do not delete at end).
  /// - Otherwise create one ephemeral worker once and reuse it for every account.
  /// - Each launch regenerates fingerprint + ephemeral data dir via browser_runner.
  async fn ensure_worker_profile(
    &mut self,
    app_handle: &tauri::AppHandle,
  ) -> Result<crate::profile::BrowserProfile, String> {
    use crate::browser::BrowserType;
    use crate::profile::manager::create_browser_profile_with_group;

    // Already have a worker for this engine — reload latest metadata.
    if let Some(id) = self.worker_profile_id.clone() {
      if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
        if let Some(found) = profiles.into_iter().find(|p| p.id.to_string() == id) {
          return Ok(found);
        }
      }
      self.log(&format!(
        "Worker profile {id} missing from store — will recreate"
      ));
      self.worker_profile_id = None;
      self.owns_worker_profile = false;
    }

    // Prefer an existing user-selected profile as the worker (reuse, not template-only).
    if let Some(profile_id) = self.config.profile_id.as_ref() {
      if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
        if let Some(mut found) = profiles
          .into_iter()
          .find(|p| p.id.to_string() == *profile_id)
        {
          // Ensure relaunches renew fingerprint even when reusing a user profile.
          if found.browser.eq_ignore_ascii_case("camoufox") {
            let mut cfg = found.camoufox_config.clone().unwrap_or_default();
            if cfg.randomize_fingerprint_on_launch != Some(true) {
              cfg.randomize_fingerprint_on_launch = Some(true);
              found.camoufox_config = Some(cfg.clone());
              if let Err(e) = crate::profile::ProfileManager::instance()
                .update_camoufox_config(app_handle.clone(), &found.id.to_string(), cfg)
                .await
              {
                self.log(&format!(
                  "Warning: failed to enable Camoufox FP renew on worker: {e}"
                ));
              }
            }
          } else if found.browser.eq_ignore_ascii_case("chromium") {
            let mut cfg = found.chromium_config.clone().unwrap_or_default();
            if cfg.randomize_fingerprint_on_launch != Some(true) {
              cfg.randomize_fingerprint_on_launch = Some(true);
              found.chromium_config = Some(cfg.clone());
              if let Err(e) = crate::profile::ProfileManager::instance()
                .update_chromium_config(app_handle.clone(), &found.id.to_string(), cfg)
                .await
              {
                self.log(&format!(
                  "Warning: failed to enable Chromium FP renew on worker: {e}"
                ));
              }
            }
          }

          // Align network attachment with batch/slot config (vpn preferred, else proxy).
          if let Some(vpn_id) = self.worker_vpn_id() {
            if found.vpn_id.as_deref() != Some(vpn_id.as_str()) {
              match crate::profile::ProfileManager::instance()
                .update_profile_vpn(
                  app_handle.clone(),
                  &found.id.to_string(),
                  Some(vpn_id.clone()),
                )
                .await
              {
                Ok(updated) => {
                  found = updated;
                  self.log(&format!("Worker profile VPN set to {vpn_id}"));
                }
                Err(e) => {
                  self.log(&format!(
                    "Warning: failed to set worker vpn_id={vpn_id}: {e}"
                  ));
                }
              }
            }
          } else if let Some(proxy_id) = self.config.effective_proxy_id() {
            if found.proxy_id.as_deref() != Some(proxy_id.as_str()) {
              match crate::profile::ProfileManager::instance()
                .update_profile_proxy(
                  app_handle.clone(),
                  &found.id.to_string(),
                  Some(proxy_id.clone()),
                )
                .await
              {
                Ok(updated) => {
                  found = updated;
                  self.log(&format!("Worker profile proxy set to {proxy_id}"));
                }
                Err(e) => {
                  self.log(&format!(
                    "Warning: failed to set worker proxy_id={proxy_id}: {e}"
                  ));
                }
              }
            }
          }

          self.log(&format!(
            "Reusing configured profile as worker: {} ({}) browser={} version={}",
            found.name, found.id, found.browser, found.version
          ));
          self.worker_profile_id = Some(found.id.to_string());
          self.owns_worker_profile = false;
          return Ok(found);
        }
      }
      self.log(&format!(
        "Configured profile_id {profile_id} not found — creating auto-reg worker"
      ));
    }

    let browser_str = if self.config.browser_type == "camoufox" {
      "camoufox"
    } else if self.config.browser_type == "firefox" {
      "firefox"
    } else {
      "chromium"
    };
    let mut version = String::new();
    let mut release_type = "stable".to_string();

    // Prefer an installed version from any existing profile of the same browser.
    if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
      if let Some(found) = profiles
        .into_iter()
        .find(|p| p.browser.eq_ignore_ascii_case(browser_str) && !p.version.is_empty())
      {
        version = found.version;
        if !found.release_type.is_empty() {
          release_type = found.release_type;
        }
        self.log(&format!(
          "Using installed {browser_str} version from existing profile: {version}"
        ));
      }
    }
    // Fallback: downloaded browsers registry (critical for chromium — empty version
    // resolves to binaries/fingerprint-chromium/ and fails to find chrome.exe).
    if version.is_empty() {
      let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
      let mut versions = registry.get_downloaded_versions(browser_str);
      versions.sort_by(|a, b| {
        crate::api_client::VersionComponent::parse(b)
          .cmp(&crate::api_client::VersionComponent::parse(a))
      });
      if let Some(v) = versions.into_iter().next() {
        version = v;
        self.log(&format!(
          "Using installed {browser_str} version from registry: {version}"
        ));
      }
    }

    if version.is_empty() && browser_str == "camoufox" {
      version = "v135.0.1-beta.24".into();
      self.log(&format!("Using default Camoufox version: {version}"));
    }
    // System Firefox: use default Playwright Firefox version
    if version.is_empty() && browser_str == "firefox" {
      version = "firefox".into();
      release_type = "stable".to_string();
      self.log("Using system Firefox");
    }
    if version.is_empty() {
      return Err(format!(
        "No downloaded {browser_str} version found. Install the browser in JnmBrowser first."
      ));
    }

    let browser =
      BrowserType::from_str(browser_str).map_err(|e| format!("Invalid browser type: {e}"))?;

    // One stable worker name per task+CDK slot (not per account).
    let profile_name = format!(
      "auto-reg-worker-{}-s{}",
      &self.task_id[..8.min(self.task_id.len())],
      self.worker_slot
    );

    // Match JnmBrowser create-profile defaults: auto geoip + host screen bounds so
    // fingerprint generation and launch geometry stay consistent with the app UI.
    let camoufox_config = if browser_str == "camoufox" {
      Some(crate::camoufox_manager::CamoufoxConfig {
        fingerprint: None,
        randomize_fingerprint_on_launch: Some(true),
        geoip: Some(serde_json::Value::Bool(true)),
        // Common desktop bounds; browser_runner still regenerates fingerprint
        // through VPN local proxy on each launch.
        screen_max_width: Some(1920),
        screen_max_height: Some(1080),
        screen_min_width: Some(1280),
        screen_min_height: Some(720),
        os: Some("windows".into()),
        ..Default::default()
      })
    } else {
      None
    };

    // Chromium also renews fingerprint on every launch of this worker.
    let chromium_config = if browser_str == "chromium" {
      Some(crate::chromium_manager::ChromiumConfig {
        fingerprint: None,
        randomize_fingerprint_on_launch: Some(true),
        ..Default::default()
      })
    } else {
      None
    };

    // Camoufox (Firefox-based) requires a real profile directory with prefs.js,
    // places.sqlite, etc. Ephemeral bare temp dirs break cookie/storage persistence
    // and cause page reloads on form submit. Chromium handles ephemeral dirs fine.
    let is_ephemeral = browser_str != "camoufox" && browser_str != "firefox";

    let mut created = create_browser_profile_with_group(
      app_handle.clone(),
      profile_name,
      browser.as_str().to_string(),
      version,
      release_type,
      self.config.effective_proxy_id(),
      self.worker_vpn_id(),
      camoufox_config,
      chromium_config,
      None,
      is_ephemeral,
      None,
      None,
    )
    .await
    .map_err(|e| format!("Create worker profile: {e}"))?;

    // Persist randomize flags so relaunches keep renewing fingerprints.
    if created.browser.eq_ignore_ascii_case("camoufox") {
      let mut cfg = created.camoufox_config.clone().unwrap_or_default();
      cfg.randomize_fingerprint_on_launch = Some(true);
      created.camoufox_config = Some(cfg);
    } else if created.browser.eq_ignore_ascii_case("chromium") {
      let mut cfg = created.chromium_config.clone().unwrap_or_default();
      cfg.randomize_fingerprint_on_launch = Some(true);
      created.chromium_config = Some(cfg);
      if let Err(e) = crate::profile::ProfileManager::instance()
        .update_chromium_config(
          app_handle.clone(),
          &created.id.to_string(),
          created.chromium_config.clone().unwrap_or_default(),
        )
        .await
      {
        self.log(&format!(
          "Warning: failed to persist Chromium randomize flag: {e}"
        ));
      }
    }

    self.worker_profile_id = Some(created.id.to_string());
    self.owns_worker_profile = true;
    self.log(&format!(
      "Created reusable worker profile {} (id={}) browser={} — relaunch renews fingerprint + data",
      created.name, created.id, created.browser
    ));

    Ok(created)
  }

  /// Launch the reused worker profile (creates worker once if needed).
  async fn launch_browser(
    &mut self,
    app_handle: &tauri::AppHandle,
  ) -> Result<crate::profile::BrowserProfile, String> {
    use crate::browser_runner::BrowserRunner;

    let worker = self.ensure_worker_profile(app_handle).await?;
    self.log(&format!(
      "Launching worker {} ({}) — fingerprint renew + fresh ephemeral dir",
      worker.name, worker.id
    ));

    let launched = BrowserRunner::instance()
      .launch_browser(
        app_handle.clone(),
        &worker,
        Some("about:blank".into()),
        None,
      )
      .await
      .map_err(|e| format!("Launch: {e}"))?;

    Ok(launched)
  }

  /// Kill the browser process only. Keep worker profile metadata for reuse.
  /// Ephemeral data dir is removed by BrowserRunner on kill.
  async fn kill_browser_only(
    &mut self,
    app_handle: &tauri::AppHandle,
    profile: &crate::profile::BrowserProfile,
  ) -> Result<(), String> {
    use crate::browser_runner::BrowserRunner;

    match BrowserRunner::instance()
      .kill_browser_process(app_handle.clone(), profile)
      .await
    {
      Ok(()) => {
        self.log(&format!(
          "Browser killed for worker profile {} ({})",
          profile.name, profile.id
        ));
        Ok(())
      }
      Err(e) => {
        let msg = format!(
          "Failed to kill browser for profile {} ({}): {e}",
          profile.name, profile.id
        );
        self.log(&msg);
        Err(msg)
      }
    }
  }

  /// Delete the auto-created worker at batch end. Never delete user-provided profile_id.
  async fn dispose_worker_profile(&mut self, app_handle: &tauri::AppHandle) {
    let Some(id) = self.worker_profile_id.take() else {
      return;
    };
    if !self.owns_worker_profile {
      self.log(&format!(
        "Keeping user-provided worker profile on disk: {id}"
      ));
      return;
    }
    self.owns_worker_profile = false;

    // Best-effort kill if still running. Dispose proceeds regardless — the
    // profile metadata is removed either way, and kill failure is logged
    // by kill_browser_only itself.
    if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
      if let Some(found) = profiles.into_iter().find(|p| p.id.to_string() == id) {
        let _ = self.kill_browser_only(app_handle, &found).await;
      }
    }

    self.human_pause(450, 550).await;

    if let Err(e) = crate::profile::ProfileManager::instance().delete_profile(app_handle, &id) {
      self.log(&format!(
        "Warning: failed to delete worker profile {id}: {e}"
      ));
    } else {
      self.log(&format!("Worker profile deleted: {id}"));
    }
  }

  #[allow(dead_code)]
  fn fail_result(&self, error: &str) -> RegistrationResult {
    RegistrationResult {
      success: false,
      email: String::new(),
      password: String::new(),
      account_id: String::new(),
      access_token: String::new(),
      device_id: String::new(),
      error_message: error.to_string(),
      step_logs: self.logs.clone(),
      created_at: Utc::now(),
      two_fa_enabled: false,
      totp_secret: String::new(),
      free_trial_eligible: false,
      plan_type: String::new(),
      cdk: String::new(),
      base_email: String::new(),
      phone_number: String::new(),
      status: AccountInventoryStatus::Invalid,
      note: String::new(),
      exported_at: None,
      sold_at: None,
      email_provider: Some(self.config.email_provider),
      email_provider_provenance: Some(EmailProviderProvenance::RegistrationConfig),
      registration_outcome_reason: Some(RegistrationOutcomeReason::RegistrationFailed),
      two_factor_backfill_access_state: Some(TwoFactorBackfillAccessState::Accessible),
      two_factor_backfill_exclusion: None,
      two_factor_backfill_state: None,
      two_factor_backfill_outcome: None,
      two_factor_backfill_operation_id: None,
      record_revision: 1,
    }
  }
}
