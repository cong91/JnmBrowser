# Project Guidelines

**JnmBrowser** (a.k.a. DonutBrowser) — an open-source anti-detect browser built with Tauri v2 + Next.js.
- Tauri binary: `JnmBrowser`, Rust lib: `donutbrowser_lib`
- Product identifier: `com.jnmbrowser`
- Current version: 0.22.9
- License: AGPL-3.0
- Package manager: **pnpm** (Node 23 via `.nvmrc` / `.node-version`)

## Repository Structure

```
JnmBrowser/
├── src/                              # Next.js frontend (App Router, Turbopack)
│   ├── app/                          # App router (page.tsx, layout.tsx)
│   ├── components/                   # React components (dialogs, tables, UI)
│   ├── hooks/                        # Event-driven React hooks (Tauri listen/invoke)
│   ├── i18n/locales/                 # Translations (en, es, fr, ja, pt, ru, zh)
│   ├── lib/                          # Utilities (themes, toast, browser-utils)
│   ├── styles/                       # Global CSS / Tailwind entry
│   └── types.ts                      # Shared TypeScript interfaces
├── src-tauri/                        # Rust backend (Tauri v2)
│   ├── src/
│   │   ├── lib.rs                    # Tauri command registration (100+ commands)
│   │   ├── browser_runner.rs         # Profile launch/kill orchestration
│   │   ├── browser.rs                # Browser trait & launch logic
│   │   ├── chromium_manager.rs       # Chromium / fingerprint browser management
│   │   ├── camoufox_manager.rs       # Camoufox (Firefox) browser management
│   │   ├── camoufox/                 # Camoufox fingerprint engine data
│   │   ├── profile/                  # Profile CRUD (manager.rs, types.rs)
│   │   ├── proxy_manager.rs          # Proxy lifecycle & connection testing
│   │   ├── proxy_server.rs           # Local proxy binary logic
│   │   ├── proxy_storage.rs          # Proxy config persistence (JSON files)
│   │   ├── api_server.rs             # REST API (utoipa + axum)
│   │   ├── mcp_server.rs             # MCP protocol server
│   │   ├── sync/                     # Cloud sync (engine, encryption, manifest, scheduler)
│   │   ├── vpn/                      # WireGuard tunnels
│   │   ├── recorder/                 # Action recorder (capture, player, recipes)
│   │   ├── auto_register/            # ChatGPT auto-registration engine
│   │   ├── email/                    # Gmail CDK / OTP helpers
│   │   ├── events/                   # Backend event helpers
│   │   ├── app_dirs.rs               # App data dir (JNMBROWSER_DATA_DIR / DONUTBROWSER_*)
│   │   ├── human_typing.rs           # Human-like typing helpers
│   │   ├── settings_manager.rs       # App settings persistence
│   │   ├── cookie_manager.rs         # Cookie import/export
│   │   ├── extension_manager.rs      # Browser extension management
│   │   ├── group_manager.rs          # Profile group management
│   │   ├── synchronizer.rs           # Real-time profile synchronizer
│   │   ├── daemon/                   # Background daemon + tray icon
│   │   └── bin/                      # donut-proxy, donut-daemon binaries
│   ├── tests/                        # Integration tests (proxy, sync, vpn, recorder)
│   └── Cargo.toml
├── donut-sync/                       # NestJS sync server (self-hostable)
├── docs/                             # Feature docs (auto-registration, self-hosting)
├── scripts/                          # Build / test harness scripts
├── flake.nix                         # Nix development environment
└── .github/workflows/                # CI/CD (build-installers.yml)
```

## Architecture boundaries

- **Frontend (`src/`)** talks to Rust only via Tauri `invoke` / `listen`. Do not put browser process control or profile file I/O in React.
- **Rust commands** are registered in `src-tauri/src/lib.rs`. New user-facing backend APIs need a command + frontend call site; dead commands fail `pnpm check-unused-commands`.
- **Browser kernels**: Chromium path is primarily `chromium_manager.rs`; Camoufox is `camoufox_manager.rs`. Legacy “Wayfern” naming still appears in some vars/API cache files — treat carefully when renaming.
- **Proxy**: local `donut-proxy` binary is required for proxy features; copy via `pnpm copy-proxy-binary` before bare `cargo` runs.
- **donut-sync**: separate NestJS app with its own `package.json` / Biome / tsc. Lint/typecheck scripts cover both trees.
- **Sensitive areas — read first**:
  - Action recorder: `src-tauri/src/recorder/`, frontend hooks like `use-recorder-session.ts`
  - Auto-register: `src-tauri/src/auto_register/`, `docs/auto-registration.md`
  - Sync: `src-tauri/src/sync/`, `docs/self-hosting-donut-sync.md`
  - MCP: `src-tauri/src/mcp_server.rs` (+ root MCP plan docs when working from a plan)

## Build and Dev Commands

| Command | Description |
|---------|-------------|
| `pnpm dev` | Start Next.js dev server on port **12341** (Turbopack) |
| `pnpm tauri dev` | Full Tauri dev (proxy binary + frontend + Rust) |
| `pnpm build` | Build Next.js frontend → `dist/` |
| `pnpm tauri build` | Build full desktop app |
| `pnpm format` | Auto-fix JS (Biome) + Rust (clippy --fix + fmt) |
| `pnpm lint` | Lint JS (Biome + tsc), Rust (clippy), spellcheck (typos) |
| `pnpm lint:js` | Biome + tsc for `src/` and `donut-sync/` |
| `pnpm lint:rust` | clippy `-D warnings -D clippy::all` + fmt |
| `pnpm test` | Rust unit tests + sync E2E harness |
| `pnpm test:rust:unit` | `cargo test --lib` + proxy + vpn integration tests |
| `pnpm test:sync-e2e` | Sync server E2E (`scripts/sync-test-harness.mjs`) |
| `pnpm check-unused-commands` | Fail if unused Tauri commands exist |
| `pnpm copy-proxy-binary` | Copy `donut-proxy` for local cargo/dev |
| `pnpm shadcn:add` | Add a shadcn/ui component |

- After meaningful changes, run `pnpm format && pnpm lint && pnpm test` before finishing a task
- JS linting uses **Biome 2.x** (not ESLint/Prettier) — `biome.json`
- Rust linting: `cargo clippy --all-targets --all-features -- -D warnings -D clippy::all`
- Spellcheck: [typos](https://github.com/crate-ci/typos); allowlist in `_typos.toml` (locale JSON + camoufox data excluded)

## Code Quality

- Don't leave comments that don't add value
- Don't duplicate logic; keep one source of truth
- User-facing copy changes must update **all** locale files
- Match surrounding naming, comment density, and idioms

## Translations (mandatory)

- Never write user-facing strings as raw English literals in JSX, toasts, dialogs, buttons, placeholders, table headers, tooltips, or empty states. Always use `t("namespace.key")` from `useTranslation()`.
- Applies to every component under `src/`. Import `useTranslation` when missing.
- New strings → add the key to **all seven** locales in `src/i18n/locales/` (`en`, `es`, `fr`, `ja`, `pt`, `ru`, `zh`). English-only is incomplete.
- Reuse existing keys (`common.buttons.*`, `common.labels.*`, `createProfile.*`, etc.) before new namespaces. Check `en.json` first.
- Excluded: `console.log/warn/error`, dev-only labels, internal IDs, CSS classes, type names. If it might render to users, translate it.
- **Never use `t(key, "fallback")`.** Every key must exist in every locale before the call site lands. Fallbacks hide missing translations.
- Empty-string values in non-English locales are forbidden. Prefer one interpolated key (`t("foo.bar", { name })`) over prefix/suffix splits.

## Singletons

- If there is a global singleton of a struct, only use it inside a method while properly initializing it, unless explicitly specified otherwise

## UI Theming

- Never use hardcoded Tailwind color classes (e.g. `text-red-500`, `bg-green-600`). Use theme CSS variables from `src/lib/themes.ts`
- Semantic classes: `background`, `foreground`, `card`, `popover`, `primary`, `secondary`, `muted`, `accent`, `destructive`, `success`, `warning`, `border`, `chart-1`…`chart-5` (and `*-foreground` pairs)
- Tailwind usage: `bg-success`, `text-destructive`, `border-warning`, or opacity variants like `bg-destructive/10`
- Path aliases: `@/components`, `@/hooks`, `@/lib`, `@/components/ui` (see `components.json`)

## Frontend / Tauri event gotchas

- **`listen()` is async.** Always `const unlisten = await listen(...)` **before** launching a profile or starting work that emits the event. Fire-and-forget `void listen().then(...)` then immediately launch can miss `profile-running-changed` and similar events.
- Launch helpers that may cancel (e.g. window-resize warning) should return a boolean / distinguish cancel from failure so UI does not toast `launchFailed` on intentional cancel.
- Profile action items often use `ActionItem { icon, label, onClick, disabled, hidden }` — mirror existing Launch-with-Sync / Launch-with-Record patterns for chromium/camoufox instead of inventing parallel controls.
- Bulk-selection table already has a checkbox column — do not add a second checkbox for record/select flows.

## Rust / test gotchas

- **Proxy binary**: `prebuild` / `pretauri:dev` / `precargo` call `copy-proxy-binary.mjs`. Bare `cargo` needs `pnpm copy-proxy-binary` first.
- **Tauri v2**: schema `https://schema.tauri.app/config/2`. Do not use Tauri v1 APIs.
- **Dev port**: frontend is `12341`, not 3000. Tauri `devUrl` matches this.
- **Data dir isolation**: prefer `JNMBROWSER_DATA_DIR` (fallback `DONUTBROWSER_*`) for process-level isolation. Lib-only `app_dirs::set_test_data_dir` is `#[cfg(test)]` on the **lib** crate — **integration tests** in `src-tauri/tests/` cannot call it; use the env var + `serial_test::serial` (env is process-global).
- **clippy `private-interfaces`**: a `pub` module/item must not expose `pub(crate)` types. Either make the type `pub`, keep the module private, or re-export a public facade.
- **Unused Tauri commands**: `pnpm check-unused-commands` / `test_no_unused_tauri_commands` must stay green.
- **Husky**: `pnpm prepare` installs git hooks.

## Docs agents should consult when relevant

| Area | Doc |
|------|-----|
| Auto-registration | `docs/auto-registration.md` |
| Self-host sync | `docs/self-hosting-donut-sync.md` |
| Active ZCode plans | `.zcode/plans/` (check for updates before continuing a plan) |
| Root plan books | MCP / kernel / Wayfern markdown plans at repo root — only when the active task references them |

## Proprietary Changes

This project is licensed under AGPL-3.0. Anyone is permitted to study and modify the code.

# Completion Rule

- 当且仅当一个完整计划的全部事项都已执行完成，并且该阶段已经真正收口时，输出：
  `plan is ok, 我们这阶段胜利了，请金木查看`
- 不要在单个步骤完成、局部修改完成、阶段中间状态、或普通回合回复时输出这句话。

- 请按照主线修改，如果过程中有问题出现，修复后，请及时回到本计划主线，不要偏离

- 请您分步骤执行此计划，你要严格按照本计划书执行

- 本计划全部完成的时候：请输出 `plan is ok, 我们这阶段胜利了，请金木查看` （我用AI程序托管监听，你发送这个代表告诉程序不需要执行了，这个计划书彻底完成了），请您严格确认是否完成，不要偏离主线。

- 程序托管的时候，程序如果觉得你的指令没问题会发送 `好的，在不偏离'XXX计划书'的前提开始继续执行` ,你要严格按照计划书执行（如果遇到不可避免的问题，你可以提出修改，但确保修改后及时回到主线，在一个功能上不要过度优化）

- 你要随时查看对应计划书的内容，我怕你忘记，而且有时候我会手动更新对应的计划书，你要自己判断计划书是否发生了更新，获取最新需求，并强化记忆，避免偏离。
- 如果存在“总主线计划书”，执行任何阶段前应先查看总主线计划书，再查看当前阶段计划书。
- 如果按照任务清单执行，并确保每完成一部分进行打勾更新任务。
