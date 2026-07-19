//! OpenAI Sentinel proof-of-work token generation.
//! Ported from codex-console's sentinel_token.py.
//! Uses FNV-1a 32-bit hash + PoW solving to generate the required token
//! for the create_account API call.

use base64::Engine;
use rand::Rng;
use std::time::Instant;

/// FNV-1a 32-bit hash with MurmurHash3-style finalizer.
fn fnv1a_32(data: &[u8]) -> u32 {
  let mut hash: u32 = 0x811c9dc5;
  for &byte in data {
    hash ^= byte as u32;
    hash = hash.wrapping_mul(0x01000193);
  }
  // MurmurHash3-style avalanche finalizer
  hash ^= hash >> 16;
  hash = hash.wrapping_mul(0x85ebca6b);
  hash ^= hash >> 13;
  hash = hash.wrapping_mul(0xc2b2ae35);
  hash ^= hash >> 16;
  hash
}

/// Encode a JSON-serialized array as base64 (matching OpenAI's SDK `E()` function).
fn base64_encode_config(config: &[serde_json::Value]) -> String {
  let json = serde_json::to_string(config).unwrap_or_default();
  base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
}

/// Fetch the sentinel challenge from OpenAI's sentinel backend.
async fn fetch_sentinel_challenge(
  client: &reqwest::Client,
  device_id: &str,
) -> Result<(String, String, String), String> {
  let mut rng = rand::rng();
  let requirements_config = vec![
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(0),           // nonce
    serde_json::json!([rng.next_u32(), rng.next_u32()]),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(format!("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(device_id),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
  ];
  let encoded = base64_encode_config(&requirements_config);
  let req_token = format!("gAAAAAB{encoded}");

  let body = serde_json::json!({
    "p": req_token,
    "t": "",
    "c": "",
    "id": device_id,
    "flow": "authorize_continue",
  });

  let resp = client
    .post("https://sentinel.openai.com/backend-api/sentinel/req")
    .header("content-type", "application/json")
    .json(&body)
    .send()
    .await
    .map_err(|e| format!("Sentinel challenge request failed: {e}"))?;

  let data: serde_json::Value = resp
    .json()
    .await
    .map_err(|e| format!("Sentinel challenge parse: {e}"))?;

  let server_token = data["token"].as_str().unwrap_or("").to_string();
  let pow = &data["proofofwork"];
  let seed = pow["seed"].as_str().unwrap_or("").to_string();
  let difficulty = pow["difficulty"].as_str().unwrap_or("0").to_string();

  Ok((server_token, seed, difficulty))
}

/// Solve the proof-of-work challenge.
fn solve_pow(seed: &str, difficulty: &str, max_attempts: u32) -> String {
  let mut rng = rand::rng();
  let start = Instant::now();

  for nonce in 0..max_attempts {
    let config = vec![
      serde_json::json!(rng.next_u32()),
      serde_json::json!(rng.next_u32()),
      serde_json::json!(rng.next_u32()),
      serde_json::json!(nonce),
      serde_json::json!([rng.next_u32(), rng.next_u32()]),
      serde_json::json!(rng.next_u32()),
      serde_json::json!("Mozilla/5.0"),
      serde_json::json!(rng.next_u32()),
      serde_json::json!(start.elapsed().as_millis() as u32),
      serde_json::json!(rng.next_u32()),
      serde_json::json!(rng.next_u32()),
      serde_json::json!(rng.next_u32()),
      serde_json::json!(rng.next_u32()),
      serde_json::json!(rng.next_u32()),
      serde_json::json!(""),
      serde_json::json!(rng.next_u32()),
      serde_json::json!(rng.next_u32()),
      serde_json::json!(rng.next_u32()),
      serde_json::json!(rng.next_u32()),
    ];
    let encoded = base64_encode_config(&config);
    let combined = format!("{seed}{encoded}");
    let hash = fnv1a_32(combined.as_bytes());
    let hex = format!("{hash:08x}");

    // Check if first hex chars meet difficulty threshold
    let diff_char = difficulty.chars().next().unwrap_or('0');
    if hex.starts_with(|c: char| c <= diff_char) {
      return format!("gAAAAAB{encoded}");
    }
  }

  // Failed — return requirements token as fallback
  let fallback_config = vec![
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(max_attempts),
    serde_json::json!([rng.next_u32(), rng.next_u32()]),
    serde_json::json!(rng.next_u32()),
    serde_json::json!("Mozilla/5.0"),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(start.elapsed().as_millis() as u32),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(""),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
    serde_json::json!(rng.next_u32()),
  ];
  let encoded = base64_encode_config(&fallback_config);
  format!("gAAAAAB{encoded}")
}

/// Build a complete sentinel token for the `openai-sentinel-token` header.
/// Returns None if the sentinel backend is unreachable.
pub async fn build_sentinel_token(device_id: &str, flow: &str) -> Option<String> {
  let client = reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(15))
    .build()
    .ok()?;

  let (server_token, seed, difficulty) = match fetch_sentinel_challenge(&client, device_id).await {
    Ok(v) => v,
    Err(_) => return None,
  };

  let pow_token = if seed.is_empty() {
    // No PoW required — use requirements
    String::new()
  } else {
    solve_pow(&seed, &difficulty, 500_000)
  };

  let sentinel = serde_json::json!({
    "p": pow_token,
    "t": "",
    "c": server_token,
    "id": device_id,
    "flow": flow,
  });

  Some(sentinel.to_string())
}
