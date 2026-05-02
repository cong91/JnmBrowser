# MCP / 浏览器自动化能力统计与扩展路线图

> 用途：
>
> 1. 盘点当前项目已经具备的 MCP / 浏览器自动化能力
> 2. 统计还未完成、但基于现有代码可以实现的能力
> 3. 给出后续扩展到 **更接近 Playwright / DevTools 级体验** 的设计建议
> 4. 作为后期逐步加功能的长期路线图

---

# 一、项目当前状态概览

## 1. 当前项目已经不是“没有自动化”
它已经具备一个可用的 MCP 浏览器自动化底座，核心特点：

- 可以启动 / 关闭浏览器 profile
- 可以获取运行中 profile 的 CDP 端口
- 可以通过 CDP WebSocket 对浏览器发命令
- 已经具备页面跳转、截图、执行 JS、点击、输入、读页面内容等能力
- 还能结合 profile / proxy / vpn / fingerprint / extension / sync 做更强的隔离与编排

核心代码入口：
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/mcp_server.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/lib.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/wayfern_manager.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/proxy_server.rs`

---

## 2. 当前缺的不是“能不能自动化”，而是“自动化工具集还不完整”
当前的差距主要在：

- 没有 Playwright 那种完整 locator 体系
- 没有真正的 auto-wait 语义
- 没有 frame / iframe 精细管理
- 没有 console / network 的原生观测能力
- 没有 tab / window 的一整套管理工具
- 没有文件上传下载工具
- selector 目前偏基础，主要还是 CSS selector
- 没有 DevTools 级别的可视化调试界面

所以后续工作的目标，不是推倒重来，而是：

> **在现有 CDP + MCP 底座上，把工具链逐步补齐。**

---

# 二、当前已完成的 MCP 原子能力（52 项）

## A. Profile / Group / Tag
1. `list_profiles`
2. `get_profile`
3. `run_profile`
4. `kill_profile`
5. `create_profile`
6. `update_profile`
7. `delete_profile`
8. `list_tags`
9. `get_profile_status`
10. `list_groups`
11. `get_group`
12. `create_group`
13. `update_group`
14. `delete_group`
15. `assign_profiles_to_group`

---

## B. Proxy / VPN
16. `list_proxies`
17. `get_proxy`
18. `create_proxy`
19. `update_proxy`
20. `delete_proxy`
21. `export_proxies`
22. `import_proxies`
23. `import_vpn`
24. `list_vpn_configs`
25. `delete_vpn`
26. `connect_vpn`
27. `disconnect_vpn`
28. `get_vpn_status`

---

## C. 指纹 / DNS / 扩展
29. `get_profile_fingerprint`
30. `update_profile_fingerprint`
31. `update_profile_proxy_bypass_rules`
32. `update_profile_dns_blocklist`
33. `get_dns_blocklist_status`
34. `list_extensions`
35. `list_extension_groups`
36. `create_extension_group`
37. `delete_extension`
38. `delete_extension_group`
39. `assign_extension_group_to_profile`

---

## D. 团队锁 / 同步会话
40. `get_team_locks`
41. `get_team_lock_status`
42. `start_sync_session`
43. `stop_sync_session`
44. `get_sync_sessions`
45. `remove_sync_follower`

---

## E. 浏览器自动化
46. `navigate`
47. `screenshot`
48. `evaluate_javascript`
49. `click_element`
50. `type_text`
51. `get_page_content`
52. `get_page_info`

---

# 三、当前已经能做出的自动化场景

这些不一定对应单个 MCP tool，但已经可以组合出来：

## 1. 基础页面自动化
- 打开页面
- 点按钮
- 输入文本
- 读页面内容
- 截图
- 执行页面 JS

## 2. Profile 隔离自动化
- 一任务一 profile
- 多账号隔离运行
- 批量运行 / 批量关闭

## 3. 代理 / VPN / 指纹配合自动化
- 不同 profile 用不同代理
- 跑自动化前换指纹
- 用 DNS blocklist / bypass rules 调整环境

## 4. 多浏览器协同
- leader / follower 同步会话
- 多窗口对照观察

## 5. 反检测友好的基础能力
- profile 隔离
- 代理/VPN
- 指纹
- human-like typing

---

# 四、未完成但可实现的 MCP 原子能力（完整版）

> 下面这部分是后续长期要补的能力池。
>
> 统计口径：按“原子接口项”统计，而不是按“场景工作流”统计。
>
> 总计：**63 项已完成 + 待补能力中的待补原子能力 63?**
>
> 为避免混淆，这里只统计“待补项”：**63 - 52 = 不适用**。
>
> 实际待补原子能力总数：**50 项**。

---

## A. 后端已有命令 / 模块，适合直接补成 MCP（16 项）

### A1. Cookie 管理（4 项）
1. `read_profile_cookies`
2. `copy_profile_cookies`
3. `import_cookies_from_file`
4. `export_profile_cookies`

**代码依据**
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/lib.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/cookie_manager.rs`

**价值**
- 登录态迁移
- 多账号环境复制
- 账号恢复 / 会话导入导出

---

### A2. 流量统计（3 项）
5. `get_all_traffic_snapshots`
6. `get_traffic_stats_for_period`
7. `clear_all_traffic_stats`

**代码依据**
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/lib.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/traffic_stats.rs`

**价值**
- 看 profile 最近流量
- 看域名访问统计
- 做自动化健康检查

---

### A3. 指纹辅助（1 项）
8. `generate_sample_fingerprint`

**代码依据**
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/lib.rs`

**价值**
- 先生成可用样本，再微调指纹

---

### A4. GeoIP 数据管理（2 项）
9. `is_geoip_database_available`
10. `download_geoip_database`

**代码依据**
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/lib.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/geoip_downloader.rs`

---

### A5. 扩展管理增强（6 项）
11. `add_extension`
12. `update_extension`
13. `get_extension_icon`
14. `update_extension_group`
15. `add_extension_to_group`
16. `remove_extension_from_group`

**代码依据**
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/lib.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/extension_manager.rs`

**价值**
- 形成完整的扩展仓库与扩展组编排

---

## B. 基于现有 CDP 底座可补齐的自动化原子能力（31 项）

### B1. 等待类（5 项）
17. `wait_for_selector`
18. `wait_for_text`
19. `wait_for_navigation`
20. `wait_for_url_contains`
21. `wait_for_network_idle`

**意义**
- 这是接近 Playwright auto-wait 体验的基础
- 能大幅减少脆弱的 sleep

---

### B2. 页面交互增强（9 项）
22. `hover_element`
23. `focus_element`
24. `scroll_to`
25. `scroll_element_into_view`
26. `press_key`
27. `press_hotkey`
28. `select_option`
29. `check_checkbox`
30. `uncheck_checkbox`

---

### B3. DOM / 页面读取增强（7 项）
31. `get_element_text`
32. `get_element_html`
33. `get_element_attribute`
34. `get_element_rect`
35. `element_exists`
36. `query_elements`
37. `count_elements`

---

### B4. Tab / Window 管理（5 项）
38. `list_tabs`
39. `switch_tab`
40. `new_tab`
41. `close_tab`
42. `get_active_tab`

---

### B5. 存储与会话（7 项）
43. `get_cookies`
44. `set_cookies`
45. `delete_cookies`
46. `get_local_storage`
47. `set_local_storage`
48. `get_session_storage`
49. `clear_storage`

---

### B6. 文件上传 / 下载（4 项）
50. `upload_file`
51. `set_file_input`
52. `download_file`
53. `wait_for_download`

---

### B7. 观测类：Console / Network（7 项）
54. `start_console_capture`
55. `get_console_logs`
56. `clear_console_logs`
57. `start_network_capture`
58. `get_network_requests`
59. `get_response_body`
60. `clear_network_requests`

**意义**
- 这是最接近 DevTools 调试能力的一批
- 对 AI 调试页面极其重要

---

## C. 同步 / 云能力，后端已有业务逻辑，可补成 MCP（3 项）
61. `get_sync_settings`
62. `save_sync_settings`
63. `request_profile_sync`

---

# 五、最关键的能力差距：locator / auto-wait / frame / console-network

这四组能力是最值得优先补的，因为它们决定了：

- 工具是否稳定
- AI 是否容易用
- 自动化是否脆弱
- 调试是否足够强

---

## 1. locator 体系

### locator 是什么
不是简单的 CSS selector，而是一层更高级的“找元素抽象”。

### 为什么重要
如果只有 CSS selector，会有这些问题：
- class / id 容易变
- AI 很难稳定生成高质量复杂 selector
- 调试成本高
- 可读性差

### 理想 locator 类型
建议最终支持：
- `css`
- `text`
- `testid`
- `xpath`
- `role`
- `label`
- `placeholder`

### 推荐分批做法
#### 第一批先做
- `css`
- `text`
- `testid`
- `xpath`

#### 第二批再做
- `role`
- `label`
- `placeholder`

### 推荐 MCP 参数结构
```json
{
  "locator": {
    "by": "css|text|testid|xpath|role|label|placeholder",
    "value": "...",
    "nth": 0,
    "exact": false
  }
}
```

### AI 为啥特别需要 locator
AI 更容易理解：
- “点文本为登录的按钮”
- “在 label 是邮箱的输入框里输入”

而不是：
- `.btn.btn-primary:nth-child(2)`

---

## 2. auto-wait

### auto-wait 是什么
不是你每次都手写 `sleep` 或 `wait_*`，而是工具自己在点击/输入/读取前知道该等什么。

### 为什么重要
没有 auto-wait 时，会经常出现：
- 页面没渲染完就点
- 输入框还没 ready 就 type
- DOM 刚切换，节点已经 stale
- 本地能跑，线上偶现失败

### 至少应该支持的等待语义
#### click 前
- 元素存在
- 元素可见
- 元素尺寸 > 0
- 元素未 disabled
- 元素可滚动进视口
- 必要时重试到 timeout

#### type 前
- 元素存在
- 可见
- 可编辑
- focus 成功

#### 读取前
- 元素存在
- 如果要求 visible，则等待可见

### 推荐实现原则
- 不依赖盲 sleep
- polling + timeout
- 带结构化错误原因
- 后续将 auto-wait **内建进 click/type/query**，而不是只靠独立 wait 工具

---

## 3. frame / iframe 管理

### 是什么
很多网站的目标元素不在主页面，而在：
- iframe
- 内嵌登录框
- 支付表单
- 验证码容器
- 第三方小组件

### 为什么重要
不做 frame 管理时，经常会出现：
- 页面上明明看见元素，脚本却找不到
- 按钮点不了
- 输入框无法输入

### 推荐能力
#### 第一阶段
- `list_frames`
- `get_frame_tree`
- `find_frame`

#### 第二阶段
让这些工具支持指定 frame：
- `click_element`
- `type_text`
- `evaluate_javascript`
- `get_page_content`
- `wait_for_selector`

### 推荐设计
**不建议优先做全局 `switch_frame`。**

因为 MCP 是离散工具调用模型，不像脚本 runtime 天然维护上下文栈。

更稳的做法是：
- 每次调用显式传 frame 参数

例如：
```json
{
  "frame": {
    "by": "main|index|url|name|id",
    "value": "..."
  }
}
```

### 难点
- cross-origin iframe
- frame 对应 execution context 管理
- 不是简单的 DOM 穿透

但由于项目已经有 CDP，这个是“工程细活”，不是“无底座无法做”。

---

## 4. Console / Network 观测

### 现在缺什么
虽然已有 `evaluate_javascript` / `get_page_content`，但还没有：
- 浏览器 console 原生日志采集
- 请求/响应事件采集
- response body 读取

### 为什么重要
这是让 AI 从“能自动化”升级到“能调试页面”的关键。

### 建议 MCP 能力
#### Console
- `start_console_capture`
- `get_console_logs`
- `clear_console_logs`

#### Network
- `start_network_capture`
- `get_network_requests`
- `get_response_body`
- `clear_network_requests`

### 底层协议
可接入：
- `Runtime.consoleAPICalled`
- `Log.entryAdded`
- `Network.requestWillBeSent`
- `Network.responseReceived`
- `Network.loadingFinished`
- `Network.loadingFailed`

---

# 六、和 Playwright 级工具集的差距

补完前面的“未完成 MCP”之后，项目会强很多，但**仍不自动等于 Playwright**。

## 补完后能获得什么
- 更完整交互能力
- 更完整等待能力
- tab/window
- cookies/storage
- upload/download
- console/network 调试能力
- frame-aware 操作

## 仍然还差什么
### 1. 统一 locator 抽象层
不仅是“多几个 tool”，而是全工具共享一套稳定 locator 模型。

### 2. 真正的内建 auto-wait
不是只有 `wait_*`，而是 click/type/query 默认就自带等待与重试。

### 3. frame / target 生命周期管理
复杂页面、弹窗、iframe、多 target 需要更强的上下文管理。

### 4. 更成熟的错误模型
例如：
- timeout
- element not found
- element not visible
- element detached
- frame not found
- cross-origin blocked

### 5. 更高阶的执行器
Playwright 强在“一个脚本里自然组织一连串步骤”。
MCP 工具是离散调用，想更接近 Playwright，还需要一层 orchestration。

---

# 七、和 DevTools 级调试能力的差距

## 补完 console/network MCP 后，你会得到什么
- AI 能看 console 输出
- AI 能看请求列表
- AI 能看状态码 / headers / body
- AI 能做页面调试和接口逆向

这已经非常接近 **DevTools 协议能力**。

## 但这仍然不等于 DevTools UI
因为真正的 DevTools 还包括：
- Console 面板 UI
- Network waterfall 时间轴
- 筛选 / 分组 / 排序
- 请求详情页
- Initiator / Timing / Priority
- DOM / Styles / Sources / Performance 等可视面板

### 结论
- **补完 MCP 后，AI 基本能用到核心调试数据**
- **但你还没有 DevTools 级的可视化界面**
- 如果要那种体验，还要额外做前端调试 UI

---

# 八、建议的总体开发路线图

## Phase 1：先把 MCP 变成“真正好用的自动化工具集”
优先级最高：
1. Console / Network capture
2. Cookie 管理
3. Traffic stats
4. wait 系列基础工具
5. locator 第一批（css/text/testid/xpath）
6. click/type 内建基础 auto-wait

### 目标
- AI 能看清发生了什么
- AI 能更稳定地操作页面

---

## Phase 2：补足“稳定性与复杂页面能力”
1. DOM 读取增强
2. Tab / Window 管理
3. frame / iframe 第一阶段
4. storage/session
5. upload/download
6. 交互增强（hover/key/select/checkbox）

### 目标
- 适配复杂站点
- 适配跨页、多标签、多上下文流程

---

## Phase 3：补足“更接近 Playwright 的体验”
1. locator 第二批（role/label/placeholder）
2. 更完整的 auto-wait 语义
3. 结构化错误模型
4. frame-aware 全量工具
5. 更高阶 orchestration 方案

### 目标
- 从“能用”提升到“顺手、稳定、可维护”

---

## Phase 4：如果需要，再做可视调试台
1. Console UI
2. Network request 列表 UI
3. waterfall 时间线
4. 请求详情页
5. filters / search / export

### 目标
- 服务人类开发者
- 补齐 DevTools 级可视化体验

---

# 九、最推荐的 MVP 范围

如果你们准备后面慢慢开始加功能，我建议第一版只做下面这些：

## MVP-1
1. `start_console_capture`
2. `get_console_logs`
3. `start_network_capture`
4. `get_network_requests`
5. `read_profile_cookies`
6. `export_profile_cookies`
7. `get_all_traffic_snapshots`
8. `wait_for_selector`

## MVP-2
9. locator 第一批（css / text / testid / xpath）
10. `click_element` / `type_text` 内建基础 auto-wait
11. `list_frames`
12. `evaluate_javascript` 支持显式 frame 参数

## MVP-3
13. `list_tabs`
14. `switch_tab`
15. `get_local_storage`
16. `set_local_storage`
17. `set_file_input`

---

# 十、实现难点与风险提示

## 1. locator 里最难的不是 css，而是 role/label
- CSS 容易
- text/testid/xpath 也相对直接
- role/label/placeholder 需要更细的 DOM / 可访问性解析

## 2. auto-wait 不能偷懒写成 sleep
真正好用的 auto-wait 必须做：
- polling
- timeout
- interactability check
- 结构化错误返回

## 3. frame 最难的是 cross-origin iframe
这部分不能简单用页面 JS 解决，必须认真利用 CDP 的 frame / execution context。

## 4. Console / Network 采集需要缓存策略
要考虑：
- 只保留最近 N 条
- 大 body 不默认全存
- requestId 到 response 的拼装

## 5. tab/window 与 frame/context 容易相互影响
后期最好统一做一层：
- target / tab / frame / execution context 管理模型

---

# 十一、对后续开发的最终建议

如果你们是准备“后面慢慢加功能”，建议按照下面顺序推进：

## 先补“AI 立刻收益最大”的
- console
- network
- wait
- cookies
- traffic stats

## 再补“自动化稳定性核心”的
- locator
- auto-wait 内建
- frame 管理

## 最后补“更接近完整工具链”的
- tabs/windows
- upload/download
- storage/session
- DevTools UI

---

# 十二、一句话结论

**这个项目已经有一个不错的 MCP 浏览器自动化底座；后续最值得做的不是推翻重写，而是围绕 locator、auto-wait、frame、console/network 这几条主线，分阶段把它补成一个真正强的自动化工具集。**
