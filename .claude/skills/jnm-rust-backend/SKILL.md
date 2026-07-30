---
name: jnm-rust-backend
description: Rust backend conventions for src-tauri in JnmBrowser — proxy binary prerequisite, data-dir isolation in tests, serial integration tests, clippy traps, singleton rule, Tauri v2 APIs. Use whenever editing Rust code or Rust tests.
---

# Rust backend conventions

## Prerequisite: proxy binary

`prebuild` / `pretauri:dev` / `precargo` hooks run `copy-proxy-binary.mjs` via pnpm, but **bare `cargo` invocations skip them**. Before any direct `cargo build/test/clippy`:

```powershell
pnpm copy-proxy-binary
```

## Tauri v2 only

Config schema `https://schema.tauri.app/config/2`. Never use Tauri v1 APIs (`app.emit_all`, v1 plugin imports, etc.). Plugins in use: deep-link, dialog, fs, log, opener (+ `tauri-plugin-macos-permissions-api`).

## Data dir & test isolation

- Runtime data dir comes from `app_dirs.rs`: prefer env **`JNMBROWSER_DATA_DIR`**, fallback `DONUTBROWSER_DATA_DIR`-style vars. Use it for process-level isolation in tests/scripts.
- `app_dirs::set_test_data_dir` is `#[cfg(test)]` **on the lib crate only** — integration tests under `src-tauri/tests/` **cannot call it**. There: set the env var + mark tests `#[serial_test::serial]` (env is process-global).
- Lib unit tests may use `set_test_data_dir` directly.

## Clippy & lint bar

CI-equivalent: `cargo clippy --all-targets --all-features -- -D warnings -D clippy::all` then `cargo fmt --all` (both via `pnpm lint:rust`).

Known traps:
- **`private-interfaces`**: a `pub` module/item must not expose `pub(crate)` types. Fix by making the type `pub`, keeping the module private, or re-exporting a public facade.
- New warnings are hard errors — fix, don't `#[allow]` without strong reason.
- Spellcheck (`typos`) covers `.rs` files too; allowlist in `_typos.toml` (locale JSON + camoufox data excluded).

## Singletons

If a struct has a global singleton, only access it inside a method while properly initializing it (per project AGENTS.md) — don't scatter lazy-static access across the codebase unless explicitly specified otherwise.

## Style

- One source of truth: don't duplicate logic across `chromium_manager.rs` / `camoufox_manager.rs` — share helpers.
- Match surrounding naming and comment density; don't add comments that restate the code.
- Legacy **Wayfern** naming exists in vars/API cache files — keep renames minimal and deliberate.

## Tests

```powershell
pnpm copy-proxy-binary
pnpm test:rust:unit   # cargo test --lib + donut_proxy_integration + vpn_integration
```

Other integration suites live in `src-tauri/tests/` (proxy, sync, vpn, recorder) — run the specific `cargo test --test <name>` when touching those areas. Sync server E2E: `pnpm test:sync-e2e` (Node harness, needs the `donut-sync` NestJS app).

## Binaries

`src-tauri/src/bin/` builds `donut-proxy` and `donut-daemon`. Changes there affect the packaged sidecars — verify `pnpm tauri build` paths if you touch them.
