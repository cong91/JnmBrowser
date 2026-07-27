//! Live auto-registration runner.
//!
//! Usage (from repo root):
//! ```text
//! pnpm copy-proxy-binary
//! cargo run --manifest-path src-tauri/Cargo.toml --features auto-reg-live --bin auto-reg-live -- \
//!   --cdk GMAIL-XXXX \
//!   --browser camoufox \
//!   --profile-id 2d31c07b-df06-4630-9081-433b16baa26c \
//!   --network nord --rotate-every 1 --accounts-per-cdk 2 --nord-group "United States"
//!
//! # VPN mode — auto-resolves first Nord config from settings, or use --vpn-name:
//! cargo run --manifest-path src-tauri/Cargo.toml --features auto-reg-live --bin auto-reg-live -- \
//!   --cdk GMAIL-XXXX --browser chromium --network vpn
//! cargo run --manifest-path src-tauri/Cargo.toml --features auto-reg-live --bin auto-reg-live -- \
//!   --cdk GMAIL-XXXX --browser chromium --network vpn --vpn-name "Japan"
//!
//! # List saved VPN configs:
//! cargo run --manifest-path src-tauri/Cargo.toml --features auto-reg-live --bin auto-reg-live -- --list-vpns
//! ```

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use donutbrowser_lib::auto_service::openai::register::execution::{
  prepare_registration, run_prepared_registration,
};
use donutbrowser_lib::auto_service::openai::register::types::NetworkMode;
use donutbrowser_lib::auto_service::openai::register::RegistrationConfig;
use donutbrowser_lib::email::EmailProvider;

#[derive(Default)]
struct LiveArgs {
  cdks: Vec<String>,
  browser: Option<String>,
  profile_id: Option<String>,
  proxy_id: Option<String>,
  max_retries: Option<u32>,
  accounts_per_cdk: Option<u32>,
  concurrency: Option<u32>,
  network_mode: Option<NetworkMode>,
  rotate_every_n: Option<u32>,
  nord_group: Option<String>,
  nord_server_name: Option<String>,
  vpn_id: Option<String>,
  vpn_name: Option<String>,
  list_vpns: bool,
  email_provider: Option<EmailProvider>,
}

fn parse_network_mode(value: &str) -> Result<NetworkMode, String> {
  match value.trim().to_ascii_lowercase().as_str() {
    "none" => Ok(NetworkMode::None),
    "proxy" => Ok(NetworkMode::Proxy),
    "vpn" | "wireguard" | "wg" => Ok(NetworkMode::Vpn),
    "nord" | "nordvpn" | "nord-cli" => Ok(NetworkMode::Nord),
    _ => Err("network must be one of: none, proxy, vpn, nord".into()),
  }
}

fn parse_email_provider(value: &str) -> Result<EmailProvider, String> {
  EmailProvider::parse(value).map_err(|_| {
    format!(
      "email provider must be {} or {}",
      EmailProvider::GMAIL_123452026_ID,
      EmailProvider::SMS_IOSMQ_ID
    )
  })
}

fn parse_u32(option: &str, value: &str) -> Result<u32, String> {
  value
    .parse()
    .map_err(|_| format!("{option} must be an unsigned integer"))
}

fn push_cdks(destination: &mut Vec<String>, raw: &str) {
  for part in raw.split([',', ';', ' ', '\n', '\t']) {
    let cdk = part.trim();
    if !cdk.is_empty() {
      destination.push(cdk.to_string());
    }
  }
}

fn next_value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
  args
    .next()
    .ok_or_else(|| format!("{option} requires a value"))
}

fn parse_args_from(
  env_value: impl Fn(&str) -> Option<String>,
  raw_args: impl IntoIterator<Item = String>,
) -> Result<LiveArgs, String> {
  let mut parsed = LiveArgs::default();

  if let Some(value) = env_value("AUTO_REG_CDK") {
    push_cdks(&mut parsed.cdks, &value);
  }
  parsed.browser = env_value("AUTO_REG_BROWSER");
  parsed.profile_id = env_value("AUTO_REG_PROFILE_ID");
  parsed.proxy_id = env_value("AUTO_REG_PROXY_ID");
  parsed.nord_group = env_value("AUTO_REG_NORD_GROUP");
  parsed.nord_server_name = env_value("AUTO_REG_NORD_SERVER");
  parsed.vpn_id = env_value("AUTO_REG_VPN_ID");
  if let Some(value) = env_value("AUTO_REG_MAX_RETRIES") {
    parsed.max_retries = Some(parse_u32("AUTO_REG_MAX_RETRIES", &value)?);
  }
  if let Some(value) = env_value("AUTO_REG_ACCOUNTS_PER_CDK") {
    parsed.accounts_per_cdk = Some(parse_u32("AUTO_REG_ACCOUNTS_PER_CDK", &value)?);
  }
  if let Some(value) = env_value("AUTO_REG_CONCURRENCY") {
    parsed.concurrency = Some(parse_u32("AUTO_REG_CONCURRENCY", &value)?);
  }
  if let Some(value) = env_value("AUTO_REG_NETWORK") {
    parsed.network_mode = Some(parse_network_mode(&value)?);
  }
  if let Some(value) = env_value("AUTO_REG_ROTATE_EVERY") {
    parsed.rotate_every_n = Some(parse_u32("AUTO_REG_ROTATE_EVERY", &value)?);
  }
  if let Some(value) = env_value("AUTO_REG_EMAIL_PROVIDER") {
    parsed.email_provider = Some(parse_email_provider(&value)?);
  }

  let mut args = raw_args.into_iter();
  while let Some(argument) = args.next() {
    match argument.as_str() {
      "--cdk" => push_cdks(&mut parsed.cdks, &next_value(&mut args, "--cdk")?),
      "--browser" => parsed.browser = Some(next_value(&mut args, "--browser")?),
      "--profile-id" => parsed.profile_id = Some(next_value(&mut args, "--profile-id")?),
      "--proxy-id" => parsed.proxy_id = Some(next_value(&mut args, "--proxy-id")?),
      "--max-retries" => {
        let value = next_value(&mut args, "--max-retries")?;
        parsed.max_retries = Some(parse_u32("--max-retries", &value)?);
      }
      "--accounts-per-cdk" => {
        let value = next_value(&mut args, "--accounts-per-cdk")?;
        parsed.accounts_per_cdk = Some(parse_u32("--accounts-per-cdk", &value)?);
      }
      "--concurrency" => {
        let value = next_value(&mut args, "--concurrency")?;
        parsed.concurrency = Some(parse_u32("--concurrency", &value)?);
      }
      "--network" => {
        parsed.network_mode = Some(parse_network_mode(&next_value(&mut args, "--network")?)?);
      }
      "--rotate-every" => {
        let value = next_value(&mut args, "--rotate-every")?;
        parsed.rotate_every_n = Some(parse_u32("--rotate-every", &value)?);
      }
      "--nord-group" => parsed.nord_group = Some(next_value(&mut args, "--nord-group")?),
      "--nord-server" => {
        parsed.nord_server_name = Some(next_value(&mut args, "--nord-server")?);
      }
      "--vpn-id" => parsed.vpn_id = Some(next_value(&mut args, "--vpn-id")?),
      "--vpn-name" => parsed.vpn_name = Some(next_value(&mut args, "--vpn-name")?),
      "--list-vpns" => parsed.list_vpns = true,
      "--email-provider" => {
        parsed.email_provider = Some(parse_email_provider(&next_value(
          &mut args,
          "--email-provider",
        )?)?);
      }
      _ => {
        let Some((option, value)) = argument.split_once('=') else {
          return Err("unknown argument".into());
        };
        match option {
          "--cdk" => push_cdks(&mut parsed.cdks, value),
          "--browser" => parsed.browser = Some(value.to_string()),
          "--profile-id" => parsed.profile_id = Some(value.to_string()),
          "--proxy-id" => parsed.proxy_id = Some(value.to_string()),
          "--max-retries" => {
            parsed.max_retries = Some(parse_u32("--max-retries", value)?);
          }
          "--accounts-per-cdk" => {
            parsed.accounts_per_cdk = Some(parse_u32("--accounts-per-cdk", value)?);
          }
          "--concurrency" => {
            parsed.concurrency = Some(parse_u32("--concurrency", value)?);
          }
          "--network" => parsed.network_mode = Some(parse_network_mode(value)?),
          "--rotate-every" => {
            parsed.rotate_every_n = Some(parse_u32("--rotate-every", value)?);
          }
          "--nord-group" => parsed.nord_group = Some(value.to_string()),
          "--nord-server" => parsed.nord_server_name = Some(value.to_string()),
          "--vpn-id" => parsed.vpn_id = Some(value.to_string()),
          "--vpn-name" => parsed.vpn_name = Some(value.to_string()),
          "--list-vpns" => parsed.list_vpns = true,
          "--email-provider" => parsed.email_provider = Some(parse_email_provider(value)?),
          _ => return Err("unknown argument".into()),
        }
      }
    }
  }

  let mut seen = HashSet::new();
  parsed.cdks.retain(|cdk| seen.insert(cdk.clone()));
  Ok(parsed)
}

fn parse_args() -> Result<LiveArgs, String> {
  parse_args_from(|name| std::env::var(name).ok(), std::env::args().skip(1))
}

fn build_config(args: LiveArgs) -> Result<RegistrationConfig, String> {
  let mut config: RegistrationConfig = serde_json::from_value(serde_json::json!({
    "cdks": args.cdks
  }))
  .map_err(|_| "failed to load registration product defaults".to_string())?;

  if let Some(value) = args.browser {
    config.browser_type = value;
  }
  if let Some(value) = args.profile_id {
    config.profile_id = Some(value);
  }
  if let Some(value) = args.proxy_id {
    config.proxy_id = Some(value);
  }
  if let Some(value) = args.max_retries {
    config.max_retries = value;
  }
  if let Some(value) = args.accounts_per_cdk {
    config.accounts_per_cdk = value;
  }
  if let Some(value) = args.concurrency {
    config.concurrency = value;
  }
  if let Some(value) = args.network_mode {
    config.network_mode = value;
  }
  if let Some(value) = args.rotate_every_n {
    config.rotate_every_n = value;
  }
  if let Some(value) = args.nord_group {
    config.nord_group = Some(value);
  }
  if let Some(value) = args.nord_server_name {
    config.nord_server_name = Some(value);
  }
  if let Some(value) = args.vpn_id {
    config.vpn_id = Some(value);
  }
  if let Some(value) = args.email_provider {
    config.email_provider = value;
  }

  // Auto-resolve VPN config from saved settings when using --network vpn
  // without an explicit --vpn-id.
  if config.network_mode == NetworkMode::Vpn && config.vpn_id.is_none() {
    let storage = donutbrowser_lib::vpn::VPN_STORAGE
      .lock()
      .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
    let configs = storage
      .list_configs()
      .map_err(|e| format!("Failed to list VPN configs: {e}"))?;
    if configs.is_empty() {
      return Err("VPN mode requires a WireGuard config, but none are saved in settings. Create one via the app's Proxies & VPNs page first.".into());
    }
    let matched = if let Some(ref name_filter) = args.vpn_name {
      let filter = name_filter.to_ascii_lowercase();
      configs
        .iter()
        .find(|c| c.name.to_ascii_lowercase().contains(&filter))
        .or_else(|| {
          configs
            .iter()
            .find(|c| c.id.to_ascii_lowercase().contains(&filter))
        })
        .ok_or_else(|| {
          format!(
            "No VPN config matching '{}' found ({} configs available)",
            name_filter,
            configs.len()
          )
        })?
        .clone()
    } else {
      // Prefer Nord-sourced config, then any WireGuard config.
      configs
        .iter()
        .find(|c| c.source.as_deref() == Some("nord"))
        .or_else(|| configs.first())
        .ok_or_else(|| "No VPN configs found".to_string())?
        .clone()
    };
    eprintln!(
      "vpn: auto-selected config id={} name='{}' source={}",
      matched.id,
      matched.name,
      matched.source.as_deref().unwrap_or("-")
    );
    config.vpn_id = Some(matched.id);
  }

  Ok(config)
}

fn startup_failure(exit_code: i32) -> ! {
  eprintln!("startup_valid=false");
  std::process::exit(exit_code);
}

fn main() {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

  let args = parse_args().unwrap_or_else(|_| startup_failure(2));

  if args.list_vpns {
    match donutbrowser_lib::vpn::VPN_STORAGE.lock() {
      Ok(storage) => match storage.list_configs() {
        Ok(configs) => {
          if configs.is_empty() {
            eprintln!("No VPN configs saved.");
          } else {
            for c in &configs {
              eprintln!(
                "id={}  name='{}'  source={}  max_sessions={}",
                c.id,
                c.name,
                c.source.as_deref().unwrap_or("-"),
                c.max_sessions
                  .map(|n| n.to_string())
                  .unwrap_or_else(|| "-".to_string())
              );
            }
          }
          std::process::exit(0);
        }
        Err(e) => {
          eprintln!("Failed to list VPN configs: {e}");
          std::process::exit(1);
        }
      },
      Err(e) => {
        eprintln!("Failed to lock VPN storage: {e}");
        std::process::exit(1);
      }
    }
  }

  let config = build_config(args).unwrap_or_else(|_| startup_failure(2));
  let cdk_count = config.cdks.len();
  let prepared = prepare_registration(config, Arc::new(AtomicBool::new(false)))
    .unwrap_or_else(|_| startup_failure(2));

  eprintln!("=== LIVE AUTO-REGISTER ===");
  eprintln!("startup_valid=true");
  eprintln!("cdk_count={cdk_count}");

  tauri::Builder::default()
    .setup(move |app| {
      let handle = app.handle().clone();
      std::thread::spawn(move || {
        let result =
          run_prepared_registration(handle, prepared).unwrap_or_else(|_| startup_failure(1));

        eprintln!("=== RESULT ===");
        eprintln!("success={}", result.success);
        eprintln!("two_fa_enabled={}", result.two_fa_enabled);
        eprintln!("has_totp_secret={}", !result.totp_secret.trim().is_empty());
        eprintln!(
          "has_access_token={}",
          !result.access_token.trim().is_empty()
        );
        eprintln!("has_error={}", !result.error_message.trim().is_empty());
        eprintln!("step_log_count={}", result.step_logs.len());

        std::process::exit(if result.success { 0 } else { 1 });
      });
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running auto-reg-live");
}

#[cfg(test)]
mod tests {
  use super::*;
  use donutbrowser_lib::auto_service::openai::register::execution::normalize_and_validate_registration;

  #[test]
  fn unknown_network_cli_and_environment_values_fail() {
    assert!(parse_args_from(|_| None, ["--network=unknown".to_string()]).is_err());
    assert!(parse_args_from(
      |name| (name == "AUTO_REG_NETWORK").then(|| "unknown".to_string()),
      Vec::<String>::new()
    )
    .is_err());
  }

  #[test]
  fn omitted_options_use_registration_config_product_defaults() {
    let parsed = parse_args_from(|_| None, ["--cdk=GMAIL-TEST".to_string()]).unwrap();
    let config = build_config(parsed).unwrap();

    assert_eq!(config.browser_type, "chromium");
    assert_eq!(config.max_retries, 3);
    assert_eq!(config.accounts_per_cdk, 1);
    assert_eq!(config.concurrency, 1);
    assert_eq!(config.nord_max_sessions, 6);
    assert_eq!(config.network_mode, NetworkMode::None);
    assert_eq!(config.email_provider, EmailProvider::Gmail123452026);
  }

  #[test]
  fn live_adapter_maps_proxy_id_into_shared_preparation() {
    let parsed = parse_args_from(
      |name| (name == "AUTO_REG_PROXY_ID").then(|| "proxy-env".to_string()),
      [
        "--cdk=GMAIL-TEST".to_string(),
        "--network=proxy".to_string(),
        "--proxy-id=proxy-cli".to_string(),
      ],
    )
    .unwrap();
    let mut config = build_config(parsed).unwrap();

    normalize_and_validate_registration(&mut config).unwrap();

    assert_eq!(config.network_mode, NetworkMode::Proxy);
    assert_eq!(config.proxy_id.as_deref(), Some("proxy-cli"));
  }

  #[test]
  fn live_adapter_uses_shared_registration_preparation() {
    let parsed = parse_args_from(
      |_| None,
      [
        "--cdk=GMAIL-TEST".to_string(),
        "--browser=CAMOUFOX".to_string(),
        "--network=none".to_string(),
      ],
    )
    .unwrap();
    let mut config = build_config(parsed).unwrap();

    normalize_and_validate_registration(&mut config).unwrap();

    assert_eq!(config.browser_type, "camoufox");
    assert_eq!(config.concurrency, 1);
  }
}
