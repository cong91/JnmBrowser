---
name: jnm-quality-gate
description: The verification loop to run before finishing any meaningful JnmBrowser change — pnpm format, lint (Biome + tsc + clippy + typos), tests, and unused-command check, with the proxy-binary prerequisite. Use at the end of every implementation task.
---

# Quality gate

Run **before declaring any meaningful change done**, in this order:

```powershell
pnpm copy-proxy-binary   # 0. prerequisite for anything cargo-related
pnpm format              # 1. auto-fix: Biome --write (src/ + donut-sync/) + clippy --fix + cargo fmt
pnpm lint                # 2. verify: lint:js + lint:rust + lint:spell
pnpm test                # 3. test:frontend + test:rust:unit + test:sync-e2e
```

Add when relevant:

```powershell
pnpm check-unused-commands   # added/removed a Tauri command
node .claude/skills/jnm-i18n/check-locales.mjs   # touched locale files
pnpm build                   # sanity-check Next.js production build (frontend-heavy changes)
```

## What each step covers

| Script | What it runs |
|--------|--------------|
| `lint:js` | `biome check src/` + `tsc --noEmit`, then the same inside `donut-sync/` |
| `lint:rust` | `cargo clippy --all-targets --all-features -- -D warnings -D clippy::all` + `cargo fmt --all` |
| `lint:spell` | `typos .` — allowlist in `_typos.toml`; locale JSON + camoufox data excluded |
| `test:frontend` | Node test runner on `src/components/login-account-selection.test.mjs` |
| `test:rust:unit` | `cargo test --lib` + `donut_proxy_integration` + `vpn_integration` |
| `test:sync-e2e` | `scripts/sync-test-harness.mjs` against the donut-sync NestJS app |

## Fixing failures

- **Biome**: `pnpm format:js` auto-fixes most issues; remaining ones are real — fix the code, don't disable rules.
- **tsc**: no `@ts-ignore` band-aids; fix types at the source (`src/types.ts` mirrors Rust structs).
- **clippy**: warnings are denied (`-D warnings`) — see traps in `jnm-rust-backend`.
- **typos**: legitimate project terms go into `_typos.toml`, everything else gets fixed in place.
- **Tests**: env-sensitive Rust integration tests need `JNMBROWSER_DATA_DIR` + `serial` — see `jnm-rust-backend`.

## Notes

- Git commits trigger husky + lint-staged: Biome fix on JS/TS/JSON/CSS, `cargo fmt` + clippy + `cargo test --lib` on staged `.rs`, `typos` on docs/code. Keeping the gate green pre-commit avoids surprise failures.
- These are pnpm scripts — run from the **repo root** on Windows PowerShell.
- If a pre-existing failure unrelated to your change blocks the gate, report it explicitly instead of silently working around it (and don't "fix" unrelated code unless asked).
