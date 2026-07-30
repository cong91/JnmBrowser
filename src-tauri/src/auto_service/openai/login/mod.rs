pub mod commands;
pub mod engine;
pub mod execution;
pub mod oauth;
pub mod pkce;
pub mod store;
pub mod sub2api;
pub mod task;
pub mod types;
mod worker_runtime;

pub(crate) fn safe_browser_url_for_log(raw: &str) -> String {
  let Ok(parsed) = url::Url::parse(raw) else {
    return "[invalid-url]".into();
  };
  let mut safe = format!(
    "{}://{}",
    parsed.scheme(),
    parsed.host_str().unwrap_or("unknown")
  );
  if let Some(port) = parsed.port() {
    safe.push_str(&format!(":{port}"));
  }
  safe.push_str(parsed.path());
  safe
}

pub(crate) fn sanitize_browser_urls_for_log(text: &str) -> String {
  let mut output = String::with_capacity(text.len());
  let mut rest = text;
  loop {
    let http = rest.find("http://");
    let https = rest.find("https://");
    let start = match (http, https) {
      (Some(left), Some(right)) => left.min(right),
      (Some(index), None) | (None, Some(index)) => index,
      (None, None) => {
        output.push_str(rest);
        break;
      }
    };
    output.push_str(&rest[..start]);
    let candidate = &rest[start..];
    let end = candidate
      .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>'))
      .unwrap_or(candidate.len());
    let raw = &candidate[..end];
    let trimmed = raw.trim_end_matches([')', ']', '}', ',', ';']);
    let suffix = &raw[trimmed.len()..];
    output.push_str(&safe_browser_url_for_log(trimmed));
    output.push_str(suffix);
    rest = &candidate[end..];
  }
  output
}

pub use engine::LoginEngine;
pub use types::{LoginConfig, LoginCredential, LoginNetworkMode, LoginResult};

#[cfg(test)]
mod tests {
  use super::sanitize_browser_urls_for_log;

  #[test]
  fn stored_step_log_url_sanitizer_removes_callback_query() {
    let sanitized = sanitize_browser_urls_for_log(
      "[04:24:07] callback url=http://localhost:1455/auth/callback?code=secret&state=secret",
    );

    assert_eq!(
      sanitized,
      "[04:24:07] callback url=http://localhost:1455/auth/callback"
    );
    assert!(!sanitized.contains("secret"));
  }
}
