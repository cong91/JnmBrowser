//! Email provider: https://sms.iosmq.xyz (MAIL card redeem + order lookup)
//!
//! Public card flow (no HMAC):
//! 1. `POST /api/v1/redeem` body `{"code":"MAIL-…"}` → assign mailbox
//! 2. `GET  /api/v1/order/lookup?code=MAIL-…&poll=true` → session + OTP codes
//!
//! One MAIL card provides a base mailbox; up to six +aliases use the same card lookup.
//! Code `2004` ("卡密使用中") means the card is already redeemed — look up instead.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{
  EmailAliasGenerator, EmailInfo, EmailService, EmailServiceError, MAX_ALIASES_PER_EMAIL,
};

const API_BASE: &str = "https://sms.iosmq.xyz";
const POLL_INTERVAL_SECS: u64 = 3;

/// Email service for `sms.iosmq.xyz`.
pub struct SmsIosmqService {
  api_base: String,
  poll_interval: Duration,
  /// card code → last known mailbox
  mailboxes: Mutex<HashMap<String, String>>,
  /// Shared +alias generator and six-account budget.
  aliases: EmailAliasGenerator,
  /// card code → OTP codes already returned to a registration attempt
  seen_codes: Mutex<HashMap<String, HashSet<String>>>,
}

impl SmsIosmqService {
  pub fn new() -> Self {
    Self::with_api_base(API_BASE, Duration::from_secs(POLL_INTERVAL_SECS))
  }

  fn with_api_base(api_base: impl Into<String>, poll_interval: Duration) -> Self {
    let api_base = api_base.into();
    Self {
      api_base: api_base.trim_end_matches('/').to_string(),
      poll_interval,
      mailboxes: Mutex::new(HashMap::new()),
      aliases: EmailAliasGenerator::default(),
      seen_codes: Mutex::new(HashMap::new()),
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
          .expect("sms.iosmq.xyz nested runtime thread panicked")
      }),
      Err(_) => {
        let rt = tokio::runtime::Builder::new_current_thread()
          .enable_all()
          .build()
          .expect("Failed to create sms.iosmq.xyz runtime");
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
      remaining_uses: Some(MAX_ALIASES_PER_EMAIL as u32),
    })
  }

  fn extract_email_info_if_present(
    data: &serde_json::Value,
  ) -> Result<Option<EmailInfo>, EmailServiceError> {
    match Self::extract_email_info(data) {
      Ok(info) => Ok(Some(info)),
      Err(EmailServiceError::EmailInvalid(_)) => Ok(None),
      Err(error) => Err(error),
    }
  }

  fn extract_codes(data: &serde_json::Value) -> Vec<(String, Option<String>, Option<i64>)> {
    // lookup: data.email.codes[{code,receivedAt,id}]
    let arrays = [
      data.pointer("/data/email/codes"),
      data.pointer("/data/codes"),
      data.get("codes"),
    ];
    let mut out = Vec::new();
    for arr in arrays.into_iter().flatten() {
      if let Some(list) = arr.as_array() {
        for item in list {
          let code = item
            .get("code")
            .and_then(|v| v.as_str())
            .or_else(|| item.as_str())
            .map(str::trim)
            .filter(|code| !code.is_empty());
          let Some(code) = code else { continue };
          let received_at = item
            .get("receivedAt")
            .and_then(|v| v.as_str())
            .map(str::to_string);
          let id = item.get("id").and_then(|v| v.as_i64());
          out.push((code.to_string(), received_at, id));
        }
      }
    }
    out
  }

  fn code_identity(code: &(String, Option<String>, Option<i64>)) -> String {
    if let Some(id) = code.2 {
      format!("id:{id}")
    } else if let Some(received_at) = &code.1 {
      format!("received:{received_at}:{}", code.0)
    } else {
      format!("code:{}", code.0)
    }
  }

  fn newest_unseen_code(
    &self,
    cdk: &str,
    codes: &[(String, Option<String>, Option<i64>)],
  ) -> Option<String> {
    let mut seen_codes = self.seen_codes.lock().unwrap();
    let seen = seen_codes.entry(cdk.to_string()).or_default();
    if let Some(code) = codes
      .iter()
      .filter(|code| !seen.contains(&Self::code_identity(code)))
      .max_by(|(_, received_a, id_a), (_, received_b, id_b)| {
        received_a.cmp(received_b).then_with(|| id_a.cmp(id_b))
      })
    {
      seen.insert(Self::code_identity(code));
      return Some(code.0.clone());
    }
    None
  }

  fn remember_codes(&self, cdk: &str, codes: &[(String, Option<String>, Option<i64>)]) {
    let mut seen_codes = self.seen_codes.lock().unwrap();
    let seen = seen_codes.entry(cdk.to_string()).or_default();
    seen.extend(codes.iter().map(Self::code_identity));
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
  ///
  /// The redeem response only confirms the card claim on some API versions;
  /// the mailbox is then returned by the follow-up lookup.
  fn redeem_card(&self, code: &str) -> Result<serde_json::Value, EmailServiceError> {
    let url = format!("{}/api/v1/redeem", self.api_base);
    let body = serde_json::json!({ "code": code });
    let data = Self::http_post_json(url, body)?;
    let biz = Self::biz_code(&data);
    if biz == 0 {
      return Ok(data);
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
      "{}/api/v1/order/lookup?code={}&poll=true",
      self.api_base,
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

impl EmailService for SmsIosmqService {
  fn redeem_cdk(&self, cdk: &str) -> Result<EmailInfo, EmailServiceError> {
    let code = cdk.trim();
    if code.is_empty() {
      return Err(EmailServiceError::CdkInvalid("empty MAIL card code".into()));
    }

    // 1) Try redeem (first use).
    match self.redeem_card(code) {
      Ok(data) => {
        // Some redeem responses only contain `{ data: { type: "email" } }`.
        // Resolve the assigned mailbox from lookup in that case.
        if let Some(info) = Self::extract_email_info_if_present(&data)? {
          self.remember(code, &info.email);
          self.remember_codes(code, &Self::extract_codes(&data));
          return Ok(info);
        }
      }
      Err(EmailServiceError::Internal(msg)) if msg == "CARD_IN_USE" => {
        // Card already active — pull mailbox via lookup.
      }
      Err(e) => return Err(e),
    }

    let data = self.lookup_order(code)?;
    let info = Self::extract_email_info(&data)?;
    self.remember(code, &info.email);
    self.remember_codes(code, &Self::extract_codes(&data));
    Ok(info)
  }

  fn generate_alias(&self, base_email: &str) -> Result<String, EmailServiceError> {
    self.aliases.generate(base_email)
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
          if let Some(otp) = self.newest_unseen_code(code, &codes) {
            return Ok(otp);
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
      let sleep_for = std::cmp::min(self.poll_interval, remaining);
      std::thread::sleep(sleep_for);
    }
  }

  fn check_health(&self) -> bool {
    let url = format!(
      "{}/api/v1/order/lookup?code=health-check&poll=false",
      self.api_base
    );
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

impl Default for SmsIosmqService {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;
  use wiremock::matchers::{body_json, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn redeem_contract_falls_back_to_lookup_for_active_card() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/redeem"))
      .and(body_json(serde_json::json!({ "code": "MAIL-TEST" })))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "code": 2004,
        "msg": "card already in use"
      })))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("GET"))
      .and(path("/api/v1/order/lookup"))
      .and(query_param("code", "MAIL-TEST"))
      .and(query_param("poll", "true"))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "code": 0,
        "data": {
          "email": {
            "session": {
              "emailAddress": "user@gmail.com",
              "status": "active"
            },
            "codes": []
          }
        }
      })))
      .expect(1)
      .mount(&server)
      .await;

    let service = SmsIosmqService::with_api_base(server.uri(), Duration::ZERO);
    let info = service.redeem_cdk(" MAIL-TEST ").unwrap();
    assert_eq!(info.email, "user@gmail.com");
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn redeem_claim_without_mailbox_uses_lookup_contract() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/redeem"))
      .and(body_json(serde_json::json!({ "code": "MAIL-TEST" })))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "code": 0,
        "data": { "type": "email" }
      })))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("GET"))
      .and(path("/api/v1/order/lookup"))
      .and(query_param("code", "MAIL-TEST"))
      .and(query_param("poll", "true"))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "code": 0,
        "data": {
          "email": {
            "session": {
              "emailAddress": "user@gmail.com",
              "status": "active"
            },
            "codes": []
          }
        }
      })))
      .expect(1)
      .mount(&server)
      .await;

    let service = SmsIosmqService::with_api_base(server.uri(), Duration::ZERO);
    let info = service.redeem_cdk("MAIL-TEST").unwrap();
    assert_eq!(info.email, "user@gmail.com");
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn polling_skips_baseline_and_returns_only_new_otp() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/redeem"))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "code": 2004,
        "msg": "card already in use"
      })))
      .expect(1)
      .mount(&server)
      .await;

    let lookup_count = Arc::new(AtomicUsize::new(0));
    let responder_count = lookup_count.clone();
    Mock::given(method("GET"))
      .and(path("/api/v1/order/lookup"))
      .and(query_param("code", "MAIL-TEST"))
      .and(query_param("poll", "true"))
      .respond_with(move |_request: &wiremock::Request| {
        let call = responder_count.fetch_add(1, Ordering::SeqCst);
        let codes = if call == 0 {
          serde_json::json!([
            { "id": 1, "code": "111111", "receivedAt": "2026-07-20T01:00:00" }
          ])
        } else {
          serde_json::json!([
            { "id": 1, "code": "111111", "receivedAt": "2026-07-20T01:00:00" },
            { "id": 2, "code": "222222", "receivedAt": "2026-07-20T01:01:00" }
          ])
        };
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
          "code": 0,
          "data": {
            "email": {
              "session": {
                "emailAddress": "user@gmail.com",
                "status": "active"
              },
              "codes": codes
            }
          }
        }))
      })
      .expect(2)
      .mount(&server)
      .await;

    let service = SmsIosmqService::with_api_base(server.uri(), Duration::ZERO);
    service.redeem_cdk("MAIL-TEST").unwrap();
    assert_eq!(
      service.poll_verification_code("MAIL-TEST", 1).unwrap(),
      "222222"
    );
  }

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
    let info = SmsIosmqService::extract_email_info(&data).unwrap();
    assert_eq!(info.email, "simonharris91581@gmail.com");
    assert_eq!(info.status, "active");
    assert_eq!(info.expires_at.as_deref(), Some("2026-07-19T18:10:13"));
  }

  #[test]
  fn redeem_claim_response_defers_mailbox_lookup() {
    let data = serde_json::json!({
      "code": 0,
      "data": { "type": "email" }
    });
    assert!(SmsIosmqService::extract_email_info_if_present(&data)
      .unwrap()
      .is_none());
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
    let codes = SmsIosmqService::extract_codes(&data);
    assert_eq!(codes[0].0, "288990");
  }

  #[test]
  fn newest_code_uses_received_time_and_skips_seen_codes() {
    let svc = SmsIosmqService::new();
    let codes = vec![
      (
        "097358".into(),
        Some("2026-07-20T02:21:30".into()),
        Some(7645),
      ),
      (
        "635779".into(),
        Some("2026-07-20T02:33:28".into()),
        Some(7646),
      ),
    ];
    assert_eq!(
      svc.newest_unseen_code("MAIL-TEST", &codes).as_deref(),
      Some("635779")
    );
    assert_eq!(
      svc.newest_unseen_code("MAIL-TEST", &codes).as_deref(),
      Some("097358")
    );
    assert_eq!(svc.newest_unseen_code("MAIL-TEST", &codes), None);
  }

  #[test]
  fn existing_codes_are_baselined_before_polling() {
    let svc = SmsIosmqService::new();
    let existing = vec![(
      "097358".into(),
      Some("2026-07-20T02:21:30".into()),
      Some(7645),
    )];
    svc.remember_codes("MAIL-TEST", &existing);
    assert_eq!(svc.newest_unseen_code("MAIL-TEST", &existing), None);

    let with_new = vec![
      existing[0].clone(),
      (
        "635779".into(),
        Some("2026-07-20T02:33:28".into()),
        Some(7646),
      ),
    ];
    assert_eq!(
      svc.newest_unseen_code("MAIL-TEST", &with_new).as_deref(),
      Some("635779")
    );
  }

  #[test]
  fn message_id_distinguishes_repeated_otp_values() {
    let svc = SmsIosmqService::new();
    let codes = vec![
      (
        "288990".into(),
        Some("2026-07-20T02:21:30".into()),
        Some(7645),
      ),
      (
        "288990".into(),
        Some("2026-07-20T02:21:30".into()),
        Some(7646),
      ),
    ];
    assert_eq!(
      svc.newest_unseen_code("MAIL-TEST", &codes).as_deref(),
      Some("288990")
    );
    assert_eq!(
      svc.newest_unseen_code("MAIL-TEST", &codes).as_deref(),
      Some("288990")
    );
    assert_eq!(svc.newest_unseen_code("MAIL-TEST", &codes), None);
  }

  #[test]
  fn generate_alias_uses_base_domain_and_has_six_slot_limit() {
    let svc = SmsIosmqService::new();
    let base = "brockbernard31156@gmail.com";
    let aliases: Vec<_> = (0..6).map(|_| svc.generate_alias(base).unwrap()).collect();
    assert!(aliases
      .iter()
      .all(|alias| alias.starts_with("brockbernard31156+")));
    assert!(aliases.iter().all(|alias| alias.ends_with("@gmail.com")));
    assert_eq!(
      svc.generate_alias(base).unwrap_err().to_string(),
      "Email invalid: maximum 6 aliases already used for brockbernard31156@gmail.com"
    );
  }
}
