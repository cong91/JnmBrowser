---
name: jnm-tauri-command
description: Add, rename, or remove a Tauri command end-to-end in JnmBrowser (Rust handler, lib.rs registration, frontend invoke call site, shared types, unused-command check). Use whenever creating or changing frontend↔backend APIs.
---

# Tauri command lifecycle

Every frontend↔backend API is a Tauri v2 command. A command is **incomplete** until all steps below are done.

## 1. Rust handler

Put the function in the module that owns the domain (see `jnm-project-overview` module map — e.g. proxy logic in `proxy_manager.rs`, profile logic in `profile/manager.rs`). Do not dump new logic into `lib.rs`.

```rust
#[tauri::command]
pub async fn my_command(profile_id: String, force: bool) -> Result<MyResult, String> {
    // ...
}
```

- Return `Result<T, String>` (or the module's error type already used by sibling commands — match the file).
- Shared structs: define/mirror them in `src/types.ts` on the frontend. Check serde attributes (`rename_all`, etc.) on the Rust struct so field names line up.

## 2. Register in `src-tauri/src/lib.rs`

Add the name to the flat list inside `tauri::generate_handler![ ... ]` (keep the existing ordering style). If it lives in a module, follow how sibling commands are imported/re-exported there.

## 3. Frontend call site

```ts
import { invoke } from "@tauri-apps/api/core";

const result = await invoke<MyResult>("my_command", {
  profileId: profile.id,   // camelCase in JS…
  force: true,             // …maps to snake_case Rust params automatically
});
```

- Invoke with the **snake_case** command name; pass args **camelCase**.
- Errors come back as rejected promises — wrap in try/catch and surface via translated toast (`jnm-i18n`).
- If the command changes backend state other components care about, also emit an event (e.g. `app.emit("profiles-changed", ...)`) and consume it from the matching `use-*-events` hook — see `jnm-tauri-events`.

## 4. Prove it is used

```powershell
pnpm check-unused-commands   # = cargo test test_no_unused_tauri_commands
```

A registered command with **no frontend call site** fails this test. When removing a feature, remove the Rust handler + registration + call site together, then re-run the check.

## 5. Quality gate

Finish with `pnpm format && pnpm lint && pnpm test` (see `jnm-quality-gate`). Rust changes also run through husky/lint-staged on commit: `cargo fmt`, `clippy -D warnings`, `cargo test --lib`, and `typos`.

## Renaming a command

Rename in 4 places in one change: Rust fn name → `generate_handler!` entry → every `invoke("...")` call site → any tests/docs referencing it. Grep the old name across both `src/` and `src-tauri/` before finishing.
