# JnmBrowser / donutbrowser 当前 MCP 功能测试文档

> 目的：这份文档是给 AI / 自动化测试执行者使用的。按本文档走，可以系统性测试当前 MCP 的全部主要能力。
>
> 当前 MCP 工具总数：**111 个**。
>
> 当前主线状态：Phase 0 ~ Phase 10 已完成；Phase 11 DevTools UI 已决定跳过。

---

# 1. MCP 连接方式

## 1.1 服务地址

MCP HTTP 服务默认监听本机：

```text
http://127.0.0.1:51080/mcp
```

如果用户在程序设置里改过端口，以程序界面显示的 MCP URL 为准。

健康检查：

```bash
curl http://127.0.0.1:51080/health
```

预期返回类似：

```json
{
  "status": "ok",
  "server": "donut-browser",
  "version": "0.22.7",
  "protocolVersion": "2025-11-25"
}
```

## 1.2 鉴权方式

MCP 请求需要 token。支持两种方式：

### 方式 A：Authorization Header

```http
Authorization: Bearer <MCP_TOKEN>
```

### 方式 B：URL path 带 token

```text
http://127.0.0.1:51080/mcp/<MCP_TOKEN>
```

建议优先用 Header。

## 1.3 JSON-RPC 基本流程

### 第一步：initialize

```bash
curl -i \
  -X POST 'http://127.0.0.1:51080/mcp' \
  -H 'Authorization: Bearer <MCP_TOKEN>' \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {
      "protocolVersion": "2025-11-25",
      "capabilities": {},
      "clientInfo": {
        "name": "mcp-test-client",
        "version": "1.0.0"
      }
    }
  }'
```

响应头里会有：

```http
mcp-session-id: <SESSION_ID>
```

后续请求建议带上：

```http
mcp-session-id: <SESSION_ID>
```

### 第二步：发送 initialized 通知

```bash
curl -X POST 'http://127.0.0.1:51080/mcp' \
  -H 'Authorization: Bearer <MCP_TOKEN>' \
  -H 'mcp-session-id: <SESSION_ID>' \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "method": "notifications/initialized",
    "params": {}
  }'
```

预期 HTTP 状态：`202 Accepted`。

### 第三步：列出工具

```bash
curl -X POST 'http://127.0.0.1:51080/mcp' \
  -H 'Authorization: Bearer <MCP_TOKEN>' \
  -H 'mcp-session-id: <SESSION_ID>' \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "tools/list",
    "params": {}
  }'
```

预期：返回 `tools` 数组，应该包含本文档列出的 111 个工具。

### 第四步：调用工具

工具调用统一使用：

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "tools/call",
  "params": {
    "name": "工具名",
    "arguments": {
      "参数": "值"
    }
  }
}
```

---

# 2. 通用约定

## 2.1 参数命名

MCP 参数以 `snake_case` 为主，同时很多参数兼容 `camelCase`。

推荐优先使用：

```json
{
  "profile_id": "...",
  "timeout_ms": 10000,
  "auto_wait": true
}
```

部分兼容写法：

```json
{
  "profileId": "...",
  "timeoutMs": 10000,
  "autoWait": true
}
```

## 2.2 通用返回格式

大多数工具返回：

```json
{
  "content": [
    {
      "type": "text",
      "text": "这里通常是 pretty JSON 字符串"
    }
  ]
}
```

注意：`content[0].text` 很多时候本身是 JSON 字符串，需要再解析一次。

截图工具返回 image 类型。

## 2.3 常见错误码

| 错误码 | 含义 |
|---|---|
| `-32602` | 参数错误，例如缺少字段、locator 无效 |
| `-32601` | JSON-RPC 方法不存在 |
| `-32001` | MCP server 未运行 |
| `-32000` | 内部错误 |
| `-32004` | 找不到资源，例如 profile/frame/tab 不存在 |
| `-32008` | 超时 |

---

# 3. 当前 MCP 工具总览

## 3.1 Profile 管理

- `list_profiles`
- `get_profile`
- `run_profile`
- `kill_profile`
- `create_profile`
- `update_profile`
- `delete_profile`
- `list_tags`
- `get_profile_status`

用途：创建、启动、停止、查询浏览器 profile。

## 3.2 Group 管理

- `list_groups`
- `get_group`
- `create_group`
- `update_group`
- `delete_group`
- `assign_profiles_to_group`

用途：管理 profile 分组。

## 3.3 Proxy 管理

- `list_proxies`
- `get_proxy`
- `create_proxy`
- `update_proxy`
- `delete_proxy`
- `export_proxies`
- `import_proxies`
- `update_profile_proxy_bypass_rules`
- `update_profile_dns_blocklist`
- `get_dns_blocklist_status`

用途：代理配置、导入导出、DNS blocklist、绕过规则。

## 3.4 VPN 管理

- `import_vpn`
- `list_vpn_configs`
- `delete_vpn`
- `connect_vpn`
- `disconnect_vpn`
- `get_vpn_status`

用途：WireGuard VPN 配置管理和连接状态。

## 3.5 Fingerprint / GeoIP

- `get_profile_fingerprint`
- `update_profile_fingerprint`
- `generate_sample_fingerprint`
- `is_geoip_database_available`
- `download_geoip_database`

用途：读取/更新 fingerprint，生成示例 fingerprint，检查/下载 GeoIP 数据库。

## 3.6 Extension 管理

- `list_extensions`
- `list_extension_groups`
- `create_extension_group`
- `delete_extension`
- `delete_extension_group`
- `assign_extension_group_to_profile`

用途：扩展和扩展组管理。

## 3.7 Team Lock

- `get_team_locks`
- `get_team_lock_status`

用途：团队 profile 锁状态。注意：可能要求团队计划。

## 3.8 Sync Settings

- `get_sync_settings`
- `save_sync_settings`
- `request_profile_sync`

用途：读取/保存自托管同步配置，请求单个 profile 同步。

> 注意：用户已说明当前主线不用考虑 `donut-sync` 云端服务，测试时可只做轻量验证，不作为阻塞项。

## 3.9 Synchronizer

- `start_sync_session`
- `stop_sync_session`
- `get_sync_sessions`
- `remove_sync_follower`

用途：多浏览器同步操作。当前不是主线重点，可做 smoke test，不要求完整 E2E。

## 3.10 浏览器页面自动化

基础页面操作：

- `navigate`
- `screenshot`
- `evaluate_javascript`
- `click_element`
- `type_text`
- `get_page_content`
- `get_page_info`

交互增强：

- `hover_element`
- `focus_element`
- `scroll_to`
- `scroll_element_into_view`
- `press_key`
- `press_hotkey`
- `select_option`
- `check_checkbox`
- `uncheck_checkbox`

DOM 读取增强：

- `element_exists`
- `get_element_text`
- `get_element_html`
- `get_element_attribute`
- `get_element_rect`
- `query_elements`
- `count_elements`

## 3.11 Tab / Window 管理

- `list_tabs`
- `switch_tab`
- `new_tab`
- `close_tab`
- `get_active_tab`

用途：多标签页创建、切换、关闭、查询。

## 3.12 文件上传 / 下载

- `set_file_input`
- `upload_file`
- `download_file`
- `wait_for_download`

用途：设置文件 input，下载当前页面或指定 URL 文件。

## 3.13 Frame / iframe

- `list_frames`
- `get_frame_tree`
- `find_frame`

用途：查看 frame tree，定位 iframe，在 iframe 里执行自动化动作。

## 3.14 Console Capture

- `start_console_capture`
- `get_console_logs`
- `clear_console_logs`

用途：捕获页面 console.log / warn / error 等。

## 3.15 Network Capture

- `start_network_capture`
- `get_network_requests`
- `get_response_body`
- `clear_network_requests`

用途：捕获请求列表、状态码、headers、post data、response body。

## 3.16 Traffic Stats

- `get_all_traffic_snapshots`
- `get_traffic_stats_for_period`
- `clear_all_traffic_stats`

用途：获取 profile 维度流量统计。

## 3.17 Cookie / Storage

Cookie：

- `read_profile_cookies`
- `copy_profile_cookies`
- `import_cookies_from_file`
- `export_profile_cookies`

Storage：

- `get_local_storage`
- `set_local_storage`
- `get_session_storage`
- `clear_storage`

## 3.18 Wait 系列

- `wait_for_selector`
- `wait_for_text`
- `wait_for_navigation`

用途：等待元素、文本、导航完成。

## 3.19 Recipe / Workflow

- `validate_recipe`
- `preview_recipe`
- `run_recipe`
- `run_batch_profile_workflow`

用途：把多个 MCP tool 编排成 workflow，支持单 profile 和多 profile 批量执行。

---

# 4. Locator / Frame / Tab 参数格式

## 4.1 Locator

当前支持：

- `css`
- `text`
- `testid`
- `xpath`
- `role`
- `label`
- `placeholder`

格式：

```json
{
  "by": "css",
  "value": "#search",
  "nth": 0,
  "exact": false
}
```

role locator 支持 `name`：

```json
{
  "by": "role",
  "value": "button",
  "name": "Search",
  "exact": true
}
```

重要规则：

- 单目标工具如果匹配多个元素，且没有传 `nth`，会报歧义错误。
- 多目标工具如 `query_elements` / `count_elements` / `element_exists` 可以接受多匹配。

## 4.2 Frame selector

支持：

```json
{ "by": "main" }
```

```json
{ "by": "index", "value": 1 }
```

```json
{ "by": "name", "value": "login-frame" }
```

```json
{ "by": "url", "value": "iframe.html" }
```

```json
{ "by": "id", "value": "FRAME_ID" }
```

规则：

- `main` 不传 `value`
- `index` 的 `value` 必须是整数
- `url/name/id` 的 `value` 必须是非空字符串
- frame index 是 preorder 展平顺序，主 frame 固定 index 0

## 4.3 Tab selector

支持：

```json
{ "by": "id", "value": "TARGET_ID" }
```

```json
{ "by": "index", "value": 0 }
```

```json
{ "by": "url", "value": "example.com" }
```

```json
{ "by": "title", "value": "Dashboard" }
```

---

# 5. 推荐本地测试页

为了测试全部页面自动化能力，建议启动一个本地测试服务。

## 5.1 创建测试目录

```bash
mkdir -p /tmp/jnm-mcp-test
cd /tmp/jnm-mcp-test
```

## 5.2 创建 `index.html`

```html
<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <title>MCP Test Page</title>
  <style>
    body { font-family: sans-serif; }
    #delayed { display: none; margin-top: 20px; }
    #scroll-target { margin-top: 1200px; }
  </style>
</head>
<body>
  <h1>MCP Test Page</h1>

  <label for="search">Search Box</label>
  <input id="search" name="search" placeholder="Type keyword" data-testid="search-input" />

  <button id="submit" data-testid="submit-button">Submit</button>
  <button id="log-button">Log Console</button>
  <button id="fetch-button">Fetch API</button>

  <select id="select">
    <option value="a">Option A</option>
    <option value="b">Option B</option>
  </select>

  <label><input id="agree" type="checkbox" /> Agree</label>

  <input id="file" type="file" />

  <div id="result" data-testid="result">empty</div>
  <div id="delayed">Delayed Text Ready</div>

  <iframe id="child-frame" name="child-frame" src="/iframe.html"></iframe>

  <div id="scroll-target">Scroll Target</div>

  <script>
    console.log('MCP test page loaded');
    localStorage.setItem('mcp_local_key', 'local-value');
    sessionStorage.setItem('mcp_session_key', 'session-value');

    setTimeout(() => {
      document.getElementById('delayed').style.display = 'block';
    }, 800);

    document.getElementById('submit').addEventListener('click', () => {
      const value = document.getElementById('search').value;
      document.getElementById('result').textContent = 'submitted:' + value;
      console.log('submit clicked', value);
    });

    document.getElementById('log-button').addEventListener('click', () => {
      console.warn('manual warning log');
      console.error('manual error log');
    });

    document.getElementById('fetch-button').addEventListener('click', async () => {
      const response = await fetch('/api/echo?value=network-test', { method: 'GET' });
      const json = await response.json();
      document.getElementById('result').textContent = 'fetch:' + json.value;
    });
  </script>
</body>
</html>
```

## 5.3 创建 `iframe.html`

```html
<!doctype html>
<html>
<body>
  <h2>Iframe Page</h2>
  <label for="iframe-input">Iframe Input</label>
  <input id="iframe-input" placeholder="Iframe keyword" />
  <button id="iframe-button">Iframe Button</button>
  <div id="iframe-result">iframe-empty</div>
  <script>
    console.log('iframe loaded');
    document.getElementById('iframe-button').addEventListener('click', () => {
      document.getElementById('iframe-result').textContent =
        'iframe-submitted:' + document.getElementById('iframe-input').value;
    });
  </script>
</body>
</html>
```

## 5.4 创建 `server.py`

```python
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json

class Handler(SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith('/api/echo'):
            self.send_response(200)
            self.send_header('content-type', 'application/json')
            self.end_headers()
            self.wfile.write(json.dumps({'ok': True, 'value': 'network-test'}).encode())
            return
        if self.path == '/download.txt':
            self.send_response(200)
            self.send_header('content-type', 'text/plain')
            self.send_header('content-disposition', 'attachment; filename="download.txt"')
            self.end_headers()
            self.wfile.write(b'MCP download works')
            return
        return super().do_GET()

ThreadingHTTPServer(('127.0.0.1', 18080), Handler).serve_forever()
```

启动：

```bash
python3 /tmp/jnm-mcp-test/server.py
```

测试页：

```text
http://127.0.0.1:18080/index.html
```

下载文件：

```text
http://127.0.0.1:18080/download.txt
```

---

# 6. 推荐完整测试顺序

下面按“从基础到高级”的顺序测试。

---

## 6.1 基础连接测试

### tools/list

目标：确认 MCP 服务可用，工具数量正确。

调用：`tools/list`

验收：

- 返回 111 个左右工具
- 至少包含：
  - `navigate`
  - `click_element`
  - `start_console_capture`
  - `start_network_capture`
  - `run_recipe`
  - `run_batch_profile_workflow`

---

## 6.2 Profile 基础测试

### list_profiles

```json
{
  "name": "list_profiles",
  "arguments": {}
}
```

从返回结果里选择一个 Wayfern 或 Camoufox profile，记作：

```text
PROFILE_ID=<实际 profile id>
```

如果没有可用 profile，可创建一个测试 profile：

```json
{
  "name": "create_profile",
  "arguments": {
    "name": "MCP Test Profile",
    "browser": "wayfern"
  }
}
```

### run_profile

```json
{
  "name": "run_profile",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "url": "http://127.0.0.1:18080/index.html"
  }
}
```

### get_profile_status

```json
{
  "name": "get_profile_status",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

验收：profile 处于 running 状态。

---

## 6.3 页面基础自动化测试

### CDP paid-plan gate 识别

如果以下页面自动化工具返回：

```text
CDP error: {"code":-32000,"message":"Browser automation requires a paid Donut Browser plan."}
```

不要再判断为 MCP 协议层 paid gate。当前源码里 MCP 自身订阅校验已放开；该错误来自 Wayfern/Chromium 外部二进制的 `Runtime.evaluate` 类 CDP 限制。测试报告里需要单独标注为“Wayfern runtime CDP gate / 待 DOM+Input fallback”，不要和 `navigate`、`screenshot`、`DOM/Network/Log` 类可用能力混在一起。

### get_page_info

```json
{
  "name": "get_page_info",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

验收：title 应包含 `MCP Test Page`。

### navigate

```json
{
  "name": "navigate",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "url": "http://127.0.0.1:18080/index.html"
  }
}
```

### screenshot

```json
{
  "name": "screenshot",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "full_page": true,
    "format": "png"
  }
}
```

验收：返回 image 内容。

### evaluate_javascript

```json
{
  "name": "evaluate_javascript",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "expression": "document.title"
  }
}
```

验收：返回 `MCP Test Page`。

---

## 6.4 Locator / 点击 / 输入测试

### type_text by css

```json
{
  "name": "type_text",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#search"
    },
    "text": "hello-mcp",
    "clear": true,
    "auto_wait": true
  }
}
```

### click_element by role

```json
{
  "name": "click_element",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "role",
      "value": "button",
      "name": "Submit",
      "exact": true
    },
    "auto_wait": true
  }
}
```

### get_element_text

```json
{
  "name": "get_element_text",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "testid",
      "value": "result"
    }
  }
}
```

验收：文本包含：

```text
submitted:hello-mcp
```

---

## 6.5 Wait 测试

### wait_for_selector

```json
{
  "name": "wait_for_selector",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#delayed"
    },
    "state": "visible",
    "timeout_ms": 5000
  }
}
```

### wait_for_text

```json
{
  "name": "wait_for_text",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "text": "Delayed Text Ready",
    "timeout_ms": 5000
  }
}
```

验收：均成功，不超时。

---

## 6.6 交互增强测试

### hover_element

```json
{
  "name": "hover_element",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#submit"
    }
  }
}
```

### focus_element

```json
{
  "name": "focus_element",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "placeholder",
      "value": "Type keyword",
      "exact": true
    }
  }
}
```

### press_hotkey

```json
{
  "name": "press_hotkey",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "keys": ["Control", "A"]
  }
}
```

### press_key

```json
{
  "name": "press_key",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "key": "Backspace"
  }
}
```

### select_option

```json
{
  "name": "select_option",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#select"
    },
    "value": "b"
  }
}
```

### check_checkbox / uncheck_checkbox

```json
{
  "name": "check_checkbox",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#agree"
    }
  }
}
```

```json
{
  "name": "uncheck_checkbox",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#agree"
    }
  }
}
```

### scroll_to / scroll_element_into_view

```json
{
  "name": "scroll_to",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "x": 0,
    "y": 1000
  }
}
```

```json
{
  "name": "scroll_element_into_view",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#scroll-target"
    }
  }
}
```

---

## 6.7 DOM 读取增强测试

### element_exists

```json
{
  "name": "element_exists",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "label",
      "value": "Search Box",
      "exact": true
    }
  }
}
```

### get_element_html

```json
{
  "name": "get_element_html",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#result"
    }
  }
}
```

### get_element_attribute

```json
{
  "name": "get_element_attribute",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#search"
    },
    "attribute": "placeholder"
  }
}
```

### get_element_rect

```json
{
  "name": "get_element_rect",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#search"
    }
  }
}
```

### query_elements

```json
{
  "name": "query_elements",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "role",
      "value": "button"
    },
    "limit": 10
  }
}
```

### count_elements

```json
{
  "name": "count_elements",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "role",
      "value": "button"
    }
  }
}
```

---

## 6.8 Frame / iframe 测试

### list_frames

```json
{
  "name": "list_frames",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

验收：至少有 2 个 frame，main index 0，iframe index 1。

### get_frame_tree

```json
{
  "name": "get_frame_tree",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

### find_frame

```json
{
  "name": "find_frame",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "frame": {
      "by": "name",
      "value": "child-frame"
    }
  }
}
```

### 在 iframe 里输入和点击

```json
{
  "name": "type_text",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "frame": {
      "by": "name",
      "value": "child-frame"
    },
    "locator": {
      "by": "placeholder",
      "value": "Iframe keyword",
      "exact": true
    },
    "text": "iframe-mcp",
    "clear": true
  }
}
```

```json
{
  "name": "click_element",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "frame": {
      "by": "name",
      "value": "child-frame"
    },
    "locator": {
      "by": "role",
      "value": "button",
      "name": "Iframe Button",
      "exact": true
    }
  }
}
```

```json
{
  "name": "get_element_text",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "frame": {
      "by": "name",
      "value": "child-frame"
    },
    "locator": {
      "by": "css",
      "value": "#iframe-result"
    }
  }
}
```

验收：返回 `iframe-submitted:iframe-mcp`。

---

## 6.9 Console Capture 测试

### start_console_capture

```json
{
  "name": "start_console_capture",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

### 触发 console 日志

```json
{
  "name": "click_element",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#log-button"
    }
  }
}
```

### get_console_logs

```json
{
  "name": "get_console_logs",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "limit": 20
  }
}
```

验收：

- `running` 应为 `true`。
- `lastError` 应为 `null`；如果非空，说明 capture 订阅失败，不能只看 `start_console_capture` 是否返回成功。
- 日志里应该看到：

- `MCP test page loaded`
- `manual warning log`
- `manual error log`

### clear_console_logs

```json
{
  "name": "clear_console_logs",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

---

## 6.10 Network Capture 测试

### start_network_capture

```json
{
  "name": "start_network_capture",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

### 触发 fetch

```json
{
  "name": "click_element",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#fetch-button"
    }
  }
}
```

### get_network_requests

```json
{
  "name": "get_network_requests",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "limit": 50
  }
}
```

验收：

- `running` 应为 `true`。
- `lastError` 应为 `null`；如果非空，说明 `Network.enable` 或 capture stream 失败。
- 请求列表中应包含：

```text
/api/echo?value=network-test
```

记录该请求的 `requestId`。

### get_response_body

```json
{
  "name": "get_response_body",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "request_id": "REQUEST_ID",
    "max_bytes": 262144
  }
}
```

验收：body 包含：

```json
{"ok":true,"value":"network-test"}
```

### clear_network_requests

```json
{
  "name": "clear_network_requests",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

---

## 6.11 Traffic Stats 测试

### get_traffic_stats_for_period

```json
{
  "name": "get_traffic_stats_for_period",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "seconds": 300
  }
}
```

### get_all_traffic_snapshots

```json
{
  "name": "get_all_traffic_snapshots",
  "arguments": {}
}
```

### clear_all_traffic_stats

```json
{
  "name": "clear_all_traffic_stats",
  "arguments": {}
}
```

---

## 6.12 Storage 测试

### get_local_storage

```json
{
  "name": "get_local_storage",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

验收：包含 `mcp_local_key`。

### set_local_storage

```json
{
  "name": "set_local_storage",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "key": "mcp_set_key",
    "value": {
      "hello": "world"
    }
  }
}
```

### get_session_storage

```json
{
  "name": "get_session_storage",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

验收：包含 `mcp_session_key`。

### clear_storage

```json
{
  "name": "clear_storage",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "storage": "both"
  }
}
```

---

## 6.13 Cookie 测试

### read_profile_cookies

```json
{
  "name": "read_profile_cookies",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

### export_profile_cookies

```json
{
  "name": "export_profile_cookies",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "format": "json"
  }
}
```

### import_cookies_from_file

说明：`content: "[]"` 是“无有效 cookie”的负向用例，不应按成功导入验收。正向测试请至少提供一条有效 cookie。

```json
{
  "name": "import_cookies_from_file",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "content": "[{\"name\":\"mcp_test_cookie\",\"value\":\"hello\",\"domain\":\"127.0.0.1\",\"path\":\"/\",\"expires\":-1,\"httpOnly\":false,\"secure\":false,\"sameSite\":\"Lax\"}]"
  }
}
```

注意：cookie 导入/复制会修改 profile 状态，测试时建议用临时 profile。

---

## 6.14 Tab / Window 测试

### list_tabs

```json
{
  "name": "list_tabs",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

### new_tab

```json
{
  "name": "new_tab",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "url": "http://127.0.0.1:18080/index.html"
  }
}
```

### get_active_tab

```json
{
  "name": "get_active_tab",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

### switch_tab

```json
{
  "name": "switch_tab",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "tab": {
      "by": "index",
      "value": 0
    }
  }
}
```

### close_tab

```json
{
  "name": "close_tab",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "tab": {
      "by": "index",
      "value": 1
    }
  }
}
```

---

## 6.15 文件上传 / 下载测试

### 准备上传文件

```bash
echo 'hello upload' > /tmp/jnm-mcp-test/upload.txt
```

### upload_file

```json
{
  "name": "upload_file",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "locator": {
      "by": "css",
      "value": "#file"
    },
    "files": ["/tmp/jnm-mcp-test/upload.txt"]
  }
}
```

### download_file

```json
{
  "name": "download_file",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "url": "http://127.0.0.1:18080/download.txt",
    "file_name": "mcp-download.txt"
  }
}
```

记录返回的 `downloadId`。

### wait_for_download

```json
{
  "name": "wait_for_download",
  "arguments": {
    "profile_id": "PROFILE_ID",
    "download_id": "DOWNLOAD_ID",
    "timeout_ms": 10000
  }
}
```

验收：状态为 completed，文件存在。

---

# 7. Recipe / Workflow 测试

## 7.1 validate_recipe

```json
{
  "name": "validate_recipe",
  "arguments": {
    "recipe": {
      "version": 1,
      "name": "simple-page-test",
      "steps": [
        {
          "id": "open",
          "tool": "navigate",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}",
            "url": "{{inputs.url}}"
          }
        },
        {
          "id": "info",
          "tool": "get_page_info",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}"
          },
          "output": "page_info"
        }
      ]
    }
  }
}
```

验收：`valid: true`。

## 7.2 preview_recipe

```json
{
  "name": "preview_recipe",
  "arguments": {
    "recipe": {
      "version": 1,
      "name": "simple-page-test",
      "defaults": {
        "timeout_ms": 10000,
        "auto_wait": true
      },
      "steps": [
        {
          "id": "open",
          "tool": "navigate",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}",
            "url": "{{inputs.url}}"
          }
        },
        {
          "id": "wait_title",
          "tool": "wait_for_text",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}",
            "text": "MCP Test Page"
          }
        }
      ]
    }
  }
}
```

验收：返回 steps，包含 `effectiveArguments` 和 `replay`。

## 7.3 run_recipe

```json
{
  "name": "run_recipe",
  "arguments": {
    "recipe": {
      "version": 1,
      "name": "full-small-flow",
      "defaults": {
        "timeout_ms": 10000,
        "auto_wait": true
      },
      "steps": [
        {
          "id": "open",
          "tool": "navigate",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}",
            "url": "{{inputs.url}}"
          }
        },
        {
          "id": "type",
          "tool": "type_text",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}",
            "locator": { "by": "css", "value": "#search" },
            "text": "recipe-keyword",
            "clear": true
          }
        },
        {
          "id": "submit",
          "tool": "click_element",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}",
            "locator": { "by": "role", "value": "button", "name": "Submit", "exact": true }
          }
        },
        {
          "id": "read_result",
          "tool": "get_element_text",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}",
            "locator": { "by": "css", "value": "#result" }
          },
          "output": "result_text"
        }
      ]
    },
    "inputs": {
      "profile_id": "PROFILE_ID",
      "url": "http://127.0.0.1:18080/index.html"
    }
  }
}
```

验收：

- `status` 为 `success`
- `outputs.result_text` 包含 `submitted:recipe-keyword`
- 每个 step 都有 `replay`

## 7.4 run_batch_profile_workflow

如果有多个 profile：

```json
{
  "name": "run_batch_profile_workflow",
  "arguments": {
    "profile_ids": ["PROFILE_ID_1", "PROFILE_ID_2"],
    "recipe": {
      "version": 1,
      "name": "batch-page-info",
      "steps": [
        {
          "id": "open",
          "tool": "navigate",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}",
            "url": "{{inputs.url}}"
          }
        },
        {
          "id": "info",
          "tool": "get_page_info",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}"
          },
          "output": "page_info"
        }
      ]
    },
    "inputs": {
      "url": "http://127.0.0.1:18080/index.html"
    },
    "max_concurrency": 2,
    "stop_on_profile_failure": false
  }
}
```

验收：

- batch `status` 为 `success` 或能明确报告失败 profile
- `results[]` 每个 profile 都有独立 `report`
- `executedProfiles` 等于 profile 数量

---

# 8. Proxy / VPN / Extension / Sync 轻量测试建议

这些工具属于配置类能力，可能会修改本地状态，建议只做轻量验证：

## 8.1 Proxy

安全 smoke test：

```json
{ "name": "list_proxies", "arguments": {} }
```

```json
{ "name": "get_dns_blocklist_status", "arguments": {} }
```

`import_proxies` 说明：

- `format: "txt"` 可用于简单导入，例如 `http://user:pass@host:port` 每行一个。
- `format: "json"` 不是最小 proxy array；它要求使用 `export_proxies(format=json)` 导出的完整结构（包含 `exported_at` 等 metadata）。测试 JSON 导入时，建议先导出，再在导出结构里改一条数据后导入。

不要随便对真实 profile 改 proxy，除非用户确认。

## 8.2 VPN

安全 smoke test：

```json
{ "name": "list_vpn_configs", "arguments": {} }
```

不要自动 `connect_vpn`，除非用户确认。

## 8.3 Extension

安全 smoke test：

```json
{ "name": "list_extensions", "arguments": {} }
```

```json
{ "name": "list_extension_groups", "arguments": {} }
```

## 8.4 Sync

安全 smoke test：

```json
{ "name": "get_sync_settings", "arguments": {} }
```

不要自动改 sync token / sync server，除非用户确认。

---

# 9. 测试验收总表

| 模块 | 核心工具 | 验收标准 |
|---|---|---|
| 连接 | `initialize`, `tools/list` | 能拿 session，工具列表正常 |
| Profile | `list_profiles`, `run_profile`, `get_profile_status` | profile 能启动并运行 |
| 页面 | `navigate`, `get_page_info`, `screenshot` | 页面可打开，可截图 |
| JS | `evaluate_javascript` | 能读 `document.title` |
| Locator | `type_text`, `click_element` | 能输入和点击 |
| Wait | `wait_for_selector`, `wait_for_text` | 动态元素能等到 |
| 交互 | hover/focus/key/select/checkbox/scroll | 均不报错，状态正确 |
| DOM | text/html/attr/rect/query/count | 返回结构正确 |
| Frame | `list_frames`, `find_frame`, iframe 操作 | iframe 内可输入点击读取 |
| Console | capture/logs/clear | 能捕获 warn/error |
| Network | capture/requests/body/clear | 能捕获 `/api/echo` 和 body |
| Traffic | traffic stats | 能返回 profile 流量统计 |
| Storage | local/session/clear | 能读写清理 storage |
| Cookie | read/export/import | 能读/导出，导入不崩 |
| Tabs | list/new/switch/close/active | 多 tab 可管理 |
| File | upload/download/wait | 上传不报错，下载完成 |
| Recipe | validate/preview/run | 单 profile workflow 成功 |
| Batch | run_batch_profile_workflow | 多 profile workflow 有汇总报告 |

---

# 10. 推荐给 AI 的执行原则

1. 先 `tools/list`，确认工具存在。
2. 先选临时 profile，不要直接动用户重要 profile。
3. 配置类工具只做 list/read，除非用户明确允许修改。
4. 页面自动化用本地测试页，避免外网波动。
5. 单工具测试通过后，再测 recipe。
6. recipe 通过后，再测 batch。
7. 每个失败都记录：
   - tool name
   - arguments
   - error code
   - error message
   - replay 信息
8. 如果测试过程中 profile 卡死，调用：

```json
{
  "name": "kill_profile",
  "arguments": {
    "profile_id": "PROFILE_ID"
  }
}
```

---

# 11. 当前不测 / 不做的内容

用户已确认不做 Phase 11 UI，因此以下不属于当前验收范围：

- Console UI
- Network UI
- Waterfall UI
- 调试台整合 UI

底层 console/network MCP 能力仍需要测试。

---

# 12. 最小冒烟测试 Recipe

如果只想快速确认核心链路，可以直接跑这个 recipe：

```json
{
  "name": "run_recipe",
  "arguments": {
    "recipe": {
      "version": 1,
      "name": "mcp-smoke-test",
      "defaults": {
        "timeout_ms": 10000,
        "auto_wait": true
      },
      "steps": [
        {
          "id": "open",
          "tool": "navigate",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}",
            "url": "{{inputs.url}}"
          }
        },
        {
          "id": "info",
          "tool": "get_page_info",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}"
          },
          "output": "page_info"
        },
        {
          "id": "type",
          "tool": "type_text",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}",
            "locator": { "by": "placeholder", "value": "Type keyword", "exact": true },
            "text": "smoke-test",
            "clear": true
          }
        },
        {
          "id": "click",
          "tool": "click_element",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}",
            "locator": { "by": "role", "value": "button", "name": "Submit", "exact": true }
          }
        },
        {
          "id": "result",
          "tool": "get_element_text",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}",
            "locator": { "by": "testid", "value": "result" }
          },
          "output": "result"
        }
      ]
    },
    "inputs": {
      "profile_id": "PROFILE_ID",
      "url": "http://127.0.0.1:18080/index.html"
    }
  }
}
```

验收：

```text
status = success
outputs.result 包含 submitted:smoke-test
```
