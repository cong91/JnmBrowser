# Audit: CDK Quota Management for Auto-Registration

**Pattern:** CDK (card) quota lifecycle — capacity model, usage ledger, reservations, failure rollback, retry/continue semantics, and the "only delete resets quota" problem.

**User concern:** "Hiện tại CDK chỉ xóa được thôi. Check CDK Stats thấy chưa đủ account per CDK, muốn tạo tiếp (fill remaining) thì sao? Trong quá trình tạo có thể lỗi giữa chừng."

**Date:** 2026-07-22  
**Scope:** Backend (store + engine + commands) + Frontend (dialog + cdk table + events)

---

## 1. Summary

The quota system is **mechanically correct**:
- Hard cap of 6 per card (MAX_ACCOUNTS_PER_CDK).
- Persisted usage ledger + in-flight reservations prevent over-allocation.
- `cdk_remaining_capacity` correctly computes headroom.
- Engine clamps requested slots to remaining at batch start (retry-cap flow).
- `release_slot` + Drop guard correctly roll back failed/cancelled attempts so later runs see capacity.

**The problem is UX + command surface:**
- Only two CDK commands exist: `list_cdk_inventory_cmd` and `delete_cdk_inventory_cmd`.
- `delete` is destructive: removes both visible `CdkInventoryRecord` (history, stats, per-account list, lastError) **and** the usage ledger entry.
- CDK Stats table only offers Refresh + Delete. No "Continue", "Top-up", or "Request N more".
- Registration dialog only accepts raw CDK codes + a fresh `accountsPerCdk`. It does not target existing partial rows or clamp to remaining.
- Engine already supports continuing a partial CDK, but users have no path to do it without deleting history.

**Severity:** Important (UX friction + surprising data loss on "reset").

---

## 2. Key Files & Evidence

### Backend

**store.rs (quota truth)**
- `MAX_ACCOUNTS_PER_CDK = 6` (line 17)
- `CdkStore { records, usage: HashMap<String,u32>, reservations }`
- `usage-ledger.json` (persisted, hashed keys)
- `check_capacity` (used + reserved + requested > 6 → error)
- `remaining_capacity` = MAX - (used + reserved)
- `CdkSlotReservation { claim_slot, release_slot }` + Drop guard
- `delete(cdk)` (342): removes record files + `usage.remove(usage_key)` + `reservations.remove` + persist
- `cdk_remaining_capacity` public helper (703)

**engine.rs (retry-cap + rollback)**
- `run()`: for each CDK, `remaining = cdk_remaining_capacity(cdk)`; if 0 → push target=0 + log "delete its CDK stats row to reset" (1397-1416)
- `reserve_cdk_slots_per` + per-task reservations
- On start for a CDK: `target_accounts += accounts_per` (cumulative) (1863)
- `release_slot()` calls on:
  - cancel before claim (1949)
  - browser kill failure (2151)
  - finished_result.is_none() after retries (2178)
- `CdkInventoryRecord::new(cdk, 0, task)` then mutate `target_accounts`

**commands.rs (exposed surface)**
- `list_cdk_inventory_cmd`
- `delete_cdk_inventory_cmd` (only destructive reset)
- No `reset_usage`, no `get_remaining`

**types.rs**
- `CdkInventoryRecord { target_accounts, attempted, free_trial_yes, free_trial_no, failed, ... }`

### Frontend

**account-registration-dialog.tsx**
- Inputs: `cdkText` (raw codes), `accountsPerCdk` (default 1, clamped by provider)
- `handleStart` parses `cdkText`, builds config with `accountsPerCdk: effective...`, calls `startRegistration`
- CDK Stats tab: just renders `<CdkInventoryTable records={cdkInventory} onDelete={deleteCdkRecord} />`
- No pre-selection of existing partial CDK rows, no "continue this row" flow

**cdk-inventory-table.tsx**
- Shows: cdk | baseEmail | attempted/target | freeTrialYes | freeTrialNo | failed | status | (expand accounts + lastError)
- Actions: Refresh + Delete (trash icon)
- No remaining column, no Continue button

**use-registration-events.ts**
- `cdkInventory` state
- `refreshCdkInventory()` → `list_cdk_inventory_cmd`
- `deleteCdkRecord(cdk)` → `delete_cdk_inventory_cmd`

---

## 3. Lifecycle Walkthrough (what actually happens)

1. **User starts batch** with CDK "MAIL-ABC", accountsPerCdk=3.
2. Engine: `remaining = cdk_remaining_capacity("MAIL-ABC")` (usually 6 first time).
3. `reserve_cdk_slots_per` creates `CdkSlotReservation` (requested=3).
4. `CdkInventoryRecord::new(cdk, 0, task)` then `target_accounts += 3`.
5. For each alias:
   - `claim_slot()` → usage +=1, reservation.remaining -=1
   - Run registration.
   - On success/fail: record into CDK stats.
6. On failure/cancel for an alias:
   - `release_slot()` → usage -=1, re-absorb into reservation.
7. After run:
   - Stats row shows e.g. attempted=1, target=3, freeTrialYes=1.
   - Usage ledger reflects 1 used (or less if failures rolled back).
8. **User wants 2 more for same CDK:**
   - Currently: must delete the stats row (which also clears usage ledger) → re-enter CDK + accountsPerCdk=2 (or 3).
   - Engine would have allowed `target=2` (clamped) without delete.

**Key invariant:** The usage ledger (not the visible `target_accounts`) is the source of truth for quota. Deleting the visible row also deletes the quota record — that's the bug in the user's mental model.

---

## 4. Issues by Severity

### Important
- **I1:** Delete is the only way to free quota. It also destroys history.  
  `store.rs:342-376` (delete clears record + usage + reservations); engine logs at `engine.rs:1400,1416`.

- **I2:** No "Continue / Top-up" action or targeting of existing CDK rows.  
  Dialog always takes raw codes; table only has Delete.

- **I3:** Remaining capacity is not visible in CDK Stats.

### Minor
- Engine messages tell users to "delete" instead of guiding to a continue path.
- `target_accounts` is cumulative (can grow >6 over multiple starts), but UI shows it as "the target".
- No non-destructive `reset_cdk_usage` command.

### Correct (do not regress)
- Hard cap enforcement via ledger + reservations.
- `release_slot` on all failure paths (within-run and across runs).
- Retry-cap clamping at batch start.
- Separation of visible stats vs quota ledger (the design is right; the delete operation just conflates them).

---

## 5. Recommended Fixes (Actionable)

**P0 (UX to unblock the user's scenario)**

1. Show "Remaining" in CDK table (compute from usage ledger or add a small query).
2. Add "Continue" / "Request more" button on rows that have remaining > 0.
   - On click: open registration dialog pre-filled with that CDK and `accountsPerCdk` clamped to remaining (or let user choose <= remaining).
3. In start flow, support targeting an existing CDK row (not only raw text input) and clamp the requested count.

**P1 (Make reset non-destructive)**

4. Add `reset_cdk_usage_cmd(cdk)` — clears only the usage ledger entry, keeps the `CdkInventoryRecord`.
5. In table, offer two actions:
   - "Reset quota (keep history)"
   - "Delete history + quota"

**P2 (Polish)**

6. Update engine log messages: instead of "delete its CDK stats row to reset", say the CDK has reached its 6-account cap or has no remaining slots under current usage; point to CDK Stats "Continue" when available.
7. After a partial run, if any CDK has remaining, surface a hint linking to CDK Stats.
8. Consider showing cumulative target vs "effective remaining for this card" clearly.

---

## 6. File:Line References (Key)

- Hard cap & ledger: `store.rs:17`, `18` (USAGE_LEDGER_FILE), `535-552` (persist)
- Delete wipes quota: `store.rs:342-376`
- Remaining capacity: `store.rs:405-420`, `703-708`
- Reservation + release: `store.rs:34-84`, `441-533`
- Engine retry clamp + "delete to reset" logs: `engine.rs:1391-1416`
- Target accumulation: `engine.rs:1861-1863`
- Failure release sites: `engine.rs:1949`, `2151`, `2178`
- Commands (only list + delete): `commands.rs:127`, `133`
- UI table (only delete): `cdk-inventory-table.tsx:43,163`
- Dialog start (raw codes): `account-registration-dialog.tsx:148-154`, `891-898`

---

## 7. Conclusion

The backend quota model is solid and already supports "continue for remaining slots." The gap is entirely in the command surface and UI:

- Only destructive delete frees quota.
- No way to target a partial CDK row to request the missing accounts.
- Remaining capacity is invisible.

Implementing P0 (Continue action + remaining visibility) + optionally P1 (quota-only reset) will directly solve the user's problem without weakening safety invariants.