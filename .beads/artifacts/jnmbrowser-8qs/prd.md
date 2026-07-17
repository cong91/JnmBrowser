# PRD: Auto-reg dual network (Nord CLI rotate / static proxy)

## Bead Metadata

```yaml
depends_on: []
parallel: false
conflicts_with: []
blocks: []
estimated_hours: 10
```

## Problem Statement

**WHEN** a user runs ChatGPT auto-registration in a batch (multiple CDKs × aliases), every account reuses the same host egress IP (or a single static `proxy_id`).

**THEN** Cloudflare / ChatGPT rate limits or geo poisoning can burn the batch after a few successes, and free-trial eligibility can degrade on a sticky bad exit IP.

**WHEN** the user already has a NordVPN Windows subscription and app installed (`NordVPN.exe` CLI: `-c` / `-d` / group / server).

**THEN** they need the auto-reg pipeline to optionally rotate system egress every N successful free-trial accounts, while still supporting the existing static proxy path when they choose proxy instead of Nord.

## Scope

### In Scope

- Dual network mode on auto-reg config: `none` | `proxy` | `nord` (mutually exclusive at start time).
- **Proxy mode:** keep existing `proxy_id` attach on ephemeral profiles (no mid-batch hop in v1).
- **Nord mode:** system-wide Windows NordVPN CLI connect/disconnect; rotate every **N successful** free-trial-eligible saves; verify egress IP changed via `ip_utils::fetch_public_ip`.
- Config fields (Rust + TS), engine success counter + rotate hook after browser cleanup, optional progress step messages.
- UI in `AccountRegistrationDialog`: network mode selector, proxy field when proxy, Nord group/server + rotateEveryN when nord.
- i18n keys for all 7 locales under `registration.*`.
- Docs update in `docs/auto-registration.md`.
- Unit tests for Nord CLI arg builder / config defaults / rotate counter logic (pure helpers).
- Best-effort Nord disconnect on task cancel if this task connected Nord.

### Out of Scope

- Nord WireGuard `.conf` export / in-app `vpn_id` rotation.
- Concurrent multi-browser auto-reg with system Nord (concurrency stays 1).
- Proxy pool multi-hop / rotating proxy list.
- Official Nord MCP server.
- Linux/macOS Nord CLI (Windows first; abstract behind helper for later).
- Changing free-trial detection logic.
- Inventory/export UI redesign (already shipped).

## Proposed Solution

### Network modes

| Mode | Behavior |
|------|----------|
| `none` | Ephemeral profile with `proxy_id=None`, `vpn_id=None`; host IP as today |
| `proxy` | Ephemeral profile with config `proxy_id`; no Nord CLI; no mid-batch rotate in v1 |
| `nord` | Ephemeral profile with both `proxy_id`/`vpn_id` None; before first account (optional connect) and after every N **successes**, run Nord disconnect → connect (group or server) → wait until public IP ≠ previous |

### Config shape (camelCase serde)

```text
RegistrationConfig {
  ...existing fields...
  networkMode: "none" | "proxy" | "nord"   // default "none" or derive: proxyId set → proxy
  proxyId?: string                         // required when mode=proxy
  rotateEveryN: u32                        // 0 = never rotate; default 2 for nord, ignored for proxy/none
  nordGroup?: string                       // e.g. "United States" → NordVPN.exe -c -g "..."
  nordServerName?: string                  // optional -n; if set prefer over group
  nordCliPath?: string                     // optional; default detect Program Files\NordVPN\NordVPN.exe
}
```

Validation at start:
- `proxy` without `proxyId` → error.
- `nord` + `proxyId` both set → reject or ignore proxyId (prefer explicit error).
- `rotateEveryN` only meaningful for `nord`.

### Engine hook

In `RegistrationEngine::run` after a successful `run_once` that was saved (free-trial gate already applied inside flow):

1. `success_count += 1`
2. If `network_mode == nord && rotate_every_n > 0 && success_count % rotate_every_n == 0`:
   - Ensure previous browser already cleaned up (already true after `run_once`)
   - Emit progress message (step `RotatingIp` or message-only)
   - Capture `old_ip = fetch_public_ip(None)`
   - `NordCli::disconnect()` then `NordCli::connect(group|server)`
   - Poll `fetch_public_ip` until changed or timeout (e.g. 60s)
   - Log/emit `old → new`; on timeout soft-warn and continue (or fail next accounts — prefer soft-warn + continue once, document)
3. On task cancel: if Nord was connected by this task, best-effort disconnect.

### Nord CLI helper

New module e.g. `src-tauri/src/auto_register/nord_cli.rs`:

- Resolve exe path (config override → `C:\Program Files\NordVPN\NordVPN.exe` → `where`/known install dirs).
- `connect` / `disconnect` via `std::process::Command` + Windows `CREATE_NO_WINDOW` (mirror `vpn_worker_runner` / `chromium_manager`).
- Args: `-d`; `-c`; `-c -g <group>`; `-c -n <server>`.
- Parse exit status; return structured errors.

### UI

`account-registration-dialog.tsx`:

- Radio/select: Network = None / Proxy / Nord.
- Proxy mode: existing proxy input.
- Nord mode: group input, optional server name, rotate every N (number ≥ 1).
- Progress card: human-readable rotate messages from backend `message` field (no step i18n map required if messages are localized later; v1 English messages from backend OK if UI shows `progress.message` — prefer i18n keys for static labels only).

### Docs

Update `docs/auto-registration.md`: Prerequisites (Nord app logged in), Usage dual-mode, Troubleshooting (CLI not found, IP not changing, free-trial geo).

## Requirements

### Functional

1. **WHEN** user starts auto-reg with `networkMode=proxy` and valid `proxyId`, **THEN** ephemeral profiles attach that proxy as today and no Nord CLI is invoked.
2. **WHEN** user starts auto-reg with `networkMode=nord` and `rotateEveryN=2`, **THEN** after every 2nd successful free-trial save the engine disconnects/reconnects Nord and verifies egress IP before the next account.
3. **WHEN** Nord CLI is missing or not logged in, **THEN** start or first rotate fails with a clear error message (not a hang).
4. **WHEN** user cancels a Nord-mode task that connected Nord, **THEN** engine best-effort disconnects Nord.
5. **WHEN** `networkMode=none`, **THEN** behavior matches pre-feature baseline (no proxy, no Nord).
6. **WHEN** UI is in Nord mode, **THEN** proxy field is hidden/disabled; when Proxy mode, Nord fields hidden/disabled.

### Non-Functional

- Windows-first Nord CLI; no new Cargo deps required (`reqwest` + `ip_utils` exist).
- Do not break free-trial gate, 2FA, or inventory export.
- Sequential engine only; do not enable concurrency with Nord.
- Match surrounding Rust/TS style; full i18n for user-facing UI strings.

## Success Criteria

- [ ] `RegistrationConfig` supports dual mode + rotateEveryN on Rust and TS with camelCase serde
- [ ] Nord CLI helper connects/disconnects with CREATE_NO_WINDOW on Windows
- [ ] Engine rotates after every N successes in nord mode; proxy/none unchanged
- [ ] IP change verified via `ip_utils::fetch_public_ip(None)` after rotate
- [ ] UI exposes mode + fields; payload matches backend
- [ ] All 7 locales have new `registration.*` keys (no empty non-English)
- [ ] Docs describe dual-mode and Nord prerequisites
- [ ] Unit tests cover pure Nord arg building / rotate-should-fire helper
- [ ] Lint/typecheck/rust tests green for touched areas

**Verify:**

```bash
pnpm lint:js
pnpm lint:rust
pnpm test:rust:unit
# Manual (optional): start batch networkMode=nord rotateEveryN=1, confirm Nord reconnect + different IP between two successes
```

## Technical Context

### Existing auto-reg

- Loop: `RegistrationEngine::run` (`src-tauri/src/auto_register/engine.rs` ~779–921) sequential CDK × alias × retries.
- Success save ~851–857 — **primary rotate insertion after cleanup inside `run_once` returns**.
- Launch: `launch_browser` passes `proxy_id` and `vpn_id=None` into `create_browser_profile_with_group`.
- Config: `src-tauri/src/auto_register/types.rs` `RegistrationConfig`; TS mirror `src/hooks/use-registration-events.ts`.
- UI: `src/components/account-registration-dialog.tsx` (proxy input only today).
- Commands: `start_auto_registration` in `commands.rs`.

### Reuse

- `ip_utils::fetch_public_ip` — `src-tauri/src/ip_utils.rs`
- Windows process flags — `vpn_worker_runner.rs`, `proxy_runner.rs`, `chromium_manager.rs`
- Host Nord CLI confirmed: `C:\Program Files\NordVPN\NordVPN.exe` v8.7.2 (`-c`, `-d`, `-g`, `-n`, `-i`)

### Architecture note

Nord mode is **system-wide**, not per-profile. It intentionally does **not** set profile `vpn_id` (WireGuard worker). Document side effects: host traffic (CDK HTTP, OTP) also exits via Nord while connected.

## Affected Files

### Backend

- `src-tauri/src/auto_register/types.rs` — config + optional `RegistrationStep::RotatingIp`
- `src-tauri/src/auto_register/nord_cli.rs` — **new** CLI wrapper
- `src-tauri/src/auto_register/mod.rs` — mod nord_cli
- `src-tauri/src/auto_register/engine.rs` — mode apply + rotate hook + cancel disconnect
- `src-tauri/src/auto_register/live_harness.rs` — config defaults
- `src-tauri/src/bin/auto_reg_live.rs` — optional CLI flags (if cheap)
- `src-tauri/src/ip_utils.rs` — reuse only (no change unless needed)

### Frontend

- `src/hooks/use-registration-events.ts` — config type
- `src/components/account-registration-dialog.tsx` — mode UI + payload
- `src/components/registration-progress-card.tsx` — only if step badge needed (prefer message-only)

### i18n

- `src/i18n/locales/{en,es,fr,ja,pt,ru,zh}.json` — network/nord/rotate keys

### Docs

- `docs/auto-registration.md`

## Risks

| Risk | Mitigation |
|------|------------|
| Nord CLI requires GUI login / not headless-auth | Fail fast with clear error; doc "must be logged in" |
| IP poll never changes (same server) | Disconnect, connect with group, longer timeout; optional force reconnect twice |
| System-wide VPN breaks other profiles mid-batch | Doc warning; disconnect on cancel; sequential only |
| Free-trial drop after region hop | Allow user-chosen group (US etc.); log plan/IP |
| Dirty main + large WIP auto-reg | Feature branch from current dirty main; don't revert inventory work |

## Open Questions

- Default `rotateEveryN` when nord selected: **2** (product default).
- Soft-continue vs hard-fail when IP does not change: **soft-warn once, continue** (v1).
- Connect Nord before first account if not already connected: **yes** at start of nord-mode run.

## Tasks

### Config dual-mode fields [backend]

`RegistrationConfig` (Rust + TS) exposes `networkMode`, `rotateEveryN`, Nord fields, keeps `proxyId`; defaults and validation reject invalid mode combinations.

**Metadata:**

```yaml
depends_on: []
parallel: true
conflicts_with: []
files:
  - src-tauri/src/auto_register/types.rs
  - src/hooks/use-registration-events.ts
  - src-tauri/src/auto_register/live_harness.rs
```

**Verification:**

- `cargo test --manifest-path src-tauri/Cargo.toml --lib` compiles types
- TS: `pnpm lint:js` includes hook typecheck

### Nord CLI helper [backend]

Windows NordVPN.exe resolve + connect/disconnect/group/server with CREATE_NO_WINDOW; unit tests for arg construction and path default.

**Metadata:**

```yaml
depends_on: []
parallel: true
conflicts_with: []
files:
  - src-tauri/src/auto_register/nord_cli.rs
  - src-tauri/src/auto_register/mod.rs
```

**Verification:**

- `cargo test --manifest-path src-tauri/Cargo.toml --lib nord_cli -- --nocapture`

### Engine rotate hook [backend]

Nord mode connects at run start; after every N successful saves rotates and verifies IP via `fetch_public_ip`; cancel best-effort disconnect; proxy/none paths unchanged.

**Metadata:**

```yaml
depends_on:
  - Config dual-mode fields
  - Nord CLI helper
parallel: false
conflicts_with: []
files:
  - src-tauri/src/auto_register/engine.rs
```

**Verification:**

- Unit test pure helper `should_rotate(success_count, every_n)`
- `cargo test --manifest-path src-tauri/Cargo.toml --lib auto_register`
- Manual optional: nord rotateEveryN=1 between two accounts

### Registration UI dual network [frontend]

Dialog selects none/proxy/nord, shows the right fields, sends correct invoke payload; no raw English strings.

**Metadata:**

```yaml
depends_on:
  - Config dual-mode fields
parallel: true
conflicts_with: []
files:
  - src/components/account-registration-dialog.tsx
  - src/hooks/use-registration-events.ts
```

**Verification:**

- `pnpm lint:js`
- Manual: toggle modes updates visible fields

### i18n network keys [i18n]

All seven locales include network mode, Nord group/server, rotateEveryN, errors/labels under `registration.*` with non-empty translations.

**Metadata:**

```yaml
depends_on:
  - Registration UI dual network
parallel: false
conflicts_with: []
files:
  - src/i18n/locales/en.json
  - src/i18n/locales/es.json
  - src/i18n/locales/fr.json
  - src/i18n/locales/ja.json
  - src/i18n/locales/pt.json
  - src/i18n/locales/ru.json
  - src/i18n/locales/zh.json
```

**Verification:**

- Grep new keys present in all 7 files
- `pnpm lint:js`

### Docs dual network [docs]

`docs/auto-registration.md` documents dual-mode, Nord prerequisites, rotate policy, troubleshooting.

**Metadata:**

```yaml
depends_on:
  - Engine rotate hook
parallel: true
conflicts_with: []
files:
  - docs/auto-registration.md
```

**Verification:**

- Doc section exists; no contradiction with free-trial/2FA sections
