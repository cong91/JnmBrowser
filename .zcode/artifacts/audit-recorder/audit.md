# Audit Report: "Launch with Record" / Recorder Feature Visibility

**Pattern searched:** `record | recorder | launch with record | capture | recipe`
**Date:** 2026-07-16
**Auditor:** ZCode (multi-agent audit-pattern workflow)

---

## 1. Pattern

Audit tính năng recorder (action recorder / launch with record) trong JnmBrowser — xác định UI entry point, điều kiện hiển thị, và lý do user không thấy nút "Launch with Record".

## 2. Occurrences found

| Layer | Files | Matches |
|-------|-------|---------|
| Backend Rust (`src-tauri/src/recorder/`) | 7 files + `recorder.rs` | 8 Tauri commands, full subsystem |
| Frontend (`src/`) | 5 files + 7 locale files | ~120+ semantic matches |
| **Total** | **20 files** | **Full subsystem present** |

## 3. Files affected

**Backend:**
- `src-tauri/src/recorder.rs` — RecorderManager singleton
- `src-tauri/src/recorder/{commands,capture,inject_script,player,recipe_export,storage,types}.rs`
- `src-tauri/src/lib.rs:2297-2305` — 8 commands registered ✓
- `src-tauri/src/browser_runner.rs:2527-2538` — auto-flush on kill ✓

**Frontend:**
- `src/app/page.tsx` — `handleLaunchWithRecord` (814-895), `handleToggleRecording` (775+)
- `src/components/profile-data-table.tsx` — nút Record trên row (2017-2041), Launch button (2045-2070)
- `src/components/profile-info-dialog.tsx` — "Launch with Record" action (291-304)
- `src/components/recorder-dialog.tsx` — dialog quản lý recordings
- `src/hooks/use-recorder-session.ts` — session tracking hook
- `src/types.ts:529-609` — 7 TypeScript interfaces
- `src/i18n/locales/{en,es,fr,ja,pt,ru,zh}.json:1612-1651` — namespace `recorder` đầy đủ 7 ngôn ngữ ✓

## 4. Issues by severity

### Critical: 0
Không có lỗi nghiêm trọng. Tính năng recorder **ĐÃ tồn tại và wiring đầy đủ**.

### Important: 2

| # | Issue | File:line | Detail |
|---|-------|-----------|--------|
| I1 | **Nút "Launch with Record" ẩn sau ProfileInfoDialog** | `profile-info-dialog.tsx:291-304` | Nút không nằm trên row table mà ẩn trong dialog info. User phải click icon `LuInfo` (i) trên row → mở dialog → mới thấy action "Launch with Record". Đây là lý do user "không thấy nút". |
| I2 | **Nút Record trên row chỉ hiện khi browser ĐANG CHẠY** | `profile-data-table.tsx:1954-1956` | `canRecord = isRunning && (browser === "chromium" \|\| "camoufox")`. Khi profile chưa launch, nút Record (hình tròn đỏ) KHÔNG hiển thị → user không biết có tính năng record. |

### Minor: 1

| # | Issue | File:line | Detail |
|---|-------|-----------|--------|
| M1 | **`handleLaunchWithRecord` không kiểm tra session đang record** | `page.tsx:814-895` | Nếu profile đã đang record mà user click "Launch with Record" lại, sẽ gọi `start_recording` lần 2. Backend `commands.rs:30-42` có thể reject nhưng UX nên guard ở frontend. |

## 5. Recommended fixes

### Fix I1 (Recommended — UX): Thêm nút "Launch with Record" trực tiếp trên row hoặc dropdown menu
**File:** `src/components/profile-data-table.tsx` (khu vực row actions, ~line 2042-2078)

**Vấn đề:** Hiện tại user phải: row → click icon (i) → mở dialog → scroll tìm "Launch with Record". Quá ẩn.

**Fix options (chọn 1):**
- **Option A (nhanh nhất):** Thêm 1 tooltip-wrapped `RippleButton` bên cạnh nút Launch trên row, icon `LuCircle` đỏ, label `t("recorder.launchWithRecord")`, `onClick={() => meta.onLaunchWithRecord(profile)}`, `disabled={isRunning \|\| isLaunching}`. Giữ điều kiện `hidden` giống `profile-info-dialog.tsx:298-303`.
- **Option B:** Thêm vào row kebab dropdown menu (nếu có) một item "Launch with Record".

### Fix I2 (Recommended — UX): Hiển thị nút Record ngay cả khi chưa running, với disabled state + tooltip giải thích
**File:** `src/components/profile-data-table.tsx:1954-1956, 2017-2041`

Hiện:
```tsx
const canRecord = isRunning && (profile.browser === "chromium" || profile.browser === "camoufox");
// ...
{canRecord && (<RippleButton ...>)}
```

Nên đổi thành hiển thị luôn (với browser hỗ trợ) nhưng disabled + tooltip "Launch profile first to start recording":
```tsx
const supportsRecord = profile.browser === "chromium" || profile.browser === "camoufox";
const canRecord = supportsRecord && isRunning;
// ...
{supportsRecord && (
  <RippleButton disabled={!isRunning} ... />
)}
```

→ User thấy nút Record ngay, hiểu profile có hỗ trợ record, biết cần launch trước.

### Fix M1: Guard frontend — không gọi start_recording nếu session đã active
**File:** `src/app/page.tsx:814-895` (handleLaunchWithRecord)

Thêm guard ở đầu:
```tsx
if (getProfileRecorderInfo(profile.id)) {
  showInfoToast(t("recorder.recordingActive"));
  return;
}
```

## 6. Correct patterns (đã đúng)

| Pattern | File:line | Status |
|---------|-----------|--------|
| 8 Tauri commands register đầy đủ trong `generate_handler!` | `lib.rs:2297-2305` | ✓ Đúng |
| Auto-flush recording khi kill browser (non-UI path) | `browser_runner.rs:2527-2538` | ✓ Đúng |
| `handleLaunchWithRecord` await `profile-running-changed` event trước khi `start_recording` (race-safe) | `page.tsx:843-880` | ✓ Đúng — listener register trước launch, timeout 60s |
| `handleToggleRecording` auto-save khi stop | `page.tsx:581-608` | ✓ Đúng — Promise.race với timeout 5s |
| i18n namespace `recorder` đầy đủ 7 locale, không có fallback 2-arg | `locales/*.json:1612-1651` | ✓ Đúng |
| `ExportedRecipe` type synced với backend | `types.ts:594-609` | ✓ Đúng |
| Browser support guard: chỉ chromium/camoufox | `page.tsx:816-818`, `profile-data-table.tsx:1956` | ✓ Đúng |
| `get_recording` flagged MCP-only trong unused-command test | `lib.rs:2374` | ✓ Đúng |

---

## 7. Kết luận cho user

**Tính năng recorder HOÀN TOÀN TỒN TẠI và đã wiring đầy đủ.** Lý do anh không thấy nút "Launch with Record":

1. **Nút nằm ẩn trong ProfileInfoDialog** — phải click icon (i) trên row profile → mở dialog → mới thấy action "Launch with Record" (dành cho chromium/camoufox profile).

2. **Nút Record (tròn đỏ) trên row chỉ hiện khi browser đang chạy** — profile phải đang running thì nút mới xuất hiện.

**Cách test ngay (không cần fix):**
- Tạo 1 profile Chromium hoặc Camoufox → Launch → nút Record (tròn đỏ) sẽ hiện trên row → click để bắt đầu record.
- HOẶC: click icon (i) trên row profile → dialog mở → chọn "Launch with Record" (khả dụng cho chromium/camoufox).

**Khuyến nghị:** Apply Fix I1 + I2 để UX rõ ràng hơn — hiện nút Record luôn (disabled khi chưa running) và thêm nút "Launch with Record" trực tiếp trên row.
