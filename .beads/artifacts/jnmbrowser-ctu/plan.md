# CDK Inventory Picker + Top-up Mode — Implementation Plan

> **For the executing agent:** REQUIRED SUB-SKILL: Use skill({ name: "incremental-implementation" }) to implement this plan task-by-task. Prefer TDD for pure store remaining helpers. Do **not** implement until `/ship jnmbrowser-ctu` (this file is the plan only).

**Goal:** Users can see ledger-backed **remaining** capacity on CDK Stats, start a **Top-up / Continue** for a partial CDK without deleting history, and the registration dialog supports picking that CDK with `accountsPerCdk` clamped to remaining.

**Architecture:** Enrich `CdkInventoryRecord` with a **derived** `remaining` field (computed on list from `cdk_remaining_capacity` / usage+reservations; not the source of quota truth). Wire CDK table Top-up → dialog Register tab prefill + clamp. Keep raw multi-CDK text entry. Engine retry-cap remains final authority. No new Cargo/npm deps. Delete stays destructive full reset.

**Tech Stack:** Tauri v2 / Rust store+types, React dialog + inventory table, i18n 7 locales, Biome + clippy.

**Discovery Level:** 0 — pure internal; `cdk_remaining_capacity` + retry-cap already exist; audit + Option C exploration done.

**Context Budget:** ~45% for full plan; execute in **3 waves**. Target ~50% context per ship wave.

**Bead:** `jnmbrowser-ctu`  
**Branch:** `feat/jnmbrowser-ctu-cdk-top-up`  
**PRD:** `.beads/artifacts/jnmbrowser-ctu/prd.md`  
**Audit:** `.zcode/artifacts/audit-recorder/audit-cdk-quota.md`

**Worktree note:** Branch may carry unrelated dirty WIP (clippy fixes in `api_client` / `proxy_manager` / `app_auto_updater`, audit md). Stage **only** feature files when committing.

---

## Must-Haves

### Observable Truths

1. User opens **CDK stats** and sees **Remaining (of 6)** per row from ledger capacity (not only Attempted/Target).
2. User can click **Top-up / Continue** on a row with remaining > 0.
3. Top-up switches to **Register** tab with that CDK prefilled and `accountsPerCdk` defaulted to remaining (user may lower).
4. User can start top-up **without deleting** the CDK row; history (accounts list, free-trial counts, baseEmail) remains.
5. User cannot request more than remaining for that CDK from the UI; engine still refuses over-quota.
6. Rows with remaining = 0 show no usable Top-up (disabled/hidden + clear reason).
7. Raw multi-line CDK entry still works for new cards / multi-CDK batches.
8. New labels exist in all 7 locales; docs describe remaining + top-up + delete = full reset.

### Required Artifacts

| Artifact | Provides | Path |
|----------|----------|------|
| `remaining` on inventory record | Serde DTO field (derived on list) | `src-tauri/src/auto_service/openai/register/types.rs` |
| List enrichment + tests | Fill remaining from ledger | `src-tauri/src/auto_service/openai/register/store.rs` |
| Commands (if needed) | No new cmd if list carries remaining | `commands.rs` / `lib.rs` only if unavoidable |
| Engine messages | Full-cap wording (not “delete for partial”) | `src-tauri/src/auto_service/openai/register/engine.rs` |
| TS type + hook | `remaining` on inventory | `src/hooks/use-registration-events.ts` |
| CDK table UX | Remaining column + Top-up | `src/components/cdk-inventory-table.tsx` |
| Dialog top-up mode | Picker/prefill + clamp start | `src/components/account-registration-dialog.tsx` |
| i18n | 7 locales | `src/i18n/locales/{en,es,fr,ja,pt,ru,zh}.json` |
| Docs | Continue/top-up section | `docs/auto-registration.md` |

### Key Links

| From | To | Via | Risk |
|------|-----|-----|------|
| Usage ledger | UI Remaining | `list_all` / `list_cdk_inventory` sets `remaining` | UI invents `target − attempted` → wrong quota |
| Top-up button | Dialog state | `onTopUp(cdk, remaining)` | Parent ignores callback → dead button |
| Dialog `accountsPerCdk` | Start config | clamp 1..remaining for selected CDK | User raises above remaining → engine only soft-clamps multi-CDK list |
| Multi-CDK paste | Remaining map | clamp each known CDK | Unknown new CDK still full request (OK) |
| Delete | Usage + history | existing `delete_cdk_inventory` | Accidental delete while top-up intended |
| Persist JSON | Disk | `save` serializes record | Stale `remaining` on disk if not recomputed on list — **must recompute on every list** |
| Running status | Top-up affordance | `status === "running"` | Double start same CDK mid-batch |

### UX gates (dialog + table)

- **Primary action:** Start Registration remains single primary on Register tab; Top-up is secondary (outline/ghost).
- **Destructive:** Delete stays trash + existing confirm if any; do not put delete next to primary Start.
- **Forms:** Labels for remaining / top-up picker; helper that remaining is of 6 provider slots.
- **State coverage:** Empty CDK inventory (existing empty copy); remaining=0 row; running row (disable top-up); loading refresh.
- **Recovery:** Failed top-up start keeps history; user can lower count and retry; full cap points to Delete as full reset only.
- **a11y:** Buttons have accessible names via `t(...)`; table actions keyboard-focusable; no icon-only without aria-label.

---

## Dependency Graph

```
Task 1 (backend-1) Remaining on list + store tests: needs nothing
Task 2 (backend-2) Engine full-cap messages: needs nothing
Task 3 (i18n-1) Keys + docs skeleton: needs nothing (keys known from this plan)
Task 4 (frontend-1) Table remaining + Top-up: needs Task 1 (+ Task 3 keys preferred)
Task 5 (frontend-2) Dialog picker + clamp start: needs Task 4

Wave 1 (parallel): Task 1, Task 2, Task 3
Wave 2: Task 4
Wave 3: Task 5
```

---

## Tasks

### Task 1 — Expose ledger-backed `remaining` on inventory list [backend-1]

**Goal:** Every listed `CdkInventoryRecord` includes `remaining: u32` matching `remaining_capacity` (MAX − used − reserved).

**Files:**
- `src-tauri/src/auto_service/openai/register/types.rs`
- `src-tauri/src/auto_service/openai/register/store.rs`
- (avoid new Tauri command unless list enrichment is impossible)

**TDD steps:**

1. **RED** — Add unit test in `store.rs` `mod tests`:
   - Seed usage for `MAIL-REM` = 2 via `usage_key` + `persist_usage`.
   - `assert_eq!(store.lock().unwrap().remaining_capacity("MAIL-REM"), 4)`.
   - After `list` path that enriches records: insert a saved record for `MAIL-REM`, then list and assert `remaining == 4`.
   - With an active reservation of 1, remaining becomes 3.
2. **GREEN** — Implement:
   - Add to `CdkInventoryRecord`:
     ```rust
     /// Ledger-backed free slots (0..=MAX). Derived on list; not quota source of truth.
     #[serde(default)]
     pub remaining: u32,
     ```
   - Update `CdkInventoryRecord::new` to set `remaining: 0` (filled on list).
   - In `list_all` (or `list_cdk_inventory` wrapper): for each record, set  
     `record.remaining = self.remaining_capacity(&record.cdk)`.
   - Prefer **not** trusting disk `remaining`; always recompute on list.
3. **VERIFY**
   ```bash
   cargo test --manifest-path src-tauri/Cargo.toml --lib auto_service::openai::register::store::tests
   ```
   Expected: all store tests pass including new remaining cases.

**Non-goals:** New `get_cdk_remaining_capacity_cmd` unless list enrichment fails; changing MAX; quota-only reset.

---

### Task 2 — Soften engine full-capacity messaging [backend-2]

**Goal:** Messages when remaining=0 describe max capacity / full ledger; do not imply delete is required for **partial** continue (partial is UI top-up).

**Files:**
- `src-tauri/src/auto_service/openai/register/engine.rs` (~1397–1416)

**Steps:**

1. Locate logs:
   - `"skipping — delete its CDK stats row to reset"`
   - `"nothing to retry — delete a CDK stats row to reset its usage"`
2. Rewrite to something like:
   - Full: CDK at max capacity (6/6 used+reserved); free slots only after Delete (full reset) or when usage drops via failed-claim release.
   - Keep behavior: still push target 0 / fail batch if all zero.
3. **VERIFY:** `rg "delete its CDK stats" src-tauri/src/auto_service/openai/register/engine.rs` → no partial-continue confusion; compile with `cargo test --lib auto_service::openai::register` or project clippy on file.

**Non-goals:** Changing clamp logic (already correct).

---

### Task 3 — i18n keys + docs [i18n-1]

**Goal:** All 7 locales have non-empty keys for remaining/top-up; docs describe the flow.

**Files:**
- `src/i18n/locales/en.json` (+ es, fr, ja, pt, ru, zh)
- `docs/auto-registration.md`

**Suggested keys** (under `registration` — adjust names only if collision; keep camelCase consistent):

| Key | EN sense |
|-----|----------|
| `cdkRemaining` | Remaining |
| `cdkRemainingOfMax` | Remaining (of {{max}}) |
| `cdkTopUp` | Top-up |
| `cdkTopUpTitle` | Continue this CDK |
| `cdkTopUpHint` | Request more accounts up to remaining free slots. History is kept. |
| `cdkTopUpDisabledFull` | No free slots left (max 6). Delete stats to fully reset quota. |
| `cdkTopUpDisabledRunning` | Registration still running for this CDK. |
| `cdkPickFromInventory` | Use CDK from stats |
| `cdkPickPlaceholder` | Select a CDK with free slots… |
| `cdkRawEntryHint` | Or paste new CDK/card codes below |

**Steps:**

1. Add keys to **en** first, then mirror non-empty translations for es/fr/ja/pt/ru/zh (no empty strings).
2. Docs: short subsection under CDK inventory / usage — remaining from ledger, Top-up preserves history, Delete = history + quota reset, max 6.
3. **VERIFY:** grep each new key across 7 locale files; no `t(key, "fallback")` in new call sites (later tasks).

**Non-goals:** Redesign entire registration namespace.

---

### Task 4 — CDK Stats: remaining column + Top-up action [frontend-1]

**Goal:** Table shows remaining; Top-up invokes parent callback when remaining > 0 and status ≠ running.

**Files:**
- `src/hooks/use-registration-events.ts` — add `remaining?: number` (or required number default 0) to `CdkInventoryRecord`
- `src/components/cdk-inventory-table.tsx`
- Parent wiring may be partial until Task 5 (`onTopUp` prop)

**UX states:**

| State | UI |
|-------|-----|
| Empty inventory | existing empty + refresh |
| remaining > 0, not running | Top-up enabled |
| remaining = 0 | Top-up hidden or disabled + tooltip `cdkTopUpDisabledFull` |
| status = running | Top-up disabled + `cdkTopUpDisabledRunning` |
| Refresh | existing busy disable |

**Steps:**

1. Extend TS interface with `remaining: number`.
2. Props:
   ```ts
   onTopUp?: (cdk: string, remaining: number) => void;
   ```
3. Column header `t("registration.cdkRemaining")` / of-max variant.
4. Action button next to delete (not primary filled): icon + `t("registration.cdkTopUp")` or aria-label.
5. `e.stopPropagation()` so expand row still works.
6. **VERIFY:** `pnpm exec tsc --noEmit` (or `pnpm lint:js` if biome clean for these files). Visual: remaining numbers match backend after a real list invoke in dev later.

**Non-goals:** Full dialog picker (Task 5).

---

### Task 5 — Dialog inventory picker + top-up start clamp [frontend-2]

**Goal:** Top-up from table (and optional in-dialog picker) prefills CDK, sets accountsPerCdk to remaining, clamps on start; multi-CDK raw entry unchanged for new cards.

**Files:**
- `src/components/account-registration-dialog.tsx`
- `src/components/cdk-inventory-table.tsx` (wire `onTopUp`)
- `src/hooks/use-registration-events.ts` if helpers needed

**State (minimal):**

```ts
// e.g. when top-up active for a single CDK
const [topUpCdk, setTopUpCdk] = useState<string | null>(null);
const [topUpRemaining, setTopUpRemaining] = useState<number | null>(null);
```

**Flow:**

1. `handleTopUp(cdk, remaining)`:
   - `setActiveTab("register")`
   - `setCdkText(cdk)` (or keep multi-line safe: replace with single cdk for top-up mode)
   - `setAccountsPerCdk(clampAccountsPerCard(emailProvider, Math.max(1, remaining)))`
   - store `topUpRemaining = remaining`
2. Optional: Select of inventory rows with `remaining > 0` above CDK textarea (`cdkPickFromInventory`).
3. `handleStart`:
   - Parse CDKs as today.
   - If single CDK matches inventory with known remaining (from `cdkInventory` map),  
     `accountsPerCdk = min(effectiveAccountsPerCdk, remaining)`.
   - Multi-CDK: for each code, engine still clamps; optionally pre-clamp when remaining known from list.
4. Clear top-up mode when user clears textarea or switches provider if needed.
5. Wire table: `onTopUp={handleTopUp}`.

**UX:**

- Helper text under accountsPerCdk when top-up: remaining hint.
- Primary Start still one filled button.
- Toast if user tries start with remaining 0 for that only CDK.

**VERIFY:**

- Logic: cannot submit accountsPerCdk > remaining when topUpRemaining set (assert in handler).
- `tsc` / biome on touched files.
- Manual: partial CDK → Top-up → Start without Delete → same row still listed after progress.

**Non-goals:** Auto-continue after batch; reset_cdk_usage.

---

## Wave Execution Notes

| Wave | Tasks | Verify focus |
|------|-------|--------------|
| 1 | 1, 2, 3 | `cargo test …store::tests`; locale key presence; engine string grep |
| 2 | 4 | tsc; table props compile |
| 3 | 5 | tsc; start clamp; manual top-up path |

After each wave: keep build green; stage only feature files.

---

## Constitutional Compliance Gate

Scan of this plan:

| Pattern | Result |
|---------|--------|
| `git add .` / `git add -A` | **Absent** — plan says stage specific feature files |
| force push / `--no-verify` / `reset --hard` | **Absent** |
| New npm/cargo deps | **Absent** — “No new Cargo/npm deps” |
| `as any` / `@ts-ignore` | **Absent** |
| Secret patterns | **Absent** |

Task file counts: Task 1 ≤3 core files; Task 5 touches 2–3 UI files — OK.

**Constitutional compliance: ✓ PASS**

---

## Out of Scope (do not implement in this plan)

- `reset_cdk_usage` (Option B)
- Changing MAX=6
- Auto top-up after partial batch
- Merging inventory tables
- SMS / network mode changes

---

## Completion Checklist (for `/ship`)

- [ ] Store tests green including remaining enrichment
- [ ] List payload includes correct remaining
- [ ] Table shows remaining + Top-up
- [ ] Dialog top-up clamp + history preserved
- [ ] 7 locales non-empty keys
- [ ] Docs updated
- [ ] Engine messages updated
- [ ] No unused Tauri commands
- [ ] Feature commits do not bundle unrelated clippy WIP unless intentional
