//! Live account checker runner.
//!
//! Usage (from repo root):
//! ```text
//! pnpm copy-proxy-binary
//! cargo run --manifest-path src-tauri/Cargo.toml --features account-checker-live --bin account-check-live -- \
//!   "user@example.com|password|TOTPSECRET"
//! ```

use donutbrowser_lib::auto_service::openai::account_checker::commands::start_openai_account_check;
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Listener;

const PROGRESS_EVENT: &str = "openai-account-check-progress";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProgressEvent {
  #[allow(dead_code)]
  task_id: String,
  account_key: String,
  #[allow(dead_code)]
  credential_index: usize,
  total_credentials: usize,
  step: String,
  outcome: Option<String>,
  reason_code: Option<String>,
  terminal: bool,
}

fn main() {
  // Ensure data dir points to JnmBrowser's installed data directory.
  // In CLI mode, the binary may not have the env var set automatically.
  if std::env::var("JNMBROWSER_DATA_DIR").is_err()
    && std::env::var("DONUTBROWSER_DATA_DIR").is_err()
  {
    if let Some(local_app_data) = std::env::var("LOCALAPPDATA").ok() {
      let default_data =
        std::path::PathBuf::from(local_app_data).join("JnmBrowser");
      if default_data.exists() {
        std::env::set_var("JNMBROWSER_DATA_DIR", &default_data);
        eprintln!("[setup] JNMBROWSER_DATA_DIR={}", default_data.display());
      }
    }
  }
  let args: Vec<String> = std::env::args().collect();

  // Credentials are accepted as arguments and are never printed.
  let credentials_text = if args.len() >= 2 {
    args[1..].join("\n")
  } else {
    eprintln!("Usage: account-check-live \"email|password|2fa\" [\"email2|pass2|2fa2\" ...]");
    eprintln!();
    eprintln!("Example:");
    eprintln!("  account-check-live \"user@example.com|password123|TOTPSECRET\"");
    std::process::exit(2);
  };

  // Replace spaces between quoted args with newlines for multi-account support
  let credentials_text = credentials_text.replace(" |", "\n").replace("| ", "|");
  
  eprintln!("=== Account Checker Live Test ===");
  let account_count = credentials_text.lines().filter(|line| !line.trim().is_empty()).count();
  eprintln!("Checking {account_count} account(s)...");
  eprintln!();

  let finished = Arc::new(AtomicBool::new(false));
  let terminal_result: Arc<Mutex<Option<ProgressEvent>>> = Arc::new(Mutex::new(None));
  let start_time = Instant::now();

  let finished_for_setup = Arc::clone(&finished);
  let terminal_result_for_setup = Arc::clone(&terminal_result);
  let credentials_text_for_setup = credentials_text.clone();

  tauri::Builder::default()
    .setup(move |app| {
      let finished = Arc::clone(&finished_for_setup);
      let finished_for_spawn = Arc::clone(&finished);
      let terminal_result = Arc::clone(&terminal_result_for_setup);

      let handle = app.handle().clone();

      // Listen for progress events
      let handle_for_listener = handle.clone();
      let _listener = handle.listen(PROGRESS_EVENT, move |event| {
        let Ok(progress) = serde_json::from_str::<ProgressEvent>(event.payload()) else {
          return;
        };

        if progress.account_key.is_empty() {
          if progress.terminal {
            eprintln!(
              "[batch] step={} terminal=true total={}",
              progress.step, progress.total_credentials
            );
            finished.store(true, Ordering::SeqCst);
            exit_after_delay(&handle_for_listener);
          }
          return;
        }

        eprintln!(
          "[account] email={} step={} outcome={:?} reason={:?} index={}/{} terminal={}",
          progress.account_key,
          progress.step,
          progress.outcome,
          progress.reason_code,
          progress.credential_index + 1,
          progress.total_credentials,
          progress.terminal
        );

        if progress.terminal {
          let mut guard = terminal_result.lock().unwrap();
          *guard = Some(progress);
        }
      });

      // Start the account check
      let credentials = credentials_text_for_setup.clone();
      tauri::async_runtime::spawn(async move {
        match start_openai_account_check(handle.clone(), credentials).await {
          Ok(task_id) => {
            eprintln!("[started] task_id={task_id}");
          }
          Err(error) => {
            eprintln!("[error] Failed to start: {error}");
            finished_for_spawn.store(true, Ordering::SeqCst);
            exit_after_delay(&handle);
          }
        }
      });

      Ok(())
    })
    .build(tauri::generate_context!())
    .expect("error while building tauri application")
    .run(|_app_handle, event| {
      if let tauri::RunEvent::ExitRequested { .. } = event {
        // Allow exit
      }
    });

  // After app exits, print results
  let elapsed = start_time.elapsed();
  eprintln!();
  eprintln!("=== Completed in {:.1}s ===", elapsed.as_secs_f64());

  // List stored results
  match donutbrowser_lib::auto_service::openai::account_checker::store::current_results() {
    results if results.is_empty() => {
      eprintln!("No results stored.");
    }
    results => {
      eprintln!("Total results: {}", results.len());
      let passed: Vec<_> = results.iter().filter(|r| r.outcome == donutbrowser_lib::auto_service::openai::account_checker::types::AccountOutcome::Passed).collect();
      let deactivated: Vec<_> = results.iter().filter(|r| r.outcome == donutbrowser_lib::auto_service::openai::account_checker::types::AccountOutcome::Deactivated).collect();
      let unresolved: Vec<_> = results.iter().filter(|r| r.outcome == donutbrowser_lib::auto_service::openai::account_checker::types::AccountOutcome::Unresolved).collect();

      eprintln!("Passed: {}", passed.len());
      for r in passed {
        eprintln!("  {} → PASSED (reason: {:?})", r.email, r.reason_code);
      }
      eprintln!("Deactivated: {}", deactivated.len());
      for r in deactivated {
        eprintln!("  {} → DEACTIVATED (reason: {:?})", r.email, r.reason_code);
      }
      eprintln!("Unresolved: {}", unresolved.len());
      for r in unresolved {
        eprintln!("  {} → UNRESOLVED (reason: {:?})", r.email, r.reason_code);
      }
    }
  }
}

fn exit_after_delay(handle: &tauri::AppHandle) {
  let handle = handle.clone();
  std::thread::spawn(move || {
    std::thread::sleep(Duration::from_secs(3));
    handle.exit(0);
  });
}
