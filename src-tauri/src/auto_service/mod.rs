//! Multi-service auto-registration / auto-login framework.
//!
//! Layout:
//! - `common/`   — shared utilities (TOTP, Nord CLI helpers, …)
//! - `openai/`   — OpenAI / ChatGPT / Codex provider
//!   - `register/` — account auto-registration
//!   - `login/`    — existing-account auto-login (Codex OAuth)
//!
//! Future providers (e.g. Claude, Gemini) go under their own sub-module
//! mirroring the openai layout.

pub mod common;
pub mod openai;
