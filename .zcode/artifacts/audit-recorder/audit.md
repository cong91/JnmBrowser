# Audit Report: Auto-registration Concurrency and Process Layout

**Pattern:** normal auto-registration concurrency ownership, one CDK per visible job, configurable parallelism, and immediate Stored-account updates
**Date:** 2026-07-23
**Method:** required three-phase `audit-pattern` workflow (discovery, three bounded reviews, synthesis)

## 1. Pattern

The requested operating model is **one CDK = one job/card**. Each card should show that CDK's current alias/account, progress, terminal outcome, and safe summary. Cancellation must clearly distinguish a batch action from a per-CDK action.

The current backend is already close to that execution model: it creates one future per CDK and processes aliases sequentially within that CDK. The contract and UI are not aligned with it. Every worker shares one batch `taskId`, and the frontend uses only that ID as the map/card key, so concurrent worker events overwrite one another.

Current path:

1. `account-registration-dialog.tsx:341` builds one batch config and starts `start_auto_registration`.
2. `register/commands.rs:20-66` creates one batch task ID, cancellation flag, and task handle.
3. `register/engine.rs:1091-1103` forks per-CDK engines while retaining the batch identity.
4. `register/engine.rs:1710-1754` schedules one future per CDK with `Semaphore + FuturesUnordered`; `engine.rs:1918` keeps aliases sequential per CDK.
5. `use-registration-events.ts:127` stores all events by `taskId` only.
6. `account-registration-dialog.tsx:999-1004` consequently renders one card per batch and connects the card cancel action to batch cancellation.

Direct answers to the operator's observations:

- **Why only one Process card for six workers?** All six CDK workers use the same batch `taskId`; every new event replaces the previous map entry.
- **Why concurrency appears fixed at six?** Six is not the global default. UI and Rust defaults are `1`; None/Proxy are editable from `1..8`; Nord CLI is fixed at `1`. VPN mode disables the input and derives `1..6` from `max_sessions`, defaulting to `6`. A UI effect can silently change None/Nord CLI to VPN when Nord inventory exists, making six appear global.
- **Why does Stored lag?** The result event is emitted before `save_registration_result`; the frontend refresh triggered by that event can read before the save. Entering Stored and batch completion do not provide reliable fallback refreshes.
- **Is one CDK already one backend job?** Scheduling largely says yes, but event identity, resource leasing, cancellation semantics, and UI state do not yet preserve that model safely.

The separate 2FA backfill plan is not part of this normal registration path. It explicitly keeps `concurrency=1` and places parallel tuning out of scope at `.zcode/plans/plan-auto-reg-2fa-backfill.md:22` and `:209`.

## 2. Occurrences Found

- **14 deduplicated semantic issue groups**
- **Critical: 0**
- **Important: 9**
- **Minor: 5**
- Broad discovery found **308 search-token matches across 23 files**. This is search surface, not 308 defects.
- End-to-end dependency tracing expanded the reviewed surface to **30 files**, including seven locales and the separate 2FA plan boundary.
- **14 files contain direct issue evidence**: three frontend state/UI files, five Rust scheduling/contract/persistence files, and six locale mirrors.

## 3. Files Affected

Primary defect files:

- `src/hooks/use-registration-events.ts`
- `src/components/account-registration-dialog.tsx`
- `src/components/registration-progress-card.tsx`
- `src-tauri/src/auto_service/openai/register/commands.rs`
- `src-tauri/src/auto_service/openai/register/engine.rs`
- `src-tauri/src/auto_service/openai/register/types.rs`
- `src-tauri/src/auto_service/openai/register/task.rs`
- `src-tauri/src/auto_service/openai/register/store.rs`
- `src/i18n/locales/{es,fr,ja,pt,ru,zh}.json`

Reference/dependency files include `src/types.ts`, `src/app/page.tsx`, `src/components/registered-accounts-table.tsx`, VPN config/storage/Nord modules, the live CLI, login engine, English locale, docs, and both relevant plan files.

## 4. Issues by Severity

### Critical: 0

No finding meets the catastrophic threshold. The Important findings still require remediation before the Process view can be trusted as an operational representation of concurrency.

### Important: 9

#### I-1. Per-CDK progress collapses into one batch card

`use-registration-events.ts:127` keys `progressMap` by `event.payload.taskId`. Forked workers share that ID (`register/engine.rs:1091-1103`), despite events also carrying `cdkIndex`. `account-registration-dialog.tsx:999-1004` reuses the same ID for the React key and cancellation.

Effect: six concurrent CDKs produce one last-writer-wins card. The card's Cancel button cancels the entire batch, not the CDK shown.

#### I-2. Batch terminal events remain spinning and cancellable

The event contract lacks explicit `scope`, `eventKind`, `terminal`, and terminal outcome fields (`register/types.rs:790-806`). Sequential and parallel batch completion emit `result: None` (`register/engine.rs:1661-1668`, `:1805-1813`).

`registration-progress-card.tsx:53-54` derives terminal state only from `result`; without one it renders the spinner and Cancel button at `:69-75` and `:112-120`.

#### I-3. Network selection can silently switch to VPN and force the six-session policy

When Nord VPN inventory exists, `account-registration-dialog.tsx:108-121` changes an explicit `none` or `nord` mode to `vpn`. That can override the preference restored at `:133-159`.

VPN concurrency is disabled and resolved as `max_sessions ?? 6`, capped at six (`:652-687`). The operator can therefore select host IP or Nord CLI and later see a locked value of six because the mode changed underneath them.

#### I-4. Stored refresh races account persistence

Both successful and persisted `free_trial_no` result paths emit result-bearing progress before returning from `run_once` (`register/engine.rs:3023-3062`, `:3128-3179`). `process_one_cdk` saves only afterward at `:2020-2024`.

The frontend refreshes only when the earlier event contains `result` (`use-registration-events.ts:130-138`). Batch terminal events have no result, and entering Stored only changes tabs. The last account can remain absent until manual refresh or remount.

#### I-5. Credential persistence can report nondurable success

`register/store.rs:109-116` inserts into memory before serialization and `fs::write`, swallowing both errors. The public API returns no `Result` (`:213-215`). Listing reads the in-memory map, so an account can appear during the current process but disappear after restart.

This is separate from the refresh race: ordering fixes visibility, while an atomic Result-returning store is needed for durability.

#### I-6. Semaphore permits are not coupled to VPN peer ownership

Workers select peers by `cdkIndex % pool_len` (`register/engine.rs:1091-1095`), while a generic semaphore controls activation (`:1712-1744`). If a different slot frees first, a queued CDK can start while its modulo-selected peer is still owned by an active worker. Peer rotation can then disrupt another CDK.

One visible CDK card is not operationally truthful until each active job exclusively leases its peer slot.

#### I-7. Explicit `profileId` is cloned into concurrent workers

`fork_for_cdk` clones the config (`register/engine.rs:1101`), and `ensure_worker_profile` adopts the same explicit profile for each worker (`:4738` onward). Concurrent workers can launch, clear, and kill the same browser profile.

The smallest safe policy is `concurrency=1` whenever an explicit profile ID is supplied; cloning isolated profiles would be a larger feature.

#### I-8. Queued jobs can start after batch cancellation

Cancellation is checked while futures are enqueued (`register/engine.rs:1728`), but not immediately after `acquire_owned().await` and before redeem/process (`:1744-1754`). A queued CDK can consume work after the operator has cancelled the batch.

#### I-9. Generic WireGuard configs are routed through the Nord peer-pool path

The UI lists all VPN configs (`account-registration-dialog.tsx:817` area), while backend validation only confirms that the ID exists (`register/types.rs:186` area). VPN execution then uses the Nord peer-pool path (`register/engine.rs:1123-1178`) regardless of source.

Generic WireGuard and Nord-backed configurations need separate validated paths, or the UI must restrict this workflow to Nord-source configs.

### Minor: 5

#### M-1. Listener readiness is not gated before Start

`use-registration-events.ts:121-143` awaits `listen()` inside an async setup function, but calls that setup fire-and-forget. `startRegistration` at `:150-161` does not await readiness. The long-lived page mount reduces the likelihood but does not establish the required ordering contract.

#### M-2. Completed task handles are retained

Tasks are registered from `register/commands.rs:66`; `remove_task` and `cleanup_completed` in `register/task.rs:33-45` have no callers. Registry growth and stale successful cancellation responses accumulate across batches.

#### M-3. VPN concurrency helper and runtime policy disagree

`register/types.rs:251` defines and tests `min(requested, budget)`, but runtime VPN setup at `register/engine.rs:1509` replaces requested concurrency with the budget. The helper has no runtime caller, so its green tests do not cover production behavior.

#### M-4. Registration UI values remain English in six non-English locales

The reviewed registration namespace contains 18 untranslated semantic values across `es`, `fr`, `ja`, `pt`, `ru`, and `zh` around locale lines `1684-1746`. This is not causal to concurrency, but it affects the same Process/Stored operator surface.

#### M-5. No regression suite covers the concurrency/event/persistence contract

No focused tests were found for interleaved CDK events, one card per CDK, batch terminal rendering, cancellation after permit acquisition, peer leasing, save-before-event ordering, failed credential writes, listener readiness, or Stored refresh.

## 5. Recommended Fixes

1. **Add explicit additive event identity.** Keep `batchTaskId` as the batch/cancellation identity; add `jobId` or use a documented `taskId + cdkIndex` worker key. Add `scope`, `eventKind`, `terminal`, and structured `outcome` fields.

2. **Render one card per CDK.** Key cards by worker/job identity. Display aliases/accounts as sequential entries inside their CDK card. Keep a separate batch summary so a batch event cannot overwrite CDK index zero.

3. **Make cancellation scope honest.** Initially expose one clearly labelled batch Cancel control. Add per-card cancellation only after backend worker-specific flags/handles exist.

4. **Persist before emitting account completion.** Make `save_registration_result(...) -> Result<revision>`, complete persistence first, then emit a persisted-account event containing a safe account key and revision. Emit persistence failure rather than success if the write fails.

5. **Make credential writes atomic.** Serialize, write a temp file, flush/sync, atomically replace, then update memory. Reuse the quota-ledger persistence pattern in `register/store.rs:539` rather than keeping best-effort writes.

6. **Refresh Stored at three points.** Refresh after persisted-account events, on batch terminal as a fallback, and when entering the Stored tab. Retain manual refresh for recovery.

7. **Stop automatic network-mode mutation.** Nord inventory may preselect a VPN ID only after the operator chooses VPN. Explain that VPN concurrency follows the stored session budget; do not present six as a global default.

8. **Lease fixed worker resources.** Model a worker slot as `{slotId, vpnId/profileId, permit}` and dispatch the next CDK to whichever complete slot becomes free. Do not preassign peer ownership by CDK modulo.

9. **Add scheduler guards.** Recheck cancellation immediately after resource acquisition and before CDK redeem. Force serial execution for explicit `profileId`. Validate VPN source. Remove task handles on every terminal path.

10. **Add focused tests.** Cover six interleaved CDKs producing six cards, aliases staying within one card, batch versus worker terminal events, batch cancellation, queued cancellation, exclusive peer slots, explicit-profile serialization, event-after-persist, disk write failure, source validation, and Stored refresh.

## 6. Correct Patterns

- One future is already created per CDK, with aliases sequential within it: `register/engine.rs:1710-1754`, `:1918`.
- Global UI/Rust concurrency defaults are `1`: `account-registration-dialog.tsx:71`, `register/types.rs:104`.
- None/Proxy expose `1..8`; Nord CLI is serial; VPN derives a bounded session budget when VPN is genuinely selected: `account-registration-dialog.tsx:652-687`, `register/engine.rs:1477-1513`.
- The UI switches to Process after a successful Start: `account-registration-dialog.tsx:388`.
- Initial and manual account refresh controls are wired: `use-registration-events.ts:221`, `registered-accounts-table.tsx:404-425`, `:473-482`.
- The login engine is an existing save-before-result-event ordering reference: `login/engine.rs:1049-1071`, `:1177-1185`.
- The CDK quota ledger already provides a local temp-write/sync/atomic persistence pattern: `register/store.rs:539` area.
- Nord source validation exists elsewhere and can be reused: `vpn/nord.rs:478` area.

## Verification Notes

This audit was read-only apart from writing this report and preserving the previous recorder report as `audit-recorder.md`. No implementation changes were made.

No test pass is claimed. Reviewers reported that the current dirty worktree's ongoing 2FA backfill edits block a clean focused Rust compile because the new module/`RegistrationResult` changes are incomplete. The findings above are source-verified; implementation verification belongs to the subsequent remediation task.
