# donut-browser MCP 测试报告

- 测试时间：2026-05-01
- 测试目标：`donut-browser` MCP（文档：`donutbrowser/MCP_当前功能测试文档.md`）
- 服务健康检查：`http://127.0.0.1:51080/health` 返回 `status=ok / server=JnmBrowser / version=0.22.7 / protocolVersion=2025-11-25`
- 测试页：`http://127.0.0.1:18080/index.html`
- 本地测试目录：`/tmp/jnm-mcp-test`
- 说明：本次使用 donut-browser MCP 实测；第二轮问题项复测改为直接调用 MCP JSON-RPC，避免本地工具包装器超时干扰。测试过程中创建的临时 profile / group / proxy / vpn 均已清理，`sync_settings` 已恢复为 `null`


## 复测更新（第二轮）

- 复测时间：2026-05-01
- 复测范围：上一版报告中全部 `❌ / ⚠️` 项
- 已确认修复：
  - `download_geoip_database`
  - `list_extensions`
  - `list_extension_groups`
  - `create_extension_group`
  - `delete_extension`
  - `delete_extension_group`
  - `assign_extension_group_to_profile`
- 仍未解决：
  - 深度浏览器自动化工具仍统一受 `paid plan` 限制
  - `get_console_logs` / `get_network_requests` 仍抓不到自动日志和自动请求
  - `import_cookies_from_file` 在 profile 停止后传入 `[]` 仍报 `No valid cookies found in the file`

## 一、结论摘要

### 总体结论

1. **MCP 协议层可正常工作**：`initialize`、`notifications/initialized`、`tools/list` 均成功，`tools/list` 实测返回 **111** 个工具。
2. **配置管理类能力大多可用**：profile/group/proxy/fingerprint/sync-session/tab/frame/download/traffic 等多数工具可正常工作。
3. **大量深度自动化 / DOM / Storage / Wait 工具被当前运行环境限制**：实测统一报错：
   - `-32000: CDP error: {"code":-32000,"message":"Browser automation requires a paid Donut Browser plan."}`
4. **Console / Network Capture 存在异常**：`start_*_capture` 可成功启动，但后续 `get_console_logs` / `get_network_requests` 持续返回空数据，即使已打开会自动产生日志/请求的测试页。
5. **Extension 模块的响应类型异常已在第二轮复测中修复**：相关工具现已能正常返回结构化结果。
6. **第二轮复测确认部分问题已修复**：GeoIP 下载与 Extension 相关响应类型异常已恢复正常；但 cookies 文档示例仍与实现不一致。

### 重点阻塞项

- 当前环境未开通/未识别到可用付费计划，导致大量依赖 CDP 深度能力的工具无法完成正向验证。
- Console / Network Capture 虽能启动，但采集结果仍为空，疑似功能缺陷。
- `import_cookies_from_file` 的文档示例与实现仍不一致。

---

## 二、协议层测试

| 项目 | 结果 | 备注 |
|---|---|---|
| `GET /health` | ✅ | 返回 `ok / JnmBrowser / 0.22.7 / 2025-11-25` |
| `initialize` | ✅ | 成功拿到 `mcp-session-id` |
| `notifications/initialized` | ✅ | HTTP `202 Accepted` |
| `tools/list` | ✅ | 返回 **111** 个工具；`navigate`、`click_element`、`start_console_capture`、`start_network_capture`、`run_recipe`、`run_batch_profile_workflow` 均存在 |

---

## 三、测试用临时资源

- 临时 profile：`MCP Test Profile A`、`MCP Test Profile B`
- 临时 group：`MCP Test Group`
- 临时 proxy：`mcp-test-proxy`
- 临时 vpn：`MCP Test VPN`
- 下载验证文件：`mcp-download.txt`
- 自动日志页：`/tmp/jnm-mcp-test/auto-log.html`
- 自动请求页：`/tmp/jnm-mcp-test/auto-fetch.html`

以上资源测试结束后已删除/恢复。

---

## 四、逐项工具测试结果

状态说明：
- ✅ 通过
- ⚠️ 受限 / 仅验证到错误路径 / 有前置条件
- ❌ 失败 / 明显异常

### 4.1 Profile 管理

| 工具 | 结果 | 备注 |
|---|---|---|
| `list_profiles` | ✅ | 可正常列出 profile |
| `get_profile` | ✅ | 返回 profile 详情 |
| `run_profile` | ✅ | 临时 profile 可成功启动 |
| `kill_profile` | ✅ | 可正常停止 |
| `create_profile` | ✅ | 成功创建 2 个临时 profile |
| `update_profile` | ✅ | 停止状态下可更新；运行中改 group 会报前置条件错误 |
| `delete_profile` | ✅ | 临时 profile 已成功删除 |
| `list_tags` | ✅ | 返回标签列表 |
| `get_profile_status` | ✅ | 可正确返回 running / stopped |

### 4.2 Group 管理

| 工具 | 结果 | 备注 |
|---|---|---|
| `list_groups` | ✅ | 可列出分组 |
| `get_group` | ✅ | 可读取分组详情 |
| `create_group` | ✅ | 成功创建临时 group |
| `update_group` | ✅ | 可成功重命名 |
| `delete_group` | ✅ | 临时 group 已成功删除 |
| `assign_profiles_to_group` | ✅ | profile 停止后可成功分配；运行中会被拒绝 |

### 4.3 Proxy 管理

| 工具 | 结果 | 备注 |
|---|---|---|
| `list_proxies` | ✅ | 可列出代理 |
| `get_proxy` | ✅ | 可读取代理详情 |
| `create_proxy` | ✅ | 成功创建临时 proxy |
| `update_proxy` | ✅ | 可修改 host/port/name/鉴权 |
| `delete_proxy` | ✅ | 临时 proxy 已成功删除 |
| `export_proxies` | ✅ | 可导出 JSON |
| `import_proxies` | ✅ | `txt` 格式导入成功；最小 JSON 会因缺 `exported_at` 失败，说明 schema 比文档更严格 |
| `update_profile_proxy_bypass_rules` | ✅ | 可成功写入 bypass rules |
| `update_profile_dns_blocklist` | ✅ | 可成功设置为 `light` |
| `get_dns_blocklist_status` | ✅ | 可返回各级 blocklist 状态 |

### 4.4 VPN 管理

| 工具 | 结果 | 备注 |
|---|---|---|
| `import_vpn` | ✅ | 有效 WireGuard 格式可导入；无效 base64 会被正确拒绝 |
| `list_vpn_configs` | ✅ | 可列出临时 VPN |
| `delete_vpn` | ✅ | 临时 VPN 已成功删除 |
| `connect_vpn` | ⚠️ | 工具可执行，但因测试用假 endpoint 握手超时失败 |
| `disconnect_vpn` | ✅ | 即使前序连接失败也可执行断开 |
| `get_vpn_status` | ✅ | 可返回 `connected=false` 等状态 |

### 4.5 Fingerprint / GeoIP

| 工具 | 结果 | 备注 |
|---|---|---|
| `get_profile_fingerprint` | ✅ | 可读取 fingerprint 配置 |
| `update_profile_fingerprint` | ✅ | 可更新 `os/randomize_fingerprint_on_launch` |
| `generate_sample_fingerprint` | ✅ | 可生成样例 fingerprint |
| `is_geoip_database_available` | ✅ | 返回 `available=true` |
| `download_geoip_database` | ✅ | 第二轮复测成功，返回 `GeoIP database download completed or is already in progress` |

### 4.6 Extension 管理

| 工具 | 结果 | 备注 |
|---|---|---|
| `list_extensions` | ✅ | 第二轮复测成功，正常返回 `[]` |
| `list_extension_groups` | ✅ | 第二轮复测成功，正常返回扩展组列表 |
| `create_extension_group` | ✅ | 第二轮复测成功，可创建扩展组并拿到结构化 JSON |
| `delete_extension` | ✅ | 第二轮复测对不存在 `extension_id` 返回 `{ "success": true }`；响应类型异常已修复 |
| `delete_extension_group` | ✅ | 第二轮复测可删除真实创建的扩展组 |
| `assign_extension_group_to_profile` | ✅ | 第二轮复测可成功把空扩展组分配给 profile |

### 4.7 Team Lock

| 工具 | 结果 | 备注 |
|---|---|---|
| `get_team_locks` | ⚠️ | 需要 team plan，返回相应错误 |
| `get_team_lock_status` | ⚠️ | 需要 team plan，返回相应错误 |

### 4.8 Sync Settings

| 工具 | 结果 | 备注 |
|---|---|---|
| `get_sync_settings` | ✅ | 可读取当前配置 |
| `save_sync_settings` | ✅ | 临时写入后已恢复为 `null/null` |
| `request_profile_sync` | ⚠️ | 第二轮复测仍返回 `-32602: Sync is not enabled for this profile` |

### 4.9 Synchronizer

| 工具 | 结果 | 备注 |
|---|---|---|
| `start_sync_session` | ✅ | 1 leader + 1 follower 启动成功 |
| `stop_sync_session` | ✅ | 可正常停止 |
| `get_sync_sessions` | ✅ | 启动后可见，停止后为空 |
| `remove_sync_follower` | ✅ | 可正常移除 follower |

### 4.10 页面自动化（基础）

| 工具 | 结果 | 备注 |
|---|---|---|
| `navigate` | ✅ | 可正常导航到测试页 |
| `screenshot` | ✅ | 成功返回页面截图 |
| `evaluate_javascript` | ⚠️ | 受付费套餐限制，返回 paid-plan CDP 错误 |
| `click_element` | ⚠️ | 同上 |
| `type_text` | ⚠️ | 同上 |
| `get_page_content` | ⚠️ | 同上 |
| `get_page_info` | ⚠️ | 同上 |

### 4.11 交互增强

| 工具 | 结果 | 备注 |
|---|---|---|
| `hover_element` | ⚠️ | 受付费套餐限制 |
| `focus_element` | ⚠️ | 受付费套餐限制 |
| `scroll_to` | ⚠️ | 受付费套餐限制 |
| `scroll_element_into_view` | ⚠️ | 受付费套餐限制 |
| `press_key` | ✅ | 可执行，返回 `pressed=true` |
| `press_hotkey` | ✅ | 可执行，返回 `pressed=true` |
| `select_option` | ⚠️ | 受付费套餐限制 |
| `check_checkbox` | ⚠️ | 受付费套餐限制 |
| `uncheck_checkbox` | ⚠️ | 受付费套餐限制 |

### 4.12 DOM 读取增强

| 工具 | 结果 | 备注 |
|---|---|---|
| `element_exists` | ⚠️ | 受付费套餐限制 |
| `get_element_text` | ⚠️ | 受付费套餐限制 |
| `get_element_html` | ⚠️ | 受付费套餐限制 |
| `get_element_attribute` | ⚠️ | 受付费套餐限制 |
| `get_element_rect` | ⚠️ | 受付费套餐限制 |
| `query_elements` | ⚠️ | 受付费套餐限制 |
| `count_elements` | ⚠️ | 受付费套餐限制 |

### 4.13 Tab / Window 管理

| 工具 | 结果 | 备注 |
|---|---|---|
| `list_tabs` | ✅ | 可列出标签页 |
| `switch_tab` | ✅ | 可按 `index` 切换 |
| `new_tab` | ✅ | 可新开标签页 |
| `close_tab` | ✅ | 可关闭标签页 |
| `get_active_tab` | ✅ | 可正确返回当前活动 tab |

### 4.14 文件上传 / 下载

| 工具 | 结果 | 备注 |
|---|---|---|
| `set_file_input` | ⚠️ | 受付费套餐限制 |
| `upload_file` | ⚠️ | 受付费套餐限制 |
| `download_file` | ✅ | 成功下载到 profile 的 `mcp-downloads` |
| `wait_for_download` | ✅ | 返回 `completed`，文件内容已核对为 `MCP download works` |

### 4.15 Frame / iframe

| 工具 | 结果 | 备注 |
|---|---|---|
| `list_frames` | ✅ | 正确返回 main frame + child frame |
| `get_frame_tree` | ✅ | 树结构正常 |
| `find_frame` | ✅ | 可通过 `name=child-frame` 定位 |

### 4.16 Console Capture

| 工具 | 结果 | 备注 |
|---|---|---|
| `start_console_capture` | ✅ | 可启动 capture |
| `get_console_logs` | ❌ | 第二轮复测仍为空；`start_console_capture` 成功后打开 `auto-log.html`，`logs=[]` |
| `clear_console_logs` | ✅ | 可执行，但因无采集结果仅 `cleared=0` |

### 4.17 Network Capture

| 工具 | 结果 | 备注 |
|---|---|---|
| `start_network_capture` | ✅ | 可启动 capture |
| `get_network_requests` | ❌ | 第二轮复测仍为空；`start_network_capture` 成功后打开 `auto-fetch.html`，`requests=[]` |
| `get_response_body` | ⚠️ | 第二轮复测仍因抓不到 `request_id`，仅验证到 `-32004 request not found` 错误路径 |
| `clear_network_requests` | ✅ | 可执行，但因无采集结果仅 `cleared=0` |

### 4.18 Traffic Stats

| 工具 | 结果 | 备注 |
|---|---|---|
| `get_all_traffic_snapshots` | ✅ | 有返回，但包含大量 `profile_id=null` 项，建议后续排查 |
| `get_traffic_stats_for_period` | ✅ | 返回最近 300 秒流量统计 |
| `clear_all_traffic_stats` | ✅ | 返回清理成功 |

### 4.19 Cookie / Storage

#### Cookie

| 工具 | 结果 | 备注 |
|---|---|---|
| `read_profile_cookies` | ✅ | 可读取，测试页下 cookies 为 0 |
| `copy_profile_cookies` | ✅ | 可执行，结果为 `0 copied / 0 replaced` |
| `import_cookies_from_file` | ❌ | 第二轮复测在 profile 停止后传入 `[]` 仍报 `No valid cookies found in the file` |
| `export_profile_cookies` | ✅ | 成功导出 `[]` |

#### Storage

| 工具 | 结果 | 备注 |
|---|---|---|
| `get_local_storage` | ⚠️ | 受付费套餐限制 |
| `set_local_storage` | ⚠️ | 受付费套餐限制 |
| `get_session_storage` | ⚠️ | 受付费套餐限制 |
| `clear_storage` | ⚠️ | 受付费套餐限制 |

### 4.20 Wait 系列

| 工具 | 结果 | 备注 |
|---|---|---|
| `wait_for_selector` | ⚠️ | 受付费套餐限制 |
| `wait_for_text` | ⚠️ | 受付费套餐限制 |
| `wait_for_navigation` | ⚠️ | 受付费套餐限制 |

### 4.21 Recipe / Workflow

| 工具 | 结果 | 备注 |
|---|---|---|
| `validate_recipe` | ✅ | `valid=true` |
| `preview_recipe` | ✅ | `effectiveArguments` / `replay` 均返回 |
| `run_recipe` | ⚠️ | 第一步 `navigate` 成功，第二步 `type_text` 因 paid-plan CDP 错误失败 |
| `run_batch_profile_workflow` | ⚠️ | 两个 profile 的 `navigate` 均成功，后续 `get_page_info` 因 paid-plan CDP 错误失败 |

---

## 五、关键问题清单

### P1：当前计划限制导致大面积自动化不可用

统一报错：

```text
-32000: CDP error: {"code":-32000,"message":"Browser automation requires a paid Donut Browser plan."}
```

受影响工具包括但不限于：
- DOM 读取
- 元素点击/输入/选择/checkbox
- Storage
- Wait
- JS 执行
- page info/content
- recipe 中依赖上述能力的步骤

### P1：Console / Network Capture 启动成功但复测后仍无数据

复现路径：
1. `start_console_capture` / `start_network_capture`
2. 打开 `auto-log.html` / `auto-fetch.html`
3. `get_console_logs` / `get_network_requests`
4. 返回空数组 / 空列表

这更像功能缺陷，而不是权限错误。

### P2：文档示例与实现不一致

`import_cookies_from_file` 文档示例使用 `[]`，但实现要求“至少一条有效 cookie”。建议同步修正文档或放宽实现。

---

## 六、建议

1. **先确认当前测试环境的计划状态**：如果目标是验证自动化主链路，需先解决 paid-plan gating，否则大量能力无法做正向验收。
2. **优先修复 Console / Network Capture**：这两项即使不依赖深度 DOM 操作，也应能在当前环境抓到日志/请求。
3. **补充 Extension 模块真实安装场景测试**：第二轮复测已确认响应类型异常修复，但当前仅验证了空扩展组和不存在的扩展 ID。
4. **修正文档**：至少同步以下差异：
   - `import_cookies_from_file` 的 `[]` 示例
   - `import_proxies` 的 JSON schema 要求
   - 说明哪些工具受 plan 限制
   - 说明 Console / Network Capture 当前已知限制或缺陷

---

## 七、清理结果

- 已删除临时 profile
- 已删除临时 group
- 已删除临时 proxy / imported proxy
- 已删除临时 extension group
- 已删除临时 VPN
- 已恢复 `sync_settings` 为：

```json
{
  "sync_server_url": null,
  "sync_token": null
}
```

