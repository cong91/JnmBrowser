# PRD: Auto-login Stored inventory table

## Bead Metadata

```yaml
depends_on: []
parallel: false
conflicts_with: []
blocks: []
estimated_hours: 6
```

## Problem Statement

**WHEN** a user finishes ChatGPT/Codex auto-login and opens the Stored tab

**THEN** they only get a single “Export JSON” button that always exports every successful account with a token — including accounts already marked `exported`.

**WHEN** they cancel the save dialog after export is invoked

**THEN** rows may already be marked `exported` (mark happens before a successful file write), so inventory status becomes wrong.

**WHEN** they need seller/ops control (export only available, export selected subset, re-export intentionally)

**THEN** there is no checkbox selection, no status chips, and no bulk status actions — unlike the registered-accounts inventory.

## Scope

### In Scope

- New inventory table component for auto-login Stored results, mirroring `RegisteredAccountsTable` UX patterns (not CSV field presets).
- Checkbox multi-select + select-all for visible/filtered rows.
- Status filter chips: All / available / exported / used / invalid (login statuses; not registration’s sold/reserved).
- Export resolution:
  - **Has selection** → export selected rows that are `success && accessToken`.
  - **No selection** → export only `success && accessToken && status === "available"` (no silent re-export of already-exported).
- Mark-as-exported **only after** the JSON file is successfully written (cancel save = no status change).
- Optional UI toggle “Mark as exported on export” (default true), same idea as registration `markExportedOnExport`.
- Bulk status actions for selection: mark available / exported / used / invalid; delete selected.
- Push-to-Sub2API uses the same selection rule (selected else available/success).
- Backend: when `account_ids` is empty, default export filter excludes `status == exported` (and non-success / empty token). Explicit ids still allow intentional re-export.
- i18n keys for all 7 locales under `autoLogin.*`.
- Wire Stored tab in `account-login-dialog.tsx` to host the new table (remove inline minimal list export-only UX).

### Out of Scope

- Changing Sub2API JSON envelope (`type: sub2api-data`) — already fixed.
- CSV/TXT multi-format export and field presets from registration inventory.
- OAuth login engine / browser worker profile reuse changes.
- Merging login inventory with registered-accounts inventory into one shared abstract table (optional later).
- Auto-delete after export.

## Proposed Solution

1. **UI:** Add `src/components/login-accounts-table.tsx` (name may be `login-results-inventory-table.tsx` if clearer) modeled on `registered-accounts-table.tsx`:
   - local `selected: Set<string>`
   - `statusFilter` chips with counts
   - export target resolution as above
   - bulk status bar
   - export/push buttons in table toolbar
2. **Dialog:** `account-login-dialog.tsx` Stored tab renders the table with props from `useLoginEvents`.
3. **Hook:** Keep `exportAccountsJson(accountIds, { markExported, includeFailed })` but call sites must:
   - pass resolved ids (never rely on “export everything” for default UX)
   - only pass `markExported: true` **after** successful `writeTextFile`, or mark via `updateAccountStatus` after write
4. **Backend:** In `export_login_results_json`, empty `account_ids` path filters `success && !access_token.is_empty() && status != Exported` (or only Available). Non-empty ids keep current lookup + success/token filter.
5. **i18n:** Mirror registration inventory strings into `autoLogin` with login-specific status names (`used` not `sold`).

## Requirements

### Functional

- **F1** User can select individual rows and select-all filtered rows via checkboxes.
- **F2** User can filter by inventory status chips.
- **F3** Export with no selection only includes available successful logins with tokens.
- **F4** Export with selection includes only selected successful logins with tokens (may include previously exported).
- **F5** Canceling the save dialog does not change any row status.
- **F6** Successful export with mark-toggle on sets status `exported` and `exported_at`.
- **F7** Bulk mark status works for selected ids via existing `update_login_result_status_cmd`.
- **F8** Push Sub2API follows the same selection vs available/success rule.
- **F9** Per-row delete remains available; bulk delete selected is supported.

### Non-Functional

- Match existing theming (no hardcoded Tailwind color classes).
- All user-facing strings via `t("autoLogin.*")` in 7 locales.
- No new Tauri commands required if existing cmd surface is enough.

## Success Criteria

- Verify: Empty selection export never includes `status=exported` accounts.
- Verify: Selected previously-exported accounts can be re-exported intentionally.
- Verify: Cancel save dialog leaves statuses unchanged.
- Verify: `pnpm lint:js` (or Biome + tsc) clean for touched frontend files.
- Verify: `cargo test --manifest-path src-tauri/Cargo.toml --lib auto_service::openai::login::store::tests` passes.
- Verify: Manual Stored tab shows chips, checkboxes, export/push, bulk status.

## Technical Context

### Existing patterns

| Piece | Path |
|-------|------|
| Registration inventory UX | `src/components/registered-accounts-table.tsx` — selection, chips, export resolution `selected \|\| available` |
| Login Stored dialog | `src/components/account-login-dialog.tsx` — `handleExportJson` always `exportAccountsJson([], { markExported: true })` **before** save |
| Login hook | `src/hooks/use-login-events.ts` — `exportAccountsJson`, `updateAccountStatus`, `pushAccountsToSub2api` |
| Export store | `src-tauri/src/auto_service/openai/login/store.rs` — `export_login_results_json` (Sub2API envelope; no exclude-exported yet) |
| Commands | `src-tauri/src/auto_service/openai/login/commands.rs` — `export_login_results_cmd` |
| Types | Rust `LoginResultStatus`; TS `LoginResultStatus` in `use-login-events.ts` |

### Export resolution (target)

```
if selected.length > 0:
  targets = selected.filter(success && accessToken)
else:
  targets = accounts.filter(success && accessToken && status === "available")
exportAccountsJson(targetIds, { markExported: false })  // build JSON
// write file; on success only:
if markOnExport: updateAccountStatus(targetIds, "exported")
```

### Backend default (empty ids)

Empty `account_ids` should not re-export already-exported inventory silently:

```
success && !access_token.is_empty() && status != Exported
```

Explicit non-empty ids: keep success+token filter only (re-export allowed).

## Affected Files

- `src/components/login-accounts-table.tsx` (new)
- `src/components/account-login-dialog.tsx`
- `src/hooks/use-login-events.ts` (if mark-after-save helpers / defaults)
- `src-tauri/src/auto_service/openai/login/store.rs`
- `src-tauri/src/auto_service/openai/login/commands.rs` (only if API docs/defaults need tweak)
- `src/i18n/locales/en.json`
- `src/i18n/locales/es.json`
- `src/i18n/locales/fr.json`
- `src/i18n/locales/ja.json`
- `src/i18n/locales/pt.json`
- `src/i18n/locales/ru.json`
- `src/i18n/locales/zh.json`

## Tasks

### Backend default exclude exported [backend]

Empty `account_ids` export excludes already-exported results while still requiring success + non-empty access token; explicit ids still allow re-export of selected accounts.

**Metadata:**

```yaml
depends_on: []
parallel: true
conflicts_with: []
files:
  - src-tauri/src/auto_service/openai/login/store.rs
```

**Verification:**

- `cargo test --manifest-path src-tauri/Cargo.toml --lib auto_service::openai::login::store::tests`
- Unit coverage asserts empty-id selection omits `status=exported` when building account list logic (or pure helper test)

### Inventory table component [frontend]

`login-accounts-table.tsx` exists with checkboxes, status chips (all/available/exported/used/invalid), bulk status actions, export/push toolbar, and export target resolution matching registration (selected else available).

**Metadata:**

```yaml
depends_on: []
parallel: true
conflicts_with: []
files:
  - src/components/login-accounts-table.tsx
```

**Verification:**

- Component imports only theme/i18n/UI patterns consistent with `registered-accounts-table.tsx`
- Typecheck includes new component (`pnpm lint:js` or `tsc`)

### Mark-exported only after successful save [frontend]

Export flow builds JSON without marking, opens save dialog, writes file, then optionally marks exported; cancel leaves statuses unchanged.

**Metadata:**

```yaml
depends_on:
  - Inventory table component
parallel: false
conflicts_with: []
files:
  - src/components/login-accounts-table.tsx
  - src/hooks/use-login-events.ts
```

**Verification:**

- Code path: `markExported: false` (or no mark) before write; `updateAccountStatus(..., "exported")` only after successful write
- Cancel branch does not call status update

### Wire Stored tab + push selection rules [frontend]

`account-login-dialog.tsx` Stored tab hosts the inventory table; push uses selected-or-available rule; old flat list export-only UI removed.

**Metadata:**

```yaml
depends_on:
  - Inventory table component
  - Mark-exported only after successful save
parallel: false
conflicts_with: []
files:
  - src/components/account-login-dialog.tsx
```

**Verification:**

- Dialog compiles; Stored tab renders table
- No remaining always-export-all handler that marks before save

### i18n inventory keys [i18n]

All 7 locales gain `autoLogin` keys for status chips, selection counts, bulk mark actions, mark-on-export toggle, export selected labels (mirror registration where applicable; `used` not `sold`).

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
```

**Verification:**

- Every new key exists in all 7 locale files with non-empty values
- No `t(key, "fallback")` usage

## Risks

| Risk | Mitigation |
|------|------------|
| Copying too much registration CSV/preset UX | Explicit non-goal; Sub2API JSON only |
| Dirty main branch with unrelated WIP | Implement carefully; avoid unrelated commits |
| Push still exports “all success” including exported | Apply same selection rule as export |
| Backend exclude-exported breaks intentional “export all including exported” | Require explicit multi-select (select-all filtered including exported) |

## Open Questions

- None blocking. Default product rule: silent re-export forbidden; intentional re-export via selection only.

## Next Step

`/ship jnmbrowser-cnu` after `prd.json` is generated.
