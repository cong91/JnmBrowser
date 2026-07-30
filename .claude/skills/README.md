# JnmBrowser Agent Skills

Project-specific skills for AI coding agents working on JnmBrowser (DonutBrowser).
Each skill lives in its own directory with a `SKILL.md` (Agent Skills format).
Read `AGENTS.md` at the repo root first — these skills operationalize it, they do not replace it.

| Skill | Use when |
|-------|----------|
| `jnm-project-overview` | Starting work in an unfamiliar area; finding where code lives; knowing which docs to read first |
| `jnm-i18n` | Adding or changing any user-facing string (JSX, toasts, dialogs, placeholders, table headers, tooltips, empty states) |
| `jnm-tauri-command` | Adding, renaming, or removing a Tauri command between Rust and the Next.js frontend |
| `jnm-ui-conventions` | Building or modifying UI: theming, shadcn/ui, dialogs, profile action items, tables |
| `jnm-tauri-events` | Working with Tauri `listen`/`emit`, profile launch flows, or `use-*-events` hooks |
| `jnm-rust-backend` | Touching `src-tauri/`: module conventions, testing isolation, clippy traps, proxy binary prerequisites |
| `jnm-quality-gate` | Before finishing any meaningful change: format, lint, spellcheck, tests |
| `jnm-plan-execution` | Executing a plan from `.zcode/plans/` or a root plan book; checkbox tracking; completion rule |
