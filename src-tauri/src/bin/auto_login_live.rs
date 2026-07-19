//! Live OpenAI/Codex auto-login smoke runner.
//!
//! Usage (repo root):
//! ```text
//! pnpm copy-proxy-binary
//! cargo run --manifest-path src-tauri/Cargo.toml --bin auto-login-live -- \
//!   --credential 'user@x.com|pass|TOTP' \
//!   --browser chromium \
//!   --sms-token VIOTP_TOKEN \
//!   --sms-service-id 1234 \
//!   --sms-network VINAPHONE \
//!   --max-retries 1
//! ```
//!
//! SMS is only rented if OpenAI shows the phone page. Prefer VINAPHONE + OpenAI|ChatGPT (id 1234).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use donutbrowser_lib::auto_service::openai::login::engine::LoginEngine;
use donutbrowser_lib::auto_service::openai::login::types::{
  LoginConfig, LoginCredential, LoginNetworkMode,
};
use donutbrowser_lib::sms::viotp::ViotpService;
use donutbrowser_lib::sms::SmsService;

struct LiveArgs {
  credential: String,
  browser: String,
  max_retries: u32,
  headless: bool,
  sms_token: Option<String>,
  sms_service_id: Option<u32>,
  sms_network: Option<String>,
  sms_country: String,
  proxy_id: Option<String>,
}

fn parse_args() -> LiveArgs {
  let mut credential = std::env::var("AUTO_LOGIN_CREDENTIAL").unwrap_or_default();
  let mut browser = std::env::var("AUTO_LOGIN_BROWSER").unwrap_or_else(|_| "chromium".into());
  let mut max_retries = 1u32;
  let mut headless = false;
  let mut sms_token = std::env::var("AUTO_LOGIN_SMS_TOKEN").ok();
  let mut sms_service_id = std::env::var("AUTO_LOGIN_SMS_SERVICE_ID")
    .ok()
    .and_then(|s| s.parse().ok());
  let mut sms_network = std::env::var("AUTO_LOGIN_SMS_NETWORK").ok();
  let mut sms_country = std::env::var("AUTO_LOGIN_SMS_COUNTRY").unwrap_or_else(|_| "vn".into());
  let mut proxy_id = std::env::var("AUTO_LOGIN_PROXY_ID").ok();

  let mut args = std::env::args().skip(1);
  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--credential" => credential = args.next().unwrap_or_default(),
      "--browser" => browser = args.next().unwrap_or(browser),
      "--max-retries" => {
        max_retries = args
          .next()
          .and_then(|v| v.parse().ok())
          .unwrap_or(max_retries);
      }
      "--headless" => headless = true,
      "--sms-token" => sms_token = args.next(),
      "--sms-service-id" => {
        sms_service_id = args.next().and_then(|v| v.parse().ok());
      }
      "--sms-network" => sms_network = args.next(),
      "--sms-country" => sms_country = args.next().unwrap_or(sms_country),
      "--proxy-id" => proxy_id = args.next(),
      other if other.starts_with("--credential=") => {
        credential = other.trim_start_matches("--credential=").to_string();
      }
      other if other.starts_with("--browser=") => {
        browser = other.trim_start_matches("--browser=").to_string();
      }
      other if other.starts_with("--sms-token=") => {
        sms_token = Some(other.trim_start_matches("--sms-token=").to_string());
      }
      other if other.starts_with("--sms-service-id=") => {
        sms_service_id = other.trim_start_matches("--sms-service-id=").parse().ok();
      }
      other if other.starts_with("--sms-network=") => {
        sms_network = Some(other.trim_start_matches("--sms-network=").to_string());
      }
      other if other.starts_with("--proxy-id=") => {
        proxy_id = Some(other.trim_start_matches("--proxy-id=").to_string());
      }
      "--help" | "-h" => {
        eprintln!(
          "Usage: auto-login-live --credential 'email|pass|totp' [--browser chromium] \
           [--sms-token TOKEN] [--sms-service-id 1234] [--sms-network VINAPHONE] [--max-retries 1]"
        );
        std::process::exit(0);
      }
      other => {
        eprintln!("Unknown arg: {other}");
        std::process::exit(2);
      }
    }
  }

  if credential.trim().is_empty() {
    eprintln!("Missing --credential EMAIL|PASSWORD|2FA");
    std::process::exit(2);
  }

  LiveArgs {
    credential,
    browser,
    max_retries,
    headless,
    sms_token,
    sms_service_id,
    sms_network,
    sms_country,
    proxy_id,
  }
}

fn main() {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
  let args = parse_args();

  let cred = LoginCredential::parse(&args.credential).unwrap_or_else(|| {
    eprintln!("Invalid credential format. Use ACCOUNT|PASSWORD|2FA");
    std::process::exit(2);
  });
  eprintln!(
    "Smoke login email={} browser={} sms_service_id={:?} sms_network={:?} max_retries={}",
    cred.email, args.browser, args.sms_service_id, args.sms_network, args.max_retries
  );

  tauri::Builder::default()
    .setup(move |app| {
      let handle = app.handle().clone();
      tauri::async_runtime::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
          let rt = tokio::runtime::Runtime::new().expect("runtime");
          rt.block_on(async move {
            let config = LoginConfig {
              credentials_text: args.credential.clone(),
              credentials: vec![cred],
              browser_type: args.browser,
              max_retries: args.max_retries,
              headless: args.headless,
              concurrency: 1,
              sub2api_url: String::new(),
              sub2api_api_key: String::new(),
              sub2api_proxy_id: None,
              sub2api_group_ids: None,
              push_to_sub2api: false,
              sms_provider: args.sms_token.as_ref().map(|_| "viotp".into()),
              sms_token: args.sms_token.clone(),
              sms_service_id: args.sms_service_id,
              sms_network: args.sms_network.clone(),
              sms_country: Some(args.sms_country.clone()),
              proxy_id: args.proxy_id.clone(),
              network_mode: if args.proxy_id.is_some() {
                LoginNetworkMode::Proxy
              } else {
                LoginNetworkMode::None
              },
            };
            if let Err(e) = config.validate() {
              eprintln!("config error: {e}");
              std::process::exit(2);
            }

            let cancel = Arc::new(AtomicBool::new(false));
            let mut engine = LoginEngine::with_cancel_flag(config, cancel);
            let viotp = args
              .sms_token
              .as_ref()
              .map(|t| ViotpService::new(t.clone()));
            let sms_ref: Option<&dyn SmsService> = viotp.as_ref().map(|s| s as &dyn SmsService);
            engine.run(handle, sms_ref).await
          })
        })
        .await
        .expect("join");

        eprintln!("=== RESULT COUNT={} ===", result.len());
        for (i, r) in result.iter().enumerate() {
          eprintln!("--- account {i} ---");
          eprintln!("success={}", r.success);
          eprintln!("email={}", r.email);
          eprintln!("account_id={}", r.account_id);
          eprintln!("access_token_len={}", r.access_token.len());
          eprintln!("refresh_token_len={}", r.refresh_token.len());
          eprintln!("phone={}", r.phone_number);
          eprintln!("sub2api_account_id={:?}", r.sub2api_account_id);
          eprintln!("error={}", r.error_message);
          eprintln!("push_error={}", r.push_error);
          eprintln!("--- logs ---");
          for line in &r.step_logs {
            eprintln!("{line}");
          }
        }

        let ok = result.iter().any(|r| r.success);
        std::process::exit(if ok { 0 } else { 1 });
      });
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running auto-login-live");
}
