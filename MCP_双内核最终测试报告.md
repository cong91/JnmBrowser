# MCP 双内核最终测试报告

## 一、报告目的

本报告用于汇总 JnmBrowser MCP 在 **Chromium（轻模式）** 与 **Camoufox（重模式）** 两条内核路径上的最终测试结论，给出：

- 双内核主线是否可用
- 关键扩展能力是否可用
- 指纹专项是否完成闭环
- 已发现缺陷是否已关闭

本报告基于以下材料整理：

- `/Users/lijinmu/Documents/New project/donutbrowser/MCP_双内核完整测试方案.md`
- `/Users/lijinmu/Documents/New project/donutbrowser/MCP_双内核测试阶段报告.md`

---

## 二、测试环境

- 测试时间：
  - `2026-05-03`
- MCP 服务：
  - `http://127.0.0.1:51080/mcp`
- MCP Health：
  - `status=ok`
  - `server=JnmBrowser`
  - `version=0.22.7`
  - `protocolVersion=2025-11-25`
- 本地测试页：
  - `http://127.0.0.1:18080/index.html`
- 测试 profile：
  - Chromium：
    - `mcp-chromium-test`
    - `855c3df3-b474-4b23-bae6-00a739377102`
  - Camoufox：
    - `mcp-camoufox-test-v3`
    - `afc4ee9e-4cd9-4e56-90f8-9739c2186bae`

---

## 三、最终结论

当前可以确认：

- **Chromium MCP 主线可用**
- **Camoufox MCP 主线可用**
- **双内核核心页面主线已通过**
- **双内核扩展能力主线已基本通过**
- **双内核指纹专项已完成闭环**
- **本轮完整 K 组回归已通过，未发现指纹回退**

结论级别：

- **P0：通过**
- **P1：已关闭**
- **P2：已关闭**

从测试结果看，当前版本已经可以支撑“双内核统一 MCP 接口”的主线交付。

---

## 四、能力覆盖结论

### 4.1 Chromium

已验证通过：

- Profile 生命周期：
  - `run_profile`
  - `get_profile_status`
  - `kill_profile`
- 页面主线：
  - `navigate`
  - `get_page_info`
  - `screenshot`
  - `get_page_content`
  - `wait_for_selector`
  - `type_text`
  - `click_element`
  - `get_element_text`
- iframe / frame：
  - `list_frames`
  - `get_frame_tree`
  - `find_frame`
  - `get_page_content(frame)`
  - `get_element_html(frame)`
  - `get_element_text(frame)`
  - `query_elements(frame)`
- tab：
  - `list_tabs`
  - `new_tab`
  - `get_active_tab`
  - `switch_tab`
  - `close_tab`
- storage：
  - `get_local_storage`
  - `set_local_storage`
  - `get_session_storage`
  - `clear_storage`
- 观测能力：
  - `start_console_capture`
  - `get_console_logs`
  - `clear_console_logs`
  - `start_network_capture`
  - `get_network_requests`
  - `get_response_body`
  - `clear_network_requests`
- 文件能力：
  - `upload_file`
  - `set_file_input`
  - `download_file`
  - `wait_for_download`
- workflow：
  - `validate_recipe`
  - `preview_recipe`
  - `run_recipe`
  - `run_batch_profile_workflow`
- 指纹能力：
  - `get_profile_fingerprint`
  - `update_profile_fingerprint`
  - `generate_sample_fingerprint`
  - 固定字段运行态生效
  - `randomize_fingerprint_on_launch`
  - `deviceMemory` 运行态生效

### 4.2 Camoufox

已验证通过：

- Profile 生命周期：
  - `run_profile`
  - `get_profile_status`
  - `kill_profile`
- 页面主线：
  - `navigate`
  - `get_page_info`
  - `screenshot`
  - `get_page_content`
  - `list_tabs`
  - `new_tab`
  - `get_active_tab`
  - `switch_tab`
  - `close_tab`
- 交互动作：
  - `click_element`
  - `type_text`
  - `hover_element`
  - `focus_element`
  - `scroll_to`
  - `scroll_element_into_view`
  - `press_key`
  - `press_hotkey`
  - `select_option`
  - `check_checkbox`
  - `uncheck_checkbox`
- storage：
  - `get_local_storage`
  - `get_session_storage`
  - `set_local_storage`
  - `clear_storage`
- 观测能力：
  - `start_console_capture`
  - `get_console_logs`
  - `clear_console_logs`
  - `start_network_capture`
  - `get_network_requests`
  - `get_response_body`
  - `clear_network_requests`
- 文件能力：
  - `upload_file`
  - `download_file`
  - `wait_for_download`
- workflow：
  - `validate_recipe`
  - `preview_recipe`
  - `run_recipe`
  - `run_batch_profile_workflow`
- 指纹能力：
  - `get_profile_fingerprint`
  - `generate_sample_fingerprint`
  - `evaluate_javascript` 主上下文可用
  - 指纹运行态字段与存储侧对齐

说明：

- Camoufox 当前已打通页面主线与指纹运行态主链。
- Camoufox 的 `evaluate_javascript(frame)` 不在本轮“已验证通过”范围内。

---

## 五、指纹专项最终结论

### 5.1 Chromium

最终确认通过：

- 固定字段写入后，运行态可正确读到：
  - `language = fr-FR`
  - `languages = ["fr-FR", "fr"]`
  - `timezone = Europe/Paris`
  - `hardwareConcurrency = 6`
  - `deviceMemory = 16`
- 开启 `randomize_fingerprint_on_launch = true` 后：
  - `seed` 会按启动变化
  - 固定字段不会被覆盖

完整 K 组回归中，连续两次 randomize 运行结果：

- 第 1 次 seed：
  - `3526799013`
- 第 2 次 seed：
  - `2486245243`

同时运行态持续保持：

- `language = fr-FR`
- `languages = ["fr-FR", "fr"]`
- `hardwareConcurrency = 6`
- `deviceMemory = 16`
- `timezone = Europe/Paris`

### 5.2 Camoufox

最终确认通过：

- 存储侧 fingerprint 可正常读取
- `generate_sample_fingerprint` 可正常生成
- `evaluate_javascript` 不再落旧 CDP 路径
- 运行态关键字段已与存储侧对齐：
  - `userAgent`
  - `platform`
  - `language`
  - `languages`
  - `hardwareConcurrency`
  - `timezone`

完整 K 组最终 spot-check 结果：

- `userAgent = Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:146.0) Gecko/20100101 Firefox/146.0`
- `platform = Win32`
- `language = zh-CN`
- `languages = ["zh-CN", "zh"]`
- `hardwareConcurrency = 4`
- `timezone = Asia/Shanghai`

---

## 六、缺陷关闭情况

本轮最终关闭缺陷如下：

- `DB-MCP-001`
  - Camoufox 页面主线阻塞
- `DB-MCP-002`
  - Chromium `get_element_text(frame)` 失败
- `DB-MCP-003`
  - Chromium `query_elements(frame)` 返回 0
- `DB-MCP-004`
  - `switch_tab(by=index)` 不兼容字符串数字
- `DB-MCP-005`
  - Chromium `deviceMemory` 写入成功但运行态不生效
- `DB-MCP-006`
  - Chromium randomize 覆盖固定 locale/timezone
- `DB-MCP-007`
  - Camoufox `evaluate_javascript` 仍走旧 CDP 路径
- `DB-MCP-008`
  - Camoufox 存储侧 fingerprint 与运行态不一致

最终状态：

- **P0：全部关闭**
- **P1：全部关闭**
- **P2：全部关闭**

---

## 七、风险与边界说明

虽然主线与指纹专项已经闭环，但仍建议保留以下边界认知：

1. 本报告结论基于当前测试环境：
   - 当前 MCP 服务版本
   - 当前双 profile
   - 当前本地测试页
2. Camoufox 的更深层 frame 级脚本执行能力，不在本轮最终“已覆盖完成”列表中。
3. 如后续继续演进：
   - Playwright 持久上下文管理
   - fingerprint 注入路径
   - runtime override 注入逻辑
   应优先重跑 K 组，防止回退。

---

## 八、建议的后续动作

建议进入以下其中一条：

### 方案 A：阶段收口

如果当前目标是交付测试结论，可直接把本报告作为：

- 双内核 MCP 当前版本的最终验收摘要

### 方案 B：继续扩大回归面

如果当前目标是做更稳的发布前验证，建议继续：

- 做一轮全量回归整理
- 增加更多真实业务页面回归
- 增加多 profile / 代理 / 指纹组合压测

---

## 九、最终判定

**最终判定：通过。**

更准确地说：

- JnmBrowser MCP 在 **Chromium + Camoufox** 双内核路径上，已经通过当前测试方案中的主线能力与指纹专项验证；
- 当前版本可以认定为：
  - **双内核主线可用**
  - **扩展能力主线可用**
  - **指纹专项闭环完成**

