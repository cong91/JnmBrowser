//! Probe ChatGPT free-trial related endpoints from an authenticated browser page.
//!
//! ```text
//! cargo run --manifest-path src-tauri/Cargo.toml --bin probe-free-trial -- \
//!   --profile-id 2d31c07b-df06-4630-9081-433b16baa26c \
//!   --token-file "C:/Users/PC/AppData/Local/JnmBrowser/registered_accounts/02051674-b3c3-4dd7-9dab-66371d96241f.json"
//! ```

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use donutbrowser_lib::browser_runner::BrowserRunner;
use donutbrowser_lib::camoufox_manager::CamoufoxManager;
use donutbrowser_lib::profile::ProfileManager;
use serde_json::Value;

fn arg_value(flag: &str) -> Option<String> {
  let mut args = std::env::args().skip(1);
  while let Some(a) = args.next() {
    if a == flag {
      return args.next();
    }
    if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
      return Some(v.to_string());
    }
  }
  None
}

async fn eval_json(page: &playwright::api::Page, expression: &str) -> Result<Value, String> {
  page
    .eval(expression)
    .await
    .map_err(|e| format!("evaluate failed: {e}"))
}

#[tokio::main]
async fn main() {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

  let profile_id =
    arg_value("--profile-id").unwrap_or_else(|| "2d31c07b-df06-4630-9081-433b16baa26c".into());
  let token_file = arg_value("--token-file").unwrap_or_else(|| {
    r"C:\Users\PC\AppData\Local\JnmBrowser\registered_accounts\02051674-b3c3-4dd7-9dab-66371d96241f.json".into()
  });

  let account_json: Value =
    serde_json::from_str(&fs::read_to_string(PathBuf::from(&token_file)).expect("read token file"))
      .expect("parse token file");
  let access_token = account_json["accessToken"]
    .as_str()
    .or_else(|| account_json["access_token"].as_str())
    .unwrap_or("")
    .to_string();
  let account_id = account_json["accountId"]
    .as_str()
    .or_else(|| account_json["account_id"].as_str())
    .unwrap_or("")
    .to_string();
  assert!(!access_token.is_empty(), "access token empty");

  eprintln!("profile_id={profile_id}");
  eprintln!("account_id={account_id}");
  eprintln!("token_len={}", access_token.len());

  // Decode JWT claims for plan signals.
  let payload_b64 = access_token.split('.').nth(1).unwrap_or("");
  let mut s = payload_b64.replace('-', "+").replace('_', "/");
  while !s.len().is_multiple_of(4) {
    s.push('=');
  }
  if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(s.as_bytes()) {
    if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
      eprintln!("=== JWT CLAIMS ===");
      eprintln!(
        "{}",
        serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
      );
    }
  }

  tauri::Builder::default()
    .setup(move |app| {
      let handle = app.handle().clone();
      std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async move {
          let profiles = ProfileManager::instance()
            .list_profiles()
            .expect("list profiles");
          let profile = profiles
            .into_iter()
            .find(|p| p.id.to_string() == profile_id)
            .expect("profile not found");

          let launched = BrowserRunner::instance()
            .launch_browser(handle.clone(), &profile, Some("https://chatgpt.com/".into()), None)
            .await
            .expect("launch browser");
          eprintln!("launched profile {}", launched.name);

          tokio::time::sleep(Duration::from_secs(3)).await;
          let profiles_dir = ProfileManager::instance().get_profiles_dir();
          let profile_path = donutbrowser_lib::ephemeral_dirs::get_effective_profile_path(
            &launched,
            &profiles_dir,
          );
          let path_str = profile_path.to_string_lossy().to_string();

          let page = CamoufoxManager::instance()
            .get_active_page(&path_str)
            .await
            .expect("get active page");
          let _ = page
            .goto_builder("https://chatgpt.com/")
            .goto()
            .await;
          tokio::time::sleep(Duration::from_secs(2)).await;

          let token_js = serde_json::to_string(&access_token).unwrap();
          let account_js = serde_json::to_string(&account_id).unwrap();

          let probe = format!(
            r#"(async () => {{
              const token = {token_js};
              const accountId = {account_js};
              const headers = {{
                accept: 'application/json',
                authorization: 'Bearer ' + token,
                'chatgpt-account-id': accountId,
              }};
              const urls = [
                '/backend-api/subscriptions',
                '/backend-api/subscriptions?include_recent_invoices=false',
                '/backend-api/accounts/check',
                '/backend-api/accounts/check/v4-2023-04-27',
                '/backend-api/accounts/check/v4-2023-04-27?timezone_offset_min=-420',
                '/backend-api/me',
                '/backend-api/settings/user',
                '/backend-api/payments/checkout',
                '/backend-api/subscriptions?limit=1',
              ];
              const out = {{}};
              for (const u of urls) {{
                try {{
                  const r = await fetch(u, {{ credentials: 'include', headers }});
                  const t = await r.text();
                  let body;
                  try {{ body = JSON.parse(t); }} catch {{ body = t.slice(0, 1000); }}
                  out[u] = {{ status: r.status, body }};
                }} catch (e) {{
                  out[u] = {{ error: String(e) }};
                }}
              }}
              // also DOM text signals
              const text = (document.body && (document.body.innerText || document.body.textContent) || '').toLowerCase();
              out.__dom = {{
                hasFreeTrial: /free trial|try plus free|try it free|start free trial|get plus free|free for \d/.test(text),
                hasPlus: /chatgpt plus|upgrade to plus|get plus|try plus/.test(text),
                snippet: text.slice(0, 800)
              }};
              return out;
            }})()"#
          );

          match eval_json(&page, &probe).await {
            Ok(v) => {
              eprintln!("=== PROBE RESULT ===");
              eprintln!("{}", serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()));
            }
            Err(e) => eprintln!("probe failed: {e}"),
          }

          let _ = BrowserRunner::instance()
            .kill_browser_process(handle, &launched)
            .await;
          std::process::exit(0);
        });
      });
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("run probe");
}
