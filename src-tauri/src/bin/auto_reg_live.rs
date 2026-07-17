//! Live auto-registration runner.
//!
//! Usage (from repo root):
//! ```text
//! pnpm copy-proxy-binary
//! cargo run --manifest-path src-tauri/Cargo.toml --bin auto-reg-live -- \
//!   --cdk GMAIL-XXXX \
//!   --browser camoufox \
//!   --profile-id 2d31c07b-df06-4630-9081-433b16baa26c
//! ```

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use donutbrowser_lib::auto_register::{RegistrationConfig, RegistrationEngine};
use donutbrowser_lib::email::gmail_cdk::GmailCdkService;

fn parse_args() -> (String, String, Option<String>, u32) {
  let mut cdk = std::env::var("AUTO_REG_CDK").unwrap_or_default();
  let mut browser = std::env::var("AUTO_REG_BROWSER").unwrap_or_else(|_| "camoufox".into());
  let mut profile_id = std::env::var("AUTO_REG_PROFILE_ID").ok();
  let mut max_retries = 1u32;

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
      other if other.starts_with("--cdk=") => {
        cdk = other.trim_start_matches("--cdk=").to_string();
      }
      other if other.starts_with("--browser=") => {
        browser = other.trim_start_matches("--browser=").to_string();
      }
      other if other.starts_with("--profile-id=") => {
        profile_id = Some(other.trim_start_matches("--profile-id=").to_string());
      }
      _ => {}
    }
  }

  if cdk.is_empty() {
    eprintln!("Usage: auto-reg-live --cdk GMAIL-XXXX [--browser camoufox] [--profile-id UUID]");
    std::process::exit(2);
  }

  (cdk, browser, profile_id, max_retries)
}

fn main() {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

  let (cdk, browser, profile_id, max_retries) = parse_args();
  eprintln!("=== LIVE AUTO-REGISTER ===");
  eprintln!("cdk={cdk}");
  eprintln!("browser={browser}");
  eprintln!("profile_id={profile_id:?}");
  eprintln!("max_retries={max_retries}");

  tauri::Builder::default()
    .setup(move |app| {
      let handle = app.handle().clone();
      // Mirror production command path: RegistrationEngine is not Send across
      // Tauri's async runtime, so run it on a dedicated blocking runtime.
      std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        let result = rt.block_on(async move {
          let config = RegistrationConfig {
            cdks: vec![cdk],
            profile_id,
            proxy_id: None,
            browser_type: browser,
            max_retries,
            accounts_per_cdk: 1,
            headless: false,
            concurrency: 1,
            network_mode: Default::default(),
            rotate_every_n: 0,
            nord_group: None,
            nord_server_name: None,
            nord_cli_path: None,
          };

          let cancel = Arc::new(AtomicBool::new(false));
          let mut engine = RegistrationEngine::with_cancel_flag(config, cancel);
          let email = GmailCdkService::new();
          engine.run(handle, &email).await
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
