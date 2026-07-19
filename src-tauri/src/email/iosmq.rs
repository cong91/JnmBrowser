//! Email provider: https://sms.iosmq.xyz (MAIL card redeem + order lookup)
//!
//! Public card flow (no HMAC):
//! 1. `POST /api/v1/redeem` body `{"code":"MAIL-…"}` → assign mailbox
//! 2. `GET  /api/v1/order/lookup?code=MAIL-…&poll=true` → session + OTP codes
//!
//! One MAIL card = one mailbox. No Gmail-style +aliases.
//! Code `2004` ("卡密使用中") means the card is already redeemed — look up instead.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{EmailInfo, EmailService, EmailServiceError};

const API_BASE: &str = "https://sms.iosmq.xyz";
const POLL_INTERVAL_SECS: u64 = 3;

/// iosmq MAIL-card email service (redeem + order/lookup).
pub struct IosmqMailService {
  /// card code → last known mailbox
  mailboxes: Mutex<HashMap<String, String>>,
}

impl IosmqMailService {
  pub fn new() -> Self {
    Self {
      mailboxes: Mutex::new(HashMap::new()),
    }
  }

  fn block_on<F, T>(future: F) -> T
  where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
  {
    match tokio::runtime::Handle::try_current() {
      Ok(handle) => std::thread::scope(|s| {
        s.spawn(move || handle.block_on(future))
          .join()
          .expect("iosmq nested runtime thread panicked")
      }),
      Err(_) => {
        let rt = tokio::runtime::Builder::new_current_thread()
          .enable_all()
          .build()
          .expect("Failed to create iosmq runtime");
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

  fn map_biz_error(code: i64, msg: &str) -> EmailServiceError {
    match code {
      1001 => EmailServiceError::Internal(format!("bad request: {msg}")),
      1002 => EmailServiceError::Internal(format!("not found: {msg}")),
      2001 => EmailServiceError::CdkInvalid(format!("card not found: {msg}")),
      2002 => EmailServiceError::CdkInvalid(format!("quota exhausted: {msg}")),
      2003 => EmailServiceError::CdkExpired(format!("card expired: {msg}")),
      // 2004 = already in use — handled specially by redeem.
      2004 => EmailServiceError::Internal(format!("card already in use: {msg}")),
      2005 => EmailServiceError::CdkInvalid(format!("card status invalid: {msg}")),
      _ => EmailServiceError::Internal(format!("iosmq code {code}: {msg}")),
    }
  }

  fn parse_json(text: &str) -> Result<serde_json::Value, EmailServiceError> {
    serde_json::from_str(text).map_err(|e| {
      EmailServiceError::Internal(format!("failed to parse iosmq response: {e} — {text}"))
    })
  }

  fn biz_code(data: &serde_json::Value) -> i64 {
    data["code"].as_i64().unwrap_or(-1)
  }

  fn biz_msg(data: &serde_json::Value) -> String {
    data["msg"]
      .as_str()
      .or_else(|| data["message"].as_str())
      .unwrap_or("unknown error")
      .to_string()
  }

  /// Extract mailbox fields from either redeem or lookup payload shapes.
  fn extract_email_info(data: &serde_json::Value) -> Result<EmailInfo, EmailServiceError> {
    // lookup shape: data.email.session.{emailAddress,status,expiresAt}
    // redeem shape may be:
    //   data.email / data.emailAddress
    //   data.session.emailAddress
    //   data.email.session.emailAddress
    let session = data
      .pointer("/data/email/session")
      .or_else(|| data.pointer("/data/session"))
      .or_else(|| data.get("data"));

    let email = session
      .and_then(|s| {
        s.get("emailAddress")
          .or_else(|| s.get("email"))
          .and_then(|v| v.as_str())
      })
      .or_else(|| data.pointer("/data/emailAddress").and_then(|v| v.as_str()))
      .or_else(|| data.pointer("/data/email").and_then(|v| v.as_str()))
      .unwrap_or("")
      .trim()
      .to_string();

    if email.is_empty() {
      return Err(EmailServiceError::EmailInvalid(
        "iosmq response missing emailAddress".into(),
      ));
    }

    let status = session
      .and_then(|s| s.get("status").and_then(|v| v.as_str()))
      .unwrap_or("active")
      .to_string();
    let expires_at = session
      .and_then(|s| s.get("expiresAt").and_then(|v| v.as_str()))
      .map(|s| s.to_string());

    Ok(EmailInfo {
      email,
      expires_at,
      status,
      remaining_uses: Some(1),
    })
  }

  fn extract_codes(data: &serde_json::Value) -> Vec<String> {
    // lookup: data.email.codes[{code}]
    let arrays = [
      data.pointer("/data/email/codes"),
      data.pointer("/data/codes"),
      data.get("codes"),
    ];
    let mut out = Vec::new();
    for arr in arrays.into_iter().flatten() {
      if let Some(list) = arr.as_array() {
        for item in list {
          if let Some(code) = item.get("code").and_then(|v| v.as_str()) {
            let code = code.trim();
            if !code.is_empty() {
              out.push(code.to_string());
            }
          } else if let Some(code) = item.as_str() {
            let code = code.trim();
            if !code.is_empty() {
              out.push(code.to_string());
            }
          }
        }
      }
    }
    out
  }

  fn http_get_json(url: String) -> Result<serde_json::Value, EmailServiceError> {
    let client = Self::client();
    let url_for_req = url.clone();
    let response = Self::block_on(async move {
      client
        .get(&url_for_req)
        .header("accept", "application/json")
        .send()
        .await
    })
    .map_err(|e| EmailServiceError::Network(format!("GET {url} failed: {e}")))?;

    let status = response.status();
    let text = Self::block_on(async move { response.text().await })
      .map_err(|e| EmailServiceError::Network(format!("failed to read GET body: {e}")))?;
    if !status.is_success() {
      return Err(EmailServiceError::Network(format!(
        "GET HTTP {status}: {text}"
      )));
    }
    Self::parse_json(&text)
  }

  fn http_post_json(
    url: String,
    body: serde_json::Value,
  ) -> Result<serde_json::Value, EmailServiceError> {
    let client = Self::client();
    let url_for_req = url.clone();
    let response = Self::block_on(async move {
      client
        .post(&url_for_req)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    })
    .map_err(|e| EmailServiceError::Network(format!("POST {url} failed: {e}")))?;

    let status = response.status();
    let text = Self::block_on(async move { response.text().await })
      .map_err(|e| EmailServiceError::Network(format!("failed to read POST body: {e}")))?;
    // Business errors often still return HTTP 200 with code != 0.
    // Non-2xx is still a hard network/server failure.
    if !status.is_success() {
      return Err(EmailServiceError::Network(format!(
        "POST HTTP {status}: {text}"
      )));
    }
    Self::parse_json(&text)
  }

  /// `POST /api/v1/redeem` — first claim of a MAIL card.
  fn redeem_card(&self, code: &str) -> Result<EmailInfo, EmailServiceError> {
    let url = format!("{API_BASE}/api/v1/redeem");
    let body = serde_json::json!({ "code": code });
    let data = Self::http_post_json(url, body)?;
    let biz = Self::biz_code(&data);
    if biz == 0 {
      return Self::extract_email_info(&data);
    }
    // 2004 = already in use → caller should fall back to lookup.
    if biz == 2004 {
      return Err(EmailServiceError::Internal("CARD_IN_USE".into()));
    }
    Err(Self::map_biz_error(biz, &Self::biz_msg(&data)))
  }

  /// `GET /api/v1/order/lookup?code=…&poll=true`
  fn lookup_order(&self, code: &str) -> Result<serde_json::Value, EmailServiceError> {
    let url = format!(
      "{API_BASE}/api/v1/order/lookup?code={}&poll=true",
      urlencoding::encode(code)
    );
    let data = Self::http_get_json(url)?;
    let biz = Self::biz_code(&data);
    if biz != 0 {
      return Err(Self::map_biz_error(biz, &Self::biz_msg(&data)));
    }
    Ok(data)
  }

  fn remember(&self, code: &str, email: &str) {
    self
      .mailboxes
      .lock()
      .unwrap()
      .insert(code.to_string(), email.to_string());
  }
}

impl EmailService for IosmqMailService {
  fn redeem_cdk(&self, cdk: &str) -> Result<EmailInfo, EmailServiceError> {
    let code = cdk.trim();
    if code.is_empty() {
      return Err(EmailServiceError::CdkInvalid("empty MAIL card code".into()));
    }

    // 1) Try redeem (first use).
    match self.redeem_card(code) {
      Ok(info) => {
        self.remember(code, &info.email);
        return Ok(info);
      }
      Err(EmailServiceError::Internal(msg)) if msg == "CARD_IN_USE" => {
        // 2) Card already active — pull mailbox via lookup.
      }
      Err(e) => return Err(e),
    }

    let data = self.lookup_order(code)?;
    let info = Self::extract_email_info(&data)?;
    self.remember(code, &info.email);
    Ok(info)
  }

  fn generate_alias(&self, base_email: &str) -> Result<String, EmailServiceError> {
    // One MAIL card = one assigned mailbox. No +alias support.
    let email = base_email.trim();
    if email.is_empty() {
      return Err(EmailServiceError::EmailInvalid(
        "iosmq base email is empty".into(),
      ));
    }
    Ok(email.to_string())
  }

  fn poll_verification_code(
    &self,
    cdk: &str,
    timeout_secs: u64,
  ) -> Result<String, EmailServiceError> {
    let code = cdk.trim();
    if code.is_empty() {
      return Err(EmailServiceError::CdkInvalid("empty MAIL card code".into()));
    }
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    loop {
      if Instant::now() >= deadline {
        return Err(EmailServiceError::Timeout(format!(
          "iosmq OTP not received within {timeout_secs}s for {code}"
        )));
      }

      match self.lookup_order(code) {
        Ok(data) => {
          // Keep mailbox cache fresh if present.
          if let Ok(info) = Self::extract_email_info(&data) {
            self.remember(code, &info.email);
          }
          let codes = Self::extract_codes(&data);
          if let Some(otp) = codes.last() {
            if !otp.is_empty() {
              return Ok(otp.clone());
            }
          }
        }
        Err(e) => {
          if Instant::now() >= deadline {
            return Err(e);
          }
          // Transient — keep polling.
        }
      }

      let remaining = deadline.saturating_duration_since(Instant::now());
      let sleep_for = std::cmp::min(Duration::from_secs(POLL_INTERVAL_SECS), remaining);
      std::thread::sleep(sleep_for);
    }
  }

  fn check_health(&self) -> bool {
    let url = format!("{API_BASE}/api/v1/order/lookup?code=health-check&poll=false");
    let client = Self::client();
    let result: Result<reqwest::Response, reqwest::Error> = Self::block_on(async move {
      client
        .get(&url)
        .header("accept", "application/json")
        .timeout(Duration::from_secs(10))
        .send()
        .await
    });
    result.is_ok()
  }
}

impl Default for IosmqMailService {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn extract_email_from_lookup_shape() {
    let data = serde_json::json!({
      "code": 0,
      "data": {
        "type": "email",
        "email": {
          "session": {
            "sessionId": 1244,
            "emailAddress": "simonharris91581@gmail.com",
            "productName": "ChatGPT",
            "status": "active",
            "expiresAt": "2026-07-19T18:10:13"
          },
          "codes": []
        }
      }
    });
    let info = IosmqMailService::extract_email_info(&data).unwrap();
    assert_eq!(info.email, "simonharris91581@gmail.com");
    assert_eq!(info.status, "active");
    assert_eq!(info.expires_at.as_deref(), Some("2026-07-19T18:10:13"));
  }

  #[test]
  fn extract_otp_from_lookup_shape() {
    let data = serde_json::json!({
      "code": 0,
      "data": {
        "type": "email",
        "email": {
          "session": {
            "sessionId": 1244,
            "emailAddress": "simonharris91581@gmail.com",
            "status": "active"
          },
          "codes": [
            { "id": 7619, "code": "288990", "receivedAt": "2026-07-19T14:23:27" }
          ]
        }
      }
    });
    let codes = IosmqMailService::extract_codes(&data);
    assert_eq!(codes, vec!["288990".to_string()]);
  }

  #[test]
  fn generate_alias_is_identity() {
    let svc = IosmqMailService::new();
    let alias = svc.generate_alias("simonharris91581@gmail.com").unwrap();
    assert_eq!(alias, "simonharris91581@gmail.com");
  }
}
