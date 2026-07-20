//! Live auto-registration runner.
//!
//! Usage (from repo root):
//! ```text
//! pnpm copy-proxy-binary
//! cargo run --manifest-path src-tauri/Cargo.toml --bin auto-reg-live -- \
//!   --cdk GMAIL-XXXX \
//!   --browser camoufox \
//!   --profile-id 2d31c07b-df06-4630-9081-433b16baa26c \
//!   --network nord --rotate-every 1 --accounts-per-cdk 2 --nord-group "United States"
//! ```

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use donutbrowser_lib::auto_service::openai::register::types::NetworkMode;
use donutbrowser_lib::auto_service::openai::register::{RegistrationConfig, RegistrationEngine};
use donutbrowser_lib::email::{build_email_service, EmailProvider, EmailService};

struct LiveArgs {
  cdk: String,
  browser: String,
  profile_id: Option<String>,
  max_retries: u32,
  accounts_per_cdk: u32,
  network_mode: NetworkMode,
  rotate_every_n: u32,
  nord_group: Option<String>,
  nord_server_name: Option<String>,
  vpn_id: Option<String>,
  email_provider: EmailProvider,
}

fn parse_network_mode(s: &str) -> NetworkMode {
  match s.trim().to_ascii_lowercase().as_str() {
    "proxy" => NetworkMode::Proxy,
    "vpn" | "wireguard" | "wg" => NetworkMode::Vpn,
    "nord" | "nordvpn" | "nord-cli" => NetworkMode::Nord,
    _ => NetworkMode::None,
  }
}

fn parse_email_provider(value: &str) -> EmailProvider {
  EmailProvider::parse(value).unwrap_or_else(|error| {
    eprintln!("{error}");
    std::process::exit(2);
  })
}

fn parse_args() -> LiveArgs {
  let mut cdk = std::env::var("AUTO_REG_CDK").unwrap_or_default();
  let mut browser = std::env::var("AUTO_REG_BROWSER").unwrap_or_else(|_| "camoufox".into());
  let mut profile_id = std::env::var("AUTO_REG_PROFILE_ID").ok();
  let mut max_retries = 1u32;
  let mut accounts_per_cdk = 1u32;
  let mut network_mode = NetworkMode::None;
  let mut rotate_every_n = 0u32;
  let mut nord_group = std::env::var("AUTO_REG_NORD_GROUP").ok();
  let mut nord_server_name = std::env::var("AUTO_REG_NORD_SERVER").ok();
  let mut vpn_id = std::env::var("AUTO_REG_VPN_ID").ok();
  let mut email_provider = std::env::var("AUTO_REG_EMAIL_PROVIDER")
    .map(|value| parse_email_provider(&value))
    .unwrap_or(EmailProvider::Gmail123452026);

  if let Ok(m) = std::env::var("AUTO_REG_NETWORK") {
    network_mode = parse_network_mode(&m);
  }
  if let Ok(n) = std::env::var("AUTO_REG_ROTATE_EVERY") {
    rotate_every_n = n.parse().unwrap_or(0);
  }
  if let Ok(n) = std::env::var("AUTO_REG_ACCOUNTS_PER_CDK") {
    accounts_per_cdk = n.parse().unwrap_or(1);
  }

  let mut args = std::env::args().skip(1);
  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--cdk" => cdk = args.next().unwrap_or_default(),
      "--browser" => browser = args.next().unwrap_or(browser),
      "--profile-id" => profile_id = args.next(),
      "--max-retries" => {
        max_retries = args
          .next()
          .and_then(|v| v.parse().ok())
          .unwrap_or(max_retries);
      }
      "--accounts-per-cdk" => {
        accounts_per_cdk = args
          .next()
          .and_then(|v| v.parse().ok())
          .unwrap_or(accounts_per_cdk);
      }
      "--network" => {
        network_mode = parse_network_mode(&args.next().unwrap_or_default());
      }
      "--rotate-every" => {
        rotate_every_n = args
          .next()
          .and_then(|v| v.parse().ok())
          .unwrap_or(rotate_every_n);
      }
      "--nord-group" => nord_group = args.next(),
      "--nord-server" => nord_server_name = args.next(),
      "--vpn-id" => vpn_id = args.next(),
      "--email-provider" => {
        if let Some(v) = args.next() {
          email_provider = parse_email_provider(&v);
        }
      }
      other if other.starts_with("--cdk=") => {
        cdk = other.trim_start_matches("--cdk=").to_string();
      }
      other if other.starts_with("--browser=") => {
        browser = other.trim_start_matches("--browser=").to_string();
      }
      other if other.starts_with("--profile-id=") => {
        profile_id = Some(other.trim_start_matches("--profile-id=").to_string());
      }
      other if other.starts_with("--network=") => {
        network_mode = parse_network_mode(other.trim_start_matches("--network="));
      }
      other if other.starts_with("--rotate-every=") => {
        rotate_every_n = other
          .trim_start_matches("--rotate-every=")
          .parse()
          .unwrap_or(rotate_every_n);
      }
      other if other.starts_with("--accounts-per-cdk=") => {
        accounts_per_cdk = other
          .trim_start_matches("--accounts-per-cdk=")
          .parse()
          .unwrap_or(accounts_per_cdk);
      }
      other if other.starts_with("--nord-group=") => {
        nord_group = Some(other.trim_start_matches("--nord-group=").to_string());
      }
      other if other.starts_with("--nord-server=") => {
        nord_server_name = Some(other.trim_start_matches("--nord-server=").to_string());
      }
      other if other.starts_with("--vpn-id=") => {
        vpn_id = Some(other.trim_start_matches("--vpn-id=").to_string());
      }
      other if other.starts_with("--email-provider=") => {
        email_provider = parse_email_provider(other.trim_start_matches("--email-provider="));
      }
      _ => {}
    }
  }

  if cdk.is_empty() {
    eprintln!(
      "Usage: auto-reg-live --cdk GMAIL-XXXX [--browser camoufox] [--profile-id UUID] \
       [--network none|proxy|vpn|nord] [--vpn-id ID] [--rotate-every N] [--accounts-per-cdk N] \
       [--nord-group \"United States\"] [--email-provider gmail.123452026.xyz|sms.iosmq.xyz]"
    );
    std::process::exit(2);
  }

  LiveArgs {
    cdk,
    browser,
    profile_id,
    max_retries,
    accounts_per_cdk,
    network_mode,
    rotate_every_n,
    nord_group,
    nord_server_name,
    vpn_id,
    email_provider,
  }
}

fn main() {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

  let args = parse_args();
  eprintln!("=== LIVE AUTO-REGISTER ===");
  eprintln!("cdk={}", args.cdk);
  eprintln!("browser={}", args.browser);
  eprintln!("profile_id={:?}", args.profile_id);
  eprintln!("max_retries={}", args.max_retries);
  eprintln!("accounts_per_cdk={}", args.accounts_per_cdk);
  eprintln!("network_mode={:?}", args.network_mode);
  eprintln!("rotate_every_n={}", args.rotate_every_n);
  eprintln!("nord_group={:?}", args.nord_group);
  eprintln!("nord_server={:?}", args.nord_server_name);
  eprintln!("email_provider={}", args.email_provider);

  tauri::Builder::default()
    .setup(move |app| {
      let handle = app.handle().clone();
      // Mirror production command path: RegistrationEngine is not Send across
      // Tauri's async runtime, so run it on a dedicated blocking runtime.
      std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        let result = rt.block_on(async move {
          let mut config = RegistrationConfig {
            cdks: vec![args.cdk],
            profile_id: args.profile_id,
            proxy_id: None,
            vpn_id: args.vpn_id,
            browser_type: args.browser,
            max_retries: args.max_retries,
            accounts_per_cdk: args.accounts_per_cdk.max(1),
            headless: false,
            concurrency: 1,
            nord_max_sessions: 6,
            network_mode: args.network_mode,
            rotate_every_n: args.rotate_every_n,
            nord_group: args.nord_group,
            nord_server_name: args.nord_server_name,
            nord_cli_path: None,
            sms_provider: None,
            sms_token: None,
            sms_service_id: None,
            sms_network: None,
            sms_country: None,
            email_provider: args.email_provider,
          };
          config.normalize_network();
          if let Err(e) = config.validate_cdks() {
            eprintln!("config error: {e}");
            std::process::exit(2);
          }
          if let Err(e) = config.validate_network() {
            eprintln!("config error: {e}");
            std::process::exit(2);
          }
          eprintln!(
            "effective network_mode={:?} rotate_every_n={} email_provider={}",
            config.network_mode, config.rotate_every_n, config.email_provider
          );

          let cancel = Arc::new(AtomicBool::new(false));
          let email_provider = config.email_provider;
          let mut engine = RegistrationEngine::with_cancel_flag(config, cancel);
          let email: Box<dyn EmailService> = build_email_service(email_provider);
          engine.run(handle, email.as_ref(), None).await
        });

        eprintln!("=== RESULT ===");
        eprintln!("success={}", result.success);
        eprintln!("email={}", result.email);
        eprintln!("password={}", result.password);
        eprintln!("account_id={}", result.account_id);
        eprintln!("two_fa_enabled={}", result.two_fa_enabled);
        eprintln!("totp_secret={}", result.totp_secret);
        eprintln!("access_token_len={}", result.access_token.len());
        eprintln!("error={}", result.error_message);
        eprintln!("--- logs ---");
        for line in &result.step_logs {
          eprintln!("{line}");
        }

        std::process::exit(if result.success { 0 } else { 1 });
      });
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running auto-reg-live");
}
