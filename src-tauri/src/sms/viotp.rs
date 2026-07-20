use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{
  HistoryEntry, HistoryQuery, NumberInfo, NumberRequest, OtpInfo, SmsNetwork, SmsService,
  SmsServiceError, SmsServiceInfo,
};

const API_BASE: &str = "https://api.viotp.com";
const POLL_INTERVAL_SECS: u64 = 3;

/// VI-OTP SMS provider (`https://api.viotp.com`).
pub struct ViotpService {
  api_token: String,
}

impl ViotpService {
  pub fn new(api_token: impl Into<String>) -> Self {
    Self {
      api_token: api_token.into(),
    }
  }

  /// Helper to run an async future safely from sync trait methods.
  ///
  /// Same bridging pattern as `Gmail123452026Service`: avoid nested `block_on` panics
  /// when already inside a Tokio runtime worker.
  fn block_on<F, T>(future: F) -> T
  where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
  {
    match tokio::runtime::Handle::try_current() {
      Ok(handle) => std::thread::scope(|s| {
        s.spawn(move || handle.block_on(future))
          .join()
          .expect("viotp nested runtime thread panicked")
      }),
      Err(_) => {
        let rt = tokio::runtime::Builder::new_current_thread()
          .enable_all()
          .build()
          .expect("Failed to create viotp runtime");
        rt.block_on(future)
      }
    }
  }

  fn client() -> reqwest::Client {
    reqwest::Client::builder()
      .timeout(Duration::from_secs(30))
      .build()
      .expect("Failed to build reqwest Client")
  }

  fn map_status(status_code: i64, message: &str) -> Option<SmsServiceError> {
    match status_code {
      200 => None,
      401 => Some(SmsServiceError::Auth(message.to_string())),
      429 => Some(SmsServiceError::LimitExceeded(message.to_string())),
      -1 => Some(SmsServiceError::Internal(message.to_string())),
      -2 => Some(SmsServiceError::InsufficientBalance(message.to_string())),
      -3 => Some(SmsServiceError::NoNumbersAvailable(message.to_string())),
      -4 => Some(SmsServiceError::ServiceUnavailable(message.to_string())),
      other => Some(SmsServiceError::Internal(format!(
        "unexpected status_code {other}: {message}"
      ))),
    }
  }

  fn get_json(
    &self,
    path: &str,
    query: &[(&str, String)],
  ) -> Result<serde_json::Value, SmsServiceError> {
    // Build query manually — reqwest 0.13 `.query()` needs the optional `query` feature.
    let mut pairs: Vec<(String, String)> = vec![("token".into(), self.api_token.clone())];
    for (k, v) in query {
      if !v.is_empty() {
        pairs.push(((*k).into(), v.clone()));
      }
    }
    let qs = pairs
      .iter()
      .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
      .collect::<Vec<_>>()
      .join("&");
    let url = format!("{API_BASE}{path}?{qs}");
    let client = Self::client();
    let response = Self::block_on(async move {
      client
        .get(&url)
        .header("accept", "application/json")
        .send()
        .await
    })
    .map_err(|e| SmsServiceError::Network(format!("request failed: {e}")))?;

    let status = response.status();
    let text = Self::block_on(async move { response.text().await })
      .map_err(|e| SmsServiceError::Network(format!("failed to read response: {e}")))?;

    if !status.is_success() && status.as_u16() != 200 {
      // VI-OTP often returns HTTP 200 with status_code in body; still handle hard HTTP errors.
      if status.as_u16() == 401 {
        return Err(SmsServiceError::Auth(text));
      }
      return Err(SmsServiceError::Network(format!("HTTP {status}: {text}")));
    }

    let data: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
      SmsServiceError::Internal(format!("failed to parse JSON: {e} — body: {text}"))
    })?;

    let status_code = data
      .get("status_code")
      .and_then(|v| v.as_i64())
      .unwrap_or(-1);
    let message = data
      .get("message")
      .and_then(|v| v.as_str())
      .unwrap_or("unknown error");

    if let Some(err) = Self::map_status(status_code, message) {
      return Err(err);
    }

    Ok(data)
  }

  fn json_f64(value: &serde_json::Value) -> Option<f64> {
    value
      .as_f64()
      .or_else(|| value.as_i64().map(|n| n as f64))
      .or_else(|| value.as_u64().map(|n| n as f64))
      .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
  }

  fn json_u32(value: &serde_json::Value) -> Option<u32> {
    value
      .as_u64()
      .map(|n| n as u32)
      .or_else(|| value.as_i64().map(|n| n as u32))
      .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
  }

  fn json_string(value: &serde_json::Value) -> Option<String> {
    if value.is_null() {
      return None;
    }
    if let Some(s) = value.as_str() {
      return Some(s.to_string());
    }
    if let Some(n) = value.as_i64() {
      return Some(n.to_string());
    }
    if let Some(n) = value.as_u64() {
      return Some(n.to_string());
    }
    Some(value.to_string())
  }

  fn parse_boolish(value: &serde_json::Value) -> bool {
    match value {
      serde_json::Value::Bool(b) => *b,
      serde_json::Value::String(s) => {
        let lower = s.to_ascii_lowercase();
        lower == "true" || lower == "1"
      }
      serde_json::Value::Number(n) => n.as_u64().unwrap_or(0) != 0,
      _ => false,
    }
  }

  fn parse_otp_data(data: &serde_json::Value) -> OtpInfo {
    OtpInfo {
      request_id: Self::json_string(&data["ID"])
        .or_else(|| Self::json_string(&data["Id"]))
        .unwrap_or_default(),
      phone: Self::json_string(&data["Phone"]).unwrap_or_default(),
      code: Self::json_string(&data["Code"]).filter(|s| !s.is_empty()),
      sms_content: Self::json_string(&data["SmsContent"]),
      status: Self::json_u32(&data["Status"]).unwrap_or(0),
      service_id: Self::json_u32(&data["ServiceID"]),
      service_name: Self::json_string(&data["ServiceName"]),
      is_sound: Self::parse_boolish(&data["IsSound"]),
      phone_original: Self::json_string(&data["PhoneOriginal"]),
      country_iso: Self::json_string(&data["CountryISO"]),
      country_code: Self::json_string(&data["CountryCode"]),
      created_time: Self::json_string(&data["CreatedTime"]),
    }
  }

  fn parse_history_entry(data: &serde_json::Value) -> HistoryEntry {
    HistoryEntry {
      id: Self::json_string(&data["ID"])
        .or_else(|| Self::json_string(&data["Id"]))
        .unwrap_or_default(),
      service_id: Self::json_u32(&data["ServiceID"]),
      service_name: Self::json_string(&data["ServiceName"]),
      status: Self::json_u32(&data["Status"]).unwrap_or(0),
      price: Self::json_f64(&data["Price"]),
      phone: Self::json_string(&data["Phone"]).unwrap_or_default(),
      code: Self::json_string(&data["Code"]).filter(|s| !s.is_empty()),
      sms_content: Self::json_string(&data["SmsContent"]),
      is_sound: Self::parse_boolish(&data["IsSound"]),
      phone_original: Self::json_string(&data["PhoneOriginal"]),
      country_iso: Self::json_string(&data["CountryISO"]),
      country_code: Self::json_string(&data["CountryCode"]),
      created_time: Self::json_string(&data["CreatedTime"]),
    }
  }
}

impl SmsService for ViotpService {
  fn get_balance(&self) -> Result<f64, SmsServiceError> {
    let data = self.get_json("/users/balance", &[])?;
    let balance = data
      .get("data")
      .and_then(|d| d.get("balance"))
      .and_then(Self::json_f64)
      .ok_or_else(|| SmsServiceError::Internal("missing balance in response".into()))?;
    Ok(balance)
  }

  fn get_networks(&self, _country: Option<&str>) -> Result<Vec<SmsNetwork>, SmsServiceError> {
    let data = self.get_json("/networks/get", &[])?;
    let arr = data
      .get("data")
      .and_then(|v| v.as_array())
      .ok_or_else(|| SmsServiceError::Internal("missing networks data".into()))?;

    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
      let id = Self::json_u32(&item["id"]).unwrap_or(0);
      let name = Self::json_string(&item["name"]).unwrap_or_default();
      if !name.is_empty() {
        out.push(SmsNetwork { id, name });
      }
    }
    Ok(out)
  }

  fn get_services(&self, country: Option<&str>) -> Result<Vec<SmsServiceInfo>, SmsServiceError> {
    let mut query = Vec::new();
    if let Some(c) = country {
      if !c.is_empty() {
        query.push(("country", c.to_string()));
      }
    }
    let data = self.get_json("/service/getv2", &query)?;
    let arr = data
      .get("data")
      .and_then(|v| v.as_array())
      .ok_or_else(|| SmsServiceError::Internal("missing services data".into()))?;

    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
      let id = Self::json_u32(&item["id"]).unwrap_or(0);
      let name = Self::json_string(&item["name"]).unwrap_or_default();
      let price = Self::json_f64(&item["price"]).unwrap_or(0.0);
      if id > 0 && !name.is_empty() {
        out.push(SmsServiceInfo { id, name, price });
      }
    }
    Ok(out)
  }

  fn request_number(&self, request: &NumberRequest) -> Result<NumberInfo, SmsServiceError> {
    if request.service_id == 0 {
      return Err(SmsServiceError::InvalidRequest(
        "serviceId is required".into(),
      ));
    }

    let mut query = vec![("serviceId", request.service_id.to_string())];
    if let Some(ref network) = request.network {
      if !network.is_empty() {
        query.push(("network", network.clone()));
      }
    }
    if let Some(ref prefix) = request.prefix {
      if !prefix.is_empty() {
        query.push(("prefix", prefix.clone()));
      }
    }
    if let Some(ref except) = request.except_prefix {
      if !except.is_empty() {
        query.push(("exceptPrefix", except.clone()));
      }
    }
    if let Some(ref number) = request.number {
      if !number.is_empty() {
        query.push(("number", number.clone()));
      }
    }
    if let Some(ref country) = request.country {
      if !country.is_empty() {
        query.push(("country", country.clone()));
      }
    }

    let data = self.get_json("/request/getv2", &query)?;
    let body = data
      .get("data")
      .ok_or_else(|| SmsServiceError::Internal("missing request data".into()))?;

    let phone_number = Self::json_string(&body["phone_number"]).unwrap_or_default();
    let request_id = Self::json_string(&body["request_id"]).unwrap_or_default();
    if phone_number.is_empty() || request_id.is_empty() {
      return Err(SmsServiceError::Internal(
        "missing phone_number or request_id".into(),
      ));
    }

    Ok(NumberInfo {
      phone_number,
      re_phone_number: Self::json_string(&body["re_phone_number"]),
      request_id,
      balance: Self::json_f64(&body["balance"]),
      country_iso: Self::json_string(&body["countryISO"]),
      country_code: Self::json_string(&body["countryCode"]),
    })
  }

  fn get_otp(&self, request_id: &str, timeout_secs: u64) -> Result<OtpInfo, SmsServiceError> {
    if request_id.trim().is_empty() {
      return Err(SmsServiceError::InvalidRequest(
        "requestId is required".into(),
      ));
    }

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    let query = vec![("requestId", request_id.to_string())];

    loop {
      if Instant::now() >= deadline {
        return Err(SmsServiceError::Timeout(format!(
          "OTP not received within {timeout_secs}s for request {request_id}"
        )));
      }

      match self.get_json("/session/getv2", &query) {
        Ok(data) => {
          if let Some(body) = data.get("data") {
            let otp = Self::parse_otp_data(body);
            // Status: 1 completed, 0 waiting, 2 expired
            if otp.status == 1 {
              if otp.code.as_ref().is_some_and(|c| !c.is_empty()) {
                return Ok(otp);
              }
              // Completed without code — keep polling briefly
            } else if otp.status == 2 {
              return Err(SmsServiceError::Timeout(format!(
                "OTP session expired for request {request_id}"
              )));
            }
          }
        }
        Err(SmsServiceError::Network(_)) => {
          // Transient network error — retry until deadline
        }
        Err(e) => return Err(e),
      }

      let remaining = deadline.saturating_duration_since(Instant::now());
      let sleep_for = std::cmp::min(Duration::from_secs(POLL_INTERVAL_SECS), remaining);
      if sleep_for.is_zero() {
        return Err(SmsServiceError::Timeout(format!(
          "OTP not received within {timeout_secs}s for request {request_id}"
        )));
      }
      std::thread::sleep(sleep_for);
    }
  }

  fn get_history(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>, SmsServiceError> {
    let mut params = Vec::new();
    if let Some(service) = query.service {
      params.push(("service", service.to_string()));
    }
    if let Some(status) = query.status {
      params.push(("status", status.to_string()));
    }
    if let Some(limit) = query.limit {
      params.push(("limit", limit.to_string()));
    }
    if let Some(ref from) = query.from_date {
      if !from.is_empty() {
        params.push(("fromDate", from.clone()));
      }
    }
    if let Some(ref to) = query.to_date {
      if !to.is_empty() {
        params.push(("toDate", to.clone()));
      }
    }

    let data = self.get_json("/session/historyv2", &params)?;
    let arr = data
      .get("data")
      .and_then(|v| v.as_array())
      .ok_or_else(|| SmsServiceError::Internal("missing history data".into()))?;

    Ok(arr.iter().map(Self::parse_history_entry).collect())
  }

  fn check_health(&self) -> bool {
    self.get_balance().is_ok()
  }
}

// --- Lightweight response shapes used only by unit tests / docs ---
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ViotpEnvelope<T> {
  status_code: i64,
  success: bool,
  message: String,
  data: T,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn map_status_success_is_none() {
    assert!(ViotpService::map_status(200, "ok").is_none());
  }

  #[test]
  fn map_status_auth() {
    match ViotpService::map_status(401, "bad token") {
      Some(SmsServiceError::Auth(_)) => {}
      other => panic!("expected Auth, got {other:?}"),
    }
  }

  #[test]
  fn map_status_balance() {
    match ViotpService::map_status(-2, "no money") {
      Some(SmsServiceError::InsufficientBalance(_)) => {}
      other => panic!("expected InsufficientBalance, got {other:?}"),
    }
  }

  #[test]
  fn parse_otp_completed_with_code() {
    let data = serde_json::json!({
      "ID": 58098,
      "ServiceID": 1,
      "ServiceName": "Momo",
      "Status": 1,
      "Phone": "987654321",
      "SmsContent": "486460 la ma xac thuc",
      "IsSound": "false",
      "CreatedTime": "2020-08-06T17:13:24.88",
      "Code": "486460",
      "PhoneOriginal": "0987654321",
      "CountryISO": "VN",
      "CountryCode": "84"
    });
    let otp = ViotpService::parse_otp_data(&data);
    assert_eq!(otp.status, 1);
    assert_eq!(otp.code.as_deref(), Some("486460"));
    assert_eq!(otp.phone, "987654321");
    assert!(!otp.is_sound);
  }

  #[test]
  fn parse_history_entry_price_as_string() {
    let data = serde_json::json!({
      "ID": "123",
      "ServiceID": 1,
      "ServiceName": "Facebook",
      "Status": 1,
      "Price": "600",
      "Phone": "938558302",
      "IsSound": false,
      "Code": "123456"
    });
    let entry = ViotpService::parse_history_entry(&data);
    assert_eq!(entry.price, Some(600.0));
    assert_eq!(entry.id, "123");
  }
}
