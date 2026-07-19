use super::viotp::ViotpService;
use super::{
  HistoryEntry, HistoryQuery, NumberInfo, NumberRequest, OtpInfo, SmsNetwork, SmsService,
  SmsServiceInfo,
};

async fn with_service<T, F>(token: String, f: F) -> Result<T, String>
where
  F: FnOnce(&ViotpService) -> Result<T, super::SmsServiceError> + Send + 'static,
  T: Send + 'static,
{
  if token.trim().is_empty() {
    return Err("SMS API token is required".into());
  }
  tokio::task::spawn_blocking(move || {
    let service = ViotpService::new(token);
    f(&service).map_err(|e| e.to_string())
  })
  .await
  .map_err(|e| format!("SMS task join error: {e}"))?
}

/// Check SMS provider account balance.
#[tauri::command]
pub async fn sms_get_balance(token: String) -> Result<f64, String> {
  with_service(token, |svc| svc.get_balance()).await
}

/// List available carriers / networks.
#[tauri::command]
pub async fn sms_get_networks(
  token: String,
  country: Option<String>,
) -> Result<Vec<SmsNetwork>, String> {
  with_service(token, move |svc| svc.get_networks(country.as_deref())).await
}

/// List rentable services for a country (`vn` / `la`).
#[tauri::command]
pub async fn sms_get_services(
  token: String,
  country: Option<String>,
) -> Result<Vec<SmsServiceInfo>, String> {
  with_service(token, move |svc| svc.get_services(country.as_deref())).await
}

/// Rent a phone number for a service.
#[tauri::command]
pub async fn sms_request_number(
  token: String,
  service_id: u32,
  network: Option<String>,
  prefix: Option<String>,
  except_prefix: Option<String>,
  number: Option<String>,
  country: Option<String>,
) -> Result<NumberInfo, String> {
  let request = NumberRequest {
    service_id,
    network,
    prefix,
    except_prefix,
    number,
    country,
  };
  with_service(token, move |svc| svc.request_number(&request)).await
}

/// Poll for an OTP code for a rented number session.
#[tauri::command]
pub async fn sms_get_otp(
  token: String,
  request_id: String,
  timeout_secs: Option<u64>,
) -> Result<OtpInfo, String> {
  let timeout = timeout_secs.unwrap_or(120);
  with_service(token, move |svc| svc.get_otp(&request_id, timeout)).await
}

/// Fetch rental / OTP history.
#[tauri::command]
pub async fn sms_get_history(
  token: String,
  service: Option<u32>,
  status: Option<u32>,
  limit: Option<u32>,
  from_date: Option<String>,
  to_date: Option<String>,
) -> Result<Vec<HistoryEntry>, String> {
  let query = HistoryQuery {
    service,
    status,
    limit,
    from_date,
    to_date,
  };
  with_service(token, move |svc| svc.get_history(&query)).await
}
