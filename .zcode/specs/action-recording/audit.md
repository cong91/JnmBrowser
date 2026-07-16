# Audit: Action Recording (record → save → replay)

**Date:** 2026-07-14  
**Pattern:** Action recording / web automation (record user ops → durable script → code replay)  
**Repo:** JnmBrowser (DonutBrowser) — Tauri v2 + Next.js anti-detect browser  

---

## 1. Pattern

User intent (VN): *automation một trang web = record thao tác → triển khai lại bằng logic code.*  
Product gap: **no durable “record actions” feature**. Closest systems today are:

1. **MCP page automation + recipes** — scripted control (hand-authored or AI-driven tools)
2. **Multi-profile synchronizer** — live leader→follower input fan-out (ephemeral)

---

## 2. Occurrences found

| Category | Count (approx) | Files |
|---|---|---|
| MCP automation tools / recipe workflow | large surface (~tool catalog + handlers) | `src-tauri/src/mcp_server.rs` |
| Live capture/replay (sync) | 1 core module + UI | `synchronizer.rs`, `use-sync-session.ts`, `sync-follower-dialog.tsx` |
| Human typing helper | 1 module | `human_typing.rs` |
| CDP launch/plumbing | managers | `chromium_manager.rs`, `camoufox_manager.rs`, `browser.rs`, `browser_runner.rs` |
| REST API (management only) | 1 server | `api_server.rs` |
| Docs / plans (MCP roadmap) | 6+ markdown plans | `MCP_*.md`, Wayfern cleanup plans |
| **Dedicated action recorder** | **0** | — |

**Files affected (core):** ~15 primary, plus MCP plan docs  
**Occurrences of true “record→save macro” product code:** **0**

---

## 3. Issues by severity

| Severity | Count |
|---|---|
| Critical | 6 |
| Important | 10 |
| Minor | 5 |

### Critical

1. **No persistent action recorder module** — no start/stop recording, save, list, export, load.  
2. **Capture depends on proprietary `Wayfern.enableInputCapture` / `Wayfern.inputCaptured`** (`synchronizer.rs:419-423`, `655-656`) while project is migrating to fingerprint-chromium (standard CDP). Capture likely broken/fragile on current kernel.  
3. **`CapturedEvent` is live-only** — Serialize exists but no timeline buffer, disk format, session header, tab id, or player (`synchronizer.rs:17-46`).  
4. **Keystrokes can capture credentials/PII with no redaction** (`text`/`key` fields; fan-out to followers).  
5. **MCP click path is synthetic DOM `.click()`**, not trusted `Input.dispatchMouseEvent` (`mcp_server.rs` click handlers) — replay fidelity ≠ real user input.  
6. **Dual-kernel asymmetry** — Chromium CDP vs Camoufox Playwright; no Camoufox capture; frame tools unsupported on Camoufox; typing fidelity differs.

### Important

1. Coordinate-only sync replay → fragile across viewports (`synchronizer.rs:734-753`).  
2. Timing ignored in sync replay (timestamp field unused; no sleep between events).  
3. Single page-target attachment; multi-tab unsupported.  
4. Incomplete mouse/key replay (no move/drag; wheel via `scrollBy`; incomplete clickCount/text).  
5. Navigation capture is 2s-heuristic, lossy for SPA/history.  
6. Recipe format has no `sleep`/`wait_ms` step for inter-action human delays.  
7. No raw mouse MCP tools (`mouse_move/down/up/drag/click_at`).  
8. No locator-ranking export API (testid > role > label > text > css).  
9. REST API cannot drive page actions (management only).  
10. Fire-and-forget CDP fan-out + no resync → desync without recovery.

### Minor

1. Leader focus no-op on Windows (`xdotool` Linux-only).  
2. Sync start UX is fire-and-forget (dialog closes before armed).  
3. Recipe `inputs` schema loosely typed.  
4. Shallow default merge for timeout/auto_wait.  
5. Empty keyboard handler in follower dialog rows.

---

## 4. What already exists (correct patterns to reuse)

### A. MCP automation (primary control plane)

**File:** `src-tauri/src/mcp_server.rs`  
**Lifecycle:** `start_mcp_server` / `stop_mcp_server` / `get_mcp_config` (`lib.rs`)  
**Backends:** Chromium → CDP WS; Camoufox → Playwright `Page`

**Page tools (building blocks):**
- Nav/content: `navigate`, `screenshot`, `evaluate_javascript`, `get_page_content`, `get_page_info`
- Interaction: `click_element`, `type_text`, `hover_element`, `focus_element`, `scroll_to`, `press_key`, `press_hotkey`, `select_option`, checkbox tools
- Query/waits: `element_exists`, `query_elements`, `wait_for_selector|text|navigation`, locator strategies (css/text/testid/xpath/role/label/placeholder)
- Tabs/frames/files/cookies/storage/network/console

**Workflow scripting:**
- Models: `McpRecipe` / `McpRecipeStep` (~`mcp_server.rs:598-646`)
- Tools: `validate_recipe`, `preview_recipe`, `run_recipe`, `run_batch_profile_workflow`
- Per-step `replay` metadata (tool + resolved args)

### B. Live multi-profile capture/replay

**File:** `src-tauri/src/synchronizer.rs`  
**UI:** `src/components/sync-follower-dialog.tsx`, `src/hooks/use-sync-session.ts`

- `CapturedEvent` DTO (mouse/key/scroll/nav fields)
- Persistent per-follower WS + channel player loop
- Session APIs: `start_sync_session`, `stop_sync_session`, `get_sync_sessions`
- Chromium-only validation; concurrent launch batching

### C. Human typing

**File:** `src-tauri/src/human_typing.rs` — `MarkovTyper` + timed `Input.dispatchKeyEvent` on Chromium MCP path.

### D. Launch / CDP plumbing

- Chromium always launches with `--remote-debugging-port` (`browser.rs`, `chromium_manager.rs`)
- Camoufox via `playwright-rust` (`camoufox/launcher.rs`)
- Profile model + proxy/VPN isolation

---

## 5. Recommended fixes / implementation plan

### Goal

**Record user actions once → save durable artifact → replay via code (MCP recipe / player).**

### Architecture (reuse-first)

```
[Capture layer]  inject page script OR standard CDP observation
        │
        ▼
[Event buffer]   Recording { version, start_url, viewport, events[] }
        │
        ├─► Mode A: RAW player  (Input.dispatch* + delays) — high fidelity, brittle
        │
        └─► Mode B: SEMANTIC lift → McpRecipe steps
                    (navigate / click_element+locator / type_text / wait_*)
                    → validate_recipe → run_recipe / run_batch_profile_workflow
```

**Recommended default product path: Mode B (semantic recipes).**  
Optional advanced: Mode A for anti-bot fidelity / multi-profile clone.

### Phase 0 — Unblock capture (must)

| Task | Detail |
|---|---|
| Replace `Wayfern.*` capture | Capability-detect; fallback to injected content-script recorder or fingerprint-chromium capture domain |
| Prove CDP Input works on fingerprint-chromium | `Input.dispatchMouseEvent` / `KeyEvent` already used in sync + MCP typing |

### Phase 1 — MVP semantic recorder (shippable)

1. **New module** `src-tauri/src/action_recorder.rs` (or under `automation/`)  
   - `start_recording(profile_id)` / `stop_recording` / `pause` / `list` / `delete`  
   - In-memory buffer → flush JSON to app data dir  
2. **Capture via injected JS** (portable, not Wayfern-dependent):  
   - click/input/change/keydown/submit/scroll/navigation  
   - For each target: collect locator candidates (testid, role+name, label, text, css)  
3. **Semantic lift** on stop:  
   - merge keydowns into `type_text` spans  
   - clicks → `click_element` with best locator  
   - navigations → `navigate` + `wait_for_navigation`  
4. **Export** to `McpRecipe` JSON (parameterize `profile_id: "{{inputs.profile_id}}"`)  
5. **MCP tools:** `start_action_recording`, `stop_action_recording`, `export_recording_as_recipe`, `list_recordings`  
6. **UI:** record button on running profile row + simple list/export dialog  
7. **Privacy:** redact `input[type=password]`, option to mask all text; warn on save  

### Phase 2 — Replay quality

| Task | Why |
|---|---|
| Add `wait_ms` / `sleep` tool or step `delay_ms` | Preserve human pacing |
| Locator ranking helper API | Robust export |
| Trusted click option (`Input.dispatchMouseEvent` at element center) | Fidelity vs bot detection |
| Multi-tab target map | Real sessions |
| Camoufox capture strategy (Playwright listeners or shared inject script) | Dual-kernel |

### Phase 3 — Raw player (optional)

- Persist extended `CapturedEvent` with `t_ms`, `target_id`, viewport  
- Reuse `follower_replay_loop` as single-profile player with delays  
- Add mouse move/drag/wheel via CDP  
- Keep separate from recipe format

### Non-goals (v1)

- Full Playwright codegen parity  
- Cloud sync of recordings (donut-sync is entity sync only)  
- REST page-action surface (use MCP unless product requires HTTP)

---

## 6. Correct patterns (already good)

| Pattern | Location | Reuse for recording |
|---|---|---|
| `McpRecipe` orchestration + validate/preview/run | `mcp_server.rs` | Export target |
| Locator strategies + auto-wait | `mcp_server.rs` locator helpers | Semantic lift |
| `MarkovTyper` human typing | `human_typing.rs` | Replay typing |
| Persistent WS player loop | `synchronizer.rs` `follower_replay_loop` | Raw player |
| Session lifecycle + Tauri events | `use-sync-session.ts` | Recording session UI |
| Profile/CDP launch isolation | `chromium_manager` / `browser_runner` | Record only on running profile with CDP |

---

## 7. Gap matrix (record → code replay)

| Capability | Status | Notes |
|---|---|---|
| Launch/control profile | ✅ | Tauri + MCP |
| Scripted click/type/nav | ✅ | MCP tools |
| Hand-authored multi-step workflow | ✅ | Recipes |
| Live multi-profile mirror | ⚠️ | Sync; Wayfern-fragile |
| Start/stop user action recording | ❌ | Missing |
| Persist recording file | ❌ | Missing |
| Export to recipe/code | ❌ | Missing |
| Replay saved recording | ❌ | Missing (only live fan-out) |
| Privacy redaction | ❌ | Missing |
| UI for record/export | ❌ | Missing |

---

## 8. Key file references

| File | Role |
|---|---|
| `src-tauri/src/mcp_server.rs` | MCP tools, recipes, CDP/Playwright actions |
| `src-tauri/src/synchronizer.rs` | Live capture/replay (seed for raw player) |
| `src-tauri/src/human_typing.rs` | Human keystroke generation |
| `src-tauri/src/chromium_manager.rs` | CDP port, fingerprint inject |
| `src-tauri/src/camoufox_manager.rs` | Playwright automation state |
| `src-tauri/src/api_server.rs` | REST management only |
| `src/components/sync-follower-dialog.tsx` | Sync UI patterns |
| `src/hooks/use-sync-session.ts` | Session event map |
| `MCP_全阶段开发计划书.md` | MCP roadmap context |
| `MCP_未完成能力统计.md` | Capability inventory |

---

## 9. Bottom line

JnmBrowser is **already strong at scripted automation (MCP)** and has a **live input mirror prototype (synchronizer)**.  
It does **not** have productized **action recording**.

To deliver “record thao tác → code replay”:

1. **Do not** build on `Wayfern.enableInputCapture` as permanent capture.  
2. **Do** export to **`McpRecipe`** as the durable/code-friendly format.  
3. **Do** add a new recorder service + privacy + locator lift.  
4. **Optionally** later add raw CDP event player for high-fidelity clone.

**Suggested next step after this audit:** write a short design/spec (`spec.md`) for Phase 1 MVP (inject-script semantic recorder → recipe export → UI), then implement.

---

## 10. Severity summary (for /audit output)

1. **Pattern:** action recording / record→replay automation  
2. **Occurrences found:** large automation surface; **0** dedicated recorder modules  
3. **Files affected:** ~15 core + MCP docs  
4. **Issues:** Critical **6** · Important **10** · Minor **5**  
5. **Recommended fixes:** see §5 Phase 0–2  
6. **Correct patterns:** MCP recipes, locators, human typing, sync player loop (§6)  
