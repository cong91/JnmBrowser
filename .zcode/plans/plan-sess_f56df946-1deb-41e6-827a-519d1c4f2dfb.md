# Kế hoạch triển khai Action Recorder (record → save → replay)

## Mục tiêu (deliverables)

1. **Backend Rust module** `recorder.rs` (file mới trong `src-tauri/src/`) — singleton `RecorderManager`, capture + replay + lưu file + export recipe, **dual-kernel** (Chromium qua CDP + Camoufox qua Playwright `Page`).
2. **Storage** — 1 file JSON `/recording_<ts>_<rand>.json` mỗi session, ở `data_dir()/recordings/`. **Event-stream JSON format (raw)** làm định dạng chính, **export McpRecipe JSON** làm format phát sinh.
3. **MCP tools** trong `mcp_server.rs` — `start_action_recording`, `stop_action_recording`, `get_recorded_events`, `save_recording`, `list_recordings`, `export_recording_as_recipe`.
4. **Tauri commands** trong `lib.rs` để frontend gọi — `start_recording`, `stop_recording`, `get_recorder_sessions`, `list_recordings`, `delete_recording`, `export_recording_as_recipe`.
5. **Frontend** — hook `use-recorder-session.ts`, dialog quản lý recording, **nút Record** trong profile row (profile-data-table.tsx), wiring qua `page.tsx`.
6. **i18n** — thêm namespace `recorder` vào **TẤT CẢ 7 locale files** (en, es, fr, ja, pt, ru, zh).
7. **Privacy**: raw capture, không redaction (anh đã accept rủi ro local-only).
8. **Verify**: `pnpm format && pnpm lint && pnpm test` xanh trước khi kết thúc.

---

## Quyết định thiết kế

### Capture path: Inject-script recorder (KHÔNG dùng Wayfern)

Lý do: khỏi phụ thuộc `Wayfern.enableInputCapture` đang legacy/fragile. Dùng một content script tag ghi sự kiện DOM (click/input/keydown/scroll/submit/navigation) giống Playwright codegen.

**Cơ chế page → Rust:** inject script gọi `console.log("__REC__:" + JSON.stringify(event))`. Persistent listener task lắng nghe:
- **Chromium**: `Runtime.consoleAPICalled` events trên persistent WS (giống `run_console_capture` ở `mcp_server.rs:3421`).
- **Camoufox**: `playwright::api::page::Event::Console` qua `page.on_event()` (đã dùng ở `mcp_server.rs:3910`).

### Injection persistent qua navigation

- **Chromium**: `Page.enable` + `Page.addScriptToEvaluateOnNewDocument { source, runImmediately: true }` (giống `chromium_manager.rs:498` để fingerprint script sống qua navigate).
- **Camoufox**: `page.add_init_script(source)` của Playwright-rust (auto inject mỗi frame mới).

### RecordedEvent struct (định dạng raw, schema v1)

```rust
pub struct RecordingHeader {
  pub version: u32,          // = 1
  pub id: String,            // "rec_<ts>_<rand>"
  pub created_at: String,    // ISO8601
  pub profile_id: String,
  pub profile_name: String,
  pub browser: String,       // "chromium" | "camoufox"
  pub start_url: String,
  pub viewport: Option<Viewport>,
  pub event_count: u32,
}

pub struct RecordedEvent {
  pub t_ms: u64,             // offset ms từ recording start
  pub kind: String,          // "click"|"input"|"keydown"|"scroll"|"submit"|"navigate"|"wait"|"tab_open"|"tab_close"|"tab_switch"
  pub target: Option<RecordedTarget>,  // element target info (nếu có)
  pub payload: serde_json::Value,        // raw event data
}

pub struct RecordedTarget {
  pub tag: Option<String>,
  pub id: Option<String>,
  pub locators: Vec<LocatorCandidate>,  // testid, role+name, label, text, css, xpath
  pub rect: Option<Rect>,
  pub attributes: serde_json::Value,
}

pub struct LocatorCandidate {
  pub by: String,            // "testid"|"role"|"label"|"text"|"css"|"xpath"|"placeholder"
  pub value: String,
  pub nth: Option<usize>,
  pub exact: Option<bool>,
  pub preferred: bool,       // best heuristic choice
}

pub struct Recording {
  pub header: RecordingHeader,
  pub events: Vec<RecordedEvent>,
}
```

### Player (replay raw): reuse tối đa code sẵn

- **Chromium**: dùng `Input.dispatchMouseEvent` / `Input.dispatchKeyEvent` (giống `synchronizer.rs:729-802` và `human_typing.rs`), kèm `tokio::time::sleep` theo `t_ms` để giữ human pacing.
- **Camoufox**: Playwright `page.mouse().click/down/up`, `page.keyboard().press/type`, `page.goto`.
- Raw player chạy trên profile đang chạy (hoặc launch mới). Mỗi raw event → 1 CDP/Playwright lệnh. Không yêu cầu selectors khớp 100% (chấp nhận coordinate-key replay).

### Recipe export (semantic lift)

Convert raw events → `McpRecipe` bằng các rule đơn giản:
- `navigate` → step `navigate` (lấy `url`)
- `click` → step `click_element` (lấy best locator từ `target.locators`, ưu tiên testid > role > label > text > css)
- `input` & `keydown` liên tiếp trên cùng target → gộp → step `type_text` với full text (cậu chuỗi keystrokes)
- `scroll` → step `scroll_to` (nếu `target`); ngược lại `evaluate_javascript` với `window.scrollBy`
- `submit` → không cần step riêng (navigations đã capture)
- `tab_open`/`tab_switch`/`tab_close` → `new_tab`/`switch_tab`/`close_tab`
- Add `wait_for_navigation` sau mỗi `navigate` và sau `submit`

Output: `McpRecipe` JSON với `inputs: { "profile_id": "{{inputs.profile_id}}" }` — chạy được ngay qua `validate_recipe` → `preview_recipe` → `run_recipe` / `run_batch_profile_workflow`.

Export tool cũng cho phép chỉ xuất text export (file `.json` riêng) cho dev tích hợp code.

---

## Files cần thêm / sửa

### Backend (Rust)

| Thao tác | File | Mô tả |
|---|---|---|
| THÊM | `src-tauri/src/recorder.rs` | Module chính (~600-800 dòng): `RecorderManager` singleton (OnceLock), `RecordingSession` internal state với `cancel_tx: watch::Sender<bool>`, `shared: Arc<AsyncMutex<RecorderShared>>` chứa `VecDeque<RecordedEvent>` + `running: AtomicBool` + `last_error`. |
| THÊM | `src-tauri/src/recorder/inject_script.rs` | Raw-string `recorder_script() -> &'static str` — JS content-script attach listeners (click/input/keydown/scroll/submit, push via `console.log("__REC__:" + json)`), tự skip trên passwords nếu cần (theo option). |
| THÊM | `src-tauri/src/recorder/capture.rs` | Hàm async `run_chromium_recorder(profile_id, ws_url, shared, shutdown_rx, ready_tx)` — enable Page/Runtime, `addScriptToEvaluateOnNewDocument`, lắng nghe `Runtime.consoleAPICalled`, parse tag, push `RecordedEvent`. |
| THÊM | `src-tauri/src/recorder/camoufox_capture.rs` | Hàm async `run_camoufox_recorder(profile_id, page, shared, shutdown_rx, ready_tx)` — `page.add_init_script`, listen `Event::Console`, parse tag, push event. |
| THÊM | `src-tauri/src/recorder/player.rs` | `replay_recording(profile_id, recording)` — dual-kernel player ăn raw events, sleep theo `t_ms`, dispatch CDP/Playwright. Reuse `human_typing.rs`. |
| THÊM | `src-tauri/src/recorder/recipe_export.rs` | `export_to_recipe(recording) -> McpRecipe` — semantic lift rules như §trên. Tái dùng `McpRecipe`/`McpRecipeStep` từ `mcp_server.rs` (export public hoặc re-define compatible struct). |
| THÊM | `src-tauri/src/recorder/storage.rs` | Free functions `save_recording`, `list_recordings`, `get_recording`, `delete_recording` — mirror `proxy_storage.rs` pattern. |
| THÊM | `src-tauri/src/recorder/commands.rs` | Tauri commands: `start_recording`, `stop_recording`, `get_recorder_sessions`, `list_recordings`, `get_recording(id)`, `delete_recording(id)`, `export_recording_as_recipe(id)`, `replay_recording(id, profile_id)`. |
| SỬA | `src-tauri/src/lib.rs` | `mod recorder;` ở phần module declarations (~line 44). Thêm commands vào `generate_handler!` ở block sau synchronizer (sau `:~2295`):- `recorder::start_recording, recorder::stop_recording, recorder::get_recorder_sessions, recorder::list_recordings, recorder::get_recording, recorder::delete_recording, recorder::export_recording_as_recipe, recorder::replay_recording`. |
| SỬA | `src-tauri/src/app_dirs.rs` | Thêm `pub fn recordings_dir() -> PathBuf { data_dir().join("recordings") }`. |
| SỬA | `src-tauri/src/mcp_server.rs` | Ở `get_tools()` (~line 4661) thêm tool `start_action_recording`, `stop_action_recording`, `get_recorded_events`, `save_recording`, `list_recordings`, `export_recording_as_recipe`. Ở `handle_tool_call` (~line 6746) thêm match arms gọi qua `recorder::RecorderManager::instance()` (import `crate::recorder`). |
| SỬA (optional) | `src-tauri/src/profile/manager.rs` | Thêm validate khi start recording: profile phải đang chạy (`process_id.is_some()`). |

### Frontend

| Thao tác | File | Mô tả |
|---|---|---|
| THÊM | `src/hooks/use-recorder-session.ts` | Clone `use-sync-session.ts`: `invoke<RecorderSessionInfo[]>("get_recorder_sessions")`, `listen("recorder-session-changed"|"recorder-session-ended", cb)`, helper `getProfileRecorderInfo(profileId)`. |
| THÊM | `src/components/recorder-dialog.tsx` | Dialog list/manage: hiển thị recent recordings, rename/delete/export buttons, `invoke("list_recordings")` để refresh. |
| THÊM | `src/components/recorder-list.tsx` (optional) | Sub-component hoặc inline trong dialog — table recording rows. |
| SỬA | `src/components/profile-data-table.tsx` | Thêm nút **Record** trên mỗi row (icon `LuCircleDot`) kế nút launch; toggle record/stop dựa trên `getProfileRecorderInfo(profile.id)`. Khi đang record, badge đỏ "Rec". Tooltip/badge khi đang record. Button block ~lines 1900-1960 làm template. |
| SỬA | `src/components/profile-data-table.tsx` | Action bar (DataTableActionBarAction) — bulk recipe export/replay icon (optional). |
| SỬA | `src/app/page.tsx` | Thêm state `recorderProfile: { profileId, profileName } | null`, callback `onStartRecording(profile)` để mở recorder dialog hoặc start session. Hook vào profile-data-table. |
| THÊM/SỬA | `src/types.ts` | Types `RecorderSessionInfo { id, profile_id, profile_name, started_at, event_count, status }`, `RecordingSummary` / `Recording` mirror Rust structs (snake_case field names). |

### i18n

| Thao tác | File | Mục |
|---|---|---|
| SỬA | `src/i18n/locales/en.json` | Thêm namespace `recorder` với keys: `start`, `stop`, `record`, `recording`, `stopRecording`, `started`, `stopped`, `startFailed`, `stopFailed`, `list`, `empty`, `export`, `exportAsRecipe`, `replay`, `delete`, `deleteConfirm`, `eventCount`, `duration`, `startedAt`, `targetProfile`, `selectProfile`, `recBadge`, `noProfiles`, `replayStarted`, `replayFailed`, `exported`, `exportFailed`, `dialogTitle`, `dialogDescription`. |
| SỬA | `src/i18n/locales/es.json` | Bản dịch tương ứng. |
| SỬA | `src/i18n/locales/fr.json` | Bản dịch tương ứng. |
| SỬA | `src/i18n/locales/ja.json` | Bản dịch tương ứng. |
| SỬA | `src/i18n/locales/pt.json` | Bản dịch tương ứng. |
| SỬA | `src/i18n/locales/ru.json` | Bản dịch tương ứng. |
| SỬA | `src/i18n/locales/zh.json` | Bản dịch tương ứng. |

Reuse `common.buttons.*` (save, cancel, close, delete) trước khi tạo mới.

### MCP semantic

MCP tool schemas minh bạch, có `input_schema` JSON Schema. Field đặt tên `profile_id`, `recording_id`, `recipe_name`, banner nhận diện `recorder`.

---

## Thứ tự thực hiện (theo task)

1. **Backend module scaffold** — `recorder.rs`, `app_dirs.rs` (`recordings_dir`), `storage.rs`, `types.rs`, register `mod recorder;` trong `lib.rs`.
2. **Inject recorder script** — `inject_script.rs` (JS content recorder). Test bằng cách inject vào 1 running profile qua MCP `evaluate_javascript`, kiểm tra sự kiện click có log `__REC__:` ra.
3. **Capture task (Chromium)** — `capture.rs`, logic loop bắt `Runtime.consoleAPICalled`. Wire `RecorderManager::start_recording`. Thêm Tauri command `start_recording`/`stop_recording`/`get_recorder_sessions`. Kiểm tra list sessions emit sự kiện.
4. **Player (Chromium)** — `player.rs`, replay raw events chuột/bàn phím theo `t_ms`. Kiểm tra trên profile test.
5. **Capture task (Camoufox)** — `camoufox_capture.rs`, dùng Playwright `page.on_event(Event::Console)` + `page.add_init_script`. Kiểm tra trên Camoufox profile.
6. **Player (Camoufox)** — mở rộng `player.rs` cho Camoufox (`page.mouse()`, `page.keyboard()`).
7. **Recipe export** — `recipe_export.rs`, semantic lift. Thêm command `export_recording_as_recipe`. Kiểm tra output JSON chạy được qua `validate_recipe` / `preview_recipe`.
8. **MCP tools** — Sửa `mcp_server.rs:4661` (get_tools) + `:6746` (handle_tool_call). Tools gọi qua `RecorderManager`.
9. **Frontend hook** — `use-recorder-session.ts` clone `use-sync-session.ts`.
10. **Frontend types** — `src/types.ts` thêm `RecorderSessionInfo`, `RecordingSummary`.
11. **Record UI button** — `profile-data-table.tsx` thêm Record toggle button kế nút launch, badge "Rec". Kết hook.
12. **Manage dialog** — `recorder-dialog.tsx` list/delete/export buttons. Wire vào `page.tsx`.
13. **i18n** — 7 locale files.
14. **Verification** — `pnpm format && pnpm lint && pnpm test`. Fix bất kỳ warning/test failure.
15. **Sanity test** — launch profile → record 1 click + 1 input → stop → list → export → replay trên profile khác → confirm recipe chạy được qua MCP run_recipe.

---

## Rủi ro / cần lưu ý

- **Dual kernel**: Camoufox dùng Playwright event API khác CDP → cần test listener console tag chạy được; nếu `Event::Console` không có payload text phải fallback `Runtime.consoleAPICalled` qua CDP port của Camoufox instance (`cdp_port` discovery ở `camoufox_manager.rs:1006+`). Fallback option đã có sẵn.
- **CamoufoxManager singleton state**: `get_automation_state` trả `Arc<AsyncMutex<CamoufoxAutomationState>>`. Listen task phải giữ rx trong task, không giữ lock. Reuse pattern giống MCP `handle_start_console_capture` ở Camoufox branch.
- **Multi-tab**: V1 chỉ capture trên page target đang active. Khi user mở tab mới events ở tab mới không bị catch (giới hạn đã nói với audit). Ghi rõ trong docs. Phase 2 mới mở rộng.
- **run_recipe gate**: tools đang có `require_paid_subscription` — Recorder nên follow same no-op gate như các MCP automation khác (đã relaxed). Không sinh license regression.
- **Human typing vs literal text**: Recorder JS sẽ gửi `InputEvent.inputType` và `data` từ DOM event — đủ để player replay nguyên văn. Recipe export chọn text cumulatively chớ không gõ key-by-key.
- **i18n**: 7 file cùng lúc, không bỏ sót key. AGENTS.md yêu cầu tất cả; theo fixture này mỗi key phải tồn tại đầy đủ trước khi đóng task.
- **Spellcheck (typos linter)**:回避 dùng placeholder `__REC__` không phải từ tiếng Anh, không trigger typos. Locale JSON đã được exclude trong `_typos.toml`.

---

## Done criteria

- [ ] Tất cả file §Files cần thêm đã tạo.
- [ ] `pnpm format && pnpm lint` sạch trên Rust + JS + spellcheck.
- [ ] `pnpm test` (Rust unit + sync e2e) pass.
- [ ] 7 locale files có đủ `recorder.*` keys.
- [ ] UI nút Record hiển thị trên profile row đang chạy, click start được, badge Rec hiện, click stop lưu file.
- [ ] `list_recordings` return recording vừa save.
- [ ] `export_recording_as_recipe` sinh McpRecipe JSON chạy được qua `validate_recipe` (MCP).
- [ ] `replay_recording` chạy được trên profile khác (manual sanity, không nhất thiết integration test).
- [ ] Tauri commands được đăng ký trong `generate_handler!` (có test `test_no_unused_tauri_commands` pass).