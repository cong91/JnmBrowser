use super::types::{AccountOutcome, ReasonCode};

/// Raw signals extracted from the auth page by browser JS.
#[derive(Debug, Clone)]
pub struct AuthSignals {
  pub body_text: String,
  pub has_visible_verification_input: bool,
}

/// Pure classifier: no browser, no async, table-testable.
///
/// Rules (applied in priority order):
/// 1. Deactivated = body contains BOTH "authentication error" AND "error_code: account_deactivated"
/// 2. Passed      = has_visible_verification_input is true
/// 3. Else        → Unresolved
///
/// Single-signal cases (only one of the two deactivation markers)
/// fall through to Unresolved — never Deactivated.
pub fn classify_auth_state(signals: &AuthSignals) -> (AccountOutcome, ReasonCode) {
  let body = signals.body_text.to_ascii_lowercase();

  // Deactivated requires both signals together.
  let has_auth_error = body.contains("authentication error");
  let has_deactivated_code = body.contains("error_code: account_deactivated");

  if has_auth_error && has_deactivated_code {
    return (AccountOutcome::Deactivated, ReasonCode::Deactivated);
  }

  // Pass when we see a visible verification input.
  // (The engine checks visibility via has_visible_selector before setting this flag.)
  if signals.has_visible_verification_input {
    return (AccountOutcome::Passed, ReasonCode::OtpReached);
  }

  // Explicit negative signals help the engine pick a better code
  // but all resolve to Unresolved in the classifier.
  if body.contains("incorrect email or password")
    || body.contains("invalid email or password")
    || body.contains("wrong password")
  {
    return (AccountOutcome::Unresolved, ReasonCode::WrongCredentials);
  }

  if body.contains("too many requests") || body.contains("rate limit") {
    return (AccountOutcome::Unresolved, ReasonCode::RateLimited);
  }

  if body.contains("verify you are human")
    || body.contains("cf-turnstile")
    || body.contains("challenges.cloudflare.com")
    || body.contains("just a moment")
  {
    return (AccountOutcome::Unresolved, ReasonCode::Cloudflare);
  }

  (AccountOutcome::Unresolved, ReasonCode::UnknownSurface)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn classify(body: &str, has_verification: bool) -> (AccountOutcome, ReasonCode) {
    classify_auth_state(&AuthSignals {
      body_text: body.into(),
      has_visible_verification_input: has_verification,
    })
  }

  // ── Deactivated ──────────────────────────────────────────

  #[test]
  fn both_signals_deactivated() {
    let body = "Authentication Error\nerror_code: account_deactivated\nrequest_id: abc";
    let (outcome, code) = classify(body, false);
    assert_eq!(outcome, AccountOutcome::Deactivated);
    assert_eq!(code, ReasonCode::Deactivated);
  }

  #[test]
  fn real_deactivated_html_extract() {
    let body = r#"You do not have an account because it has been deleted or deactivated.
If you believe this was an error, please contact us through our help center at help.openai.com.
Authentication Error
error_code: account_deactivated
request_id: 5bf50d43-5a10-4d55-b15d-b0528f935729"#;
    let (outcome, code) = classify(body, false);
    assert_eq!(outcome, AccountOutcome::Deactivated);
    assert_eq!(code, ReasonCode::Deactivated);
  }

  #[test]
  fn auth_error_without_deactivated_code_unresolved() {
    let body = "Authentication Error\nYour account has been locked for security reasons.";
    let (outcome, _) = classify(body, false);
    assert_eq!(outcome, AccountOutcome::Unresolved);
  }

  #[test]
  fn deactivated_code_without_auth_error_unresolved() {
    let body = "error_code: account_deactivated";
    let (outcome, _) = classify(body, false);
    assert_eq!(outcome, AccountOutcome::Unresolved);
  }

  #[test]
  fn generic_deactivated_text_alone_unresolved() {
    let body = "account has been deactivated";
    let (outcome, _) = classify(body, false);
    assert_eq!(outcome, AccountOutcome::Unresolved);
  }

  #[test]
  fn account_locked_text_alone_unresolved() {
    let body = "authentication error\naccount has been locked";
    let (outcome, _) = classify(body, false);
    assert_eq!(outcome, AccountOutcome::Unresolved);
  }

  // ── Passed ───────────────────────────────────────────────

  #[test]
  fn visible_verification_input_passed() {
    let (outcome, code) = classify("Please enter your authentication code", true);
    assert_eq!(outcome, AccountOutcome::Passed);
    assert_eq!(code, ReasonCode::OtpReached);
  }

  #[test]
  fn verification_visible_even_with_auth_error_text_unresolved() {
    // has_visible_verification_input = true but body also has "authentication error"
    // without deactivated_code → NOT deactivated. OTP wins over error text.
    let (outcome, code) = classify("Authentication Error\nEnter code:", true);
    assert_eq!(outcome, AccountOutcome::Passed);
    assert_eq!(code, ReasonCode::OtpReached);
  }

  #[test]
  fn deactivated_code_and_otp_visible_deactivated_wins() {
    // Both deactivation signals present AND OTP visible → deactivated wins
    // (real accounts won't show both; if they do, safer to classify deactivated)
    let body = "Authentication Error\nerror_code: account_deactivated\nEnter code:";
    let (outcome, code) = classify(body, true);
    assert_eq!(outcome, AccountOutcome::Deactivated);
    assert_eq!(code, ReasonCode::Deactivated);
  }

  #[test]
  fn no_verification_and_no_error_unresolved() {
    let (outcome, code) = classify("Welcome to ChatGPT", false);
    assert_eq!(outcome, AccountOutcome::Unresolved);
    assert_eq!(code, ReasonCode::UnknownSurface);
  }

  // ── Explicit unresolved codes ────────────────────────────

  #[test]
  fn wrong_credentials_unresolved() {
    let (outcome, code) = classify("incorrect email or password", false);
    assert_eq!(outcome, AccountOutcome::Unresolved);
    assert_eq!(code, ReasonCode::WrongCredentials);
  }

  #[test]
  fn rate_limited_unresolved() {
    let (outcome, code) = classify("too many requests", false);
    assert_eq!(outcome, AccountOutcome::Unresolved);
    assert_eq!(code, ReasonCode::RateLimited);
  }

  #[test]
  fn cloudflare_unresolved() {
    let (outcome, code) = classify("cf-turnstile\nchallenges.cloudflare.com", false);
    assert_eq!(outcome, AccountOutcome::Unresolved);
    assert_eq!(code, ReasonCode::Cloudflare);
  }

  // ── Case insensitivity ───────────────────────────────────

  #[test]
  fn deactivated_case_insensitive() {
    let body = "AUTHENTICATION ERROR\nERROR_CODE: ACCOUNT_DEACTIVATED";
    let (outcome, code) = classify(body, false);
    assert_eq!(outcome, AccountOutcome::Deactivated);
    assert_eq!(code, ReasonCode::Deactivated);
  }

  #[test]
  fn wrong_password_case_insensitive() {
    let (outcome, code) = classify("Wrong Password", false);
    assert_eq!(outcome, AccountOutcome::Unresolved);
    assert_eq!(code, ReasonCode::WrongCredentials);
  }
}
