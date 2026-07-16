pub mod error;
pub mod gmail_cdk;

use serde::{Deserialize, Serialize};

pub use error::EmailServiceError;

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

/// Abstract interface for email services used in auto-registration.
/// Each implementation handles: CDK redemption → email, alias generation, and OTP polling.
pub trait EmailService: Send + Sync {
  /// Redeem a CDK to obtain a base email address.
  fn redeem_cdk(&self, cdk: &str) -> Result<EmailInfo, EmailServiceError>;

  /// Generate an alias from a base Gmail address.
  /// Example: "user@gmail.com" → "user+abc12@gmail.com"
  fn generate_alias(&self, base_email: &str) -> Result<String, EmailServiceError>;

  /// Poll for a verification code sent to the email associated with the CDK.
  /// Returns the OTP code string on success.
  fn poll_verification_code(
    &self,
    cdk: &str,
    timeout_secs: u64,
  ) -> Result<String, EmailServiceError>;

  /// Check if the service is reachable.
  fn check_health(&self) -> bool;
}
