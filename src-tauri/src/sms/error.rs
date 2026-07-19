use std::fmt;

#[derive(Debug)]
#[allow(dead_code)]
pub enum SmsServiceError {
  Auth(String),
  InsufficientBalance(String),
  NoNumbersAvailable(String),
  ServiceUnavailable(String),
  LimitExceeded(String),
  Timeout(String),
  Network(String),
  InvalidRequest(String),
  Internal(String),
}

impl fmt::Display for SmsServiceError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Auth(msg) => write!(f, "SMS auth error: {msg}"),
      Self::InsufficientBalance(msg) => write!(f, "SMS insufficient balance: {msg}"),
      Self::NoNumbersAvailable(msg) => write!(f, "SMS no numbers available: {msg}"),
      Self::ServiceUnavailable(msg) => write!(f, "SMS service unavailable: {msg}"),
      Self::LimitExceeded(msg) => write!(f, "SMS limit exceeded: {msg}"),
      Self::Timeout(msg) => write!(f, "SMS timeout: {msg}"),
      Self::Network(msg) => write!(f, "SMS network error: {msg}"),
      Self::InvalidRequest(msg) => write!(f, "SMS invalid request: {msg}"),
      Self::Internal(msg) => write!(f, "SMS internal error: {msg}"),
    }
  }
}

impl std::error::Error for SmsServiceError {}
