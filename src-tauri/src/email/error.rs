use std::fmt;

#[derive(Debug)]
pub enum EmailServiceError {
  CdkInvalid(String),
  CdkExpired(String),
  Timeout(String),
  Network(String),
  EmailInvalid(String),
  Internal(String),
}

impl fmt::Display for EmailServiceError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::CdkInvalid(msg) => write!(f, "CDK invalid: {msg}"),
      Self::CdkExpired(msg) => write!(f, "CDK expired: {msg}"),
      Self::Timeout(msg) => write!(f, "Timeout: {msg}"),
      Self::Network(msg) => write!(f, "Network error: {msg}"),
      Self::EmailInvalid(msg) => write!(f, "Email invalid: {msg}"),
      Self::Internal(msg) => write!(f, "Internal error: {msg}"),
    }
  }
}

impl std::error::Error for EmailServiceError {}
