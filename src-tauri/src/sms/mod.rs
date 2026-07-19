pub mod commands;
pub mod error;
pub mod viotp;

use serde::{Deserialize, Serialize};

pub use error::SmsServiceError;

/// Mobile network / carrier offered by an SMS provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmsNetwork {
  pub id: u32,
  pub name: String,
}

/// Rentable service (e.g. Facebook, Momo) offered by an SMS provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmsServiceInfo {
  pub id: u32,
  pub name: String,
  pub price: f64,
}

/// Parameters for renting a phone number.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NumberRequest {
  pub service_id: u32,
  /// Pipe-separated carrier names, e.g. `"VIETTEL|MOBIFONE"`.
  pub network: Option<String>,
  /// Preferred number prefixes, e.g. `"90|91"`.
  pub prefix: Option<String>,
  /// Excluded prefixes, e.g. `"94|96"`.
  pub except_prefix: Option<String>,
  /// Re-rent a previous number (provider-specific re-phone value).
  pub number: Option<String>,
  /// Country code: `"vn"` (default) or `"la"`.
  pub country: Option<String>,
}

/// Result of a successful number rental.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NumberInfo {
  pub phone_number: String,
  /// Provider-specific value used to re-rent the same number later.
  pub re_phone_number: Option<String>,
  pub request_id: String,
  pub balance: Option<f64>,
  pub country_iso: Option<String>,
  pub country_code: Option<String>,
}

/// OTP / SMS payload received for a rented number.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OtpInfo {
  pub request_id: String,
  pub phone: String,
  pub code: Option<String>,
  pub sms_content: Option<String>,
  /// 0 = waiting, 1 = completed, 2 = expired.
  pub status: u32,
  pub service_id: Option<u32>,
  pub service_name: Option<String>,
  pub is_sound: bool,
  pub phone_original: Option<String>,
  pub country_iso: Option<String>,
  pub country_code: Option<String>,
  pub created_time: Option<String>,
}

/// Filters for rental history queries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryQuery {
  pub service: Option<u32>,
  /// 0 = waiting, 1 = completed, 2 = expired.
  pub status: Option<u32>,
  pub limit: Option<u32>,
  /// `yyyy-MM-dd`
  pub from_date: Option<String>,
  /// `yyyy-MM-dd`
  pub to_date: Option<String>,
}

/// One history row from an SMS provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
  pub id: String,
  pub service_id: Option<u32>,
  pub service_name: Option<String>,
  pub status: u32,
  pub price: Option<f64>,
  pub phone: String,
  pub code: Option<String>,
  pub sms_content: Option<String>,
  pub is_sound: bool,
  pub phone_original: Option<String>,
  pub country_iso: Option<String>,
  pub country_code: Option<String>,
  pub created_time: Option<String>,
}

/// Abstract SMS / phone-OTP provider used by standalone UI and auto-registration.
/// Each implementation handles: balance, networks, services, number rental, OTP polling.
pub trait SmsService: Send + Sync {
  fn get_balance(&self) -> Result<f64, SmsServiceError>;

  fn get_networks(&self, country: Option<&str>) -> Result<Vec<SmsNetwork>, SmsServiceError>;

  fn get_services(&self, country: Option<&str>) -> Result<Vec<SmsServiceInfo>, SmsServiceError>;

  fn request_number(&self, request: &NumberRequest) -> Result<NumberInfo, SmsServiceError>;

  /// Poll until an OTP arrives or `timeout_secs` elapses.
  fn get_otp(&self, request_id: &str, timeout_secs: u64) -> Result<OtpInfo, SmsServiceError>;

  fn get_history(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>, SmsServiceError>;

  #[allow(dead_code)]
  fn check_health(&self) -> bool;
}
