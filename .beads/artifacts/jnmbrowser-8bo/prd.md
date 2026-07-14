# PRD: Action Recorder UX Improvements

## Problem Statement

**WHEN** users want to record browser actions for automation, they must manually launch a profile first, then find and click a small record button. If they forget to stop recording before killing the browser, all recorded actions are silently lost.

**THEN** users experience friction in starting recordings and data loss when recordings aren't saved.

The current action recorder implementation has four UX gaps:
1. No "Launch with Record" menu item (users must launch first, then enable recording)
2. Recordings are lost when the browser is killed while recording is active
3. REC status indicator is only visible in the actions column, not prominent
4. Missing i18n keys for the new UX flows

## Scope

### In Scope
- Add "Launch with Record" menu item to ProfileInfoDialog (mirroring "Launch with Sync")
- Auto-save recording when browser is killed (hook into kill flow)
- Add prominent REC badge/indicator visible when recording is active
- Add i18n keys for all 7 locales (en, es, fr, ja, pt, ru, zh)
- Integration tests for browser kill + recording auto-save

### Out of Scope
- Bulk recording controls (record multiple profiles at once)
- Recording editing/trimming UI
- Cloud sync for recordings
- Recording sharing/export formats beyond MCP recipe

## Proposed Solution

### 1. Launch with Record Menu Item
Add a new `ActionItem` to `ProfileInfoDialog` following the exact pattern of "Launch with Sync":
- Icon: `LuCircle` (matching the record button)
- Label: `recorder.launchWithRecord`
- Hidden when: profile is not chromium/camoufox OR `onLaunchWithRecord` callback not provided
- Disabled when: `isDisabled || isRunning`
- Behavior: Launch profile → wait for running → start recording

### 2. Auto-Save on Browser Kill
Hook into `kill_browser_profile` command (backend) or `handleKillProfile` (frontend) to call `stop_recording` before killing the browser.

**Preferred approach**: Frontend hook in `handleKillProfile` because:
- We already have `getProfileRecorderInfo(profile.id)` available
- Keeps kill logic centralized in one place
- Easier to show user feedback (toast) after auto-save

**Alternative considered**: Backend hook in `browser_runner.rs:2527` (before `kill_browser_process`)
- Pros: Catches all kill paths
- Cons: Harder to show user feedback, requires passing `app_handle` through more layers

### 3. Prominent REC Indicator
Add a persistent badge/indicator visible when recording is active:
- Location: Next to profile name or in a dedicated status column
- Style: Red "REC" badge (matching existing `recorder.recBadge`)
- Visibility: Always visible when `getProfileRecorderInfo(profile.id)` returns a session

### 4. i18n Keys
Add new keys under `recorder.*` namespace:
- `recorder.launchWithRecord` - menu item label
- `recorder.autoSaved` - toast message when recording auto-saved on kill
- `recorder.launchFailed` - error when launch-with-record fails
- `recorder.recordingActive` - tooltip/status text when recording is active

## Success Criteria

- [ ] ProfileInfoDialog shows "Launch with Record" menu item for chromium/camoufox profiles
- [ ] Clicking "Launch with Record" launches profile and starts recording automatically
- [ ] Killing a browser while recording is active saves the recording before kill
- [ ] User sees success toast when recording is auto-saved on kill
- [ ] REC badge is visible in profile list when recording is active
- [ ] All new UI strings have i18n keys in all 7 locales
- [ ] Integration test passes: launch → record → kill → verify recording saved

**Verify:**
```bash
pnpm lint
pnpm test
# Manual test: Launch chromium profile via "Launch with Record", perform actions, kill browser, verify recording appears in list
```

## Technical Context

### Backend Architecture
- `RecorderManager` (singleton in `src-tauri/src/recorder.rs:85`) manages active recording sessions
- `stop_recording` command (`src-tauri/src/recorder/commands.rs:99-106`) persists recording and emits events
- `stop_all()` method exists (`src-tauri/src/recorder.rs:205-229`) but is marked `#[allow(dead_code)]` and never called
- `kill_browser_profile` command (`src-tauri/src/browser_runner.rs:2517-2620`) orchestrates browser shutdown

### Frontend Architecture
- `useRecorderSessions` hook (`src/hooks/use-recorder-session.ts`) provides `getProfileRecorderInfo(profileId)`
- `handleKillProfile` (`src/app/page.tsx:575-592`) calls `kill_browser_profile` without checking recording state
- `ProfileInfoDialog` (`src/components/profile-info-dialog.tsx`) uses `ActionItem[]` array for menu items
- "Launch with Sync" pattern at `profile-info-dialog.tsx:279-287` shows exact structure needed

### Key Integration Points
- Profile launch flow: `launchProfile` → `profile-running-changed` event → `runningProfiles` state update
- Recording start: `start_recording(profileId)` requires profile to already be running
- Profile kill: `kill_browser_profile` emits `profile-running-changed` with `is_running: false`

## Affected Files

### Frontend
- `src/components/profile-info-dialog.tsx` - add "Launch with Record" menu item
- `src/app/page.tsx` - wire `onLaunchWithRecord` callback, hook `handleKillProfile` to auto-save
- `src/hooks/use-recorder-session.ts` - no changes needed (already provides `getProfileRecorderInfo`)

### Backend
- `src-tauri/src/browser_runner.rs` - no changes needed (frontend hook preferred)
- `src-tauri/src/recorder.rs` - no changes needed (`stop_all` exists but unused)
- `src-tauri/src/recorder/commands.rs` - no changes needed

### i18n
- `src/i18n/locales/en.json` - add `recorder.launchWithRecord`, `recorder.autoSaved`, `recorder.launchFailed`, `recorder.recordingActive`
- `src/i18n/locales/es.json` - Spanish translations
- `src/i18n/locales/fr.json` - French translations
- `src/i18n/locales/ja.json` - Japanese translations
- `src/i18n/locales/pt.json` - Portuguese translations
- `src/i18n/locales/ru.json` - Russian translations
- `src/i18n/locales/zh.json` - Chinese translations

### Tests
- `src-tauri/tests/recorder_integration.rs` - new test file for browser kill + recording auto-save

## Tasks

### Task 1: Add i18n keys for recorder UX [localization]

Add new i18n keys to all 7 locale files for the launch-with-record flow and auto-save feedback.

**Metadata:**
- `depends_on`: []
- `parallel`: true
- `conflicts_with`: []
- `files`: [src/i18n/locales/en.json, src/i18n/locales/es.json, src/i18n/locales/fr.json, src/i18n/locales/ja.json, src/i18n/locales/pt.json, src/i18n/locales/ru.json, src/i18n/locales/zh.json]

**Verification:**
- [ ] `pnpm lint:spell` passes (no typos)
- [ ] All 7 locale files have `recorder.launchWithRecord`, `recorder.autoSaved`, `recorder.launchFailed`, `recorder.recordingActive` keys
- [ ] Keys are non-empty strings in all locales

---

### Task 2: Add "Launch with Record" menu item to ProfileInfoDialog [ui]

Add a new ActionItem to ProfileInfoDialog following the "Launch with Sync" pattern, with proper visibility and disabled states.

**Metadata:**
- `depends_on`: [Task 1]
- `parallel`: false
- `conflicts_with`: []
- `files`: [src/components/profile-info-dialog.tsx]

**Verification:**
- [ ] ProfileInfoDialog shows "Launch with Record" menu item
- [ ] Menu item is hidden for non-chromium/camoufox profiles
- [ ] Menu item is disabled when `isRunning || isDisabled`
- [ ] Menu item uses `LuCircle` icon and `recorder.launchWithRecord` label
- [ ] `pnpm lint` passes

---

### Task 3: Wire onLaunchWithRecord callback in page.tsx [integration]

Add `onLaunchWithRecord` prop to ProfilesDataTable and wire it to launch profile + start recording sequence.

**Metadata:**
- `depends_on`: [Task 2]
- `parallel`: false
- `conflicts_with`: []
- `files`: [src/app/page.tsx, src/components/profile-data-table.tsx]

**Verification:**
- [ ] Clicking "Launch with Record" launches the profile
- [ ] Recording starts automatically after profile is running
- [ ] Success toast shows `recorder.started` message
- [ ] Error toast shows `recorder.launchFailed` if launch or recording fails
- [ ] `pnpm lint` passes

---

### Task 4: Hook handleKillProfile to auto-save recording [integration]

Modify `handleKillProfile` in page.tsx to check for active recording and call `stop_recording` before killing the browser.

**Metadata:**
- `depends_on`: [Task 1]
- `parallel`: true
- `conflicts_with`: []
- `files`: [src/app/page.tsx]

**Verification:**
- [ ] Killing a browser while recording is active saves the recording
- [ ] Success toast shows `recorder.autoSaved` message with event count
- [ ] Recording appears in `list_recordings` after kill
- [ ] `pnpm lint` passes

---

### Task 5: Add prominent REC indicator [ui]

Add a persistent badge or status indicator visible when recording is active, positioned prominently in the profile list.

**Metadata:**
- `depends_on`: []
- `parallel`: true
- `conflicts_with`: []
- `files`: [src/components/profile-data-table.tsx]

**Verification:**
- [ ] REC badge is visible when `getProfileRecorderInfo(profile.id)` returns a session
- [ ] Badge uses `recorder.recBadge` label
- [ ] Badge is styled with red/destructive color
- [ ] `pnpm lint` passes

---

### Task 6: Add integration test for recording auto-save [testing]

Create `src-tauri/tests/recorder_integration.rs` to test the browser kill + recording auto-save flow.

**Metadata:**
- `depends_on`: [Task 4]
- `parallel`: false
- `conflicts_with`: []
- `files`: [src-tauri/tests/recorder_integration.rs]

**Verification:**
- [ ] Test launches chromium profile
- [ ] Test starts recording
- [ ] Test kills browser
- [ ] Test verifies recording was saved to disk
- [ ] `pnpm test` passes
- [ ] `cargo test --test recorder_integration` passes

---

### Task 7: Verify end-to-end flow [verification]

Manual verification of the complete launch-with-record and auto-save-on-kill flows.

**Metadata:**
- `depends_on`: [Task 3, Task 4, Task 5, Task 6]
- `parallel`: false
- `conflicts_with`: []
- `files`: []

**Verification:**
- [ ] Launch chromium profile via "Launch with Record" menu
- [ ] Perform browser actions (navigate, click, type)
- [ ] Kill browser
- [ ] Open RecorderDialog and verify recording appears
- [ ] Export recording as MCP recipe and verify it contains the performed actions
- [ ] Replay recording on another profile and verify actions are reproduced

## Risks

### High Risk
- **Race condition in launch-then-record**: If `start_recording` is called before profile is fully running, it may fail. Mitigation: wait for `profile-running-changed` event with `is_running: true` before calling `start_recording`.

### Medium Risk
- **Auto-save blocking kill**: If `stop_recording` takes too long, it may delay browser kill. Mitigation: add timeout (e.g., 5 seconds) and proceed with kill even if save fails.

### Low Risk
- **i18n translation quality**: Non-English translations may be machine-generated. Mitigation: use simple, clear English source strings that translate well.

## Open Questions

None.
