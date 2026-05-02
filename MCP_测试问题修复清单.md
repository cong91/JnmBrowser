# MCP 测试问题修复清单

> 来源：`/Users/lijinmu/Documents/New project/donutbrowser/MCP_测试报告.md`
>
> 目标：只针对本次实测暴露的问题列修复项，不扩展新功能。
>
> 当前核心结论：MCP 协议层和大部分配置管理工具可用；主要问题集中在 **paid-plan CDP 限制、Console/Network Capture 空数据、Extension MCP 返回格式、文档示例不一致、GeoIP 下载失败、Traffic stats 脏数据**。

---

# 一、优先级总览

## P0 / 阻塞正向自动化验收

- [x] T01：定位 paid-plan CDP 深度自动化限制（真实采集待 MCP 复测）

## P1 / 明确功能缺陷，优先修

- [x] T02：修复 Extension MCP 返回格式异常
- [x] T03：修复 Console Capture 启动成功但日志为空（启动握手/错误冒泡已修，真实采集待 MCP 复测）
- [x] T04：修复 Network Capture 启动成功但请求为空（启动握手/错误冒泡已修，真实采集待 MCP 复测）

## P2 / 文档与边缘能力修正

- [x] T05：修正 `import_cookies_from_file` 测试文档示例
- [x] T06：补充 `import_proxies` JSON schema 要求说明
- [x] T07：排查 `download_geoip_database` GitHub release 下载失败（源有效，已补下载 UA 和错误详情）
- [x] T08：排查 Traffic stats 中大量 `profile_id=null` 项（MCP 返回中过滤 unscoped 项并给出 omitted 计数）

## P3 / 回归与二次验证

- [x] T09：更新 MCP 测试文档
- [~] T10：重新跑 MCP 问题项回归测试（单元/静态已过，真实 MCP 运行态待复测）
- [~] T11：重新跑完整 MCP 冒烟测试（cargo 测试已过，完整运行态冒烟待复测）

---

# 二、详细修复任务

---

## T01：定位并解除 paid-plan CDP 深度自动化限制

**优先级**：P0

**状态**：partial / blocked-by-external-binary

**报告现象**

大量工具返回：

```text
-32000: CDP error: {"code":-32000,"message":"Browser automation requires a paid Donut Browser plan."}
```

受影响工具包括：

- `evaluate_javascript`
- `click_element`
- `type_text`
- `get_page_content`
- `get_page_info`
- `hover_element`
- `focus_element`
- `scroll_to`
- `scroll_element_into_view`
- `select_option`
- `check_checkbox`
- `uncheck_checkbox`
- `element_exists`
- `get_element_text`
- `get_element_html`
- `get_element_attribute`
- `get_element_rect`
- `query_elements`
- `count_elements`
- `set_file_input`
- `upload_file`
- `get_local_storage`
- `set_local_storage`
- `get_session_storage`
- `clear_storage`
- `wait_for_selector`
- `wait_for_text`
- `wait_for_navigation`
- `run_recipe` 中依赖上述工具的步骤
- `run_batch_profile_workflow` 中依赖上述工具的步骤

**已知线索**

**当前排查结果（2026-05-01）**

- 源码内未找到该错误字符串；`mcp_server.rs` 的 `require_paid_subscription()` 已返回 `Ok(())`。
- 字符串实际存在于本机 Wayfern/Chromium 外部二进制：`~/Library/Application Support/JnmBrowser/binaries/wayfern/.../Chromium Framework`。
- 直接启动 Wayfern Chromium 做 CDP 探针确认：`Runtime.evaluate` 返回 paid-plan 错误；`DOM.getDocument`、`Page.captureScreenshot`、`Log.enable`、`Network.enable` 可成功。
- 传入伪 `--wayfern-token=dummy` 仍失败，说明不是 MCP handler 层限制，也不是任意 token 可绕过。
- 下一步应走“CDP DOM/Input fallback”或换可控 runtime；不能只改 MCP paid gate。

`/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/mcp_server.rs` 里：

```rust
async fn require_paid_subscription(_feature: &str) -> Result<(), McpError> {
  Ok(())
}
```

说明 MCP 层自己的 paid gate 已经放开。

因此错误更可能来自：

1. Wayfern 浏览器自身 CDP bridge
2. browser runtime / automation bridge
3. 本地浏览器二进制内置 plan gate
4. CDP 命令转发层
5. 某个 Tauri command / manager 层残留校验

**排查步骤**

- [ ] 全局搜索错误字符串：

```bash
rg -n "Browser automation requires a paid Donut Browser plan|paid Donut Browser plan|requires a paid" \
  '/Users/lijinmu/Documents/New project/donutbrowser'
```

- [ ] 确认错误是否存在于源码、构建产物、外部二进制、Wayfern 资源中。
- [ ] 对一个最小 CDP 命令做直接复现：
  - `Runtime.evaluate`
  - `DOM.getDocument`
  - `Input.dispatchMouseEvent`
- [ ] 对比可用命令：
  - `Page.navigate` 为什么成功
  - `Page.captureScreenshot` 为什么成功
  - `Runtime.evaluate` 为什么失败
- [ ] 确认 MCP 当前调用的 CDP websocket URL 指向哪个 target。
- [ ] 如错误来自 Wayfern 内部 gate，继续定位：
  - gate 配置来源
  - 是否和登录态/订阅缓存有关
  - 是否能通过启动参数、配置、license 状态绕开
  - 是否需要修改 Wayfern 管理层或 browser binary 配套逻辑

**建议修复方向**

- 优先找出错误来源，不要盲目改 MCP handler。
- 如果源码可控，应把本地 MCP 自动化能力从付费 gate 中移除。
- 如果是外部二进制内置，应评估：
  - 是否有本地 dev/test bypass 配置
  - 是否需要换调用路径
  - 是否需要在 Wayfern 启动参数中打开 automation entitlement

**验收标准**

以下工具至少在本地测试页正向通过：

- [ ] `evaluate_javascript` 返回 `document.title`
- [ ] `type_text` 可输入搜索框
- [ ] `click_element` 可点击 Submit
- [ ] `get_element_text` 可读到 `submitted:<keyword>`
- [ ] `wait_for_selector` 可等待 `#delayed`
- [ ] `wait_for_text` 可等待 `Delayed Text Ready`
- [ ] `get_local_storage` 可读到 `mcp_local_key`
- [ ] `set_local_storage` 可写入 key
- [ ] `run_recipe` 冒烟流程 `status=success`
- [ ] `run_batch_profile_workflow` 至少两个 profile 的简单流程可成功

**回归命令**

```bash
cd '/Users/lijinmu/Documents/New project/donutbrowser/src-tauri'
cargo fmt --all
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings -D clippy::all
cargo test mcp_server::tests --lib
```

---

## T02：修复 Extension MCP 返回格式异常

**优先级**：P1

**状态**：done

**报告现象**

以下工具全部返回：

```text
Unexpected response type
```

受影响工具：

- `list_extensions`
- `list_extension_groups`
- `create_extension_group`
- `delete_extension`
- `delete_extension_group`
- `assign_extension_group_to_profile`

**已知线索**

当前 handler 返回裸 JSON：

```rust
Ok(serde_json::to_value(extensions).unwrap())
Ok(serde_json::to_value(groups).unwrap())
Ok(serde_json::to_value(group).unwrap())
Ok(serde_json::json!({"success": true}))
Ok(serde_json::to_value(profile).unwrap())
```

但 MCP 工具统一期望：

```json
{
  "content": [
    {
      "type": "text",
      "text": "...pretty json..."
    }
  ]
}
```

**涉及文件**

- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/mcp_server.rs`

**涉及函数**

- `handle_list_extensions`
- `handle_list_extension_groups`
- `handle_create_extension_group`
- `handle_delete_extension_mcp`
- `handle_delete_extension_group_mcp`
- `handle_assign_extension_group_to_profile`

**修复动作**

- [ ] 将所有 Extension MCP handler 返回值统一改为 `Self::json_tool_result(...)`
- [ ] 删除 `unwrap()`，改成错误可控的序列化路径
- [ ] 补单测：验证返回中存在 `content[0].type=text`
- [ ] 补单测：验证 `content[0].text` 可解析为 JSON

**建议实现形态**

```rust
Self::json_tool_result(&extensions)
Self::json_tool_result(&groups)
Self::json_tool_result(&group)
Self::json_tool_result(&serde_json::json!({ "success": true }))
Self::json_tool_result(&profile)
```

**验收标准**

- [ ] `list_extensions` 不再返回 `Unexpected response type`
- [ ] `list_extension_groups` 不再返回 `Unexpected response type`
- [ ] `create_extension_group` 返回标准 MCP text content
- [ ] `delete_extension_group` 返回标准 MCP text content
- [ ] `assign_extension_group_to_profile` 返回标准 MCP text content
- [x] `cargo test mcp_server::tests --lib` 通过

---

## T03：修复 Console Capture 启动成功但日志为空

**优先级**：P1

**状态**：done / pending MCP runtime retest

**报告现象**

复现流程：

1. `start_console_capture`
2. 打开会自动产生 console 日志的页面
3. `get_console_logs`
4. 返回 `total=0 / logs=[]`

`start_console_capture` 返回成功，但实际没有日志。

**涉及工具**

- `start_console_capture`
- `get_console_logs`
- `clear_console_logs`

**涉及文件**

- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/mcp_server.rs`

**重点函数**

- `handle_start_console_capture`
- `run_console_capture`
- `get_cdp_ws_url`
- `resolve_active_tab_id`
- `push_console_log`

**可能原因**

1. WebSocket 连接到了错误 tab target。
2. Capture 启动时连接的是旧 tab，随后导航/新页面不在同一 target。
3. `Runtime.enable` / `Log.enable` 实际失败，但 `start_console_capture` 没暴露出来。
4. 后台任务启动后立刻异常退出，但调用方只看到 `started`。
5. console 事件来自 iframe/worker，当前只监听了 page target。
6. paid-plan gate 也影响了 `Runtime.enable` 或 console 事件，但未正确记录 lastError。

**排查步骤**

- [ ] `start_console_capture` 后立刻调用 `get_console_logs`，检查：
  - `running`
  - `lastError`
- [ ] 增加临时日志或测试断言，确认：
  - ws_url 是当前 active tab 的 `webSocketDebuggerUrl`
  - `Runtime.enable` 返回成功
  - `Log.enable` 返回成功
  - 后台任务未退出
- [ ] 在 capture 启动后执行：

```json
{
  "name": "evaluate_javascript",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "expression": "console.log('capture-probe')"
  }
}
```

如果 T01 未修复，此步骤可能仍受 paid-plan 限制。

- [ ] 用直接 CDP websocket 测试 `Runtime.consoleAPICalled` 是否产生事件。
- [ ] 测试顺序调整：
  - 先 navigate 到测试页
  - 再 start capture
  - 再点击产生日志
  - 再 get logs
- [ ] 测试反向顺序：
  - 先 start capture
  - 再 navigate
  - 再 get logs

**建议修复方向**

- [ ] `start_console_capture` 不应只表示“后台任务已 spawn”，应尽量确认订阅已成功。
- [ ] 增加 capture 状态：
  - `starting`
  - `running`
  - `failed`
- [ ] `start_console_capture` 可等待 `Runtime.enable` / `Log.enable` 成功后再返回。
- [ ] 如果 active tab 切换，应支持重新绑定或提示 capture 仍绑定旧 target。
- [ ] `get_console_logs` 返回中保留 `lastError`，测试侧必须检查。

**验收标准**

- [ ] 自动日志页加载后能捕获 `MCP test page loaded`
- [ ] 点击 `#log-button` 后能捕获：
  - `manual warning log`
  - `manual error log`
- [ ] `get_console_logs` 返回 `total > 0`
- [ ] `clear_console_logs` 后 `total=0`
- [ ] 如果订阅失败，`lastError` 必须非空，不能假成功

---

## T04：修复 Network Capture 启动成功但请求为空

**优先级**：P1

**状态**：done / pending MCP runtime retest

**报告现象**

复现流程：

1. `start_network_capture`
2. 打开自动 fetch 页面或点击按钮触发 `/api/echo`
3. `get_network_requests`
4. 返回空列表

`start_network_capture` 返回成功，但没有请求记录。

**涉及工具**

- `start_network_capture`
- `get_network_requests`
- `get_response_body`
- `clear_network_requests`

**涉及文件**

- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/mcp_server.rs`

**重点函数**

- `handle_start_network_capture`
- `run_network_capture`
- `get_cdp_ws_url`
- `push_network_request`
- request/response 聚合逻辑

**可能原因**

1. WebSocket 连接到了错误 tab target。
2. Capture 启动后页面导航导致 target 变化。
3. `Network.enable` 实际失败，但未冒泡。
4. 事件来了但 requestId 聚合逻辑未写入 buffer。
5. fetch 请求发生在 capture 订阅完成前。
6. 测试页请求被缓存/未真正发起。
7. paid-plan gate 影响 Network domain。

**排查步骤**

- [ ] `start_network_capture` 后立刻调用 `get_network_requests`，检查：
  - `running`
  - `lastError`
- [ ] 增加临时日志确认：
  - `Network.enable` 返回成功
  - 收到 `Network.requestWillBeSent`
  - 收到 `Network.responseReceived`
  - 收到 `Network.loadingFinished`
- [ ] 测试时避免请求缓存：

```text
/api/echo?value=network-test&t=<timestamp>
```

- [ ] 测试两个顺序：
  - 先 start capture，再点击 fetch
  - 先 navigate，再 start capture，再点击 fetch
- [ ] 用直接 CDP websocket 独立验证 Network 事件是否能收到。

**建议修复方向**

- [ ] `start_network_capture` 等待 `Network.enable` 成功后再返回。
- [ ] 如果订阅失败，返回错误或至少写入 `lastError`。
- [ ] 增加 capture target 信息到返回：
  - `targetId`
  - `targetUrl`
  - `wsUrl` 可选脱敏
- [ ] 如果 active tab 改变，提示 capture 绑定的 target 和当前 active tab 不一致。
- [ ] 确保 request buffer 写入路径覆盖：
  - request only
  - response only
  - loading failed
  - loading finished

**验收标准**

- [ ] 点击 `#fetch-button` 后，`get_network_requests` 能看到 `/api/echo`
- [ ] 请求状态码为 `200`
- [ ] `hasResponseBody=true`
- [ ] `get_response_body` 可返回 `{"ok":true,"value":"network-test"}`
- [ ] `clear_network_requests` 后列表清空
- [ ] 如果订阅失败，`lastError` 必须非空

---

## T05：修正 `import_cookies_from_file` 测试文档示例

**优先级**：P2

**状态**：done

**报告现象**

文档示例：

```json
{
  "content": "[]"
}
```

实测返回：

```text
No valid cookies found in the file
```

**判断**

这是文档问题，不是实现问题。空数组确实不是有效 cookie 导入内容。

**涉及文件**

- `/Users/lijinmu/Documents/New project/donutbrowser/MCP_当前功能测试文档.md`

**修复动作**

- [ ] 删除“`[]` 可成功导入”的暗示。
- [ ] 改成“空数组应返回无有效 cookie”。
- [ ] 提供有效 JSON cookie 示例。

**建议示例**

```json
[
  {
    "name": "mcp_test_cookie",
    "value": "hello",
    "domain": "127.0.0.1",
    "path": "/",
    "expires": -1,
    "httpOnly": false,
    "secure": false,
    "sameSite": "Lax"
  }
]
```

如实现实际要求字段不同，应以 `cookie_manager` 解析逻辑为准。

**验收标准**

- [ ] 文档中不再把 `[]` 作为成功导入示例
- [ ] 文档包含有效 cookie 示例
- [ ] 测试报告中该项不再标为实现失败

---

## T06：补充 `import_proxies` JSON schema 要求说明

**优先级**：P2

**状态**：done

**报告现象**

`import_proxies`：

- `txt` 格式导入成功
- 最小 JSON 因缺 `exported_at` 失败

说明 JSON schema 比测试文档描述更严格。

**涉及文件**

- `/Users/lijinmu/Documents/New project/donutbrowser/MCP_当前功能测试文档.md`
- 如需放宽实现，则涉及 proxy import 逻辑

**修复选项**

### 选项 A：只修文档

- [ ] 明确 JSON 格式应使用 `export_proxies` 导出的完整结构
- [ ] 推荐测试时优先用：
  - 先 `export_proxies(format=json)`
  - 再把导出结构改一项后 import

### 选项 B：放宽实现

- [ ] 允许最小 JSON proxy array 导入
- [ ] 自动补齐 `exported_at` 等 metadata

**建议**

优先选 A，减少导入 schema 兼容风险。

**验收标准**

- [ ] 文档明确 JSON import 要求
- [ ] 不再用最小 JSON 作为成功示例

---

## T07：排查 `download_geoip_database` GitHub release 下载失败

**优先级**：P2

**状态**：done

**报告现象**

`download_geoip_database` 下载失败：GitHub release 请求错误。

**涉及工具**

- `is_geoip_database_available`
- `download_geoip_database`

**涉及文件/模块**

需搜索确认，可能涉及：

- `camoufox`
- `geoip`
- `downloader`
- `settings_manager`

**排查步骤**

- [ ] 搜索 GeoIP 下载 URL：

```bash
rg -n "geoip|GeoIP|maxmind|download_geoip|release" \
  '/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src'
```

- [ ] 确认当前 URL 是否仍有效。
- [ ] 用 curl 直接请求目标 URL。
- [ ] 检查 GitHub release asset 是否改名/迁移。
- [ ] 检查是否需要 User-Agent。
- [ ] 检查是否网络环境问题。

**建议修复方向**

- [ ] 若 URL 失效，更新下载源。
- [ ] 增加备用下载 URL。
- [ ] 返回更明确错误：
  - HTTP status
  - URL
  - response snippet
- [ ] 如果本地已经 available，则下载失败不应阻塞其他测试。

**验收标准**

- [ ] `download_geoip_database` 可成功下载或明确返回可解释错误
- [ ] `is_geoip_database_available` 下载后为 `true`

---

## T08：排查 Traffic stats 中大量 `profile_id=null` 项

**优先级**：P2

**状态**：done

**报告现象**

`get_all_traffic_snapshots` 有返回，但包含大量：

```json
"profile_id": null
```

**可能原因**

1. 历史统计数据没有绑定 profile_id。
2. 代理层记录连接时缺少 profile 上下文。
3. 清理旧数据后仍有 null 项生成。
4. API 返回结构中某些 global stats 被误当 profile stats。

**涉及工具**

- `get_all_traffic_snapshots`
- `get_traffic_stats_for_period`
- `clear_all_traffic_stats`

**排查步骤**

- [ ] 调用 `clear_all_traffic_stats`
- [ ] 只启动一个临时 profile 访问测试页
- [ ] 再调用 `get_all_traffic_snapshots`
- [ ] 判断 `profile_id=null` 是否仍生成
- [ ] 搜索 traffic stats 写入点：

```bash
rg -n "traffic|profile_id|profileId|snapshot" \
  '/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src'
```

**建议修复方向**

- [ ] 如果是历史数据，文档说明可清理。
- [ ] 如果是新数据，修写入路径，确保 profile_id 绑定。
- [ ] 如果确实是 global stats，返回字段应区分 `scope=global/profile`。

**验收标准**

- [ ] 清理后新生成数据不再出现无意义 `profile_id=null`
- [ ] 如保留 null，必须能解释其语义

---

# 三、回归测试计划

## T09：更新 MCP 测试文档

**优先级**：P3

**状态**：done

**动作**

- [x] 更新 `/Users/lijinmu/Documents/New project/donutbrowser/MCP_当前功能测试文档.md`
- [x] 修正 cookie import 示例
- [x] 修正 proxy import JSON 说明
- [x] 增加 paid-plan/CDP gate 的排查说明
- [x] 增加 console/network capture 的 lastError 检查要求

**验收标准**

- [ ] 文档示例和实现一致
- [ ] AI 按文档测试不会把预期错误误判为实现失败

---

## T10：重新跑 MCP 问题项回归测试

**优先级**：P3

**状态**：partial / pending MCP runtime retest

**需重点回归**

- [ ] Extension 工具 6 项
- [ ] Console Capture 3 项
- [ ] Network Capture 4 项
- [ ] Cookie import 示例
- [ ] Proxy import JSON 示例
- [ ] GeoIP 下载
- [ ] Traffic stats null profile_id

**验收标准**

- [ ] 修复项均有明确通过/跳过理由
- [ ] 新测试报告不再出现同类误判

---

## T11：重新跑完整 MCP 冒烟测试

**优先级**：P3

**状态**：partial / pending MCP runtime retest

**动作**

- [ ] 使用本地测试页重新跑完整流程
- [ ] 重点确认：
  - profile 启停
  - navigate/screenshot
  - evaluate/type/click/wait/storage
  - console/network
  - frame/tab/download
  - run_recipe
  - run_batch_profile_workflow

**验收标准**

- [x] `cargo test mcp_server::tests --lib` 通过
- [ ] MCP 实测报告关键路径通过
- [ ] 若仍有受限项，必须明确是环境/计划限制还是实现问题

---

# 四、建议执行顺序

建议按下面顺序执行：

1. [x] T02：Extension MCP 返回格式
2. [x] T03：Console Capture
3. [x] T04：Network Capture
4. [~] T01：paid-plan CDP 限制定位与解除
5. [x] T05：Cookie 文档示例
6. [x] T06：Proxy import 文档说明
7. [x] T08：Traffic stats null profile_id
8. [x] T07：GeoIP 下载失败
9. [x] T09：更新测试文档
10. [~] T10：问题项回归
11. [~] T11：完整冒烟测试

> 说明：T01 是最大阻塞，但定位可能较深。为了尽快消除确定性 bug，建议先修 T02/T03/T04；同时记录 T01 的错误来源。

---

# 五、当前不纳入本修复清单的内容

以下不是本次测试问题修复范围：

- DevTools UI
- Console UI
- Network UI
- Waterfall UI
- recipe 变量增强
- 更强条件分支
- batch 失败重跑策略
- recipe 模板库
- network/console 导出能力

这些属于后续增强，不是本轮测试报告暴露的缺陷修复。
