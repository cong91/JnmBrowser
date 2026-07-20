mod alias;
pub mod error;
pub mod gmail_123452026;
pub mod sms_iosmq;

use serde::{Deserialize, Serialize};

pub use alias::{EmailAliasGenerator, MAX_ALIASES_PER_EMAIL};
pub use error::EmailServiceError;
pub use gmail_123452026::Gmail123452026Service;
pub use sms_iosmq::SmsIosmqService;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailInfo {
  pub email: String,
  pub expires_at: Option<String>,
  pub status: String,
  pub remaining_uses: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationCodeResponse {
  pub status: String,
  pub code: Option<String>,
  pub message: Option<String>,
}

/// Supported email OTP providers — named by API domain.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum EmailProvider {
  /// http://gmail.123452026.xyz — CDK + Gmail +aliases (up to 6).
  #[default]
  #[serde(
    rename = "gmail.123452026.xyz",
    alias = "gmail_cdk",
    alias = "gmail-cdk",
    alias = "123452026"
  )]
  Gmail123452026,
  /// https://sms.iosmq.xyz — MAIL redeem + order/lookup +aliases (up to 6).
  #[serde(
    rename = "sms.iosmq.xyz",
    alias = "iosmq",
    alias = "sms.iosmq",
    alias = "iosmq.xyz",
    alias = "mail"
  )]
  SmsIosmq,
}

impl EmailProvider {
  pub const GMAIL_123452026_ID: &'static str = "gmail.123452026.xyz";
  pub const SMS_IOSMQ_ID: &'static str = "sms.iosmq.xyz";

  pub fn as_str(self) -> &'static str {
    match self {
      Self::Gmail123452026 => Self::GMAIL_123452026_ID,
      Self::SmsIosmq => Self::SMS_IOSMQ_ID,
    }
  }

  pub fn parse(raw: &str) -> Result<Self, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
      "gmail.123452026.xyz" | "gmail_cdk" | "gmail-cdk" | "123452026" => Ok(Self::Gmail123452026),
      "sms.iosmq.xyz" | "iosmq" | "sms.iosmq" | "iosmq.xyz" | "mail" => Ok(Self::SmsIosmq),
      other => Err(format!(
        "unsupported email provider '{other}'; expected {} or {}",
        Self::GMAIL_123452026_ID,
        Self::SMS_IOSMQ_ID
      )),
    }
  }

  /// Whether this provider supports multiple accounts via aliases per card.
  pub fn supports_aliases(self) -> bool {
    matches!(self, Self::Gmail123452026 | Self::SmsIosmq)
  }

  /// Max accounts that can be created from one card/CDK.
  pub fn max_accounts_per_card(self) -> u32 {
    match self {
      Self::Gmail123452026 | Self::SmsIosmq => 6,
    }
  }

  /// Clamp requested accounts/card to provider capability.
  pub fn clamp_accounts_per_card(self, requested: u32) -> u32 {
    requested.clamp(1, self.max_accounts_per_card())
  }
}

impl std::fmt::Display for EmailProvider {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

/// Abstract interface for email services used in auto-registration.
/// Each implementation handles: card/CDK redemption → email, alias generation, and OTP polling.
pub trait EmailService: Send + Sync {
  /// Redeem a card/CDK to obtain a base email address.
  fn redeem_cdk(&self, cdk: &str) -> Result<EmailInfo, EmailServiceError>;

  /// Generate an alias from a base email.
  fn generate_alias(&self, base_email: &str) -> Result<String, EmailServiceError>;

  /// Poll for a verification code sent to the email associated with the card/CDK.
  fn poll_verification_code(
    &self,
    cdk: &str,
    timeout_secs: u64,
  ) -> Result<String, EmailServiceError>;

  /// Check if the service is reachable.
  #[allow(dead_code)]
  fn check_health(&self) -> bool;
}

/// Build the email OTP provider selected by auto-registration config.
pub fn build_email_service(provider: EmailProvider) -> Box<dyn EmailService> {
  match provider {
    EmailProvider::SmsIosmq => Box::new(SmsIosmqService::new()),
    EmailProvider::Gmail123452026 => Box::new(Gmail123452026Service::new()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_provider_by_domain() {
    assert_eq!(
      EmailProvider::parse("gmail.123452026.xyz"),
      Ok(EmailProvider::Gmail123452026)
    );
    assert_eq!(
      EmailProvider::parse("sms.iosmq.xyz"),
      Ok(EmailProvider::SmsIosmq)
    );
  }

  #[test]
  fn accept_legacy_provider_ids() {
    for alias in ["gmail_cdk", "gmail-cdk", "123452026"] {
      assert_eq!(
        EmailProvider::parse(alias),
        Ok(EmailProvider::Gmail123452026)
      );
    }
    for alias in ["iosmq", "sms.iosmq", "iosmq.xyz", "mail"] {
      assert_eq!(EmailProvider::parse(alias), Ok(EmailProvider::SmsIosmq));
    }
    assert!(EmailProvider::parse("unknown").is_err());
  }

  #[test]
  fn provider_account_limits() {
    assert!(EmailProvider::Gmail123452026.supports_aliases());
    assert!(EmailProvider::SmsIosmq.supports_aliases());
    assert_eq!(EmailProvider::Gmail123452026.clamp_accounts_per_card(9), 6);
    assert_eq!(EmailProvider::SmsIosmq.clamp_accounts_per_card(9), 6);
  }

  #[test]
  fn serde_domain_wire_format() {
    let g: EmailProvider = serde_json::from_str(r#""gmail.123452026.xyz""#).unwrap();
    let i: EmailProvider = serde_json::from_str(r#""sms.iosmq.xyz""#).unwrap();
    assert_eq!(g, EmailProvider::Gmail123452026);
    assert_eq!(i, EmailProvider::SmsIosmq);
    assert_eq!(
      serde_json::to_string(&g).unwrap(),
      r#""gmail.123452026.xyz""#
    );
    assert_eq!(serde_json::to_string(&i).unwrap(), r#""sms.iosmq.xyz""#);
    let legacy_g: EmailProvider = serde_json::from_str(r#""gmail_cdk""#).unwrap();
    let legacy_i: EmailProvider = serde_json::from_str(r#""iosmq""#).unwrap();
    assert_eq!(legacy_g, EmailProvider::Gmail123452026);
    assert_eq!(legacy_i, EmailProvider::SmsIosmq);
  }
}
