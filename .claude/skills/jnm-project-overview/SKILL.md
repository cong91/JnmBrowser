---
name: jnm-project-overview
description: Orient in the JnmBrowser (DonutBrowser) codebase — architecture boundaries, module map, and which docs to read per feature area. Use when starting a task in an unfamiliar area or when unsure where something lives.
---

# JnmBrowser Project Overview

Anti-detect browser, AGPL-3.0. **Tauri v2 + Next.js 16 (App Router, webpack) + React 19**.
Tauri binary `JnmBrowser`, Rust lib `donutbrowser_lib`, identifier `com.jnmbrowser`.
Package manager: **pnpm** (Node 23 via `.nvmrc`). Frontend dev port: **12341** (never 3000).

## Hard architecture boundaries

- Frontend (`src/`) talks to Rust **only** via `invoke()` / `listen()` from `@tauri-apps/api`. Never do browser process control or profile file I/O in React.
- Every user-facing backend API = a `#[tauri::command]` fn registered in `src-tauri/src/lib.rs` inside `tauri::generate_handler![...]` (140+ commands) **plus** a frontend call site. Registered-but-unused commands fail CI (`pnpm check-unused-commands`).
- Browser kernels: Chromium/fingerprint browsers → `chromium_manager.rs`; Camoufox (Firefox) → `camoufox_manager.rs`. Legacy **"Wayfern"** naming still appears in some vars/API cache files — do not bulk-rename casually.
- Proxy features require the local `donut-proxy` binary: run `pnpm copy-proxy-binary` before bare `cargo` commands.
- `donut-sync/` is a **separate NestJS app** with its own `package.json`, Biome and tsc — lint scripts cover both trees (`pnpm lint:js` cds into it).

## Module map (src-tauri/src/)

| Area | Location |
|------|----------|
| Command registration | `lib.rs` |
| Profile launch/kill orchestration | `browser_runner.rs`, `browser.rs` |
| Chromium / Camoufox management | `chromium_manager.rs`, `camoufox_manager.rs`, `camoufox/` |
| Profile CRUD | `profile/` (`manager.rs`, `types.rs`) |
| Proxy | `proxy_manager.rs`, `proxy_server.rs`, `proxy_storage.rs`, `proxy_runner.rs` |
| REST API / MCP | `api_server.rs` (utoipa + axum), `mcp_server.rs` |
| Cloud sync | `sync/` (engine, encryption, manifest, scheduler) + `synchronizer.rs` |
| VPN (WireGuard) | `vpn/`, `vpn_worker_runner.rs` |
| Action recorder | `recorder/` (capture, player, recipes) |
| Account automation | `auto_service/`, `email/`, `sms/` |
| Data dir | `app_dirs.rs` (`JNMBROWSER_DATA_DIR`, fallback `DONUTBROWSER_*`) |
| Daemon / tray | `daemon/`, `src/bin/` (`donut-proxy`, `donut-daemon`) |

Frontend mirrors this: `src/components/*-dialog.tsx`, `src/hooks/use-*-events.ts`, shared TS types in `src/types.ts`, utilities in `src/lib/`.

## Sensitive areas — read the doc BEFORE editing

| Area | Doc |
|------|-----|
| Auto-registration / login automation | `docs/auto-registration.md`, `docs/research/` |
| Action recorder | `src-tauri/src/recorder/`, `src/hooks/use-recorder-session.ts` |
| Sync / self-hosting | `docs/self-hosting-donut-sync.md`, `src-tauri/src/sync/` |
| MCP server | `src-tauri/src/mcp_server.rs` + root `MCP_*.md` plan books (only when the task references them) |
| Active ZCode plans | `.zcode/plans/` — check for updates before continuing any plan |

## Finding things fast

- Command call site: search `invoke("command_name"` in `src/`.
- Command handler: search `fn command_name` in `src-tauri/src/`.
- Event producer/consumer: search the event string, e.g. `"profiles-changed"`, in both trees.
- Locale key: search `"key":` in `src/i18n/locales/en.json` first (English is the source of truth).

## Cross-platform

App ships on Windows/macOS/Linux. Dev environment here is **Windows + PowerShell** — quote paths, use `;` to sequence commands, avoid bash-isms outside `lint-staged` hooks.
