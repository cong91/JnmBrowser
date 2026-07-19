# Plan: CDK concurrency (1 CDK = 1 thread) + CDK free-trial stats

## Goal / success criteria

1. **Concurrency**: `concurrency` = max parallel CDKs. Each CDK runs as one worker task; aliases (up to 6) inside a CDK stay sequential.
2. **Worker profiles**: each concurrent CDK gets its own worker profile for that CDK’s aliases.
3. **Nord CLI**: force `concurrency = 1` when `network_mode == Nord` (system-wide IP).
4. **CDK inventory**: persist every CDK used with stats (target / attempted / free_trial_yes / free_trial_no / failed / accounts).
5. **Free-trial no longer invisible**: registered-but-not-free-trial accounts are saved (`success=false`, `free_trial_eligible=false`) and counted on that CDK.
6. **UI**: concurrency control + CDK stats tab; i18n all 7 locales.
7. Verify: compile + auto_register tests.

## Design

### Concurrency

- `run()` uses a semaphore of size `max(1, concurrency)` (capped, e.g. 8).
- Spawn one async task per CDK (join_all / FuturesUnordered).
- Shared: `cancel_flag`, `config`, `task_id`, atomic `success_count` for Nord rotate (only when concurrency forced to 1).
- Per CDK: own logs slice, own worker profile create/reuse/dispose for that CDK only.

### Free-trial path change

Today free-trial fail is `return Err(...)` and is never saved. Change to return `Ok(RegistrationResult { success: false, free_trial_eligible: false, email/password/tokens if any, ... })` so inventory + CDK stats record it. Do not retry free-trial gate failures.

### CDK stats model

```rust
pub struct CdkAccountEntry {
  pub email: String,
  pub account_id: String,
  pub success: bool,
  pub free_trial_eligible: bool,
  pub plan_type: String,
  pub error_message: String,
  pub created_at: DateTime<Utc>,
}

pub struct CdkInventoryRecord {
  pub cdk: String,
  pub base_email: String,
  pub target_accounts: u32,
  pub attempted: u32,
  pub free_trial_yes: u32,
  pub free_trial_no: u32,
  pub failed: u32,
  pub status: String, // pending|running|done|failed|cancelled
  pub last_error: String,
  pub accounts: Vec<CdkAccountEntry>,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
  pub task_id: String,
}
```

Storage: `data_dir()/cdk_inventory/{safe_cdk_key}.json` + in-memory map in store.

### Files

- `src-tauri/src/auto_register/types.rs` — CdkInventoryRecord
- `src-tauri/src/auto_register/store.rs` — CDK CRUD + list
- `src-tauri/src/auto_register/engine.rs` — concurrent CDK processing + stats updates + free-trial save
- `src-tauri/src/auto_register/commands.rs` — list_cdk_inventory_cmd
- `src-tauri/src/lib.rs` — register command
- `src/hooks/use-registration-events.ts` — types + list CDKs
- `src/components/account-registration-dialog.tsx` — concurrency + CDK tab
- `src/components/cdk-inventory-table.tsx` — new
- `src/i18n/locales/*` — keys
- `docs/auto-registration.md`

## Checklist

- [x] Types + store for CDK inventory
- [x] Free-trial fail saves account + CDK counters
- [x] Concurrent CDK processing (1 CDK = 1 task)
- [x] Per-CDK worker profile lifecycle
- [x] Nord forces concurrency 1
- [x] Commands + frontend + i18n
- [x] Docs + verify (`cargo check --lib`, `tsc`, biome green)
