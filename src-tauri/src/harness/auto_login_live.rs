//! Live OpenAI/Codex auto-login smoke runner.
//!
//! Usage (repo root):
//! ```text
//! pnpm copy-proxy-binary
//! cargo run --manifest-path src-tauri/Cargo.toml --features auto-login-live --bin auto-login-live -- \
//!   --credential 'user@x.com|pass|TOTP' \
//!   --browser chromium \
//!   --sms-token VIOTP_TOKEN \
//!   --sms-service-id 1234 \
//!   --sms-network VINAPHONE \
//!   --max-retries 3
//! ```

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use donutbrowser_lib::auto_service::openai::login::execution::{prepare_login, run_prepared_login};
use donutbrowser_lib::auto_service::openai::login::types::{
  LoginConfig, LoginNetworkMode, DEFAULT_MAX_RETRIES,
};
use donutbrowser_lib::profile_runtime::{DataMode, FingerprintMode};

struct LiveArgs {
  credential: String,
  browser: String,
  max_retries: u32,
  headless: bool,
  sms_token: Option<String>,
  use_sms_settings: bool,
  sms_service_id: Option<u32>,
  sms_network: Option<String>,
  sms_country: String,
  proxy_id: Option<String>,
  vpn_id: Option<String>,
}

fn parse_args() -> Result<LiveArgs, String> {
  let mut credential = std::env::var("AUTO_LOGIN_CREDENTIAL").unwrap_or_default();
  let mut browser = std::env::var("AUTO_LOGIN_BROWSER").unwrap_or_else(|_| "chromium".into());
  let mut max_retries = DEFAULT_MAX_RETRIES;
  let mut headless = false;
  let mut sms_token = std::env::var("AUTO_LOGIN_SMS_TOKEN").ok();
  let mut use_sms_settings = std::env::var("AUTO_LOGIN_USE_SMS_SETTINGS")
    .ok()
    .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
  let mut sms_service_id = std::env::var("AUTO_LOGIN_SMS_SERVICE_ID")
    .ok()
    .and_then(|value| value.parse().ok());
  let mut sms_network = std::env::var("AUTO_LOGIN_SMS_NETWORK").ok();
  let mut sms_country = std::env::var("AUTO_LOGIN_SMS_COUNTRY").unwrap_or_else(|_| "vn".into());
  let mut proxy_id = std::env::var("AUTO_LOGIN_PROXY_ID").ok();
  let mut vpn_id = std::env::var("AUTO_LOGIN_VPN_ID").ok();

  let mut args = std::env::args().skip(1);
  while let Some(argument) = args.next() {
    match argument.as_str() {
      "--credential" => credential = next_value(&mut args)?,
      "--browser" => browser = next_value(&mut args)?,
      "--max-retries" => max_retries = parse_u32(&next_value(&mut args)?)?,
      "--headless" => headless = true,
      "--sms-token" => sms_token = Some(next_value(&mut args)?),
      "--use-sms-settings" => use_sms_settings = true,
      "--sms-service-id" => sms_service_id = Some(parse_u32(&next_value(&mut args)?)?),
      "--sms-network" => sms_network = Some(next_value(&mut args)?),
      "--sms-country" => sms_country = next_value(&mut args)?,
      "--proxy-id" => proxy_id = Some(next_value(&mut args)?),
      "--vpn-id" => vpn_id = Some(next_value(&mut args)?),
      "--help" | "-h" => {
        eprintln!(
          "Usage: auto-login-live --credential ACCOUNT|PASSWORD|2FA [--browser chromium] \
           [--sms-token TOKEN | --use-sms-settings] --sms-service-id ID \
           [--sms-network VINAPHONE] [--vpn-id ID | --proxy-id ID] [--max-retries 3]"
        );
        std::process::exit(0);
      }
      _ => {
        let Some((option, value)) = argument.split_once('=') else {
          return Err("invalid command line".into());
        };
        match option {
          "--credential" => credential = value.to_string(),
          "--browser" => browser = value.to_string(),
          "--max-retries" => max_retries = parse_u32(value)?,
          "--sms-token" => sms_token = Some(value.to_string()),
          "--sms-service-id" => sms_service_id = Some(parse_u32(value)?),
          "--sms-network" => sms_network = Some(value.to_string()),
          "--sms-country" => sms_country = value.to_string(),
          "--proxy-id" => proxy_id = Some(value.to_string()),
          "--vpn-id" => vpn_id = Some(value.to_string()),
          _ => return Err("invalid command line".into()),
        }
      }
    }
  }

  if credential.trim().is_empty() {
    return Err("missing credential".into());
  }

  Ok(LiveArgs {
    credential,
    browser,
    max_retries,
    headless,
    sms_token,
    use_sms_settings,
    sms_service_id,
    sms_network,
    sms_country,
    proxy_id,
    vpn_id,
  })
}

fn next_value(args: &mut impl Iterator<Item = String>) -> Result<String, String> {
  args.next().ok_or_else(|| "missing option value".into())
}

fn parse_u32(value: &str) -> Result<u32, String> {
  value.parse().map_err(|_| "invalid unsigned integer".into())
}

fn build_config(args: LiveArgs) -> LoginConfig {
  let network_mode = if args.vpn_id.is_some() {
    LoginNetworkMode::Vpn
  } else if args.proxy_id.is_some() {
    LoginNetworkMode::Proxy
  } else {
    LoginNetworkMode::None
  };
  let sms_enabled = args.use_sms_settings || args.sms_token.is_some();

  LoginConfig {
    credentials_text: args.credential,
    credentials: Vec::new(),
    profile_id: None,
    data_mode: DataMode::Ephemeral,
    fingerprint_mode: FingerprintMode::RandomPerLaunch,
    browser_type: args.browser,
    max_retries: args.max_retries,
    headless: args.headless,
    concurrency: 1,
    sub2api_url: String::new(),
    sub2api_api_key: String::new(),
    sub2api_proxy_id: None,
    sub2api_group_ids: None,
    push_to_sub2api: false,
    sms_provider: sms_enabled.then(|| "viotp".into()),
    sms_token: args.sms_token,
    sms_service_id: args.sms_service_id,
    sms_network: args.sms_network,
    sms_country: Some(args.sms_country),
    proxy_id: args.proxy_id,
    vpn_id: args.vpn_id,
    rotate_every_n: u32::from(matches!(network_mode, LoginNetworkMode::Vpn)),
    network_mode,
  }
}

fn setup_failure(exit_code: i32) -> ! {
  eprintln!("setup_error=true");
  std::process::exit(exit_code);
}

fn main() {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

  let args = parse_args().unwrap_or_else(|_| setup_failure(2));
  let config = build_config(args);

  let run_result = tauri::Builder::default()
    .setup(move |app| {
      let handle = app.handle().clone();
      tauri::async_runtime::spawn(async move {
        let prepared = prepare_login(&handle, config, Arc::new(AtomicBool::new(false)))
          .await
          .unwrap_or_else(|_| setup_failure(2));

        std::thread::spawn(move || {
          let results = run_prepared_login(handle, prepared).unwrap_or_else(|_| setup_failure(1));
          let success_count = results.iter().filter(|result| result.success).count();
          let failure_count = results.len().saturating_sub(success_count);

          eprintln!("result_count={}", results.len());
          eprintln!("success_count={success_count}");
          eprintln!("failure_count={failure_count}");

          std::process::exit(if failure_count == 0 && success_count > 0 {
            0
          } else {
            1
          });
        });
      });
      Ok(())
    })
    .run(tauri::generate_context!());

  if run_result.is_err() {
    setup_failure(1);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn live_args(max_retries: u32) -> LiveArgs {
    LiveArgs {
      credential: "person@example.com|password|secret".into(),
      browser: "chromium".into(),
      max_retries,
      headless: false,
      sms_token: None,
      use_sms_settings: false,
      sms_service_id: None,
      sms_network: None,
      sms_country: "vn".into(),
      proxy_id: None,
      vpn_id: None,
    }
  }

  #[test]
  fn harness_default_matches_product_retry_policy() {
    assert_eq!(build_config(live_args(DEFAULT_MAX_RETRIES)).max_retries, 3);
  }

  #[test]
  fn explicit_retry_override_is_preserved() {
    assert_eq!(build_config(live_args(7)).max_retries, 7);
  }
}
