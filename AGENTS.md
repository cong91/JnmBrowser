# Project Guidelines

**JnmBrowser** (a.k.a. DonutBrowser) — an open-source anti-detect browser built with Tauri v2 + Next.js.
- Tauri binary: `JnmBrowser`, Rust lib: `donutbrowser_lib`
- Current version: 0.22.9
- License: AGPL-3.0

## Repository Structure

```
JnmBrowser/
├── src/                              # Next.js frontend (App Router, Turbopack)
│   ├── app/                          # App router (page.tsx, layout.tsx)
│   ├── components/                   # 50+ React components (dialogs, tables, UI)
│   ├── hooks/                        # Event-driven React hooks
│   ├── i18n/locales/                 # Translations (en, es, fr, ja, pt, ru, zh)
│   ├── lib/                          # Utilities (themes, toast, browser-utils)
│   └── types.ts                      # Shared TypeScript interfaces
├── src-tauri/                        # Rust backend (Tauri v2)
│   ├── src/
│   │   ├── lib.rs                    # Tauri command registration (100+ commands)
│   │   ├── browser_runner.rs         # Profile launch/kill orchestration
│   │   ├── browser.rs               # Browser trait & launch logic
│   │   ├── profile/                  # Profile CRUD (manager.rs, types.rs)
│   │   ├── proxy_manager.rs         # Proxy lifecycle & connection testing
│   │   ├── proxy_server.rs          # Local proxy binary (donut-proxy)
│   │   ├── proxy_storage.rs         # Proxy config persistence (JSON files)
│   │   ├── api_server.rs            # REST API (utoipa + axum)
│   │   ├── mcp_server.rs            # MCP protocol server
│   │   ├── sync/                    # Cloud sync (engine, encryption, manifest, scheduler)
│   │   ├── vpn/                     # WireGuard tunnels
│   │   ├── camoufox/                # Camoufox fingerprint engine (Bayesian network)
│   │   ├── wayfern_manager.rs       # Wayfern (Chromium) browser management
│   │   ├── camoufox_manager.rs      # Camoufox (Firefox) browser management
│   │   ├── downloader.rs           # Browser binary downloader
│   │   ├── extraction.rs           # Archive extraction (zip, tar, dmg, msi)
│   │   ├── settings_manager.rs     # App settings persistence
│   │   ├── cookie_manager.rs       # Cookie import/export
│   │   ├── extension_manager.rs    # Browser extension management
│   │   ├── group_manager.rs        # Profile group management
│   │   ├── synchronizer.rs         # Real-time profile synchronizer
│   │   ├── daemon/                 # Background daemon + tray icon (currently disabled)
│   │   └── cloud_auth.rs           # Cloud authentication
│   ├── tests/                      # Integration tests (proxy, sync, vpn)
│   └── Cargo.toml                  # Rust dependencies
├── donut-sync/                     # NestJS sync server (self-hostable)
│   └── src/                        # Controllers, services, auth, S3 sync
├── docs/                           # Documentation (self-hosting guide)
├── flake.nix                       # Nix development environment
└── .github/workflows/              # CI/CD (build-installers.yml)
```

## Build and Dev Commands

| Command | Description |
|---------|-------------|
| `pnpm dev` | Start Next.js dev server on port 12341 (Turbopack) |
| `pnpm tauri dev` | Full Tauri dev mode (copies proxy binary + starts frontend + Rust) |
| `pnpm build` | Build Next.js frontend |
| `pnpm tauri build` | Build full desktop app |
| `pnpm format` | Auto-fix JS (Biome) + Rust (clippy --fix + fmt) |
| `pnpm lint` | Lint JS (Biome + tsc), Rust (clippy), and spellcheck (typos) |
| `pnpm test` | Run Rust unit tests + sync E2E tests |
| `pnpm test:rust:unit` | Rust unit tests (lib + proxy + vpn integration) |
| `pnpm test:sync-e2e` | Sync server E2E test harness |
| `pnpm check-unused-commands` | Verify no unused Tauri commands |
| `pnpm shadcn:add` | Add a shadcn/ui component |

- After making changes, run `pnpm format && pnpm lint && pnpm test` before finishing a task
- JS linting uses **Biome** (not ESLint/Prettier) — config in `biome.json`
- Rust linting uses `cargo clippy --all-targets --all-features -- -D warnings -D clippy::all`
- `pnpm lint` includes spellcheck via [typos](https://github.com/crate-ci/typos). False positives are allowlisted in `_typos.toml` (locale JSON files and camoufox data are excluded)

## Code Quality

- Don't leave comments that don't add value
- Don't duplicate code unless there's a very good reason; keep the same logic in one place
- Anytime you make changes that affect copy or add new text, it has to be reflected in all translation files

## Translations (mandatory)

- Never write user-facing strings as raw English literals in JSX, toast messages, dialog titles/descriptions, button labels, placeholders, table headers, tooltips, or empty-state text. Always go through `t("namespace.key")` from `useTranslation()`.
- This applies to every component under `src/` — including new ones. If a component doesn't already import `useTranslation`, add it.
- Adding a new string means adding the key to ALL seven locale files in `src/i18n/locales/` (en, es, fr, ja, pt, ru, zh) — not just `en.json`. The English version alone is incomplete work.
- Reuse existing keys (`common.buttons.*`, `common.labels.*`, `createProfile.*`, etc.) before creating new namespaces. Check `en.json` first.
- Strings excluded from this rule: `console.log/warn/error`, dev-only debug labels, internal IDs, CSS class names, type names. If unsure whether a string renders to the user, assume it does and translate it.
- **Never use `t(key, "fallback")` with a default-value second argument.** The 2-arg form is forbidden — every key must exist in every locale file before the call site lands. Fallbacks mask missing translations: a key missing from `ru.json` will silently render the English fallback to Russian users, so the bug never surfaces in CI or review. Only call `t("namespace.key")`. If a translation is missing for any locale, that's a bug to fix at the JSON, not a hole to paper over at the call site.
- Empty-string values in non-English locales are also forbidden — a locale either has the right translation or it has the same content as English; never `""`. If a particular language doesn't need a particular phrase (e.g. a suffix that doesn't grammatically apply), refactor the JSX to use a single interpolated key (`t("foo.bar", { name })` with `"...{{name}}..."` in each locale) instead of splitting prefix/suffix.

## Singletons

- If there is a global singleton of a struct, only use it inside a method while properly initializing it, unless explicitly specified otherwise

## UI Theming

- Never use hardcoded Tailwind color classes (e.g., `text-red-500`, `bg-green-600`, `border-yellow-400`). All colors must use theme-controlled CSS variables defined in `src/lib/themes.ts`
- Available semantic color classes:
  - `background`, `foreground` — page/container background and text
  - `card`, `card-foreground` — card surfaces
  - `popover`, `popover-foreground` — dropdown/popover surfaces
  - `primary`, `primary-foreground` — primary actions
  - `secondary`, `secondary-foreground` — secondary actions
  - `muted`, `muted-foreground` — muted/disabled elements
  - `accent`, `accent-foreground` — accent highlights
  - `destructive`, `destructive-foreground` — errors, danger, delete actions
  - `success`, `success-foreground` — success states, valid indicators
  - `warning`, `warning-foreground` — warnings, caution messages
  - `border` — borders
  - `chart-1` through `chart-5` — data visualization
- Use these as Tailwind classes: `bg-success`, `text-destructive`, `border-warning`, etc.
- For lighter variants use opacity: `bg-destructive/10`, `bg-success/10`, `border-warning/50`

## Gotchas

- **Proxy binary**: `donut-proxy` must be copied before dev/build. The `prebuild`, `pretauri:dev`, and `precargo` scripts handle this automatically via `copy-proxy-binary.mjs`. If you run `cargo` commands directly, run `pnpm copy-proxy-binary` first.
- **Tauri v2**: This project uses Tauri v2 (schema `https://schema.tauri.app/config/2`). APIs differ from v1 — check Tauri v2 docs.
- **Biome, not ESLint**: JS/TS linting and formatting uses Biome 2.x. Use `pnpm format:js` to auto-fix, not `prettier` or `eslint --fix`.
- **Dev port**: Frontend dev server runs on port `12341`, not the default 3000.
- **donut-sync**: The sync server is a separate NestJS app in `donut-sync/`. It has its own `package.json`, `biome.json`, and TypeScript config. Lint and typecheck commands cover both `src/` and `donut-sync/src/`.
- **Husky**: Git hooks are managed by Husky (`pnpm prepare` sets them up).
- **Unused Tauri commands**: Run `pnpm check-unused-commands` to verify no dead Tauri commands exist — there's a test (`test_no_unused_tauri_commands`) that enforces this.
- **Integration tests**: Rust integration tests live in `src-tauri/tests/` (proxy, sync, VPN). They may require specific environment setup.

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
