# MCP 双内核测试报告（阶段版）

## 测试时间
- 2026-05-03（Asia/Shanghai）

## 测试环境
- MCP Health：
  - `http://127.0.0.1:51080/health`
  - 返回：`status=ok`, `server=JnmBrowser`, `version=0.22.7`, `protocolVersion=2025-11-25`
- 本地测试页：
  - `http://127.0.0.1:18080/index.html`
  - 本地目录：`/tmp/jnm-mcp-test`
- 测试 profile：
  - Chromium：`mcp-chromium-test`
    - id: `855c3df3-b474-4b23-bae6-00a739377102`
  - Camoufox：`mcp-camoufox-test-v3`
    - id: `afc4ee9e-4cd9-4e56-90f8-9739c2186bae`

---

## 总结

### 总体结论
当前可以认定：

- **Chromium MCP 主线可用**
- **Camoufox 页面主线已打通**
- **双内核核心页面主线已通过**
- **扩展能力主线已基本补齐**

当前剩余重点，已经收敛到：

- **指纹专项收口**
  - Chromium 固定指纹字段运行态一致性
  - Chromium randomize 与固定项共存
  - Camoufox 指纹运行态 spot-check

### 当前阶段结论
- Chromium：主线通过，既有缺陷已修复
- Camoufox：启动 / 页面 / tab 主线通过
- 当前主线：进入**指纹专项收口**

---

# 一、Chromium 测试结果

## 1. P0 冒烟测试

### 通过项
- `run_profile`
- `get_profile_status`
- `get_page_info`
- `screenshot`
- `get_page_content`
- `wait_for_selector`
- `type_text`
- `click_element`
- `get_element_text`
- `kill_profile`

### 关键验证结果
- 页面标题正确：
  - `MCP Test Page`
- URL 正确：
  - `http://127.0.0.1:18080/index.html`
- `readyState`：
  - `complete`
- 输入 `hello-mcp` 后点击 `Submit`
- 页面结果文本正确：
  - `submitted:hello-mcp`

### 判定
- **P0 主链通过**

---

## 2. Frame / iframe 测试

### 通过项
- `list_frames`
- `get_frame_tree`
- `find_frame`
- `type_text`（iframe 内）
- `click_element`（iframe 内）
- `get_page_content`（frame）
- `get_element_html`（frame）

### 关键结果
- 检测到 2 个 frame：
  - main frame index 0
  - iframe index 1
- iframe 名称：
  - `child-frame`
- iframe 内交互后，`get_page_content(frame)` 返回：
  - `iframe-submitted:iframe-mcp`
- `get_element_html(frame, #iframe-result)` 也能正确读到：
  - `<div id="iframe-result">iframe-submitted:iframe-mcp</div>`

### 回归结果
- `get_element_text(frame, #iframe-result)` 已修复
- 现可正确返回：
  - `iframe-submitted:iframe-mcp-2`

### 判定
- **通过**

---

## 3. query / 元素读取一致性

### 回归结果
- `query_elements(frame, #iframe-result)` 已修复
- 现可正确返回：
  - `count = 1`
  - 元素文本为 `iframe-submitted:iframe-mcp-2`

### 判定
- **通过**

---

## 4. Tab 测试

### 通过项
- `list_tabs`
- `new_tab`
- `get_active_tab`
- `switch_tab`
- `close_tab`

### 关键结果
- 新 tab 创建成功
- active tab 可切换
- 关闭后剩余 tab 正常

### 判定
- Tab 功能本身：**通过**
- `{"by":"index","value":"1"}` 字符串数字兼容已修复

---

## 5. Storage 测试

### 通过项
- `get_local_storage`
- `set_local_storage`
- `get_session_storage`
- `clear_storage`

### 关键结果
- 初始 localStorage 包含：
  - `mcp_local_key=local-value`
- 初始 sessionStorage 包含：
  - `mcp_session_key=session-value`
- 写入 `mcp_set_key` 后可成功读回
- `clear_storage(storage=both)` 后 local/session 均为空

### 判定
- **通过**

---

## 6. Console Capture 测试

### 通过项
- `start_console_capture`
- `click_element(#log-button)`
- `get_console_logs`
- `clear_console_logs`

### 实际捕获
成功捕获：
- `manual warning log`
- `manual error log`

### 注意
未捕获到初始加载时的：
- `MCP test page loaded`

原因更像是：
- capture 启动时机晚于页面初次加载

### 判定
- **主功能通过**
- 当前不作为缺陷记阻塞

---

## 7. Network Capture 测试

### 通过项
- `start_network_capture`
- `click_element(#fetch-button)`
- `get_network_requests`
- `get_response_body`
- `clear_network_requests`

### 实际结果
成功捕获请求：
- `http://127.0.0.1:18080/api/echo?value=network-test`

请求信息正确：
- method: `GET`
- status: `200`
- mimeType: `application/json`

响应体正确：

```json
{"ok": true, "value": "network-test"}
```

### 判定
- **通过**

---

# 二、Camoufox 测试结果

## 1. Profile 生命周期

### 通过项
- `run_profile`
- `get_profile_status`
- `kill_profile`

### 关键结果
- profile 可正常启动
- `is_running=true`
- 可正常关闭

### 判定
- **生命周期通过**

---

## 2. 页面自动化主线

### 通过项
- `get_page_info`
- `screenshot`
- `get_page_content`
- `list_tabs`
- `new_tab`
- `get_active_tab`
- `switch_tab`
- `close_tab`

### 关键结果
- `run_profile` 后立刻 `get_profile_status`：
  - `is_running = true`
- `get_page_info`：
  - `title = MCP Test Page`
  - `url = http://127.0.0.1:18080/index.html`
  - `readyState = complete`
- `screenshot`：
  - 页面非空白
  - 关键内容可见：`MCP Test Page`、输入框、按钮、iframe、`Scroll Target`
- `get_page_content(format=text)`：
  - 返回文本包含 `MCP Test Page`、`Search Box`、`Delayed Text Ready`、`Scroll Target`
- `get_page_content(format=html)`：
  - 返回完整 DOM
  - 包含 `<title>MCP Test Page</title>`、`#search`、`#result`、`#child-frame`
- Tab 主线：
  - `list_tabs` / `new_tab` / `get_active_tab` / `switch_tab` / `close_tab` 均通过
  - `switch_tab` 使用 `{"by":"index","value":"0"}` 成功
  - 末尾再次 `get_profile_status` 仍为 `is_running = true`

### 判定
- **主线通过**

---

# 三、双内核差异对比

## Chromium
- 页面自动化主链可用
- frame / tab / storage / console / network 已通过当前阶段验证

## Camoufox
- profile 生命周期可用
- 页面自动化主链可用
- tab 主线可用

## 当前差异结论
- 双内核核心页面主线已通过
- 扩展能力主线已基本通过
- 当前剩余差异主要集中在**指纹运行态一致性**

---

# 四、指纹专项回归结果（本轮新增）

## 1. generate_sample_fingerprint

### Chromium
- **通过**
- 能生成样例指纹
- 返回了：
  - `fingerprint`
  - `nextStep.argumentsTemplate`

### Camoufox
- **通过**
- 能生成完整样例 fingerprint JSON

## 2. get_profile_fingerprint

### Chromium
- **通过**
- 能正常读取当前 profile fingerprint 配置

### Camoufox
- **通过**
- 能正常读取当前 profile fingerprint 配置

## 3. Chromium 固定字段写入 + 运行态 spot-check

### 写入字段
- `language = fr-FR`
- `languages = ["fr-FR", "fr"]`
- `timezone = Europe/Paris`
- `hardwareConcurrency = 6`
- `deviceMemory = 16`
- `brand / brandVersion`
- `webglVendor / webglRenderer`

### 运行态结果

#### 已生效
- `language` ✅
- `languages` ✅
- `timezone` ✅
- `hardwareConcurrency` ✅

#### 未完全生效
- `deviceMemory` ❌
  - 存储值：`16`
  - 运行态：`8`

### 判定
- **部分通过**
- 结论：
  - 固定字段链路不是全坏
  - 但 `deviceMemory` 仍存在“写入成功、运行态未生效”的问题

## 4. Chromium randomize_fingerprint_on_launch

### 测试方式
- 保留固定字段
- 开启 `randomize_fingerprint_on_launch = true`
- 连续运行 2 次

### 结果

#### 已通过
- `seed` 确实变化 ✅
  - 第 1 次：`443810837`
  - 第 2 次：`4192614193`
- 固定字段已保留 ✅
  - `language = fr-FR`
  - `languages = ["fr-FR", "fr"]`
  - `timezone = Europe/Paris`
  - `hardwareConcurrency = 6`

### 判定
- **通过**
- 结论：
  - `randomize_fingerprint_on_launch` 已能与固定简单指纹项共存
  - 但 `deviceMemory` 仍继续表现为运行态 `8`

## 5. Camoufox 指纹运行态 spot-check

### 已确认
- 存储侧 fingerprint 可正常读取
- `generate_sample_fingerprint` 使用 profile 实际版本 `v135.0.1-beta.24` 可正常生成

### 本轮前置动作
- 先重编译并重启了最新后端实例
- 原因：
  - 运行中的 `JnmBrowser` 二进制时间早于 `mcp_server.rs / chromium_manager.rs`
  - 先避免“代码已改，但进程仍是旧逻辑”的假回归

### 本轮回归结果

#### 已修复
- `evaluate_javascript` 不再报旧的 CDP 错误 ✅
- 运行态 spot-check 已能真正执行

#### 新发现
- Camoufox **存储侧 fingerprint 与运行态不一致** ❌

存储侧关键字段：
- `navigator.userAgent`
  - `Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:146.0) Gecko/20100101 Firefox/146.0`
- `navigator.platform`
  - `Win32`
- `navigator.hardwareConcurrency`
  - `4`
- `locale:language / locale:region`
  - `zh / CN`
- `timezone`
  - `Asia/Shanghai`

运行态读取结果：
- `userAgent`
  - `Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:135.0) Gecko/20100101 Firefox/135.0`
- `platform`
  - `MacIntel`
- `hardwareConcurrency`
  - `10`
- `language`
  - `en-US`
- `languages`
  - `["en-US", "en"]`
- `timezone`
  - `Asia/Shanghai`

### 判定
- **未通过**
- 旧的“CDP 路径错误”已关闭
- 当前新阻塞是：
  - Camoufox 启动时的 fingerprint 注入 / 运行态覆盖并没有和存储配置保持一致

## 6. 本轮定向复测（按最新修复点重跑）

本轮按 3 组目标重跑：

- A. Camoufox `evaluate_javascript` 指纹运行态 spot-check
- B. Chromium `deviceMemory = 16` 运行态校验
- C. Chromium `randomize_fingerprint_on_launch + 固定 locale/timezone`

### A. Camoufox `evaluate_javascript`

#### 结果
- **部分通过**
- `evaluate_javascript` 已不再报：
  - `No CDP connection available`
- 运行态实际返回：
  - `userAgent = Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:135.0) Gecko/20100101 Firefox/135.0`
  - `platform = MacIntel`
  - `language = en-US`
  - `languages = ["en-US", "en"]`
  - `hardwareConcurrency = 10`
  - `timezone = Asia/Shanghai`

#### 结论
- **DB-MCP-007 可确认关闭**
- 但 **DB-MCP-008 仍在**
  - 工具链路已通
  - 运行态 fingerprint 仍未与存储配置对齐

### B. Chromium `deviceMemory`

#### 测试方式
- 固定写入：
  - `deviceMemory = 16`
- 启动后读取：
  - `navigator.deviceMemory`

#### 结果
- **未通过**
- 存储侧：
  - `deviceMemory = 16`
- 运行态：
  - `navigator.deviceMemory = 8`

#### 结论
- **DB-MCP-005 仍未修复**
- 本轮针对 `proxy navigator` 的补强还没有把运行态值改到 `16`

### C. Chromium randomize + 固定 locale/timezone

#### 结果
- **通过**
- 连续两次启动：
  - 第 1 次 seed：`1357245001`
  - 第 2 次 seed：`3848622005`
- 两次运行态均保持：
  - `language = fr-FR`
  - `languages = ["fr-FR", "fr"]`
  - `timezone = Europe/Paris`
  - `hardwareConcurrency = 6`

#### 结论
- **DB-MCP-006 持续确认已修复**
- 当前 randomize 只表现为 seed 变化，不再把固定 locale/timezone 改回默认值

## 7. 最新两项复测（重编译最新二进制并重启服务后）

本轮先执行：

- 重编译最新 `JnmBrowser` 二进制
- 重启 `127.0.0.1:51080` MCP 服务

然后只重跑：

- `DB-MCP-005`
- `DB-MCP-008`

### A. DB-MCP-005：Chromium `deviceMemory`

#### 结果
- **未通过**
- 存储侧仍正确写入：
  - `deviceMemory = 16`
- 运行态再次读取仍是：
  - `navigator.deviceMemory = 8`

#### 结论
- 这说明本轮“当前页立即注入 override”的修复，**仍未把运行态值改成 16**
- **DB-MCP-005 继续保持未关闭**

### B. DB-MCP-008：Camoufox 运行态一致性

#### 结果
- **通过**
- 存储侧关键字段：
  - `navigator.userAgent = Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:146.0) Gecko/20100101 Firefox/146.0`
  - `navigator.platform = Win32`
  - `navigator.hardwareConcurrency = 4`
  - `locale:language / locale:region = zh / CN`
  - `timezone = Asia/Shanghai`
- 运行态读取结果现已对齐：
  - `userAgent = Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:146.0) Gecko/20100101 Firefox/146.0`
  - `platform = Win32`
  - `hardwareConcurrency = 4`
  - `language = zh-CN`
  - `languages = ["zh-CN", "zh"]`
  - `timezone = Asia/Shanghai`

#### 结论
- Camoufox 已从“只能读存储配置”推进到“运行态实际对齐”
- **DB-MCP-008 可关闭**

## 8. 最新单项复测：DB-MCP-005

本轮先执行：

- 重编译最新 `JnmBrowser` 二进制
- 重启 `127.0.0.1:51080` MCP 服务

然后只重跑：

- `DB-MCP-005`

### 结果
- **通过**
- 存储侧：
  - `deviceMemory = 16`
- 运行态：
  - `navigator.deviceMemory = 16`

### 结论
- 这次“在运行态读取前强制刷新当前 target 的 fingerprint override”已经生效
- **DB-MCP-005 可关闭**

## 9. 完整 K 组回归（本轮最终收口）

本轮按 `/Users/lijinmu/Documents/New project/donutbrowser/MCP_双内核完整测试方案.md` 的 K 组完整重跑：

- K1 `get_profile_fingerprint`
- K2 `update_profile_fingerprint`
- K3 `generate_sample_fingerprint`
- K4 运行态 spot-check

### K1. get_profile_fingerprint

#### Chromium
- **通过**
- 可正常读取当前 profile 指纹配置

#### Camoufox
- **通过**
- 可正常读取当前 profile 指纹配置

### K2. update_profile_fingerprint

#### Chromium
- **通过**
- 固定写入字段后，可正常读回：
  - `language = fr-FR`
  - `languages = ["fr-FR", "fr"]`
  - `timezone = Europe/Paris`
  - `hardwareConcurrency = 6`
  - `deviceMemory = 16`
  - `brand / brandVersion`
  - `webglVendor / webglRenderer`

### K3. generate_sample_fingerprint

#### Chromium
- **通过**
- 可正常生成样例 fingerprint

#### Camoufox
- **通过**
- 使用 profile 实际版本 `v135.0.1-beta.24` 可正常生成样例 fingerprint

### K4. 运行态 spot-check

#### Chromium：固定字段运行态
- **通过**
- 运行态读取结果：
  - `language = fr-FR`
  - `languages = ["fr-FR", "fr"]`
  - `hardwareConcurrency = 6`
  - `deviceMemory = 16`
  - `timezone = Europe/Paris`

#### Chromium：randomize_fingerprint_on_launch
- **通过**
- 连续两次启动：
  - 第 1 次 seed：`3526799013`
  - 第 2 次 seed：`2486245243`
- 两次运行态均保持：
  - `language = fr-FR`
  - `languages = ["fr-FR", "fr"]`
  - `hardwareConcurrency = 6`
  - `deviceMemory = 16`
  - `timezone = Europe/Paris`

#### Camoufox：运行态自洽性
- **通过**
- 存储侧关键字段与运行态一致：
  - `userAgent = Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:146.0) Gecko/20100101 Firefox/146.0`
  - `platform = Win32`
  - `language = zh-CN`
  - `languages = ["zh-CN", "zh"]`
  - `hardwareConcurrency = 4`
  - `timezone = Asia/Shanghai`

### 本轮 K 组结论
- **完整 K 组回归通过**
- 当前未发现指纹专项回退
- Chromium / Camoufox 两条指纹主链已闭环

---

# 五、缺陷清单

## 缺陷 1
- 编号：DB-MCP-001
- 现象：
  - Camoufox 页面主线阻塞
- 当前状态：
  - **已修复**
- 影响范围：
  - Camoufox 页面自动化主线
- 优先级：
  - **P0（已关闭）**

## 缺陷 2
- 编号：DB-MCP-002
- 现象：
  - Chromium `get_element_text(frame, #iframe-result)` 失败
- 当前状态：
  - **已修复**
- 影响范围：
  - frame 内文本读取
- 优先级：
  - **P1（已关闭）**

## 缺陷 3
- 编号：DB-MCP-003
- 现象：
  - Chromium `query_elements(frame, #iframe-result)` 返回 0
- 当前状态：
  - **已修复**
- 影响范围：
  - frame 下元素查询一致性
- 优先级：
  - **P1（已关闭）**

## 缺陷 4
- 编号：DB-MCP-004
- 现象：
  - `switch_tab` 在 `by=index` 时要求真整数，传字符串报 `-32602`
- 当前状态：
  - **已修复**
- 影响范围：
  - 参数模型/调用体验
- 优先级：
  - **P2（已关闭）**

## 缺陷 5
- 编号：DB-MCP-005
- 现象：
  - Chromium `deviceMemory` 写入成功，但运行态仍固定读到 `8`
- 当前状态：
  - **已修复**
- 本轮复测：
  - **仍复现**
- 最新复测（重编译 + 重启服务后）：
  - **已通过**
- 影响范围：
  - Chromium 指纹运行态一致性
- 优先级：
  - **P1（已关闭）**

## 缺陷 6
- 编号：DB-MCP-006
- 现象：
  - Chromium 开启 `randomize_fingerprint_on_launch` 后，固定 `language / languages / timezone` 被覆盖
- 当前状态：
  - **已修复**
- 本轮复测：
  - **继续通过**
- 影响范围：
  - Chromium 随机指纹与固定项共存
- 优先级：
  - **P1（已关闭）**

## 缺陷 7
- 编号：DB-MCP-007
- 现象：
  - Camoufox `evaluate_javascript` 仍走旧 CDP 路径，阻塞运行态指纹 spot-check
- 当前状态：
  - **已修复**
- 本轮复测：
  - **继续通过**
- 影响范围：
  - Camoufox 指纹专项 spot-check
- 优先级：
  - **P1（已关闭）**

## 缺陷 8
- 编号：DB-MCP-008
- 现象：
  - Camoufox 存储侧 fingerprint 与运行态指纹不一致
- 当前状态：
  - **已修复**
- 本轮复测：
  - **已通过**
- 影响范围：
  - Camoufox 指纹注入 / 运行态一致性
- 优先级：
  - **P1（已关闭）**

---

# 六、下一阶段建议

## 当前收口顺序
1. **可将指纹专项阶段正式收口**
2. **如需继续主线，可进入最终总报告 / 全量回归整理**

---

# 七、当前阶段判定

## P0
- Chromium：通过
- Camoufox：通过

## P1
- 已关闭

## P2
- 已关闭

## 最终阶段结论
当前可以确认：

- **Chromium MCP 主线可用**
- **Camoufox MCP 页面主线可用**
- **双内核扩展能力主线已基本通过**
- **Chromium 指纹专项关键缺陷已补齐**
- **Camoufox 指纹专项运行态一致性已补齐**
- **完整 K 组回归已通过，当前未见指纹回退**
