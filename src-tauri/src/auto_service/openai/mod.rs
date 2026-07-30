//! OpenAI / ChatGPT / Codex auto-service provider.
//!
//! - `register` — create new free-trial ChatGPT accounts (CDK + SMS flow)
//! - `login`    — log into existing accounts via Codex OAuth PKCE flow
//! - `account_checker` — check existing accounts (email/pass only, stop at 2FA)

pub mod account_checker;
pub(crate) mod browser;
pub mod chatgpt_auth;
pub mod chatgpt_two_factor;
pub mod login;
pub mod register;
pub(crate) mod turnstile_bypass;
pub mod two_factor_backfill;
