# Plan: Auto-Register Profile Reuse (stop profile spam)

## Objective / success criteria

1. Auto-registration **does not create a brand-new profile metadata entry per account**.
2. One **worker profile per concurrent slot** is created (default concurrency = 1 → 1 worker for whole batch).
3. Between accounts, browser is **killed + relaunched** so:
   - ephemeral data dir is wiped (`remove_ephemeral_dir` on kill)
   - fingerprint is regenerated (`randomize_fingerprint_on_launch = true` for Camoufox **and** Chromium)
   - site cookies/storage are cleared (existing `clear_all_site_data`)
4. Worker profiles are deleted only at **batch end** (or cancel), not after every account.
5. If `config.profile_id` points to a real profile: **reuse that profile** as the worker (template was wrong; docs/types already say “optional existing profile”).
6. Cloudflare authorize retry **relaunches the same worker**, does not spawn extra profile IDs.
7. Docs updated; Rust lint/tests green for touched paths.

## Root cause (current code)

In `src-tauri/src/auto_register/engine.rs`:

| Location | Behavior |
|----------|----------|
| `run_once` ~1378–1422 | Every account: `launch_and_connect` → register → `cleanup_browser` |
| `launch_browser` ~2994–3122 | **Always** `create_browser_profile_with_group(..., ephemeral: true)` with unique name `auto-reg-{task8}-{uuid8}` |
| `config.profile_id` | Treated only as **version template**, never launched |
| Chromium create | `chromium_config: None` → **no** `randomize_fingerprint_on_launch` |
| Camoufox create | sets `randomize_fingerprint_on_launch: true` but still new profile each time |
| `cleanup_browser` ~3125–3168 | Kill + **delete** ephemeral profile after every account |
| `authorize_with_retry` ~2870+ | On CF retry: `cleanup_browser` + new `launch_and_connect` → another create |

Ephemeral data dirs already wipe on kill (`browser_runner` → `remove_ephemeral_dir`). Fingerprint renew on relaunch already exists in `browser_runner` for both kernels when the flag is set. Creating a new profile row every account is the bug.

## Target lifecycle

```
batch start
  ensure worker profile(s)  // create once if missing; or adopt config.profile_id
  for each account:
    launch worker (randomize FP + fresh ephemeral dir)
    clear_all_site_data + seed device_id
    register
    kill browser only (keep profile metadata)
  batch end / cancel
    kill if needed + delete only auto-created worker profiles
```

## Implementation steps

### 1. Engine state

Add to `RegistrationEngine`:

```rust
/// Reused worker profile for this engine (one slot; concurrency>1 later can expand).
worker_profile_id: Option<String>,
/// True when we created the worker and must delete it at batch end.
owns_worker_profile: bool,
```

### 2. `ensure_worker_profile`

- If `config.profile_id` is set and exists → use it, `owns_worker_profile = false`.
- Else if `worker_profile_id` already set → reload from ProfileManager and return.
- Else create **one** ephemeral profile:
  - name: `auto-reg-worker-{task_id[0..8]}`
  - browser/version from config / template fallback (keep existing version resolution)
  - Camoufox: `randomize_fingerprint_on_launch: Some(true)`
  - Chromium: `ChromiumConfig { randomize_fingerprint_on_launch: Some(true), fingerprint: None, ..Default }`
  - `ephemeral: true`
  - store id, `owns_worker_profile = true`

### 3. Rewrite `launch_browser`

- Call `ensure_worker_profile`.
- Launch **that** profile via `BrowserRunner::launch_browser` (no create).
- Do **not** create a new UUID profile name per account.

### 4. Split cleanup

- `kill_browser_only(app, profile)` — kill process only (ephemeral dir removed by runner).
- `cleanup_browser` used mid-account becomes kill-only.
- `dispose_worker_profiles(app)` at end of `run()` (success, fail, cancel): if `owns_worker_profile`, delete worker by id.

### 5. `authorize_with_retry`

- On retry: kill + `launch_and_connect` on **same** worker id (new device_id still OK).
- Do not call delete.

### 6. Docs

Update `docs/auto-registration.md`:

- Profile lifecycle: one worker per engine, relaunch + FP renew + wipe per account.
- `profile_id` = optional reusable worker (not mere template).
- Cloudflare retry reuses same worker.

### 7. Verify

- `cargo clippy` / unit tests for `auto_register` if any; at least compile `donutbrowser_lib` auto_register paths.
- Manual logic check: N accounts → 1 create + N launches + 1 delete (when no external profile_id).

## Non-goals / avoid

- Do not redesign concurrency pool (UI hardcodes concurrency=1).
- Do not change inventory / CDK / Nord rotation logic except profile lifecycle.
- Do not over-optimize fingerprint generation APIs outside auto_reg.

## Files to touch

- `src-tauri/src/auto_register/engine.rs` (main)
- `docs/auto-registration.md`
- Possibly `src-tauri/src/auto_register/types.rs` comment only for `profile_id`

## Checklist

- [x] Worker ensure + reuse in launch
- [x] Chromium randomize on launch
- [x] Kill-only between accounts
- [x] Delete owned workers at batch end
- [x] authorize_with_retry no extra profiles
- [x] Docs
- [x] Build/lint verify (`cargo check --lib` green; only pre-existing auto_login dead_code warnings)
