# Auto-reg Dual Network (Nord / Proxy) Implementation Plan

> **For the executing agent:** REQUIRED SUB-SKILL: Use skill({ name: "incremental-implementation" }) and skill({ name: "executing-plans" }) to implement this plan task-by-task. Prefer TDD for pure helpers.

**Goal:** Auto-registration can run in dual network mode — **NordVPN CLI** rotates system egress every N successful free-trial accounts, **or** a **static proxy** attaches as today — so batch inventory stays usable without sticky-IP burn.

**Architecture:** Extend `RegistrationConfig` with `networkMode` (`none` | `proxy` | `nord`) plus Nord rotate fields. New pure `nord_cli` module builds/runs Windows `NordVPN.exe` (`-c`/`-d`/`-g`/`-n`) with `CREATE_NO_WINDOW`. `RegistrationEngine::run` connects Nord at start (nord mode), counts successes after free-trial-gated saves, rotates + verifies IP via `ip_utils::fetch_public_ip(None)`, best-effort disconnect on cancel. UI mode selector + i18n; docs. No new Cargo deps. WireGuard `vpn_id` path untouched.

**Tech Stack:** Tauri v2 / Rust (`std::process::Command`, existing `reqwest` via `ip_utils`), React + shadcn Select/Input, i18n 7 locales, Biome.

**Discovery Level:** 0 — PRD locked; Nord CLI and insertion points already mapped this session.

**Context Budget:** ~45% for full plan; execute in **3 waves** (pure helpers → engine → UI/i18n/docs). Ship one wave per agent turn when possible.

**Bead:** `jnmbrowser-8qs`  
**Branch:** `feat/jnmbrowser-8qs-auto-reg-dual-network`  
**PRD:** `.beads/artifacts/jnmbrowser-8qs/prd.md`

---

## Must-Haves

### Observable Truths

1. User can choose network mode **None / Proxy / Nord** in Auto Registration dialog.
2. User can start a batch with **proxy** and accounts use the selected `proxyId` (no Nord CLI).
3. User can start a batch with **Nord**, optional group/server, and **rotate every N successes**.
4. After every N free-trial-eligible saves in Nord mode, progress shows IP rotation and egress IP changes (or soft-warn if not).
5. User can cancel a Nord-mode task; Nord is best-effort disconnected if this task connected it.
6. User sees all new labels in their locale (7 languages); no raw English in JSX.
7. Docs describe dual-mode, Nord prerequisites (app installed + logged in), and rotate policy.

### Required Artifacts

| Artifact | Provides | Path |
|----------|----------|------|
| Config fields + step | Serde network mode | `src-tauri/src/auto_register/types.rs` |
| TS config mirror | Frontend invoke payload | `src/hooks/use-registration-events.ts` |
| Nord CLI module | Arg build + spawn + tests | `src-tauri/src/auto_register/nord_cli.rs` |
| Module export | `mod nord_cli` | `src-tauri/src/auto_register/mod.rs` |
| Engine hooks | Connect / rotate / cancel | `src-tauri/src/auto_register/engine.rs` |
| Live harness defaults | Compile with new fields | `src-tauri/src/auto_register/live_harness.rs` |
| Dialog UI | Mode + fields + payload | `src/components/account-registration-dialog.tsx` |
| i18n keys | 7 locales | `src/i18n/locales/{en,es,fr,ja,pt,ru,zh}.json` |
| Docs | Dual-network section | `docs/auto-registration.md` |
| IP helper (reuse) | Egress verify | `src-tauri/src/ip_utils.rs` (no change unless needed) |

### Key Links

| From | To | Via | Risk |
|------|-----|-----|------|
| Dialog `handleStart` | `start_auto_registration` | `RegistrationConfig` camelCase | Missing field → serde default wrong mode |
| `networkMode=proxy` | Profile create | `proxy_id` on ephemeral | Still pass proxy when mode=none by mistake |
| `networkMode=nord` | Host egress | `NordCli::connect/disconnect` | CLI not found / not logged in → hang if no timeout |
| Success counter | Next `run_once` | rotate after save + cleanup | Rotate while browser still open → mixed IP |
| Rotate | `fetch_public_ip(None)` | poll loop | IP unchanged soft-warn vs hard fail |
| Cancel flag | Nord disconnect | end of `run` / cancel check | Leave host VPN up after cancel |
| UI strings | Locales | `t("registration.*")` | Missing key in non-en |

### UX gates (dialog)

- **Primary action:** Start Registration remains single primary.
- **Forms:** Labels visible; helper for Nord system-wide warning; mode select keyboardable.
- **State:** Invalid combo (proxy mode empty proxyId) blocks start with toast/error — do not silent-fail.
- **Recovery:** Nord connect failure surfaces in progress message; user can cancel.
- **a11y:** `Label htmlFor` + control ids; Select already accessible.

---

## Dependency Graph

```
Task 1 Config + pure should_rotate: needs nothing
Task 2 Nord CLI module + tests: needs nothing
Task 3 Engine hooks: needs Task 1 + 2
Task 4 Dialog UI: needs Task 1
Task 5 i18n: needs Task 4 keys finalized
Task 6 Docs: needs Task 3 behavior stable

Wave 1 (parallel): Task 1, Task 2
Wave 2 (serial):   Task 3
Wave 3 (parallel after 1 for UI): Task 4 → Task 5; Task 6 after Task 3
```

**Size:** L (8+ files) — single plan, 3 waves, no child beads (scope coherent).

---

## Task 1: Config dual-mode + pure rotate helper

**Maps to PRD:** backend-1  
**Files:**

- Modify: `src-tauri/src/auto_register/types.rs`
- Modify: `src/hooks/use-registration-events.ts`
- Modify: `src-tauri/src/auto_register/live_harness.rs`
- Optional pure helper can live in `types.rs` or `nord_cli.rs` — prefer `types.rs` for `should_rotate` + `NetworkMode` enum

### Step 1: Write failing tests (types)

Add at bottom of `types.rs` (or `#[cfg(test)]` module):

```rust
#[cfg(test)]
mod network_config_tests {
  use super::*;

  #[test]
  fn default_network_mode_is_none() {
    let json = r#"{"cdks":["GMAIL-X"]}"#;
    let c: RegistrationConfig = serde_json::from_str(json).unwrap();
    assert_eq!(c.network_mode, NetworkMode::None);
    assert_eq!(c.rotate_every_n, 0);
  }

  #[test]
  fn should_rotate_every_n_successes() {
    assert!(!should_rotate(0, 2));
    assert!(!should_rotate(1, 2));
    assert!(should_rotate(2, 2));
    assert!(!should_rotate(3, 2));
    assert!(should_rotate(4, 2));
    assert!(!should_rotate(2, 0)); // 0 = never
  }

  #[test]
  fn validate_proxy_mode_requires_proxy_id() {
    let mut c = RegistrationConfig {
      cdks: vec!["GMAIL-X".into()],
      profile_id: None,
      proxy_id: None,
      browser_type: "chromium".into(),
      max_retries: 1,
      accounts_per_cdk: 1,
      headless: false,
      concurrency: 1,
      network_mode: NetworkMode::Proxy,
      rotate_every_n: 0,
      nord_group: None,
      nord_server_name: None,
      nord_cli_path: None,
    };
    assert!(c.validate_network().is_err());
    c.proxy_id = Some("p1".into());
    assert!(c.validate_network().is_ok());
  }

  #[test]
  fn validate_nord_rejects_proxy_id() {
    let c = RegistrationConfig {
      cdks: vec!["GMAIL-X".into()],
      profile_id: None,
      proxy_id: Some("p1".into()),
      browser_type: "chromium".into(),
      max_retries: 1,
      accounts_per_cdk: 1,
      headless: false,
      concurrency: 1,
      network_mode: NetworkMode::Nord,
      rotate_every_n: 2,
      nord_group: Some("United States".into()),
      nord_server_name: None,
      nord_cli_path: None,
    };
    assert!(c.validate_network().is_err());
  }
}
```

### Step 2: Run tests — expect FAIL

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib auto_register::types::network_config_tests -- --nocapture
```

Expected: compile errors (missing types/fns).

### Step 3: Implement minimal config

In `types.rs`:

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum NetworkMode {
  #[default]
  None,
  Proxy,
  Nord,
}

// On RegistrationConfig add (all with serde defaults):
// network_mode: NetworkMode  default None
// rotate_every_n: u32        default 0
// nord_group: Option<String>
// nord_server_name: Option<String>
// nord_cli_path: Option<String>

pub fn should_rotate(success_count: u32, every_n: u32) -> bool {
  every_n > 0 && success_count > 0 && success_count % every_n == 0
}

impl RegistrationConfig {
  pub fn validate_network(&self) -> Result<(), String> {
    match self.network_mode {
      NetworkMode::None => Ok(()),
      NetworkMode::Proxy => {
        if self.proxy_id.as_ref().map(|s| s.trim().is_empty()).unwrap_or(true) {
          Err("proxy mode requires proxyId".into())
        } else {
          Ok(())
        }
      }
      NetworkMode::Nord => {
        if self.proxy_id.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
          Err("nord mode cannot be combined with proxyId".into())
        } else {
          Ok(())
        }
      }
    }
  }

  /// Effective proxy for profile create: only when mode=proxy.
  pub fn effective_proxy_id(&self) -> Option<String> {
    match self.network_mode {
      NetworkMode::Proxy => self.proxy_id.clone(),
      _ => None,
    }
  }
}
```

Add `RegistrationStep::RotatingIp` + label `"Rotating IP"`.

Update `live_harness.rs` struct literal with new fields (defaults).

### Step 4: Mirror TS types

In `use-registration-events.ts`:

```ts
export type NetworkMode = "none" | "proxy" | "nord";

export interface RegistrationConfig {
  cdks: string[];
  profileId?: string;
  proxyId?: string;
  browserType: string;
  maxRetries: number;
  accountsPerCdk: number;
  headless: boolean;
  concurrency: number;
  networkMode?: NetworkMode;
  rotateEveryN?: number;
  nordGroup?: string;
  nordServerName?: string;
  nordCliPath?: string;
}
```

### Step 5: Verify

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib auto_register::types::network_config_tests -- --nocapture
# Expected: PASS
pnpm exec tsc --noEmit -p . 2>&1 | tail -20
# or pnpm lint:js if faster in env
```

### Step 6: Commit (specific files only)

```bash
git add src-tauri/src/auto_register/types.rs src-tauri/src/auto_register/live_harness.rs src/hooks/use-registration-events.ts
git commit -m "feat(auto-reg): add dual networkMode config and should_rotate helper"
```

---

## Task 2: Nord CLI helper (TDD)

**Maps to PRD:** backend-2  
**Files:**

- Create: `src-tauri/src/auto_register/nord_cli.rs`
- Modify: `src-tauri/src/auto_register/mod.rs` → `pub mod nord_cli;`

### Step 1: Failing tests first (in nord_cli.rs)

```rust
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn connect_args_best() {
    assert_eq!(connect_args(None, None), vec!["-c".into()]);
  }

  #[test]
  fn connect_args_group() {
    assert_eq!(
      connect_args(Some("United States"), None),
      vec!["-c".into(), "-g".into(), "United States".into()]
    );
  }

  #[test]
  fn connect_args_server_wins_over_group() {
    assert_eq!(
      connect_args(Some("United States"), Some("United States #5")),
      vec!["-c".into(), "-n".into(), "United States #5".into()]
    );
  }

  #[test]
  fn disconnect_args() {
    assert_eq!(disconnect_args(), vec!["-d".into()]);
  }

  #[test]
  fn default_cli_path_windows_shape() {
    let p = default_cli_path();
    assert!(p.to_string_lossy().contains("NordVPN"));
  }
}
```

### Step 2: Run — FAIL

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib auto_register::nord_cli -- --nocapture
```

### Step 3: Implement

```rust
//! NordVPN Windows CLI wrapper for system-wide IP rotation.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub fn connect_args(group: Option<&str>, server_name: Option<&str>) -> Vec<String> {
  let mut a = vec!["-c".to_string()];
  if let Some(n) = server_name.map(str::trim).filter(|s| !s.is_empty()) {
    a.push("-n".into());
    a.push(n.to_string());
  } else if let Some(g) = group.map(str::trim).filter(|s| !s.is_empty()) {
    a.push("-g".into());
    a.push(g.to_string());
  }
  a
}

pub fn disconnect_args() -> Vec<String> {
  vec!["-d".to_string()]
}

pub fn default_cli_path() -> PathBuf {
  #[cfg(windows)]
  {
    PathBuf::from(r"C:\Program Files\NordVPN\NordVPN.exe")
  }
  #[cfg(not(windows))]
  {
    PathBuf::from("nordvpn")
  }
}

pub fn resolve_cli_path(override_path: Option<&str>) -> Result<PathBuf, String> {
  if let Some(p) = override_path.map(str::trim).filter(|s| !s.is_empty()) {
    let pb = PathBuf::from(p);
    if pb.is_file() {
      return Ok(pb);
    }
    return Err(format!("NordVPN CLI not found at override path: {p}"));
  }
  let def = default_cli_path();
  if def.is_file() {
    return Ok(def);
  }
  Err(format!(
    "NordVPN CLI not found at {}. Install NordVPN or set nordCliPath.",
    def.display()
  ))
}

fn run_nord(exe: &Path, args: &[String]) -> Result<String, String> {
  let mut cmd = Command::new(exe);
  cmd.args(args);
  #[cfg(windows)]
  {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
  }
  let output = cmd
    .output()
    .map_err(|e| format!("failed to spawn NordVPN CLI: {e}"))?;
  let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
  let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
  if !output.status.success() {
    return Err(format!(
      "NordVPN CLI {:?} failed (status {:?}): {stderr} {stdout}",
      args,
      output.status.code()
    ));
  }
  Ok(if stdout.is_empty() { stderr } else { stdout })
}

pub fn disconnect(cli_path: Option<&str>) -> Result<(), String> {
  let exe = resolve_cli_path(cli_path)?;
  let _ = run_nord(&exe, &disconnect_args())?;
  Ok(())
}

pub fn connect(
  cli_path: Option<&str>,
  group: Option<&str>,
  server_name: Option<&str>,
) -> Result<(), String> {
  let exe = resolve_cli_path(cli_path)?;
  let _ = run_nord(&exe, &connect_args(group, server_name))?;
  Ok(())
}

/// Disconnect then connect; used for rotation.
pub fn rotate(
  cli_path: Option<&str>,
  group: Option<&str>,
  server_name: Option<&str>,
) -> Result<(), String> {
  let _ = disconnect(cli_path); // best-effort even if already down
  std::thread::sleep(Duration::from_secs(2));
  connect(cli_path, group, server_name)
}
```

Wire `mod nord_cli;` in `mod.rs`.

### Step 4: Verify

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib auto_register::nord_cli -- --nocapture
# Expected: PASS (path test may fail if Nord not installed — if so, assert path string only, not is_file)
```

**Note:** Keep `default_cli_path` test as string shape only; use `resolve_cli_path` live checks only in engine.

### Step 5: Commit

```bash
git add src-tauri/src/auto_register/nord_cli.rs src-tauri/src/auto_register/mod.rs
git commit -m "feat(auto-reg): add NordVPN Windows CLI wrapper"
```

---

## Task 3: Engine connect / rotate / cancel

**Maps to PRD:** backend-3  
**Files:**

- Modify: `src-tauri/src/auto_register/engine.rs` (~`run` 779–922, `launch_browser` proxy arg, cancel path)
- Optionally: `src-tauri/src/auto_register/commands.rs` — call `validate_network` before spawn

### Step 1: Validate at command entry

In `start_auto_registration` (commands.rs), after building engine config:

```rust
config.validate_network().map_err(|e| e)?;
```

### Step 2: Engine state

On `RegistrationEngine` add fields (or locals in `run` only):

```rust
// inside run:
let mut success_count: u32 = 0;
let mut nord_connected_by_us = false;
let cli = self.config.nord_cli_path.as_deref();
let group = self.config.nord_group.as_deref();
let server = self.config.nord_server_name.as_deref();
```

### Step 3: Nord ensure at start of `run`

After cloning config / before CDK loop:

```rust
if self.config.network_mode == NetworkMode::Nord {
  self.emit(..., RegistrationStep::RotatingIp, "Connecting NordVPN...", ...);
  // capture baseline IP optional
  super::nord_cli::connect(cli, group, server).map_err(|e| {
    // cannot return Result from run easily — emit Failed and return early RegistrationResult failure
  })?;
  nord_connected_by_us = true;
  // wait_for_ip_change or just sleep 3s + fetch_public_ip for log
}
```

Because `run` returns `RegistrationResult` not `Result`, on connect failure:

```rust
self.emit(..., Failed, &err, ...);
return RegistrationResult { success: false, error_message: err, ...defaults... };
```

### Step 4: Success counter + rotate (critical insertion)

After `Ok(result)` in `run` (~853–857), **only count free-trial successes** (`result.success && result.free_trial_eligible` — note skipped non-trial currently returns Err so Ok already means saved eligible):

```rust
Ok(result) => {
  save_registration_result(&result);
  all_results.push(result);
  succeeded = true;
  success_count += 1;

  if self.config.network_mode == NetworkMode::Nord
    && should_rotate(success_count, self.config.rotate_every_n)
  {
    self.emit(..., RotatingIp,
      &format!("Rotating NordVPN IP after {success_count} successes..."), ...);
    let old_ip = crate::ip_utils::fetch_public_ip(None).await.unwrap_or_default();
    if let Err(e) = super::nord_cli::rotate(cli, group, server) {
      self.log(&format!("Nord rotate error: {e}"));
      // soft-warn continue
    } else {
      // poll up to ~60s for new IP
      let mut new_ip = old_ip.clone();
      for _ in 0..20 {
        sleep(Duration::from_secs(3)).await;
        if let Ok(ip) = crate::ip_utils::fetch_public_ip(None).await {
          if !ip.is_empty() && ip != old_ip {
            new_ip = ip;
            break;
          }
          new_ip = ip;
        }
      }
      if new_ip == old_ip {
        self.log(&format!(
          "WARN: egress IP unchanged after rotate ({old_ip}); continuing"
        ));
      } else {
        self.log(&format!("Egress IP {old_ip} → {new_ip}"));
      }
      self.emit(..., RotatingIp, &format!("IP {old_ip} → {new_ip}"), ...);
    }
  }
  break;
}
```

### Step 5: `effective_proxy_id` in launch_browser

Replace `self.config.proxy_id.clone()` with `self.config.effective_proxy_id()` so nord/none never attach proxy.

### Step 6: Cancel / end disconnect

At end of `run` (before return summary) and when `is_cancelled` breaks loops:

```rust
if nord_connected_by_us {
  let _ = super::nord_cli::disconnect(cli);
}
```

Use `scopeguard` pattern or simple flag — no new dep; just call before every return path / after loops.

### Step 7: Verify

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib auto_register -- --nocapture
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings 2>&1 | tail -40
```

Expected: unit tests pass; clippy clean on touched code.

### Step 8: Commit

```bash
git add src-tauri/src/auto_register/engine.rs src-tauri/src/auto_register/commands.rs
git commit -m "feat(auto-reg): Nord IP rotate after N successes in engine loop"
```

---

## Task 4: Registration dialog dual network UI

**Maps to PRD:** frontend-1  
**Files:**

- Modify: `src/components/account-registration-dialog.tsx`
- May touch: `src/hooks/use-registration-events.ts` (already done in Task 1)

### UX states

| State | Behavior |
|-------|----------|
| Mode none | Hide proxy + Nord fields |
| Mode proxy | Show proxy input; require non-empty on start |
| Mode nord | Show group, server, rotateEveryN; helper warning system-wide |
| Start loading | Existing `loading` disables start |
| Validation error | toast.error with i18n key |

### Step 1: State + mode Select

```tsx
const [networkMode, setNetworkMode] = useState<"none" | "proxy" | "nord">("none");
const [rotateEveryN, setRotateEveryN] = useState(2);
const [nordGroup, setNordGroup] = useState("");
const [nordServerName, setNordServerName] = useState("");
```

Replace proxy-only grid cell with:

1. Network mode `Select` (none/proxy/nord) — labels via `t("registration.networkMode*")`
2. Conditional proxy `Input`
3. Conditional Nord fields: group, server, rotateEveryN number min 0
4. Muted helper: `t("registration.nordSystemWideWarning")` when nord

### Step 2: handleStart validation + payload

```tsx
if (networkMode === "proxy" && !proxyId.trim()) {
  toast.error(t("registration.proxyRequired"));
  return;
}
if (networkMode === "nord" && proxyId.trim()) {
  // should not happen if UI clears proxy; still strip
}
await startRegistration({
  cdks,
  browserType,
  proxyId: networkMode === "proxy" ? proxyId || undefined : undefined,
  maxRetries,
  accountsPerCdk,
  headless,
  concurrency: 1,
  networkMode,
  rotateEveryN: networkMode === "nord" ? rotateEveryN : 0,
  nordGroup: networkMode === "nord" ? nordGroup || undefined : undefined,
  nordServerName: networkMode === "nord" ? nordServerName || undefined : undefined,
});
```

Import `toast` from sonner (match other dialogs).

### Step 3: a11y

- Every Label has `htmlFor` matching input id.
- Mode select has label.
- Helper text not only-color; use `text-muted-foreground`.

### Step 4: Verify

```bash
pnpm lint:js
```

### Step 5: Commit

```bash
git add src/components/account-registration-dialog.tsx
git commit -m "feat(auto-reg): dual network mode UI for Nord and proxy"
```

---

## Task 5: i18n (7 locales)

**Maps to PRD:** i18n-1  
**Files:** `src/i18n/locales/{en,es,fr,ja,pt,ru,zh}.json`

### Keys to add under `registration`

```json
"networkMode": "Network",
"networkModeNone": "None (host IP)",
"networkModeProxy": "Proxy",
"networkModeNord": "NordVPN",
"nordGroup": "Nord group / country",
"nordGroupPlaceholder": "e.g. United States",
"nordServerName": "Nord server name (optional)",
"nordServerPlaceholder": "e.g. United States #5",
"rotateEveryN": "Rotate IP every N successes",
"nordSystemWideWarning": "NordVPN changes the system IP for the whole PC while registration runs.",
"proxyRequired": "Proxy mode requires a proxy ID."
```

Translate all non-empty for es/fr/ja/pt/ru/zh (no empty strings).

### Verify

```bash
# each key present in all 7
for k in networkMode networkModeNone networkModeProxy networkModeNord nordGroup rotateEveryN nordSystemWideWarning proxyRequired; do
  for f in src/i18n/locales/{en,es,fr,ja,pt,ru,zh}.json; do
    rg -q "\"$k\"" "$f" || echo "MISSING $k in $f"
  done
done
pnpm lint:js
```

### Commit

```bash
git add src/i18n/locales/en.json src/i18n/locales/es.json src/i18n/locales/fr.json src/i18n/locales/ja.json src/i18n/locales/pt.json src/i18n/locales/ru.json src/i18n/locales/zh.json
git commit -m "feat(i18n): auto-reg dual network strings for 7 locales"
```

---

## Task 6: Docs

**Maps to PRD:** docs-1  
**Files:** `docs/auto-registration.md`

### Content to add

1. **Prerequisites:** NordVPN Windows app installed + logged in (for Nord mode).
2. **Usage UI:** Network mode None / Proxy / Nord; rotate every N successes.
3. **Architecture note:** Nord = system-wide CLI; proxy = profile `proxy_id`; mutually exclusive.
4. **Troubleshooting:**
   - CLI not found → install path / `nordCliPath`
   - IP unchanged → wait, try group, re-login Nord
   - Free trial drop after hop → choose stable country group
5. Update example invoke JSON with `networkMode`, `rotateEveryN`.

### Commit

```bash
git add docs/auto-registration.md
git commit -m "docs: auto-reg dual network Nord/proxy rotation"
```

---

## Final verification (all waves)

```bash
pnpm lint:js
pnpm lint:rust
pnpm test:rust:unit
```

Optional manual:

1. Nord logged in → mode nord, `rotateEveryN=1`, 2 CDKs/aliases → observe two connects and IP log.
2. Mode proxy with valid proxyId → no Nord process.
3. Cancel mid-batch nord → host disconnects.

Update `.beads/artifacts/jnmbrowser-8qs/prd.json` task `passes: true` as each task completes.  
Append progress notes to `progress.txt`.

---

## Constitutional compliance

| Check | Result |
|-------|--------|
| `git add .` / `-A` | **PASS** — plan uses specific paths only |
| force push / `--no-verify` / hard reset | **PASS** — not present |
| New deps (`cargo add`, `pnpm add`) | **PASS** — none |
| `as any` / `@ts-ignore` | **PASS** — not used |
| Secrets | **PASS** — no credentials in plan |
| Task file blast | Task 5 has 7 locale files (expected i18n); engine single-file Task 3 |

**Constitutional compliance: ✓ PASS**

---

## Execution notes for `/ship`

1. Stay on `feat/jnmbrowser-8qs-auto-reg-dual-network` (dirty WIP auto-reg already present — do not revert inventory work).
2. Wave 1 Tasks 1–2 parallel-safe (different files).
3. Wave 2 Task 3 alone (engine.rs large).
4. Wave 3 Task 4 → 5; Task 6 anytime after 3.
5. Do not enable concurrency with Nord.
6. Soft-warn if IP unchanged (PRD).
7. Default `rotateEveryN` UI = 2 when nord selected; send `0` for non-nord.

---

## Plan complete

Saved: `.beads/artifacts/jnmbrowser-8qs/plan.md`

**Next:** `/ship jnmbrowser-8qs`

**Execution options when shipping:**

1. **Subagent-driven (this session)** — fresh worker per task, review between  
2. **Single-agent incremental** — walk waves with `incremental-implementation`
