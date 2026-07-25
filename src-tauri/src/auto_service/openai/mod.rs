//! OpenAI / ChatGPT / Codex auto-service provider.
//!
//! - `register` — create new free-trial ChatGPT accounts (CDK + SMS flow)
//! - `login`    — log into existing accounts via Codex OAuth PKCE flow

pub mod chatgpt_auth;
pub mod chatgpt_two_factor;
pub mod login;
pub mod register;
pub mod two_factor_backfill;
