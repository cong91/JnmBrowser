# MCP 全阶段开发计划书

> 目标：以 **“最终补齐一个完整、稳定、可扩展、适合 AI 使用的 MCP 浏览器自动化平台”** 为终局目标，给出从当前状态到完整能力的分阶段开发计划。
>
> 本计划书面向：
> - 产品规划
> - 技术路线设计
> - 分阶段排期
> - 后续逐步落地实施

---

# 一、项目目标定义

## 1. 终局目标
将当前项目的 MCP 能力，从“可用的基础浏览器控制接口”，升级为一个具备以下特征的完整平台：

- 具备 **Playwright 风格的核心自动化体验**
- 具备 **DevTools 协议级调试能力**
- 具备 **适合 AI 消费的结构化工具接口**
- 具备 **多 profile / 多代理 / 多指纹 / 多同步场景编排能力**
- 具备 **长期可维护、可扩展、可观测** 的 MCP 架构

---

## 2. 终局能力范围
最终计划覆盖以下能力域：

### A. 浏览器自动化核心
- 页面导航
- 点击、输入、滚动、悬停、快捷键
- DOM / 页面数据读取
- auto-wait
- locator 体系
- frame / iframe 管理
- tab / window 管理
- 文件上传下载
- storage / session / cookie 管理

### B. 调试与观测
- Console 捕获
- Network 请求捕获
- Response Body 获取
- Traffic Stats
- 后续可视化 DevTools 风格调试台

### C. 浏览器环境编排
- profile 生命周期管理
- proxy / vpn 管理
- fingerprint 管理
- DNS blocklist / bypass rules
- extension 管理

### D. 协同与同步
- team locks
- sync sessions
- sync settings
- profile / proxy / vpn / extension 同步

### E. AI 体验增强
- 统一 locator 参数模型
- 统一错误模型
- 稳定的超时 / 重试 / 等待机制
- 工具返回格式统一
- 更易于 agent 调用的调用语义

---

# 二、当前基线

## 1. 已有 MCP 基线
当前项目已经具备：

- Profile / Group / Proxy / VPN / Extension / Sync 会话管理
- 基础浏览器自动化：
  - `navigate`
  - `screenshot`
  - `evaluate_javascript`
  - `click_element`
  - `type_text`
  - `get_page_content`
  - `get_page_info`
- 已有 CDP 连接能力
- 已有 profile 级浏览器运行环境隔离能力

核心文件：
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/mcp_server.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/lib.rs`

---

## 2. 当前主要短板
### 自动化体验不足
- 没有完整 locator 体系
- 没有完整 auto-wait
- 没有 frame / iframe 管理
- 没有 tab / window 工具集
- 没有文件上传下载

### 调试能力不足
- 没有 console / network MCP 工具
- 没有 DevTools 风格 UI

### AI 友好性不足
- selector 仍偏基础 CSS
- 错误语义不统一
- 操作工具是离散的，缺少高级 orchestration 设计

---

# 三、总开发原则

## 1. 不推翻现有 MCP 结构
现有项目已经有 MCP + CDP 基础，不应引入第二套自动化主框架替代。

优先策略：
- 继续基于当前 `mcp_server.rs` 演进
- 继续复用现有 CDP 连接逻辑
- 在此基础上补统一抽象层

---

## 2. 优先补“AI 立刻收益最大”的能力
优先顺序原则：
1. 可观测性
2. 稳定性
3. 易用性
4. 可视化

也就是优先：
- console/network
- wait
- locator
- frame
- storage/cookie
- tabs
- upload/download
- 最后再做调试 UI

---

## 3. 先做原子能力，再做场景编排
先把底层 MCP tool 做扎实：
- click
- type
- wait
- frame
- network
- cookies

再做更高阶的：
- 登录工作流
- 列表抓取工作流
- 多账号批量任务编排

---

# 四、总体阶段规划

本计划按 **8 个阶段** 设计。

---

# Phase 0：基线治理与架构准备

## 目标
在开始大量加 MCP 工具前，先把扩展基础打好，避免后续不断返工。

## 范围
### 1. 工具模型整理
- 梳理现有 MCP tool 定义方式
- 梳理 tool input/output 风格
- 统一返回格式：
  - text
  - json
  - binary/base64

### 2. 错误模型设计
定义统一错误分类：
- timeout
- invalid_locator
- element_not_found
- element_not_visible
- frame_not_found
- tab_not_found
- navigation_failed
- network_capture_not_started
- unsupported_operation

### 3. 命名规范确定
统一命名风格：
- `wait_for_*`
- `get_*`
- `list_*`
- `start_*`
- `stop_*`
- `clear_*`

### 4. 上下文参数规范
统一这些参数模型：
- `profile_id`
- `locator`
- `frame`
- `tab`
- `timeout_ms`
- `wait` / `auto_wait`

## 交付物
- MCP 参数规范
- MCP 错误规范
- 工具命名规范

## 验收标准
- 新增工具都遵循同一 input/output/error 模型
- 文档中明确说明调用约定

---

# Phase 1：可观测性优先（Console / Network / Traffic）

## 目标
让 AI 和开发者先“看清发生了什么”。

## 功能范围
### 1. Console 捕获
- `start_console_capture`
- `get_console_logs`
- `clear_console_logs`

### 2. Network 捕获
- `start_network_capture`
- `get_network_requests`
- `get_response_body`
- `clear_network_requests`

### 3. Traffic Stats MCP 暴露
- `get_all_traffic_snapshots`
- `get_traffic_stats_for_period`
- `clear_all_traffic_stats`

## 技术要点
- 通过 CDP 事件订阅：
  - `Runtime.consoleAPICalled`
  - `Log.entryAdded`
  - `Network.requestWillBeSent`
  - `Network.responseReceived`
  - `Network.loadingFinished`
  - `Network.loadingFailed`
- 设计 ring buffer / 截断策略
- response body 按需读取

## 风险
- body 过大导致内存增长
- requestId 拼装复杂
- 多 profile 并发缓存隔离

## 验收标准
- 可对任意运行中 profile 启动/获取/清空 console 与 network
- 请求数据具备基础字段：url/method/status/resourceType/timestamps
- 大响应不会无限制占用内存

---

# Phase 2：会话与环境资产能力（Cookies / Storage / Fingerprint / GeoIP）

## 目标
把 profile 从“可启动”升级为“可恢复、可复制、可迁移”。

## 功能范围
### 1. Cookie 管理
- `read_profile_cookies`
- `copy_profile_cookies`
- `import_cookies_from_file`
- `export_profile_cookies`
- `get_cookies`
- `set_cookies`
- `delete_cookies`

### 2. Storage 管理
- `get_local_storage`
- `set_local_storage`
- `get_session_storage`
- `clear_storage`

### 3. 指纹辅助
- `generate_sample_fingerprint`

### 4. GeoIP 数据工具
- `is_geoip_database_available`
- `download_geoip_database`

## 技术要点
- cookie 可优先复用后端已有 manager
- local/session storage 可优先走页面 JS / CDP evaluate
- 指纹生成和 profile 指纹更新形成闭环

## 验收标准
- profile 间 cookie 能复制
- 可导入/导出 cookie 文件
- localStorage/sessionStorage 可读写
- GeoIP 数据状态可查询并触发下载

---

# Phase 3：等待能力（Wait）

## 目标
降低脆弱 sleep，提高自动化稳定性。

## 功能范围
- `wait_for_selector`
- `wait_for_text`
- `wait_for_navigation`
- `wait_for_url_contains`
- `wait_for_network_idle`

## 技术要点
- polling + timeout + retry
- 明确每种等待的成功条件
- 错误输出包含：
  - waited condition
  - timeout
  - last observed state

## 验收标准
- 所有 wait 工具都有稳定 timeout 行为
- 在 SPA / 异步渲染页面上可稳定工作

---

# Phase 4：locator 体系（一）

## 目标
把找元素从“脆弱 CSS”升级到“统一可扩展的定位器模型”。

## 第一批 locator 类型
- `css`
- `text`
- `testid`
- `xpath`

## 功能范围
让以下工具支持统一 locator：
- `click_element`
- `type_text`
- `wait_for_selector`
- `get_element_text`
- `element_exists`
- `query_elements`

## 参数结构建议
```json
{
  "locator": {
    "by": "css|text|testid|xpath",
    "value": "...",
    "nth": 0,
    "exact": false
  }
}
```

## 技术要点
- 先统一 locator 解析器
- 页面内查询与 CDP 操作统一使用解析结果
- text 匹配要支持 exact / contains 语义

## 验收标准
- 所有接入工具均支持同一 locator 参数模型
- text/testid/xpath 可稳定工作

---

# Phase 5：auto-wait 内建化

## 目标
不只是“有 wait 工具”，而是让 click/type/query 自带稳定等待语义。

## 范围
### 1. 内建到交互工具
- `click_element`
- `type_text`
- `select_option`
- `check_checkbox`
- `uncheck_checkbox`

### 2. 内建到读取工具
- `get_element_text`
- `element_exists`
- `query_elements`

## 等待策略建议
### click 前
- 存在
- 可见
- 尺寸 > 0
- 未 disabled
- 可滚动进视口

### type 前
- 存在
- 可见
- 可编辑
- focus 成功

### read 前
- 存在
- 如要求 visible，则等待可见

## 验收标准
- 多数页面操作不再需要手动 sleep
- 常见异步页面操作成功率明显提高

---

# Phase 6：frame / iframe 管理

## 目标
让自动化能稳定处理 iframe、嵌入式登录框、支付组件等复杂页面结构。

## 第一阶段能力
- `list_frames`
- `get_frame_tree`
- `find_frame`

## 第二阶段能力
让以下工具支持显式 frame 参数：
- `evaluate_javascript`
- `click_element`
- `type_text`
- `get_page_content`
- `wait_for_selector`

## 设计原则
### 不优先做全局 `switch_frame`
原因：
- MCP 是离散调用模型
- 全局上下文切换容易在并发下串状态
- 不利于 agent 并发使用

### 推荐做法
每个工具支持显式 frame：
```json
{
  "frame": {
    "by": "main|index|url|name|id",
    "value": "..."
  }
}
```

## 技术难点
- cross-origin iframe
- execution context 映射
- frame tree 与 target 生命周期管理

## 验收标准
- 能列出 frame 结构
- 能在指定 frame 中执行 evaluate/click/type
- iframe 场景下 selector 不再“页面上看得见但脚本找不到”

---

# Phase 7：交互能力扩展 + Tab/Window + Upload/Download

## 目标
补齐 Playwright 风格的核心交互面。

## 功能范围
### 1. 页面交互增强
- `hover_element`
- `focus_element`
- `scroll_to`
- `scroll_element_into_view`
- `press_key`
- `press_hotkey`
- `select_option`
- `check_checkbox`
- `uncheck_checkbox`

### 2. DOM / 页面读取增强
- `get_element_text`
- `get_element_html`
- `get_element_attribute`
- `get_element_rect`
- `element_exists`
- `query_elements`
- `count_elements`

### 3. Tab / Window 管理
- `list_tabs`
- `switch_tab`
- `new_tab`
- `close_tab`
- `get_active_tab`

### 4. 文件上传下载
- `upload_file`
- `set_file_input`
- `download_file`
- `wait_for_download`

## 验收标准
- 复杂表单、跳新页、上传附件、导出文件等场景可覆盖
- 多标签页流程可稳定操作

---

# Phase 8：locator 体系（二）+ 高级可用性

## 目标
把 locator 进一步升级到更接近 Playwright 体验。

## 功能范围
### 1. 第二批 locator 类型
- `role`
- `label`
- `placeholder`

### 2. 更强组合定位
后续可考虑：
- and/or 组合
- 父子层级定位
- 多条件 narrowing

### 3. 更高级错误模型
- invalid_locator
- ambiguous_match
- cross_origin_frame_blocked
- not_interactable
- stale_node

## 技术难点
- role/label 很可能需要更深的 DOM / accessibility 语义支持
- 模糊匹配与多元素歧义处理

## 验收标准
- locator 不再只停留在“高级 CSS 替代品”
- 对 AI 更友好，能更自然表达“点登录按钮”“找邮箱输入框”

---

# Phase 9：同步 / 云 / 环境编排补全

## 目标
把非页面自动化但对大规模使用很关键的能力补全到 MCP。

## 范围
- `get_sync_settings`
- `save_sync_settings`
- `request_profile_sync`
- 后续视需要增加：
  - proxy sync 控制
  - extension sync 控制
  - e2e password 相关工具

## 验收标准
- AI 可控制同步设置和手动触发同步

---

# Phase 10：高级执行器与工作流抽象

## 目标
让系统从“离散工具集合”升级为“更适合任务编排的自动化平台”。

## 能力方向
- 任务级 action batching
- 更高阶 script / recipe 执行能力
- 结构化步骤计划与回放
- 失败重试与补偿
- 批量 profile 编排

## 说明
这阶段不一定表现为单个 MCP tool，更可能是：
- MCP 上层 orchestrator
- 模板化工作流
- AI 专用 recipe

## 当前阶段设计结论

### 1. Recipe 先走 JSON DSL，不引入独立脚本语言
- 首版不做 Lua / Python / 自定义 JS 脚本运行时。
- recipe 只负责编排现有 MCP tool，复杂页面逻辑继续交给 `evaluate_javascript` 等既有能力。
- 这样可以避免第二套运行时、权限边界和调试模型。

### 2. 单 profile recipe 与多 profile 批处理解耦
- `run_recipe` 负责单 profile 顺序执行。
- `run_batch_profile_workflow` 负责把同一 recipe 批量分发到多个 profile。
- recipe 本身不内嵌“多 profile 循环”，这样结构更稳定，也更利于错误回放。

### 3. 首版最小 step 模型
- `id`
- `tool`
- `arguments`
- `condition`
- `output`
- `on_error`
- `retry`

### 4. 首版建议优先暴露的 MCP tool
- `validate_recipe`（已实现）
- `preview_recipe`（已实现）
- `run_recipe`（已实现）
- `run_batch_profile_workflow`（已实现）

### 5. 回滚策略以“可观测”优先
- 首版不做自动推断回滚。
- 先把结构化执行记录、失败原因、重放线索做扎实。
- 后续如确有场景，再增加显式补偿步骤或 cleanup recipe。

### 6. 当前已落地的结构化执行输出
- `run_recipe` 已输出统一的 recipe 级执行摘要
- 单个 step 已输出统一记录：
  - 状态
  - 尝试次数
  - 条件判断结果
  - 结构化失败原因
  - replay 信息
- 这为下一步 `run_batch_profile_workflow` 直接复用报告模型打下基础

### 7. 当前已落地的批处理执行能力
- `run_batch_profile_workflow` 已可运行同一 recipe 到多个 profile
- 已支持：
  - 共享 `inputs`
  - `per_profile_inputs`
  - `max_concurrency`
  - `stop_on_profile_failure`
- 已输出统一 batch 级摘要与逐 profile 报告
- 逐 profile 报告直接复用 `run_recipe` 结构，便于回放、复查、二次修复

---

# Phase 11：DevTools 级可视化调试台（可选最终阶段）

## 目标
如果产品方向需要服务“人类开发者调试”，再做可视界面。

## 能力范围
### 1. Console UI
- 实时日志流
- 级别筛选
- 搜索

### 2. Network UI
- 请求列表
- waterfall 时间线
- 过滤 / 分组 / 排序
- 请求详情页
- 响应体预览

### 3. 后续可扩展
- frame tree 可视化
- tab tree
- screenshot timeline
- traffic charts

## 说明
这不是 AI 必需的第一优先级，
但对人工调试、产品展示、团队内部定位问题非常有价值。

---

# 五、跨阶段依赖关系

## 关键依赖
### locator 依赖 Phase 0 规范
没有统一参数模型，后面会返工。

### auto-wait 依赖 wait + locator
没有统一定位器和等待模型，auto-wait 很难做整齐。

### frame 依赖 locator / evaluate 基础
frame-aware 工具需要统一 context 传递设计。

### DevTools UI 依赖 console/network 数据层
UI 必须建立在结构化采集数据之上。

---

# 六、推荐优先级排序

## P0（最值得优先做）
1. Console / Network capture
2. Cookies / Storage 基础
3. Traffic Stats MCP 暴露
4. wait 系列

## P1（稳定性核心）
5. locator 第一批
6. auto-wait 内建
7. frame 第一阶段

## P2（复杂页面能力）
8. tab/window
9. 文件上传下载
10. 交互增强
11. DOM 读取增强

## P3（更接近 Playwright）
12. locator 第二批
13. 高级错误模型
14. 高级 orchestration

## P4（产品化可视体验）
15. DevTools 风格 UI

---

# 七、每阶段建议交付形式

## 文档交付
每一阶段都建议补：
- tool schema 文档
- 示例调用
- 错误码说明

## 测试交付
每一阶段都建议补：
- 单元测试
- 集成测试
- MCP 端到端测试（如适合）

## 演示交付
每一阶段建议准备：
- 最小 demo 场景
- 回归验证清单

---

# 八、验收维度

所有阶段建议统一从这几个维度验收：

## 1. 功能正确性
工具是否真能完成预期操作。

## 2. 稳定性
在动态页面 / 异步页面 / 多 profile 下是否稳定。

## 3. AI 友好性
参数是否易懂，错误是否清晰，是否利于 agent 使用。

## 4. 性能与资源控制
console/network/log/body 缓存是否可控。

## 5. 可维护性
实现是否复用统一抽象，而不是散乱堆接口。

---

# 九、长期架构建议

## 1. 抽出统一 Locator Resolver
建议后续独立成模块，不要散在各个 tool handler 里。

## 2. 抽出统一 Wait Engine
将 wait / auto-wait 逻辑集中，避免每个工具自己写一套。

## 3. 抽出 Target / Tab / Frame / Context 管理层
后期复杂度一定会上升，越早抽象越省事。

## 4. 抽出 Capture Manager
统一管理：
- console capture
- network capture
- traffic stats

---

# 十、最终结论

**这个项目完全有条件，基于现有 MCP + CDP 底座，逐步演进成一个强大的浏览器自动化平台。**

最现实的路线不是“一次性做成 Playwright 替代品”，而是：

1. 先补可观测性
2. 再补稳定性（wait / locator / frame）
3. 再补复杂页面能力（tabs / upload / storage）
4. 再补更高级抽象和可视调试台

如果按这个计划推进，后期每一阶段都能交付实际可用价值，同时不会把架构拖乱。
