# MCP 可执行任务清单

> 说明：
>
> 本清单由 `/Users/lijinmu/Documents/New project/donutbrowser/MCP_全阶段开发计划书.md` 拆分而来。
>
> 目标不是做抽象规划，而是提供一份可以逐项推进、逐项验收、逐项打勾的执行清单。

---

# 一、执行规则

## 1. 任务状态建议
每个任务建议维护以下状态：
- `todo`
- `doing`
- `blocked`
- `done`

## 2. 执行顺序规则
- 先做基础规范与抽象
- 再做可观测性
- 再做稳定性（wait / locator / frame）
- 再做复杂页面能力（tabs / upload / storage）
- 最后再做可视调试与高级编排

## 3. 每个任务最少要有的交付物
- 代码实现
- 对应测试
- 最小调用示例
- 文档更新

---

# 二、Phase 0：基线治理与统一规范

## P0-01 统一 MCP tool 输入输出规范
**目标**
统一现有与未来 MCP 工具的输入/输出结构。

**子任务**
- [x] 梳理现有所有 MCP tool 的输入结构
- [x] 设计统一 JSON 参数约定
- [x] 设计统一响应格式约定
- [x] 明确 text/json/base64 返回策略
- [x] 写入文档

**依赖**
- 无

**验收**
- 新工具不再随意定义输入结构
- 文档中明确调用契约

---

## P0-02 统一错误模型
**目标**
统一 MCP 工具的错误码与错误语义。

**子任务**
- [x] 定义错误分类枚举
- [x] 统一 timeout / not found / invalid locator / frame not found 等错误格式
- [x] 统一错误 message 风格
- [x] 将现有关键工具的错误逐步对齐

**建议错误类型**
- `timeout`
- `invalid_locator`
- `element_not_found`
- `element_not_visible`
- `element_not_interactable`
- `frame_not_found`
- `tab_not_found`
- `navigation_failed`
- `capture_not_started`

**验收**
- 文档中有错误语义说明
- 新增 MCP 工具不再随意返回模糊错误

---

## P0-03 统一核心参数模型
**目标**
为后续所有自动化工具定义统一参数模型。

**子任务**
- [x] 定义 `profile_id` 使用规范
- [x] 定义 `locator` 结构
- [x] 定义 `frame` 结构
- [x] 定义 `tab` 结构
- [x] 定义 `timeout_ms` / `auto_wait` 等公共参数
- [x] 写入文档

**当前实现约定（已落地）**
- MCP 参数以 `snake_case` 为主，兼容 `camelCase` 别名
- 结构化结果统一走 `content[0].type = "text"`，文本内容为 pretty JSON
- 二进制仍按 MCP 内容类型返回，例如截图继续使用 `image`
- 共享公共模型已在 `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/mcp_server.rs` 中建立：
  - `McpCommonOptions`
  - `McpLocator`
  - `McpFrameRef`
  - `McpTabRef`
- 共享错误构造已建立：
  - `invalid_params`
  - `method_not_found`
  - `not_running`
  - `internal`
  - `not_found`

**验收**
- locator/frame/tab 参数在文档中有统一 schema

---

## P0-04 抽象层预留
**目标**
为后续能力扩展准备模块边界。

**子任务**
- [ ] 规划 `locator resolver` 模块位置
- [ ] 规划 `wait engine` 模块位置
- [ ] 规划 `capture manager` 模块位置
- [ ] 规划 `context manager`（tab/frame/context）模块位置

**验收**
- 文档中明确后续模块归属

---

# 三、Phase 1：可观测性（Console / Network / Traffic）

## P1-01 Console capture 基础能力
**目标**
实现 console 捕获的底层能力。

**子任务**
- [x] 设计 console capture 内存状态结构
- [x] 建立 profile 级 capture 生命周期管理
- [x] 通过 CDP 订阅 `Runtime.consoleAPICalled`
- [x] 通过 CDP 订阅 `Log.entryAdded`
- [x] 设计 ring buffer / 最近 N 条缓存策略

**验收**
- 单个运行中 profile 能稳定记录 console 输出

---

## P1-02 Console MCP 工具暴露
**目标**
暴露 console 相关 MCP 工具。

**子任务**
- [x] 新增 `start_console_capture`
- [x] 新增 `get_console_logs`
- [x] 新增 `clear_console_logs`
- [x] 补 tool schema
- [x] 补最小示例
- [x] 补测试

**最小示例（已可用）**
```json
{
  "name": "start_console_capture",
  "arguments": {
    "profile_id": "PROFILE_UUID"
  }
}
```

```json
{
  "name": "get_console_logs",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "limit": 50
  }
}
```

```json
{
  "name": "clear_console_logs",
  "arguments": {
    "profile_id": "PROFILE_UUID"
  }
}
```

**依赖**
- P1-01

**验收**
- MCP 可对运行中 profile 启动/获取/清空 console 日志

---

## P1-03 Network capture 基础能力
**目标**
实现 network 请求捕获基础设施。

**子任务**
- [x] 设计 request/response 聚合结构
- [x] 处理 `requestId` 维度拼装
- [x] 订阅 `Network.requestWillBeSent`
- [x] 订阅 `Network.responseReceived`
- [x] 订阅 `Network.loadingFinished`
- [x] 订阅 `Network.loadingFailed`
- [x] 设计最近 N 条缓存策略

**验收**
- 可以稳定记录请求列表与响应摘要

---

## P1-04 Response body 按需读取
**目标**
支持按 requestId 获取 response body。

**子任务**
- [x] 设计 `get_response_body` 参数模型
- [x] 调用 CDP body 获取接口
- [x] 处理二进制/文本返回
- [x] 限制大 body 读取策略

**依赖**
- P1-03

**验收**
- 可按需获取响应体，不导致内存无限增长

---

## P1-05 Network MCP 工具暴露
**目标**
暴露 network 相关 MCP 工具。

**子任务**
- [x] 新增 `start_network_capture`
- [x] 新增 `get_network_requests`
- [x] 新增 `get_response_body`
- [x] 新增 `clear_network_requests`
- [x] 补 tool schema
- [x] 补示例
- [x] 补测试

**最小示例（已可用）**
```json
{
  "name": "start_network_capture",
  "arguments": {
    "profile_id": "PROFILE_UUID"
  }
}
```

```json
{
  "name": "get_network_requests",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "limit": 100
  }
}
```

```json
{
  "name": "get_response_body",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "request_id": "REQUEST_ID",
    "max_bytes": 262144
  }
}
```

```json
{
  "name": "clear_network_requests",
  "arguments": {
    "profile_id": "PROFILE_UUID"
  }
}
```

**依赖**
- P1-03
- P1-04

**验收**
- MCP 可获取 profile 的请求列表与 body

---

## P1-06 Traffic stats MCP 暴露
**目标**
把已有流量统计能力接入 MCP。

**子任务**
- [x] 暴露 `get_all_traffic_snapshots`
- [x] 暴露 `get_traffic_stats_for_period`
- [x] 暴露 `clear_all_traffic_stats`
- [x] 统一返回格式
- [x] 补示例
- [x] 补测试

**最小示例（已可用）**
```json
{
  "name": "get_all_traffic_snapshots",
  "arguments": {}
}
```

```json
{
  "name": "get_traffic_stats_for_period",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "seconds": 3600
  }
}
```

```json
{
  "name": "clear_all_traffic_stats",
  "arguments": {}
}
```

**依赖**
- 无（已有后端能力）

**验收**
- AI 可直接查看 profile 流量快照、时间段流量统计

---

# 四、Phase 2：会话与环境资产（Cookies / Storage / Fingerprint / GeoIP）

## P2-01 Cookie MCP 接入
**子任务**
- [x] 暴露 `read_profile_cookies`
- [x] 暴露 `copy_profile_cookies`
- [x] 暴露 `import_cookies_from_file`
- [x] 暴露 `export_profile_cookies`
- [x] 统一返回格式
- [x] 补示例与测试

**最小示例（已可用）**
```json
{
  "name": "read_profile_cookies",
  "arguments": {
    "profile_id": "PROFILE_UUID"
  }
}
```

```json
{
  "name": "copy_profile_cookies",
  "arguments": {
    "source_profile_id": "SOURCE_PROFILE_UUID",
    "target_profile_ids": ["TARGET_PROFILE_UUID"],
    "selected_cookies": [
      { "domain": ".example.com", "name": "session_id" }
    ]
  }
}
```

```json
{
  "name": "import_cookies_from_file",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "content": "# Netscape HTTP Cookie File\n.example.com\tTRUE\t/\tTRUE\t1700000000\tsession_id\tabc123"
  }
}
```

```json
{
  "name": "export_profile_cookies",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "format": "json"
  }
}
```

**验收**
- profile 间 cookie 可复制
- cookie 可导入导出

---

## P2-02 Storage MCP 接入
**子任务**
- [x] 实现 `get_local_storage`
- [x] 实现 `set_local_storage`
- [x] 实现 `get_session_storage`
- [x] 实现 `clear_storage`
- [x] 处理 JSON/字符串序列化策略
- [x] 补测试

**最小示例（已可用）**
```json
{
  "name": "get_local_storage",
  "arguments": {
    "profile_id": "PROFILE_UUID"
  }
}
```

```json
{
  "name": "set_local_storage",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "key": "app.settings",
    "value": {
      "theme": "dark",
      "remember": true
    }
  }
}
```

```json
{
  "name": "get_session_storage",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "key": "csrf_token"
  }
}
```

```json
{
  "name": "clear_storage",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "storage": "both"
  }
}
```

**验收**
- localStorage/sessionStorage 可读写清理

---

## P2-03 Fingerprint helper MCP 接入
**子任务**
- [x] 暴露 `generate_sample_fingerprint`
- [x] 设计与 `update_profile_fingerprint` 的联动示例
- [x] 补测试

**最小示例（已可用）**
```json
{
  "name": "generate_sample_fingerprint",
  "arguments": {
    "browser": "wayfern",
    "version": "136.0.0.0",
    "config": {
      "os": "windows"
    }
  }
}
```

**联动示例（已可用）**
1. 先调用 `generate_sample_fingerprint`
2. 取返回值里的 `fingerprint`
3. 再调用：
```json
{
  "name": "update_profile_fingerprint",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "fingerprint": "GENERATED_FINGERPRINT_JSON_STRING"
  }
}
```

**验收**
- AI 可先生成样本指纹，再写回 profile

---

## P2-04 GeoIP MCP 接入
**子任务**
- [x] 暴露 `is_geoip_database_available`
- [x] 暴露 `download_geoip_database`
- [x] 补工具说明
- [x] 补测试

**最小示例（已可用）**
```json
{
  "name": "is_geoip_database_available",
  "arguments": {}
}
```

```json
{
  "name": "download_geoip_database",
  "arguments": {}
}
```

**工具说明**
- `is_geoip_database_available`：查询 Camoufox 所需 GeoIP 数据库是否已存在
- `download_geoip_database`：触发 GeoIP 数据库下载/刷新
- 下载会复用现有后端下载逻辑与进度事件通道

**验收**
- AI 可查询 GeoIP 状态并触发下载

---

# 五、Phase 3：等待能力（Wait）

## P3-01 wait engine 抽象
**目标**
抽出统一等待引擎，避免每个 tool 自己写一套。

**子任务**
- [x] 设计 wait engine 接口
- [x] 支持 polling + timeout
- [x] 支持 condition callback
- [x] 支持结构化超时错误

**当前实现约定（已落地）**
- 统一等待入口为后端 `wait_with_timeout`
- 使用固定轮询间隔 + 总超时
- condition callback 返回 `Option<T>`
  - `Some(T)`：等待成功
  - `None`：继续轮询
- 超时统一返回结构化 timeout 错误
  - 错误码：`-32008`

**验收**
- 后续 wait 系列工具统一复用该引擎

---

## P3-02 wait_for_selector
**子任务**
- [x] 支持 locator 参数
- [x] 支持 timeout
- [x] 支持 visible/exists 条件
- [x] 补测试

**最小示例（已可用）**
```json
{
  "name": "wait_for_selector",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "css",
      "value": "#login-button"
    },
    "state": "visible",
    "timeout_ms": 10000
  }
}
```

```json
{
  "name": "wait_for_selector",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "text",
      "value": "Welcome back",
      "exact": false
    },
    "state": "exists"
  }
}
```

---

## P3-03 wait_for_text
**子任务**
- [x] 支持页面/selector 范围等待文本
- [x] 支持 exact/contains
- [x] 补测试

**最小示例（已可用）**
```json
{
  "name": "wait_for_text",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "text": "Welcome back",
    "timeout_ms": 10000
  }
}
```

```json
{
  "name": "wait_for_text",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "text": "Submit",
    "selector": "#checkout-form",
    "exact": false
  }
}
```

**当前实现约定（已落地）**
- 支持整页文本等待，也支持通过 `selector` 或 `locator` 缩小范围
- `selector` 与 `locator` 互斥；`locator` 复用 `css|text|testid|xpath`
- 默认 `exact = false`，即 contains 匹配
- 超时错误会带上最近一次观测到的文本摘要

**验收**
- AI 可稳定等待整页文本或局部文本出现

---

## P3-04 wait_for_navigation
**子任务**
- [x] 设计页面跳转判定规则
- [x] 支持超时
- [x] 补测试

**最小示例（已可用）**
```json
{
  "name": "wait_for_navigation",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "timeout_ms": 15000
  }
}
```

```json
{
  "name": "wait_for_navigation",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "from_url": "https://example.com/login",
    "timeout_ms": 15000
  }
}
```

**当前实现约定（已落地）**
- 先抓取初始页面快照：`url/title/readyState`
- 成功条件：
  - 当前 URL 相比基线 URL 发生变化，且 `readyState = complete`
  - 或等待期间曾观测到非 `complete`，随后回到 `complete`（覆盖同 URL reload）
- `from_url` 可显式指定基线 URL；不传时默认使用开始等待时的当前 URL
- 超时错误会带上最近一次页面快照

**验收**
- AI 可稳定等待跳转完成或页面 reload 收口

---

## P3-05 wait_for_url_contains
**子任务**
- [ ] 读取当前 URL
- [ ] 轮询匹配规则
- [ ] 补测试

---

## P3-06 wait_for_network_idle
**子任务**
- [ ] 基于 network capture 或 CDP 网络事件做空闲判断
- [ ] 支持 quiet period 参数
- [ ] 补测试

---

# 六、Phase 4：locator 体系（第一批）

## P4-01 locator schema 定稿
**子任务**
- [x] 定义 `css|text|testid|xpath`
- [x] 定义 `nth`
- [x] 定义 `exact`
- [x] 文档落地

**当前实现约定（已落地）**
- 统一 locator 结构：
```json
{
  "by": "css|text|testid|xpath",
  "value": "locator value",
  "nth": 0,
  "exact": false
}
```
- `value` 不允许为空或纯空白
- `nth` 为从 `0` 开始的匹配索引；默认 `0`
- `exact` 当前主要用于 `text` locator；未传时默认 `false`
- MCP schema 已统一复用到：
  - `wait_for_selector`
  - `wait_for_text`

**验收**
- 所有已接入工具都使用同一套 locator 参数定义

---

## P4-02 locator resolver 实现
**子任务**
- [x] 解析 CSS locator
- [x] 解析 text locator
- [x] 解析 testid locator
- [x] 解析 xpath locator
- [x] 输出统一查询结果结构

**当前实现约定（已落地）**
- 后端已抽出共享 locator resolver 脚本：
  - 位置：`/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/mcp_server.rs`
  - 入口：`resolveLocatorSnapshot(locator, rootNode?)`
- resolver 统一输出：
  - `locator`
  - `strategy`
  - `index`
  - `count`
  - `exists`
  - `visible`
  - `text`
  - `matchedTexts`
- 现阶段已复用到：
  - `wait_for_selector`
  - `wait_for_text`（locator scope）
- 非法 locator 会在真正访问 profile/CDP 前直接返回参数错误

**验收**
- locator 解析逻辑不再散落在各个工具里，已有 wait 工具共享同一 resolver

---

## P4-03 click_element 接 locator
**子任务**
- [x] `click_element` 改为支持 locator 参数
- [x] 保留兼容老 selector 参数或设计迁移方案
- [x] 补测试

**最小示例（已可用）**
```json
{
  "name": "click_element",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "text",
      "value": "Sign in",
      "exact": true
    }
  }
}
```

```json
{
  "name": "click_element",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "selector": "#legacy-submit-button"
  }
}
```

**当前实现约定（已落地）**
- 优先支持统一 `locator`
- 继续兼容旧 `selector`
- `selector` 与 `locator` 互斥
- click 返回结构化 target 信息，不再只返回纯文本
- 如果 click 触发跳转，仍会等待页面 load 收口

**验收**
- AI 可用 locator 或旧 selector 点击元素，且兼容现有链路

---

## P4-04 type_text 接 locator
**子任务**
- [x] `type_text` 改为支持 locator 参数
- [x] 补测试

**最小示例（已可用）**
```json
{
  "name": "type_text",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "testid",
      "value": "email-input"
    },
    "text": "user@example.com",
    "clear_first": true
  }
}
```

```json
{
  "name": "type_text",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "selector": "#legacy-search",
    "text": "hello world",
    "instant": false
  }
}
```

**当前实现约定（已落地）**
- 优先支持统一 `locator`
- 继续兼容旧 `selector`
- `selector` 与 `locator` 互斥
- 聚焦/清空阶段已经走统一 target resolver
- 输入完成后返回结构化 target 与输入摘要

**验收**
- AI 可用 locator 或旧 selector 给输入框稳定输入文本

---

## P4-05 DOM 读取工具第一批
**子任务**
- [x] 实现 `element_exists`
- [x] 实现 `get_element_text`
- [x] 实现 `query_elements`
- [x] 实现 `count_elements`
- [x] 补测试

**最小示例（已可用）**
```json
{
  "name": "element_exists",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "css",
      "value": "#login-form"
    }
  }
}
```

```json
{
  "name": "get_element_text",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "text",
      "value": "Welcome back",
      "exact": true
    }
  }
}
```

```json
{
  "name": "query_elements",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "css",
      "value": ".result-card"
    },
    "limit": 10
  }
}
```

```json
{
  "name": "count_elements",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "testid",
      "value": "menu-item"
    }
  }
}
```

**当前实现约定（已落地）**
- 四个读取类工具均统一使用 `locator`
- `element_exists` / `get_element_text` 复用统一 locator snapshot
- `query_elements` 返回结构化元素摘要：
  - `tagName`
  - `id`
  - `className`
  - `text`
  - `visible`
  - `rect`
  - 常见 attributes
- `query_elements.limit` 默认 `20`，最大 `100`
- `count_elements` 返回纯计数与 `exists`

**验收**
- AI 可基于统一 locator 做存在判断、取文本、列元素、计元素

---

# 七、Phase 5：auto-wait 内建化

## P5-01 click auto-wait
**子任务**
- [x] 点击前等待元素存在
- [x] 等待元素可见
- [x] 等待尺寸 > 0
- [x] 等待未 disabled
- [x] 自动滚动进视口
- [x] 失败时返回结构化错误

**最小示例（已可用）**
```json
{
  "name": "click_element",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "css",
      "value": "#submit"
    },
    "timeout_ms": 12000
  }
}
```

**当前实现约定（已落地）**
- `click_element` 默认内建 auto-wait
- 等待条件：
  - exists
  - visible
  - hasSize
  - enabled（未 disabled）
- 等待阶段会自动 `scrollIntoView`
- 成功结果会返回 `autoWait` 快照
- 超时错误会带最近一次观测状态，便于 AI 判断失败原因

**验收**
- AI 调用 click 不必手动 sleep，也能拿到结构化等待状态

---

## P5-02 type auto-wait
**子任务**
- [x] 输入前等待元素存在
- [x] 等待可见
- [x] 等待可编辑
- [x] 尝试 focus
- [x] 失败时返回结构化错误

**最小示例（已可用）**
```json
{
  "name": "type_text",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "testid",
      "value": "password-input"
    },
    "text": "secret",
    "timeout_ms": 12000
  }
}
```

**当前实现约定（已落地）**
- `type_text` 默认内建 auto-wait
- 等待条件：
  - exists
  - visible
  - hasSize
  - enabled
  - editable
  - focused
- 等待阶段会自动尝试 `focus`
- 成功结果会返回 `autoWait` 快照
- 超时错误会带最近一次观测状态

**验收**
- AI 调用 type 不必手动 sleep，也能拿到结构化等待状态

---

## P5-03 读取类 auto-wait
**子任务**
- [x] 为 `get_element_text`
- [x] `element_exists`
- [x] `query_elements`
  增加基础等待策略

**当前实现约定（已落地）**
- `element_exists`：默认内建 auto-wait；若超时仍未出现，不抛错，返回 `exists: false` 与最后一次观测状态
- `get_element_text`：默认内建 auto-wait；若超时仍未出现，返回 timeout 错误，并附最近观测状态
- `query_elements`：默认内建 auto-wait；若超时仍无匹配，不抛错，返回空结果与最后一次观测状态
- 三个工具都新增 `timeout_ms` 参数，默认 `10000`
- 三个工具返回结构化 `autoWait` 信息，便于 AI 判断是否命中等待、是否超时、最后状态是什么

---

## P5-04 auto-wait 文档化
**子任务**
- [x] 明确哪些工具默认 auto-wait
- [x] 明确超时和禁用方式
- [x] 补调用示例

**当前实现约定（已落地）**
- 当前默认内建 auto-wait 的工具：
  - `click_element`
  - `type_text`
  - `element_exists`
  - `get_element_text`
  - `query_elements`
- 所有上述工具都支持：
  - `timeout_ms`：等待超时，默认 `10000`
  - `auto_wait`：是否启用内建等待，默认 `true`
- 当 `auto_wait: false` 时：
  - 工具会直接执行一次，不做轮询等待
  - `timeout_ms` 不再产生实际等待效果
- 返回约定：
  - `click_element` / `type_text` 成功结果包含 `autoWait`
  - `element_exists` / `get_element_text` / `query_elements` 结果也包含结构化 `autoWait`
  - 读取类工具在超时场景下会保留最后一次观测状态，便于 AI 判断页面是否只是“还没出来”

**调用示例**
```json
{
  "name": "click_element",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "text",
      "value": "Login",
      "exact": true
    },
    "timeout_ms": 12000
  }
}
```

```json
{
  "name": "query_elements",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "css",
      "value": ".result-item"
    },
    "limit": 10,
    "auto_wait": false
  }
}
```

```json
{
  "name": "get_element_text",
  "arguments": {
    "profile_id": "PROFILE_UUID",
    "locator": {
      "by": "testid",
      "value": "status-badge"
    },
    "auto_wait": true,
    "timeout_ms": 5000
  }
}
```

---

# 八、Phase 6：frame / iframe 管理

## P6-01 frame 数据模型设计
**子任务**
- [x] 设计 `frame` 参数模型
- [x] 设计 frame tree 数据结构
- [x] 确定 `main|index|url|name|id` 识别方案

**当前实现约定（已落地）**
- `frame` 选择器采用统一结构：
```json
{
  "by": "main|index|url|name|id",
  "value": "..."
}
```
- 其中：
  - `by=main`：不允许传 `value`
  - `by=index`：`value` 必须是整数
  - `by=url|name|id`：`value` 必须是非空字符串
- `index` 约定为：
  - 基于 frame tree 展平后的 **0-based** 顺序编号
  - 主 frame 固定为 `index=0`
- 已落地的数据结构：
  - `McpFrameRef`
  - `McpFrameNode`
  - `McpFrameTreeNode`
- 已补基础单测，保证：
  - frame selector 解析正确
  - 非法 `main/index/url/name/id` 值形态会被拦截
  - schema 明确支持 `main|index|url|name|id`

---

## P6-02 list_frames / get_frame_tree
**子任务**
- [x] 实现 `list_frames`
- [x] 实现 `get_frame_tree`
- [x] 补测试

**当前实现约定（已落地）**
- `list_frames`
  - 返回展平后的 frame 列表
  - 遍历顺序为 **preorder**
  - 主 frame 固定 `index=0`
  - 每项包含：
    - `id`
    - `parentId`
    - `index`
    - `depth`
    - `url`
    - `name`
    - `origin`
    - `main`
    - `childCount`
- `get_frame_tree`
  - 返回嵌套树结构
  - 顶层字段为 `root`
  - 同时返回 `total`
- 数据来源：
  - 统一使用 CDP `Page.getFrameTree`
- 已补测试覆盖：
  - CDP frame tree 解析
  - preorder index 生成
  - 扁平化结果与 `childCount`
  - 缺失根 frame 的错误处理

---

## P6-03 find_frame 能力
**子任务**
- [x] 按 url/name/id/index 查找 frame
- [x] 补测试

**当前实现约定（已落地）**
- 新增工具：`find_frame`
- 支持的查找方式：
  - `by=main`：返回主 frame
  - `by=index`：按 `index` 精确查找
  - `by=id`：按 `id` 精确查找
  - `by=name`：按 `name` 精确查找
  - `by=url`：按 `url contains(value)` 查找
- 结果约定：
  - 成功时返回 `resolvedFrame`
  - 未命中时返回 `not_found`
  - 命中多个 frame 时返回 `invalid_params`
  - 歧义错误会附前几个候选 frame，方便 AI 继续收窄选择器
- 已补测试覆盖：
  - `FindFrameArgs` 解析
  - `main/index/id/name/url` 五种查找
  - 歧义命中错误
  - 未命中错误

---

## P6-04 evaluate_javascript 支持 frame
**子任务**
- [x] 在指定 frame context 中执行 JS
- [x] 处理 execution context 映射
- [x] 补测试

**当前实现约定（已落地）**
- `evaluate_javascript` 新增可选参数：
```json
{
  "frame": {
    "by": "main|index|url|name|id",
    "value": "..."
  }
}
```
- 当传入 `frame` 时：
  1. 先基于 `Page.getFrameTree` 解析当前 frame tree
  2. 用 `find_frame` 同一套 selector 语义解析目标 frame
  3. 调用 `Page.createIsolatedWorld`
  4. 取回 `executionContextId`
  5. 将 `Runtime.evaluate` 绑定到该 `contextId`
- 返回结果新增：
  - `frame`
  - `resolvedFrame`
  - `awaitPromise`
  - `waitForLoad`
  - `result`
- 已补测试覆盖：
  - `EvaluateJavascriptArgs` 解析
  - schema 暴露 `frame`
  - `executionContextId` 解析
  - 非法 frame 在 profile lookup 前拦截

---

## P6-05 click/type/content/wait 支持 frame
**子任务**
- [x] `click_element` 接 frame
- [x] `type_text` 接 frame
- [x] `get_page_content` 接 frame
- [x] `wait_for_selector` 接 frame
- [x] 补测试

**当前实现约定（已落地）**
- 以下工具现已支持可选 `frame` 参数：
  - `click_element`
  - `type_text`
  - `get_page_content`
  - `wait_for_selector`
- 统一执行链路：
  1. `Page.getFrameTree`
  2. 解析 frame selector
  3. `Page.createIsolatedWorld`
  4. 取 `executionContextId`
  5. 在该 `contextId` 中执行 `Runtime.evaluate`
- 返回约定：
  - `click_element` / `type_text` 结果新增：
    - `frame`
    - `resolvedFrame`
  - `get_page_content` 结果新增：
    - `frame`
    - `resolvedFrame`
    - `format`
    - `selector`
    - `content`
  - `wait_for_selector` 结果新增：
    - `frame`
    - `resolvedFrame`
- 已补测试覆盖：
  - 四个工具的 schema 均暴露 `frame`
  - 非法 frame 在 profile lookup 前拦截

---

## P6-06 cross-origin iframe 风险验证
**子任务**
- [x] 做最小 cross-origin iframe 场景测试
- [x] 明确哪些能力可做，哪些受限
- [x] 文档写明限制

**当前验证结论（已落地）**
- 已补最小 cross-origin iframe 场景单测：
  - 主页面：`http://127.0.0.1:4010`
  - 子 iframe：`http://127.0.0.1:4011`
  - 验证点：
    - frame tree 会保留不同 `origin`
    - cross-origin 子 frame 仍可被 `name` / `url` 定向选中
- 当前**可做**：
  - `list_frames`
  - `get_frame_tree`
  - `find_frame`
  - `evaluate_javascript(frame=...)`
  - `click_element(frame=...)`
  - `type_text(frame=...)`
  - `get_page_content(frame=...)`
  - `wait_for_selector(frame=...)`
- 当前**受限/注意事项**：
  - 需要**显式指定目标 frame**；不要假设主 frame 可以直接操作 cross-origin 子 iframe 内部 DOM
  - 在某个 frame 的 JS context 中执行时，若脚本主动访问 `window.parent.document` / 兄弟 frame DOM，仍可能触发浏览器同源限制
  - 若 iframe 自身是更严格的受限环境（如脚本不可执行、页面未成功加载、目标 frame 生命周期已变化），相关工具仍可能失败
  - 目前 `wait_for_navigation` / 其他尚未接 frame 的工具，**还不能**直接声明支持 cross-origin iframe 目标
- 推荐用法：
  - 先 `list_frames` / `find_frame`
  - 再把 `frame` 显式传给后续动作工具

---

# 九、Phase 7：交互增强 + Tab/Window + Upload/Download

## P7-01 交互增强工具
**子任务**
- [x] `hover_element`
- [x] `focus_element`
- [x] `scroll_to`
- [x] `scroll_element_into_view`
- [x] `press_key`
- [x] `press_hotkey`
- [x] `select_option`
- [x] `check_checkbox`
- [x] `uncheck_checkbox`
- [x] 补测试

**当前实现约定（已落地）**
- 新增 9 个交互工具：
  - `hover_element`
  - `focus_element`
  - `scroll_to`
  - `scroll_element_into_view`
  - `press_key`
  - `press_hotkey`
  - `select_option`
  - `check_checkbox`
  - `uncheck_checkbox`
- 其中：
  - `hover_element` / `focus_element` / `scroll_element_into_view` / `select_option` / `check_checkbox` / `uncheck_checkbox`
    - 支持 `selector|locator`
    - 支持 `frame`
    - 支持基础 `auto_wait`
  - `scroll_to`
    - 支持 `frame`
    - 支持 `x/y/behavior`
  - `press_key` / `press_hotkey`
    - 支持 `frame`
    - 如指定 frame，会先聚焦该 frame 再发键盘事件
- `select_option` 当前支持三种选择方式：
  - `value`
  - `label`
  - `index`
  - 且三者必须**恰好提供一个**
- `check_checkbox` / `uncheck_checkbox` 当前约定：
  - 仅面向 checkbox / radio 输入元素
  - `uncheck_checkbox` 对 radio 会直接报错
- 已补测试覆盖：
  - 新工具已注册到 MCP tool list
  - frame-enabled schema 暴露 `frame`
  - 非法 frame / 空 key / 非法 scroll behavior / 非法 select 参数在 profile lookup 前拦截

---

## P7-02 DOM 读取增强工具
**子任务**
- [x] `get_element_html`
- [x] `get_element_attribute`
- [x] `get_element_rect`
- [x] 补测试

**当前实现约定（已落地）**
- 新增 3 个 DOM 读取工具：
  - `get_element_html`
  - `get_element_attribute`
  - `get_element_rect`
- 三个工具统一支持：
  - `locator`
  - `frame`
  - `auto_wait`
  - `timeout_ms`
- `get_element_html`
  - 返回目标元素 `outerHTML`
- `get_element_attribute`
  - 需要传 `name`
  - 返回指定属性值（不存在则为 `null`）
- `get_element_rect`
  - 返回：
    - `x`
    - `y`
    - `width`
    - `height`
    - `top`
    - `right`
    - `bottom`
    - `left`
- 三个工具都复用：
  - frame context 解析
  - 读取类 auto-wait
  - locator snapshot 预校验
- 已补测试覆盖：
  - 新工具注册到 MCP tool list
  - schema 暴露 `frame`
  - blank locator / blank attribute name / invalid frame 的前置拦截

---

## P7-03 Tab / Window 管理
**子任务**
- [x] `list_tabs`
- [x] `switch_tab`
- [x] `new_tab`
- [x] `close_tab`
- [x] `get_active_tab`
- [x] 设计 tab 标识模型
- [x] 补测试

---

## P7-04 文件上传下载
**子任务**
- [x] `set_file_input`
- [x] `upload_file`
- [x] `download_file`
- [x] `wait_for_download`
- [x] 设计下载文件保存策略
- [x] 补测试

---

# 十、Phase 8：locator 体系（第二批）

## P8-01 role locator
- [x] 设计 role 查询策略
- [x] 实现 role locator
- [x] 补测试

**当前实现约定（已落地）**
- locator 新增 `by=role`
- role locator 结构支持：
  - `value`：role 名称，如 `button` / `textbox` / `link`
  - `name`：可选 accessible name 过滤
  - `exact`：当传 `name` 时，控制精确匹配还是包含匹配
- 当前 role 解析优先级：
  - 先读显式 `role`
  - 再做一批常见隐式 role 映射（如 `button`、`a[href]`、`input[type=checkbox]`、`textarea`、`select` 等）
- accessible name 当前支持：
  - `aria-label`
  - `aria-labelledby`
  - `<label for>` / `element.labels`
  - 按钮 value / 图片 alt / title / placeholder / innerText 回退
- 已补测试覆盖：
  - schema 暴露 `role`
  - schema 暴露 `name`
  - role locator 表达式序列化
  - blank role name 前置拦截

## P8-02 label locator
- [x] 设计 label 到 input 的映射策略
- [x] 实现 label locator
- [x] 补测试

**当前实现约定（已落地）**
- locator 新增 `by=label`
- label locator 使用：
  - `value`：label 文本
  - `exact`：控制精确匹配或包含匹配
- 当前 label → control 映射优先支持：
  - `aria-label`
  - `aria-labelledby`
  - `<label for=...>`
  - `element.labels`
- 当前返回目标是对应的 form control，而不是 `<label>` 元素本身
- 当前枚举的控件范围：
  - `input`
  - `textarea`
  - `select`
  - `button`
  - `output`
  - `meter`
  - `progress`
- 已补测试覆盖：
  - schema 暴露 `label`
  - label locator 表达式序列化
  - 解析与校验回归

## P8-03 placeholder locator
- [x] 实现 placeholder locator
- [x] 补测试

**当前实现约定（已落地）**
- locator 新增 `by=placeholder`
- placeholder locator 使用：
  - `value`：placeholder 文本
  - `exact`：控制精确匹配或包含匹配
- 当前匹配范围：
  - `input[placeholder]`
  - `textarea[placeholder]`
- 已补测试覆盖：
  - schema 暴露 `placeholder`
  - placeholder locator 表达式序列化
  - 解析与校验回归

## P8-04 locator 歧义与组合策略
- [x] 处理多元素匹配歧义
- [x] 明确 nth/exact 行为
- [x] 视情况设计后续组合 locator 扩展

**当前实现约定（已落地）**
- 单目标工具在 `locator.nth` 未传且匹配数 `> 1` 时，会直接报歧义错误，而不是默认偷偷取第一个
- 当前已接入单目标歧义拦截的典型场景：
  - `wait_for_selector`
  - `get_element_text`
  - `get_element_html`
  - `get_element_attribute`
  - `get_element_rect`
  - 以及基于 `build_element_target_expression` / `build_element_actionability_expression` 的交互工具
- `query_elements` / `count_elements` / `element_exists` 这类“集合/统计类”工具保留多匹配能力，不因多元素而报错
- `nth` 行为现已明确：
  - `nth` 未传：默认索引仍是 `0`，但对于单目标工具，如果出现多匹配会报 `ambiguous` 风格错误
  - `nth` 已传：允许显式选择第 N 个匹配目标
- `exact` 行为现已明确：
  - 作用于 `text`
  - 作用于 `role + name`
  - 作用于 `label`
  - 作用于 `placeholder`
- 组合 locator 暂不引入新的 schema 复杂度，先把现有歧义模型收紧；后续若扩展，再考虑：
  - and/or
  - 父子 narrowing
  - 多条件 narrowing
- 已补测试覆盖：
  - 单目标多匹配必须传 `nth`
  - target/actionability expression 内含歧义保护
  - snapshot 结构兼容 `ambiguous/ambiguityMessage`

---

# 十一、Phase 9：同步 / 云 / 环境编排补全

## P9-01 sync settings MCP 暴露
- [x] `get_sync_settings`
- [x] `save_sync_settings`
- [x] `request_profile_sync`
- [x] 补测试

**当前实现约定（已落地）**
- 已暴露 MCP 工具：
  - `get_sync_settings`
  - `save_sync_settings`
  - `request_profile_sync`
- `get_sync_settings`
  - 返回当前 self-hosted sync 配置
  - 包含：
    - `sync_server_url`
    - `sync_token`
- `save_sync_settings`
  - 支持保存或清空：
    - `sync_server_url`
    - `sync_token`
  - 空白字符串会在进入底层逻辑前直接拦截
- `request_profile_sync`
  - 用于对单个 profile 发起立即同步请求
  - 会复用现有 sync engine / scheduler 逻辑
  - blank `profile_id` 会前置拦截
- 当前错误映射约定：
  - invalid profile id / disabled sync → 参数错误
  - profile not found → not found
  - scheduler 或 app handle 问题 → internal
- 已补测试覆盖：
  - tool 注册计数
  - camelCase 参数解析
  - get/save 对 app_handle 的要求
  - blank url/token/profile_id 前置校验

## P9-02 同步相关后续能力评估
- [x] 评估 proxy/group/vpn/extension sync 是否也要 MCP 化
- [x] 评估 e2e password 相关命令是否需要 MCP 化

**当前评估结论（已落地）**
- 建议下一批优先 MCP 化的 sync 能力：
  - `set_proxy_sync_enabled`
  - `set_group_sync_enabled`
  - `set_vpn_sync_enabled`
  - `set_extension_sync_enabled`
  - `set_extension_group_sync_enabled`
  - `get_unsynced_entity_counts`
  - `enable_sync_for_all_entities`
- 原因：
  - 这些后端能力已经存在于 Tauri 命令层
  - 与 `request_profile_sync` 属于同一条“同步编排链”
  - 适合 AI 进行批量环境整理、补齐依赖实体同步状态
- 优先级建议：
  - 第一优先级：
    - `get_unsynced_entity_counts`
    - `set_proxy/group/vpn sync enabled`
  - 第二优先级：
    - `set_extension/extension_group sync enabled`
    - `enable_sync_for_all_entities`
- 关于 e2e password：
  - 后端已有：
    - `set_e2e_password`
    - `check_has_e2e_password`
    - `delete_e2e_password`
  - 建议 **谨慎 MCP 化**
- 原因：
  - 属于高敏感配置
  - 一旦进入 MCP，模型侧误操作风险高于普通 sync 开关
  - 更适合最开始只暴露“状态读取”，后续再决定是否开放写入/删除
- 当前建议：
  - 先做：
    - `check_has_e2e_password`
  - 暂缓：
    - `set_e2e_password`
    - `delete_e2e_password`

---

# 十二、Phase 10：高级执行器与工作流抽象

## P10-01 Recipe / Workflow 设计调研
- [x] 设计高阶步骤模型
- [x] 评估是否需要“批量动作执行”接口
- [x] 评估是否需要“脚本化 recipe”能力

**当前设计结论（已确认）**
- 第一阶段不引入独立脚本语言，先采用 **JSON recipe DSL**，避免把 MCP 变成第二套运行时。
- recipe 只编排**现有 MCP tool**，不复制底层能力；复杂页面逻辑仍通过已有 `evaluate_javascript` 等 step 完成。
- 多 profile 能力不放进单个 step 内部，而是由**外层批处理执行器**负责调度，保持单 profile recipe 与批量执行语义分离。

**最小 Recipe 模型**
- 顶层字段建议：
  - `version`：recipe 版本，首版固定 `1`
  - `name`：recipe 名称
  - `description`：可选说明
  - `defaults`：共享默认参数，如 `timeout_ms` / `auto_wait`
  - `inputs`：运行时输入参数声明（后续可扩展）
  - `steps`：顺序步骤列表
- 单个 step 建议字段：
  - `id`：稳定步骤 ID，便于回放与引用
  - `tool`：要调用的 MCP tool 名称
  - `arguments`：该 tool 的参数对象
  - `condition`：可选执行条件，首版只建议支持简单布尔/变量存在判断
  - `output`：可选输出命名，用于后续步骤引用
  - `on_error`：`stop | continue`
  - `retry`：可选重试配置，但首版只对幂等读取类步骤开放

**变量引用边界**
- 首版只建议支持：
  - `inputs.xxx`
  - `steps.<step_id>.result`
  - `steps.<step_id>.status`
- 不建议首版支持任意表达式求值、循环、函数、用户自定义脚本。

**关于“批量动作执行”接口的评估**
- 需要，但不建议做成“裸数组 actions 一把梭”的无结构接口。
- 更推荐：
  - `validate_recipe`
  - `preview_recipe`
  - `run_recipe`
  - `run_batch_profile_workflow`
- 理由：
  - AI 更容易先校验、再预览、再执行
  - 错误定位会落到具体 step，而不是落到一大坨 actions
  - 后续可以自然复用结构化回放与失败报告

**关于“脚本化 recipe”能力的评估**
- 需要“可编排”，但**不需要首版就做成任意脚本执行器**。
- 推荐分层：
  - 第 1 层：JSON recipe DSL
  - 第 2 层：recipe 中允许调用 `evaluate_javascript`
  - 第 3 层：若未来确实有复杂控制流，再考虑受限表达式/模板能力
- 这样既能满足 AI 自动化，又避免引入调试困难、权限难控的自定义脚本语言。

**首版建议暴露的 MCP 能力**
- [x] `validate_recipe`：只做结构校验、tool 存在性校验、参数形态初校验
- [x] `preview_recipe`：展开 defaults / 变量占位信息，输出规范化步骤视图
- [x] `run_recipe`：单 profile 顺序执行
- [x] `run_batch_profile_workflow`：同一 recipe 在多个 profile 上批量执行

**首版明确不做**
- 自定义循环语法
- 自定义函数
- 任意宿主脚本语言（Lua / JS DSL / Python DSL）
- 通用自动回滚框架
- 跨 recipe 共享全局变量状态

## P10-02 多 profile 批处理执行器
- [x] 设计批处理调度模型
- [x] 定义失败重试与回滚策略
- [x] 设计最小 demo 场景

**当前设计结论（已确认）**

**调度模型**
- 最小调度单元：`profile_id + recipe`
- 一个批处理任务包含：
  - `run_id`
  - `recipe`
  - `profile_ids`
  - `max_concurrency`
  - `stop_on_profile_failure`
  - `per_profile_inputs`
- 执行顺序：
  - 先校验 recipe
  - 再为每个 profile 生成独立执行上下文
  - 按 `max_concurrency` 控制并发
  - 每个 profile 内部仍保持 step 串行

**失败重试策略**
- 首版只建议对**读取类/等待类/可重复执行且无副作用或副作用可接受**的 step 开放重试。
- 建议默认不重试以下高风险步骤：
  - `click_element`
  - `type_text`
  - `upload_file`
  - 可能导致状态提交的自定义 `evaluate_javascript`
- 重试维度分两层：
  - step 级：短暂抖动/等待不足
  - profile 级：可选整体重跑，但首版默认关闭

**回滚策略**
- 不做“平台自动推断回滚”。
- 首版仅建议支持**补偿步骤**思路：
  - 某些未来 recipe 可以声明 `cleanup_steps`
  - 但默认不自动执行，以免二次破坏现场
- 结论：首版以“失败可观测、可回放”优先，高于“自动回滚”。

**最小 demo 场景**
- demo 1：多 profile 登录态巡检
  - `navigate`
  - `wait_for_selector`
  - `get_page_info`
  - `screenshot`
- demo 2：多 profile 搜索页采样
  - `navigate`
  - `type_text`
  - `click_element`
  - `wait_for_navigation`
  - `query_elements`
- demo 3：多 profile 下载链路检查
  - `navigate`
  - `download_file`
  - `wait_for_download`

**首版批处理接口建议**
- [x] `run_batch_profile_workflow`
  - 输入：`profile_ids + recipe + options`
  - 输出：每个 profile 一份独立执行报告
- 暂不建议首版做：
  - 跨 profile 依赖
  - leader/follower 联动编排
  - DAG 调度
  - 分布式队列

## P10-03 Structured orchestration 输出
- [x] 统一步骤执行记录
- [x] 统一失败原因与回放信息

**当前实现状态（已落地）**
- 已新增 `run_recipe` MCP tool
- 单 profile recipe 执行结果已统一输出：
  - `runId`
  - `recipeName`
  - `version`
  - `status`
  - `stepCount`
  - `completedSteps`
  - `successfulSteps`
  - `failedSteps`
  - `skippedSteps`
  - `stoppedAtStepId`
  - `warnings`
  - `inputs`
  - `outputs`
  - `steps`

**单个步骤当前输出字段**
- `index`
- `id`
- `tool`
- `status`（`success | failed | skipped`）
- `onError`
- `attempts`
- `durationMs`
- `condition`
- `result`
- `rawResponse`
- `error`
- `attemptErrors`
- `output`
- `replay`

**失败原因与回放信息**
- 失败已统一包含：
  - `error.code`
  - `error.message`
  - `attemptErrors[].stage`
- 当前 stage 已覆盖：
  - `argument_resolution`
  - `tool_call`
- 每个步骤都带 `replay.tool + replay.arguments`
- 可直接用于：
  - 问题复现
  - AI 二次修复
  - 后续批处理执行报告复用

**批处理执行器当前实现状态（已落地）**
- 已新增 `run_batch_profile_workflow` MCP tool
- 已支持：
  - `profile_ids`
  - `inputs`
  - `per_profile_inputs`
  - `max_concurrency`
  - `stop_on_profile_failure`
- 已输出统一 batch 级结果：
  - `batchRunId`
  - `recipeName`
  - `version`
  - `status`
  - `maxConcurrency`
  - `stopOnProfileFailure`
  - `totalProfiles`
  - `executedProfiles`
  - `successfulProfiles`
  - `partialProfiles`
  - `failedProfiles`
  - `skippedProfileIds`
  - `stoppedAfterProfileId`
  - `warnings`
  - `results`
- `results[]` 中每个 profile 都附带独立 `report`，直接复用 `run_recipe` 的结构化执行记录

---

# 十三、Phase 11：DevTools 风格可视化调试台（可选）

## P11-01 Console UI
- [ ] 实时 console 面板
- [ ] 级别筛选
- [ ] 搜索

## P11-02 Network UI
- [ ] 请求列表
- [ ] 基础筛选
- [ ] 请求详情页
- [ ] 响应体查看

## P11-03 Waterfall 时间线
- [ ] request timeline
- [ ] 基础 waterfall 视图

## P11-04 调试台整合
- [ ] frame tree 展示（可选）
- [ ] tab 列表展示（可选）
- [ ] traffic stats 图表（可选）

---

# 十四、建议的第一批开工任务（最推荐）

如果马上开始做，建议按下面顺序开工：

## Sprint 1
- [x] P0-01 统一 MCP tool 输入输出规范
- [x] P0-02 统一错误模型
- [x] P0-03 统一核心参数模型
- [x] P1-01 Console capture 基础能力
- [x] P1-02 Console MCP 工具暴露

## Sprint 2
- [x] P1-03 Network capture 基础能力
- [x] P1-04 Response body 按需读取
- [x] P1-05 Network MCP 工具暴露
- [x] P1-06 Traffic stats MCP 暴露

## Sprint 3
- [x] P2-01 Cookie MCP 接入
- [x] P3-01 wait engine 抽象
- [x] P3-02 wait_for_selector
- [x] P3-03 wait_for_text
- [x] P3-04 wait_for_navigation
- [x] P4-01 locator schema 定稿
- [x] P4-02 locator resolver 实现

## Sprint 4
- [x] P4-03 click_element 接 locator
- [x] P4-04 type_text 接 locator
- [x] P5-01 click auto-wait
- [x] P5-02 type auto-wait
- [ ] P6-01 frame 数据模型设计
- [ ] P6-02 list_frames / get_frame_tree

---

# 十五、执行建议

## 1. 每完成一个阶段，都做三件事
- 补自动化测试
- 补最小示例
- 更新根目录文档

## 2. 不要同时大面积并行改 locator / wait / frame
因为三者强依赖，建议按顺序推进。

## 3. console/network 应该尽早做
因为这两项会让后续调 locator / wait / frame 的开发体验提升很多。

---

# 十六、最终目标回看

当这份清单全部完成时，项目应达到：

- 一个完整的 MCP 浏览器自动化工具集
- 一个适合 AI 使用的结构化浏览器控制层
- 一个接近 Playwright 核心体验的自动化平台
- 一个具备 DevTools 协议级调试能力的浏览器自动化系统
- 一个可继续向可视化调试台演进的产品基础
