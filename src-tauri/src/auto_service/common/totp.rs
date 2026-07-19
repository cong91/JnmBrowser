//! Minimal RFC 6238 TOTP (HMAC-SHA1, 30s, 6 digits) for ChatGPT authenticator setup.

use ring::hmac;

/// Generate a 6-digit TOTP code for the current Unix time.
pub fn generate_totp_now(secret_base32: &str) -> Result<String, String> {
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map_err(|e| format!("system time: {e}"))?
    .as_secs();
  generate_totp(secret_base32, now, 30, 6)
}

/// Generate a TOTP code for an explicit Unix timestamp.
pub fn generate_totp(
  secret_base32: &str,
  unix_secs: u64,
  time_step: u64,
  digits: u32,
) -> Result<String, String> {
  let key = decode_base32(secret_base32)?;
  if key.is_empty() {
    return Err("empty TOTP secret".into());
  }
  let counter = unix_secs / time_step.max(1);
  let mut msg = [0u8; 8];
  msg.copy_from_slice(&counter.to_be_bytes());

  let signing_key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &key);
  let tag = hmac::sign(&signing_key, &msg);
  let hash = tag.as_ref();
  if hash.len() < 20 {
    return Err("HMAC-SHA1 produced short digest".into());
  }

  let offset = (hash[hash.len() - 1] & 0x0f) as usize;
  if offset + 3 >= hash.len() {
    return Err("invalid dynamic truncation offset".into());
  }
  let bin_code = ((hash[offset] as u32 & 0x7f) << 24)
    | ((hash[offset + 1] as u32) << 16)
    | ((hash[offset + 2] as u32) << 8)
    | (hash[offset + 3] as u32);

  let modulo = 10u32.pow(digits.min(9));
  let code = bin_code % modulo;
  Ok(format!("{code:0digits$}", digits = digits as usize))
}

/// Normalize a copied authenticator secret (spaces/dashes) and validate base32 charset.
pub fn normalize_secret(raw: &str) -> Result<String, String> {
  let cleaned: String = raw
    .chars()
    .filter(|c| !c.is_whitespace() && *c != '-' && *c != '=')
    .map(|c| c.to_ascii_uppercase())
    .collect();
  if cleaned.len() < 16 {
    return Err(format!("secret too short: {} chars", cleaned.len()));
  }
  if !cleaned.chars().all(|c| matches!(c, 'A'..='Z' | '2'..='7')) {
    return Err("secret is not valid base32".into());
  }
  // Round-trip decode to ensure padding-safe secrets work.
  let _ = decode_base32(&cleaned)?;
  Ok(cleaned)
}

fn decode_base32(input: &str) -> Result<Vec<u8>, String> {
  let cleaned: String = input
    .chars()
    .filter(|c| !c.is_whitespace() && *c != '-' && *c != '=')
    .map(|c| c.to_ascii_uppercase())
    .collect();

  let mut bits: u32 = 0;
  let mut bit_count: u32 = 0;
  let mut out = Vec::with_capacity(cleaned.len() * 5 / 8);

  for ch in cleaned.chars() {
    let val = match ch {
      'A'..='Z' => ch as u32 - 'A' as u32,
      '2'..='7' => ch as u32 - '2' as u32 + 26,
      _ => return Err(format!("invalid base32 char: {ch}")),
    };
    bits = (bits << 5) | val;
    bit_count += 5;
    if bit_count >= 8 {
      bit_count -= 8;
      out.push((bits >> bit_count) as u8);
      bits &= (1 << bit_count) - 1;
    }
  }
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rfc6238_sha1_vectors() {
    // Secret "12345678901234567890" as ASCII bytes, base32-encoded.
    // Common test secret: JBSWY3DPEHPK3PXP ("Hello!" demo) is not the RFC vector.
    // RFC 6238 Appendix B uses ASCII key "12345678901234567890".
    let secret = base32_encode(b"12345678901234567890");
    // At T=59, RFC expected OTP is 94287082 for 8 digits; we use 6 digits → last 6.
    let code8 = generate_totp(&secret, 59, 30, 8).unwrap();
    assert_eq!(code8, "94287082");
    let code6 = generate_totp(&secret, 59, 30, 6).unwrap();
    assert_eq!(code6, "287082");
  }

  #[test]
  fn normalize_strips_spaces() {
    let s = normalize_secret("abcd efgh ijkl mnop qrst uvwx").unwrap();
    assert_eq!(s, "ABCDEFGHIJKLMNOPQRSTUVWX");
  }

  fn base32_encode(data: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bits: u32 = 0;
    let mut bit_count: u32 = 0;
    let mut out = String::new();
    for &b in data {
      bits = (bits << 8) | b as u32;
      bit_count += 8;
      while bit_count >= 5 {
        bit_count -= 5;
        let idx = ((bits >> bit_count) & 0x1f) as usize;
        out.push(ALPHA[idx] as char);
        bits &= (1 << bit_count) - 1;
      }
    }
    if bit_count > 0 {
      let idx = ((bits << (5 - bit_count)) & 0x1f) as usize;
      out.push(ALPHA[idx] as char);
    }
    out
  }
}
