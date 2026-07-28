//! Live 2FA backfill runner.
//!
//! Usage (from repo root):
//! ```text
//! cargo run --manifest-path src-tauri/Cargo.toml --features twofa-backfill-live --bin twofa-backfill-live -- \
//!   --account-key ACCOUNT_ID --browser chromium --network none --mode canary
//! ```

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use donutbrowser_lib::auto_service::openai::register::commands::{
  start_auto_registration, AutoRegistrationRequest, ExistingAccountAutoRegistrationOperation,
  ExistingAccountAutoRegistrationRequest,
};
use donutbrowser_lib::auto_service::openai::register::store::list_registered_accounts;
use donutbrowser_lib::auto_service::openai::two_factor_backfill::commands::{
  cancel_two_factor_backfill, recover_stale_two_factor_backfill, BackfillBrowser, BackfillMode,
  BackfillNetworkConfig, TwoFactorBackfillStartRequest,
};
use serde::Deserialize;
use tauri::Listener;

const PROGRESS_EVENT: &str = "twofa-backfill-progress";

#[derive(Debug)]
struct LiveArgs {
  account_key: String,
  browser: BackfillBrowser,
  network: BackfillNetworkConfig,
  mode: BackfillMode,
  timeout_secs: u64,
  allow_free_trial_no: bool,
  acknowledge_legacy_access: bool,
  recover_operation_id: Option<String>,
  worker_profile_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProgressEvent {
  task_id: String,
  account_key: String,
  step: String,
  outcome: Option<String>,
  error_code: Option<String>,
  retryable: bool,
}

fn parse_browser(value: &str) -> BackfillBrowser {
  match value.trim().to_ascii_lowercase().as_str() {
    "camoufox" => BackfillBrowser::Camoufox,
    _ => BackfillBrowser::Chromium,
  }
}

fn parse_mode(value: &str) -> BackfillMode {
  match value.trim().to_ascii_lowercase().as_str() {
    "bulk" => BackfillMode::Bulk,
    _ => BackfillMode::Canary,
  }
}

fn parse_network(value: &str) -> BackfillNetworkConfig {
  match value.trim().to_ascii_lowercase().as_str() {
    "proxy" => BackfillNetworkConfig::Proxy {
      proxy_id: std::env::var("BACKFILL_PROXY_ID").unwrap_or_default(),
    },
    "vpn" => BackfillNetworkConfig::Vpn {
      vpn_id: std::env::var("BACKFILL_VPN_ID").unwrap_or_default(),
    },
    _ => BackfillNetworkConfig::None,
  }
}

fn parse_args() -> LiveArgs {
  let mut account_key = std::env::var("BACKFILL_ACCOUNT_KEY").unwrap_or_default();
  let mut browser = std::env::var("BACKFILL_BROWSER")
    .map(|value| parse_browser(&value))
    .unwrap_or(BackfillBrowser::Chromium);
  let mut network = std::env::var("BACKFILL_NETWORK")
    .map(|value| parse_network(&value))
    .unwrap_or(BackfillNetworkConfig::None);
  let mut mode = std::env::var("BACKFILL_MODE")
    .map(|value| parse_mode(&value))
    .unwrap_or(BackfillMode::Canary);
  let mut timeout_secs = std::env::var("BACKFILL_TIMEOUT_SECS")
    .ok()
    .and_then(|value| value.parse().ok())
    .unwrap_or(600);
  let mut allow_free_trial_no = false;
  let mut acknowledge_legacy_access = true;
  let mut recover_operation_id = None;
  let mut worker_profile_id = None;

  let mut args = std::env::args().skip(1);
  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--account-key" => account_key = args.next().unwrap_or_default(),
      "--browser" => browser = parse_browser(&args.next().unwrap_or_default()),
      "--network" => network = parse_network(&args.next().unwrap_or_default()),
      "--mode" => mode = parse_mode(&args.next().unwrap_or_default()),
      "--timeout-secs" => {
        timeout_secs = args
          .next()
          .and_then(|value| value.parse().ok())
          .unwrap_or(timeout_secs);
      }
      "--allow-free-trial-no" => allow_free_trial_no = true,
      "--no-legacy-ack" => acknowledge_legacy_access = false,
      "--recover-operation-id" => recover_operation_id = args.next(),
      "--profile-id" => worker_profile_id = args.next(),
      other if other.starts_with("--account-key=") => {
        account_key = other.trim_start_matches("--account-key=").to_string();
      }
      other if other.starts_with("--browser=") => {
        browser = parse_browser(other.trim_start_matches("--browser="));
      }
      other if other.starts_with("--network=") => {
        network = parse_network(other.trim_start_matches("--network="));
      }
      other if other.starts_with("--mode=") => {
        mode = parse_mode(other.trim_start_matches("--mode="));
      }
      other if other.starts_with("--timeout-secs=") => {
        timeout_secs = other
          .trim_start_matches("--timeout-secs=")
          .parse()
          .unwrap_or(timeout_secs);
      }
      other if other.starts_with("--recover-operation-id=") => {
        recover_operation_id = Some(
          other
            .trim_start_matches("--recover-operation-id=")
            .to_string(),
        );
      }
      other if other.starts_with("--profile-id=") => {
        worker_profile_id = Some(other.trim_start_matches("--profile-id=").to_string());
      }
      "--help" | "-h" => {
        eprintln!(
          "Usage: twofa-backfill-live --account-key ACCOUNT_ID [--browser chromium|camoufox] [--network none|proxy|vpn] [--mode canary|bulk] [--recover-operation-id OPERATION_ID --profile-id PROFILE_ID]"
        );
        std::process::exit(0);
      }
      other => {
        eprintln!("Unknown arg: {other}");
        std::process::exit(2);
      }
    }
  }

  if account_key.trim().is_empty() {
    eprintln!("Missing --account-key");
    std::process::exit(2);
  }

  LiveArgs {
    account_key,
    browser,
    network,
    mode,
    timeout_secs,
    allow_free_trial_no,
    acknowledge_legacy_access,
    recover_operation_id,
    worker_profile_id,
  }
}

fn persisted_two_fa_enabled(account_key: &str) -> bool {
  list_registered_accounts()
    .unwrap_or_default()
    .into_iter()
    .any(|account| {
      (account.account_id.trim() == account_key.trim()
        || account.email.trim() == account_key.trim())
        && account.two_fa_enabled
        && !account.totp_secret.trim().is_empty()
    })
}

fn request_exit(handle: &tauri::AppHandle, exit_code: &AtomicI32, code: i32) {
  exit_code.store(code, Ordering::SeqCst);
  handle.exit(code);
}

fn main() {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
  let args = parse_args();

  eprintln!("=== LIVE 2FA BACKFILL ===");
  eprintln!("account_key={}", args.account_key);
  eprintln!("browser={:?}", args.browser);
  eprintln!("network={:?}", args.network);
  eprintln!("mode={:?}", args.mode);
  eprintln!("timeout_secs={}", args.timeout_secs);

  let requested_exit_code = Arc::new(AtomicI32::new(1));
  let requested_exit_code_for_setup = Arc::clone(&requested_exit_code);
  let app = tauri::Builder::default()
    .setup(move |app| {
      let requested_exit_code = Arc::clone(&requested_exit_code_for_setup);
      let handle = app.handle().clone();
      let account_key = args.account_key.clone();
      let terminal: Arc<Mutex<Option<ProgressEvent>>> = Arc::new(Mutex::new(None));
      let terminal_for_listener = Arc::clone(&terminal);
      let account_key_for_listener = account_key.clone();
      let _listener = handle.listen(PROGRESS_EVENT, move |event| {
        let Ok(progress) = serde_json::from_str::<ProgressEvent>(event.payload()) else {
          return;
        };
        if !progress.account_key.is_empty() && progress.account_key != account_key_for_listener {
          return;
        }
        eprintln!(
          "progress task={} account={} step={} outcome={:?} error={:?} retryable={}",
          progress.task_id,
          progress.account_key,
          progress.step,
          progress.outcome,
          progress.error_code,
          progress.retryable
        );
        if matches!(
          progress.step.as_str(),
          "completed" | "failed" | "cancelled" | "batchPaused"
        ) {
          *terminal_for_listener.lock().unwrap() = Some(progress);
        }
      });

      tauri::async_runtime::spawn(async move {
        if let Some(operation_id) = args.recover_operation_id.as_deref() {
          let Some(profile_id) = args.worker_profile_id.as_deref() else {
            eprintln!("recovery_requires_profile_id");
            request_exit(&handle, requested_exit_code.as_ref(), 2);
            return;
          };
          match recover_stale_two_factor_backfill(
            handle.clone(),
            operation_id.to_string(),
            account_key.clone(),
            profile_id.to_string(),
          )
          .await
          {
            Ok(()) => {
              eprintln!("stale_recovery_complete");
              request_exit(&handle, requested_exit_code.as_ref(), 0);
            }
            Err(error) => {
              eprintln!("stale_recovery_error={error}");
              request_exit(&handle, requested_exit_code.as_ref(), 1);
            }
          }
          return;
        }

        let request = TwoFactorBackfillStartRequest {
          selected_account_keys: vec![account_key.clone()],
          allow_free_trial_no: args.allow_free_trial_no,
          acknowledge_legacy_access: args.acknowledge_legacy_access,
          browser: args.browser,
          network: args.network,
          mode: args.mode,
        };
        let request =
          AutoRegistrationRequest::ExistingAccount(ExistingAccountAutoRegistrationRequest {
            operation: ExistingAccountAutoRegistrationOperation::ExistingAccount,
            request,
          });

        let task_id = match start_auto_registration(handle.clone(), request).await {
          Ok(task_id) => {
            eprintln!("task_id={task_id}");
            task_id
          }
          Err(error) => {
            eprintln!("start_error={error}");
            request_exit(&handle, requested_exit_code.as_ref(), 1);
            return;
          }
        };

        let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);
        while Instant::now() < deadline {
          if terminal.lock().unwrap().is_some() {
            let enabled = persisted_two_fa_enabled(&account_key);
            eprintln!("persisted_two_fa_enabled={enabled}");
            request_exit(
              &handle,
              requested_exit_code.as_ref(),
              if enabled { 0 } else { 1 },
            );
            return;
          }
          tokio::time::sleep(Duration::from_millis(500)).await;
        }

        eprintln!("timeout_waiting_for_terminal_event; requesting_cancellation");
        if let Err(error) = cancel_two_factor_backfill(task_id) {
          eprintln!("cancellation_request_error={error}");
          request_exit(&handle, requested_exit_code.as_ref(), 1);
          return;
        }

        loop {
          if terminal.lock().unwrap().is_some() {
            let enabled = persisted_two_fa_enabled(&account_key);
            eprintln!("persisted_two_fa_enabled={enabled}");
            request_exit(
              &handle,
              requested_exit_code.as_ref(),
              if enabled { 0 } else { 1 },
            );
            return;
          }
          tokio::time::sleep(Duration::from_millis(250)).await;
        }
      });
      Ok(())
    })
    .build(tauri::generate_context!())
    .expect("error while building twofa-backfill-live");
  let _runtime_exit_code = app.run_return(|_, _| {});
  std::process::exit(requested_exit_code.load(Ordering::SeqCst));
}
