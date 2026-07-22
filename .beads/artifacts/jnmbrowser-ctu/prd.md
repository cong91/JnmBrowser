# PRD: CDK inventory picker + top-up mode

## Bead Metadata

```yaml
depends_on: []
parallel: false
conflicts_with: []
blocks: []
estimated_hours: 8
```

## Problem Statement

**WHEN** a user runs ChatGPT auto-registration for a CDK/card and the run ends early (errors, cancel, free-trial gate, network flake)

**THEN** CDK Stats shows `attempted < targetAccounts` for that card, but the only action is **Delete**.

**WHEN** they want to create more accounts on the **same** CDK up to the remaining provider budget (max 6)

**THEN** they must re-type the CDK, guess remaining capacity, or **delete the stats row** — which also wipes the usage ledger and history (`baseEmail`, per-account list, `lastError`).

**WHEN** the engine already clamps requests via `cdk_remaining_capacity` + `reserve_cdk_slots_per` (retry-cap flow)

**THEN** the UI does not surface remaining capacity or offer a first-class **Continue / Top-up** path that preserves history.

## Scope

### In Scope

- **Remaining capacity** visible in CDK Stats (table column and/or summary), sourced from the usage ledger truth (`cdk_remaining_capacity` / MAX 6 − used − reserved), not only `targetAccounts − attempted`.
- **Continue / Top-up action** on CDK inventory rows that still have remaining > 0.
- **Dialog CDK inventory picker / top-up mode** in Auto Registration:
  - User can pick an existing partial CDK from inventory (with remaining badge).
  - Selecting a row prefills the CDK code and sets default `accountsPerCdk` to remaining (user may lower, never raise above remaining for that start without engine clamp).
  - Raw multi-CDK text entry remains for **new** cards / multi-card batches.
- **Clamp** requested `accountsPerCdk` (and multi-CDK targets) to remaining capacity at start (UI + existing engine clamp).
- **Preserve history** on continue: no delete required; usage ledger and `CdkInventoryRecord` keep prior attempts/stats.
- Optional small backend surface:
  - Expose remaining capacity on list payload and/or a dedicated `get_cdk_remaining_capacity_cmd` (prefer enriching list if simple).
- i18n for all new user-facing strings in **7 locales** (`en`, `es`, `fr`, `ja`, `pt`, `ru`, `zh`).
- Docs note in `docs/auto-registration.md` for continue/top-up + remaining.

### Out of Scope

- Changing the hard cap (`MAX_ACCOUNTS_PER_CDK = 6`) or provider alias limits.
- Redesigning the usage ledger hashing / privacy model.
- **Quota-only reset** command (`reset_cdk_usage`) separate from delete — deferred (Option B from exploration); delete remains the destructive full clear.
- Auto-continue without user action after a partial batch.
- Concurrent multi-task reservation UI for the same CDK beyond existing engine reservations.
- SMS phone verification or network mode (proxy/VPN/Nord) changes.
- Merging CDK inventory with registered-accounts inventory tables.

## Proposed Solution

**Option C (from exploration):** treat “fill remaining slots on an existing CDK” as a first-class top-up flow, not a side-effect of retyping codes or deleting stats.

1. **Backend (minimal):** Ensure remaining capacity is queryable by the UI from the same source of truth as the engine (`cdk_remaining_capacity`). Prefer extending `list_cdk_inventory` / `CdkInventoryRecord` with a derived `remaining` field (or parallel map) so the table does not invent math from `targetAccounts`.
2. **CDK Stats table:** Show Remaining; enable **Top-up / Continue** when `remaining > 0`; keep Delete for full clear (history + quota).
3. **Registration dialog:** Add inventory picker / top-up mode:
   - List CDKs with remaining > 0 (or all with remaining badge).
   - On select: prefill CDK, default `accountsPerCdk = remaining` (clamped 1..remaining).
   - Start path clamps again before invoke; engine still applies retry-cap.
4. **Hooks:** Wire `list` refresh after progress events (already present); optional continue callback from table → switch tab + set top-up state.
5. **i18n + docs:** New keys; short docs section for continue/top-up.

## Requirements

### Functional

- **F1** CDK Stats shows remaining capacity per row from ledger-backed capacity (0..6), not a guess from UI-only counters alone.
- **F2** Rows with `remaining > 0` offer a Continue/Top-up control; rows at 0 do not (or show disabled + reason).
- **F3** Activating Continue/Top-up opens/focuses the Register tab with that CDK selected and `accountsPerCdk` defaulted to remaining (user can lower).
- **F4** Starting a top-up (or any start that includes a known CDK) never requests more than remaining for that CDK; engine clamp remains the final authority.
- **F5** Completing a top-up **does not** require deleting the CDK stats row; history (`accounts[]`, free-trial counts, lastError, baseEmail) is preserved and updated.
- **F6** Raw multi-line CDK entry still works for new cards and multi-CDK batches.
- **F7** Delete still removes stats row **and** usage ledger entry (existing behavior); document that this is the full reset path.
- **F8** Cancel mid-top-up continues to release unclaimed slots (existing `release_slot` / Drop behavior).

### Non-Functional

- Theme tokens only (no hardcoded Tailwind color classes).
- All user-facing strings via `t("registration.*")` (or existing namespace) in **all 7 locales**; no `t(key, "fallback")`.
- No unused Tauri commands (`pnpm check-unused-commands` stays green).
- Match surrounding idioms (hooks, dialog tabs, inventory tables).

## Success Criteria

- Verify: CDK Stats displays remaining capacity consistent with `cdk_remaining_capacity` for a partial CDK (e.g. used=2 → remaining=4).
- Verify: Continue/Top-up on a partial CDK prefills dialog and clamps `accountsPerCdk` to remaining.
- Verify: Starting top-up without delete updates the same `CdkInventoryRecord` (history retained; attempted/freeTrial counters advance).
- Verify: CDK at remaining=0 cannot start more accounts without delete (engine skip / UI disable).
- Verify: `pnpm lint:js` (Biome + tsc) clean for touched frontend files.
- Verify: `cargo test --manifest-path src-tauri/Cargo.toml --lib auto_service::openai::register::store::tests` passes.
- Verify: `pnpm lint:rust` or clippy on touched Rust files clean if commands/types change.
- Verify: New i18n keys present in all 7 locales with non-empty values.
- Verify: `docs/auto-registration.md` documents continue/top-up + remaining.

## Technical Context

### Existing patterns

| Piece | Path |
|-------|------|
| Quota ledger + remaining | `src-tauri/src/auto_service/openai/register/store.rs` — `MAX_ACCOUNTS_PER_CDK`, `cdk_remaining_capacity`, `reserve_cdk_slots_per`, `delete` clears usage |
| Engine retry-cap clamp | `src-tauri/src/auto_service/openai/register/engine.rs` — `run()` clamps `per_cdk_target` via remaining; logs “delete to reset” when full |
| Commands | `src-tauri/src/auto_service/openai/register/commands.rs` — `list_cdk_inventory_cmd`, `delete_cdk_inventory_cmd` only |
| Types | `src-tauri/src/auto_service/openai/register/types.rs` — `CdkInventoryRecord` (`target_accounts`, `attempted`, …) |
| CDK table | `src/components/cdk-inventory-table.tsx` — Refresh + Delete only |
| Dialog | `src/components/account-registration-dialog.tsx` — raw `cdkText` + `accountsPerCdk`; CDK tab hosts table |
| Hook | `src/hooks/use-registration-events.ts` — list/delete/start |
| Audit | `.zcode/artifacts/audit-recorder/audit-cdk-quota.md` — Option C chosen |

### Capacity model (do not break)

```
remaining = MAX_ACCOUNTS_PER_CDK - used(usage ledger) - reserved(in-flight)
claim_slot → usage += 1
release_slot / Drop → roll back unclaimed or failed claims
delete(cdk) → remove record + usage entry (destructive full reset)
```

UI remaining **must** match this formula (via backend), not invent from `targetAccounts − attempted` alone (target is cumulative ask, not quota truth).

### UX flow (target)

```
CDK Stats row remaining=4
  → Top-up
  → Register tab: CDK prefilled, accountsPerCdk default 4 (user may set 1..4)
  → Start
  → engine clamps + reserves ≤ remaining
  → same inventory row updates; history kept
```

## Affected Files

- `src-tauri/src/auto_service/openai/register/store.rs` (expose remaining on list / helper)
- `src-tauri/src/auto_service/openai/register/types.rs` (optional `remaining` on record or DTO)
- `src-tauri/src/auto_service/openai/register/commands.rs` (list enrichment and/or remaining cmd + register in `lib.rs` if new)
- `src-tauri/src/lib.rs` (only if new command)
- `src/hooks/use-registration-events.ts`
- `src/components/cdk-inventory-table.tsx`
- `src/components/account-registration-dialog.tsx`
- `src/i18n/locales/en.json`
- `src/i18n/locales/es.json`
- `src/i18n/locales/fr.json`
- `src/i18n/locales/ja.json`
- `src/i18n/locales/pt.json`
- `src/i18n/locales/ru.json`
- `src/i18n/locales/zh.json`
- `docs/auto-registration.md`

## Tasks

### Expose remaining capacity to UI [backend]

List inventory (or companion DTO) includes ledger-backed remaining slots per CDK (0..6) using the same calculation as `cdk_remaining_capacity`; unit tests cover partial used/reserved cases.

**Metadata:**

```yaml
depends_on: []
parallel: true
conflicts_with: []
files:
  - src-tauri/src/auto_service/openai/register/store.rs
  - src-tauri/src/auto_service/openai/register/types.rs
  - src-tauri/src/auto_service/openai/register/commands.rs
  - src-tauri/src/lib.rs
```

**Verification:**

- `cargo test --manifest-path src-tauri/Cargo.toml --lib auto_service::openai::register::store::tests`
- Assert remaining decreases with used/reserved; full CDK reports 0

### CDK Stats remaining + Top-up action [frontend]

`CdkInventoryTable` shows remaining and a Continue/Top-up control when remaining > 0; Delete remains for full clear; callback notifies parent with cdk + remaining.

**Metadata:**

```yaml
depends_on:
  - Expose remaining capacity to UI
parallel: false
conflicts_with: []
files:
  - src/components/cdk-inventory-table.tsx
  - src/hooks/use-registration-events.ts
```

**Verification:**

- Typecheck/biome clean for touched files
- UI: remaining visible; Top-up disabled or hidden when remaining=0

### Dialog picker + top-up start clamp [frontend]

Registration dialog supports selecting an existing inventory CDK (picker/top-up mode), prefills code, defaults and clamps `accountsPerCdk` to remaining; raw multi-CDK entry still works for new batches; start invokes engine without requiring delete.

**Metadata:**

```yaml
depends_on:
  - CDK Stats remaining + Top-up action
parallel: false
conflicts_with: []
files:
  - src/components/account-registration-dialog.tsx
  - src/hooks/use-registration-events.ts
```

**Verification:**

- Top-up path cannot submit accountsPerCdk > remaining for selected CDK
- History retained path: same CDK row still listed after top-up start (no delete)

### i18n + docs [i18n]

All 7 locales include new registration/CDK remaining and top-up strings (non-empty); `docs/auto-registration.md` documents remaining, Continue/Top-up, and that delete is full reset.

**Metadata:**

```yaml
depends_on: []
parallel: true
conflicts_with: []
files:
  - src/i18n/locales/en.json
  - src/i18n/locales/es.json
  - src/i18n/locales/fr.json
  - src/i18n/locales/ja.json
  - src/i18n/locales/pt.json
  - src/i18n/locales/ru.json
  - src/i18n/locales/zh.json
  - docs/auto-registration.md
```

**Verification:**

- Every new key exists in all 7 locales with non-empty values
- No `t(key, fallback)` usage
- Docs mention continue/top-up and remaining capacity

### Soften engine full-capacity messaging [backend]

When a CDK has remaining=0, engine messages guide users to free capacity via Delete (full reset) or note max 6 — not the only path for partial continue (partial continue is UI-led).

**Metadata:**

```yaml
depends_on: []
parallel: true
conflicts_with: []
files:
  - src-tauri/src/auto_service/openai/register/engine.rs
```

**Verification:**

- Logs/messages no longer imply delete is required for **partial** remaining cases
- Full capacity still refuses additional reservations

## Risks

| Risk | Mitigation |
|------|------------|
| UI remaining diverges from ledger | Single source: backend `cdk_remaining_capacity` / list enrichment |
| User confuses targetAccounts with quota | Label “Remaining (of 6)” / document cumulative target vs remaining |
| Top-up races with in-flight task | Engine reservations already block over-allocation; disable top-up while same CDK is `status=running` if detectable |
| New Tauri command unused | Prefer enrich list; if new cmd, wire UI immediately and keep check-unused green |
| i18n incomplete | Task gate: all 7 locales non-empty |

## Open Questions

- None blocking. Optional later: Option B `reset_cdk_usage` (quota-only reset without deleting history) — explicitly out of scope here.

## Non-Goals Recap

- No automatic background top-up.
- No change to MAX=6 or provider limits.
- No merge of inventory tables.
- No destructive rewrite of usage ledger format.
