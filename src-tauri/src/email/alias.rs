use rand::Rng;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use super::EmailServiceError;

pub const MAX_ALIASES_PER_EMAIL: usize = 6;

#[derive(Default)]
pub struct EmailAliasGenerator {
  used_aliases: Mutex<HashMap<String, HashSet<String>>>,
}

impl EmailAliasGenerator {
  pub fn generate(&self, base_email: &str) -> Result<String, EmailServiceError> {
    let email = base_email.trim();
    let (username, domain) = email
      .split_once('@')
      .filter(|(username, domain)| !username.is_empty() && !domain.is_empty())
      .ok_or_else(|| EmailServiceError::EmailInvalid(format!("invalid base email: {email}")))?;

    let mut aliases = self.used_aliases.lock().unwrap();
    let used = aliases.entry(email.to_string()).or_default();
    if used.len() >= MAX_ALIASES_PER_EMAIL {
      return Err(EmailServiceError::EmailInvalid(format!(
        "maximum {MAX_ALIASES_PER_EMAIL} aliases already used for {email}"
      )));
    }

    let mut rng = rand::rng();
    for _ in 0..50 {
      let suffix: String = (0..5)
        .map(|_| {
          let idx = (rng.next_u32() % 36) as u8;
          if idx < 10 {
            (b'0' + idx) as char
          } else {
            (b'a' + (idx - 10)) as char
          }
        })
        .collect();
      if used.insert(suffix.clone()) {
        return Ok(format!("{username}+{suffix}@{domain}"));
      }
    }

    Err(EmailServiceError::Internal(
      "failed to generate unique alias after 50 attempts".into(),
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn generates_unique_aliases_and_preserves_domain() {
    let generator = EmailAliasGenerator::default();
    let base = "user@example.com";
    let aliases: HashSet<_> = (0..MAX_ALIASES_PER_EMAIL)
      .map(|_| generator.generate(base).unwrap())
      .collect();
    assert_eq!(aliases.len(), MAX_ALIASES_PER_EMAIL);
    assert!(aliases.iter().all(|alias| alias.starts_with("user+")));
    assert!(aliases.iter().all(|alias| alias.ends_with("@example.com")));
  }

  #[test]
  fn enforces_alias_budget() {
    let generator = EmailAliasGenerator::default();
    let base = "user@example.com";
    for _ in 0..MAX_ALIASES_PER_EMAIL {
      generator.generate(base).unwrap();
    }
    assert_eq!(
      generator.generate(base).unwrap_err().to_string(),
      "Email invalid: maximum 6 aliases already used for user@example.com"
    );
  }
}
