# JnmBrowser MCP 双内核完整测试方案

> 目标：用 **AI 通过 MCP** 直接驱动 JnmBrowser，系统性验证 **Chromium（轻模式）** 与 **Camoufox（重模式）** 两条内核路径是否仍存在功能缺陷、兼容性差异或回归问题。
>
> 这份文档重点不是“列功能”，而是“真的把问题测出来”。

---

# 0. 先说结论：现在是不是已经“全部兼容两个内核”了？

**不要直接下“100% 全兼容”这个结论。**

更准确的说法应该是：

- **MCP 公共能力层** 现在已经按“双内核共用一套工具接口”设计好了；
- **大部分页面自动化能力** 应该能同时跑在 Chromium / Camoufox 上；
- 但是否真“全部兼容”，必须按下面这份 **双内核测试矩阵** 实测后才能确认；
- 特别要重点盯这几类风险：
  - 浏览器启动与关闭
  - 页面导航 / DOM 操作 / 输入点击
  - auto-wait / locator / frame / tab
  - storage / cookie / console / network capture
  - 下载上传
  - 指纹固定项 + 随机 seed 的共同生效
  - 两内核在页面运行态上的差异

也就是说：

- **架构上：已经尽量统一**
- **工程上：还必须靠测试矩阵收口**

---

# 1. 这些控制台 warning 是什么问题？

## 1.1 `Missing Description or aria-describedby={undefined} for {DialogContent}`

这是 **Radix Dialog 的可访问性 warning**，不是浏览器内核错误，也不是 MCP 协议错误。

意思是：

- 某些弹窗组件用了 `DialogContent`
- 但没有给这个弹窗提供 `DialogDescription`
- 或者没有显式设置 `aria-describedby={undefined}`
- 所以 Radix 在开发环境报警告

它的影响：

- **不会直接导致 MCP 自动化失效**
- **不会影响浏览器启动/页面操作本身**
- 但说明某些对话框组件的无障碍语义不完整

从当前代码看，**确实存在若干 Dialog 没有 Description**，高概率来源包括：

- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/settings-dialog.tsx`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/integrations-dialog.tsx`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/profile-selector-dialog.tsx`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/proxy-form-dialog.tsx`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/camoufox-config-dialog.tsx`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/dns-blocklist-dialog.tsx`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/profile-info-dialog.tsx`（部分弹窗）
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/import-profile-dialog.tsx`

一句大白话：

**这是前端弹窗语义 warning，不是 MCP 核心故障。**

---

## 1.2 `The width(-1) and height(-1) of chart should be greater than 0`

这是 **Recharts 图表容器尺寸异常 warning**。

一般表示：

- 图表在弹窗里
- 弹窗还没真正显示出来
- 或容器此时宽高还没测出来
- `ResponsiveContainer` 先渲染了图表
- 于是拿到了 `-1 / 0` 尺寸

从当前代码看，高概率来自：

- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/traffic-details-dialog.tsx`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/bandwidth-mini-chart.tsx`

它的影响：

- **通常不影响 MCP 页面自动化主线**
- 但会说明某些图表在“隐藏态/初次打开”时存在渲染时机问题
- 如果你后面要做 UI 稳定性测试，这个要记一笔

一句大白话：

**图表组件在容器尺寸还没准备好时先渲染了，所以报 warning。**

---

# 2. 测试总目标

本方案要验证四件事：

1. **同一套 MCP 工具** 是否能同时控制两个内核；
2. 两内核在 **启动、导航、DOM 操作、等待、frame/tab、storage、console/network** 上是否一致；
3. Chromium 轻模式下的 **随机 seed + 固定简单指纹项** 是否真的共同生效；
4. Camoufox 与 Chromium 在 **运行时页面表现** 上是否有功能性差异。

---

# 3. 测试分层

本方案分三层：

## 3.1 P0：阻塞级（必须通过）

这些不过，就不能说 MCP 双内核可用：

- MCP 连接初始化
- profile 启动 / 关闭
- 页面导航
- 截图
- 获取页面信息
- 获取页面内容
- 点击 / 输入 / 键盘事件
- wait_for_selector / wait_for_text / wait_for_navigation
- list_tabs / new_tab / switch_tab / close_tab
- list_frames / get_frame_tree / find_frame
- localStorage / sessionStorage 读写
- console capture
- network capture

## 3.2 P1：重要增强（建议通过）

- upload / download
- checkbox / select / hover / focus / scroll
- locator 多类型（css / text / role / label / placeholder / xpath / testid）
- response body 抓取
- recipe / batch workflow 基础执行
- get_profile_fingerprint / update_profile_fingerprint / generate_sample_fingerprint

## 3.3 P2：非阻塞项（可后测）

- proxy / vpn 复杂联动
- sync / multi-follower
- traffic stats 图表 UI 观感
- 团队锁 / 商业功能边界

---

# 4. 测试环境要求

## 4.1 必备

- JnmBrowser 正常启动
- MCP server 正常监听
- 至少有两个 profile：
  - 一个 `browser = chromium`
  - 一个 `browser = camoufox`
- 两个 profile 都可正常启动

## 4.2 推荐准备的测试 profile

建议准备：

- `mcp-chromium-test`
- `mcp-camoufox-test`

配置建议：

- 不挂代理（先做最纯净链路）
- 不开扩展
- 不开 VPN
- 非 ephemeral
- 指纹配置尽量最小化

之后再补做：

- 带代理版本
- 开启随机指纹版本
- 带固定指纹项版本

---

# 5. 推荐测试页面

为了让 AI 通过 MCP 稳定复现，推荐用 **可控页面 + 少量公开稳定页面** 的组合。

## 5.1 第一优先：本地测试页（推荐）

最好准备一个本地静态测试页，至少包含：

- 输入框
- 按钮
- checkbox
- select
- iframe
- 打开新标签按钮
- localStorage / sessionStorage 按钮
- console.log / console.error 按钮
- fetch 请求按钮
- 文件上传 input
- 文件下载链接

如果后续要做更严谨验证，建议单独做 `mcp-test-harness`。

## 5.2 第二优先：JnmBrowser 自己的界面

可以测试：

- profile 列表页
- 新建 profile 弹窗
- 编辑指纹页
- 代理管理页
- 流量详情页

优点：

- 离业务最近
- 能直接发现真实 UI 问题

缺点：

- 页面更复杂
- 受业务状态影响更大

## 5.3 第三优先：简单公开站点

可以少量使用：

- `https://example.com/`
- `https://httpbin.org/forms/post`
- 其它结构简单、稳定的页面

注意：

- 不要把公共站点作为唯一真相来源
- 以免把外部波动误判成 MCP 故障

---

# 6. 双内核测试矩阵

下面每一项都要 **Chromium 跑一遍，Camoufox 再跑一遍**。

---

## 6.1 A 组：MCP 连接与工具面

### A1. MCP initialize / tools/list

目标：

- 确认 MCP 连接正常
- 工具列表完整返回

通过标准：

- `initialize` 成功
- `notifications/initialized` 成功
- `tools/list` 成功
- 工具数量与当前版本一致

---

## 6.2 B 组：Profile 生命周期

### B1. list_profiles / get_profile

目标：

- 找到两个测试 profile

通过标准：

- 能正确列出 Chromium / Camoufox profile
- browser 字段正确

### B2. run_profile

目标：

- 两内核都能正常启动

通过标准：

- 启动返回成功
- `get_profile_status` 显示 running
- 没有崩溃 / 卡死 / 无 CDP 响应

### B3. kill_profile

目标：

- 两内核都能正常关闭

通过标准：

- profile 停止成功
- `get_profile_status` 回到 stopped

---

## 6.3 C 组：页面基础自动化

### C1. navigate

目标：

- 访问目标页

通过标准：

- `navigate` 成功
- `get_page_info.url` 正确
- `document.readyState` 最终是 `interactive` 或 `complete`

### C2. get_page_info

目标：

- 获取页面基础信息

通过标准：

- title / url / readyState 正确返回

### C3. screenshot

目标：

- 页面截图成功

通过标准：

- 返回 image 内容
- 图片非空白
- 目标元素可见

### C4. get_page_content

目标：

- 能抓文本 / HTML

通过标准：

- 能取整页文本
- 能取整页 HTML
- 能按 selector / locator 取内容

---

## 6.4 D 组：交互动作

### D1. type_text

目标：

- 输入框输入文本

通过标准：

- 输入成功
- 输入值与预期一致

### D2. click_element

目标：

- 点击按钮 / 链接

通过标准：

- 点击后状态发生变化
- 若触发跳转，则最终 URL 正确

### D3. press_key / press_hotkey

目标：

- 键盘事件工作正常

通过标准：

- Enter 可提交表单
- 快捷键可触发预期动作

### D4. hover_element / focus_element / scroll_to / scroll_element_into_view

目标：

- 辅助交互稳定

通过标准：

- hover 后浮层出现
- focus 正确聚焦
- scroll 后目标进入视口

### D5. check_checkbox / uncheck_checkbox / select_option

目标：

- 表单组件可控

通过标准：

- checkbox 状态切换正确
- select 值变化正确

---

## 6.5 E 组：Wait / Locator

### E1. wait_for_selector

目标：

- 等待元素出现/可见

通过标准：

- 常规异步加载场景成功
- timeout 时错误码合理

### E2. wait_for_text

目标：

- 等待文本出现

通过标准：

- 动态文本成功命中

### E3. wait_for_navigation

目标：

- 点击后跳转等待稳定

通过标准：

- 跳转完成后返回成功

### E4. locator 全类型

至少测试：

- `css`
- `text`
- `role`
- `label`
- `placeholder`
- `xpath`
- `testid`

通过标准：

- 能定位到唯一目标
- 多匹配时歧义错误合理
- `nth` 能正确选中

---

## 6.6 F 组：Tab / Window

### F1. new_tab / list_tabs

目标：

- 创建新标签并可见

通过标准：

- 新 tab 出现在列表里

### F2. switch_tab / get_active_tab

目标：

- 正确切换活动页

通过标准：

- 切换后 active tab 变化
- `get_page_info` 对应新的活动页

### F3. close_tab

目标：

- 标签页能关闭

通过标准：

- 目标 tab 消失
- 不影响主 tab

---

## 6.7 G 组：Frame / iframe

### G1. get_frame_tree / list_frames

目标：

- 能看到 frame 结构

通过标准：

- 主 frame + 子 frame 信息完整

### G2. find_frame

目标：

- 按 index / name / url / id 定位 frame

通过标准：

- 定位结果正确

### G3. 在 iframe 内执行 click/type/get_text

目标：

- frame 上下文切换生效

通过标准：

- 子 frame 内交互成功
- 不误作用于主文档

---

## 6.8 H 组：Storage / Cookie

### H1. set_local_storage / get_local_storage / clear_storage

目标：

- localStorage 可写可读可清

通过标准：

- 写入后能读到
- 清除后消失

### H2. get_session_storage

目标：

- sessionStorage 能读到预期值

### H3. read_profile_cookies / export / import / copy

目标：

- cookie 工具链可用

通过标准：

- 可读
- 可导出
- 可重新导入

---

## 6.9 I 组：Console / Network

### I1. start_console_capture / get_console_logs / clear_console_logs

目标：

- 能抓 console.log / warn / error

通过标准：

- 触发三类日志后能被读到
- clear 后列表清空

### I2. start_network_capture / get_network_requests / clear_network_requests

目标：

- 能抓 fetch / XHR / document 请求

通过标准：

- 请求 URL / method / status 正确
- clear 后列表清空

### I3. get_response_body

目标：

- 能抓某条请求的响应体

通过标准：

- response body 内容可解析
- text/json 类型正确

---

## 6.10 J 组：下载 / 上传

### J1. set_file_input / upload_file

目标：

- 文件 input 能正确注入文件

通过标准：

- 页面能识别已选文件
- 表单提交后服务端收到文件

### J2. download_file / wait_for_download

目标：

- 下载流程打通

通过标准：

- 下载成功
- `wait_for_download` 返回文件路径/元信息

---

## 6.11 K 组：指纹相关（双内核都要测，但预期不同）

### K1. get_profile_fingerprint

目标：

- 能读取当前 profile 的指纹配置

### K2. update_profile_fingerprint

目标：

- 能写入固定字段

建议测试值：

- `language`
- `languages`
- `timezone`
- `platform`
- `brand`
- `brandVersion`
- `hardwareConcurrency`
- `deviceMemory`
- `webglVendor`
- `webglRenderer`

### K3. generate_sample_fingerprint

目标：

- 两内核都能生成样例指纹

### K4. 运行态 spot-check

在页面里用 `evaluate_javascript` 读取：

```js
({
  userAgent: navigator.userAgent,
  platform: navigator.platform,
  language: navigator.language,
  languages: navigator.languages,
  hardwareConcurrency: navigator.hardwareConcurrency,
  deviceMemory: navigator.deviceMemory,
  timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
})
```

通过标准：

- **Chromium 轻模式**：
  - 固定项应该生效
  - 开启随机 seed 后，固定项继续保留
- **Camoufox**：
  - 指纹字段与其自身实现一致
  - 不要求和 Chromium 完全相同，但要自洽

---

## 6.12 L 组：Recipe / Workflow

### L1. validate_recipe / preview_recipe

目标：

- recipe schema 和预览正常

### L2. run_recipe

目标：

- 单 profile 工作流能跑通

建议 recipe：

- 打开页面
- 等待元素
- 输入文本
- 点击按钮
- 截图
- 获取页面文本

### L3. run_batch_profile_workflow

目标：

- 同一 workflow 同时跑在 Chromium / Camoufox 两个 profile

通过标准：

- 两条任务都能返回结果
- 结果结构一致
- 失败时能准确标记是哪个 profile 失败

---

# 7. AI 测试执行提示词（可直接喂）

下面是推荐给 AI 的主提示词。

---

## 7.1 主提示词

```text
你现在是 JnmBrowser MCP 双内核测试执行器。

你的目标不是展示功能，而是找问题。
请你通过 MCP 分别对 Chromium 测试 profile 和 Camoufox 测试 profile 执行同一套测试矩阵，并输出：

1. 每一步调用了哪个 MCP 工具
2. 入参是什么
3. 返回结果是否符合预期
4. Chromium 与 Camoufox 是否行为一致
5. 如果不一致，差异是什么
6. 是否属于阻塞问题（P0/P1/P2）
7. 最终生成一份结构化测试报告

执行要求：
- 先测试 Chromium，再测试 Camoufox
- 每一大类测试结束后，立即写阶段结论
- 不要因为单项失败就中断全部测试，除非 profile 无法启动
- 遇到页面 warning、console warning、network 异常，都要记录
- 对“看起来成功但结果不对”的情况，要判定为失败
- 重点关注：启动、导航、点击、输入、wait、tab、frame、storage、console、network、下载上传、指纹运行态
```

---

## 7.2 指纹专项提示词

```text
请专门验证双内核的指纹相关能力。

测试要求：
1. 读取两个测试 profile 的 fingerprint 配置
2. 对 Chromium profile 写入以下固定项：
   - language
   - languages
   - timezone
   - hardwareConcurrency
   - deviceMemory
   - brand
   - brandVersion
3. 开启“每次启动随机指纹”
4. 启动 profile
5. 在页面里读取 navigator / Intl 运行态信息
6. 判断：固定项是否仍然生效，seed 派生特征是否重新生成
7. 再对 Camoufox 做同类 spot-check
8. 输出：两内核差异、当前设计预期、是否是 bug
```

---

## 7.3 Console / Network 专项提示词

```text
请只验证 console capture 和 network capture。

要求：
- 打开页面
- 启动 console capture
- 触发 console.log / warn / error
- 拉取日志并校验内容
- 启动 network capture
- 触发 fetch / document 请求
- 拉取请求列表
- 读取其中一条 response body
- 清空 capture 后再次确认为空
- Chromium 与 Camoufox 各跑一遍
- 输出差异与失败项
```

---

# 8. 推荐执行顺序

建议不要一口气全测到底，按下面顺序来：

## 第 1 轮：双内核冒烟测试

只测：

- run_profile
- navigate
- get_page_info
- screenshot
- get_page_content
- click_element
- type_text
- wait_for_selector
- kill_profile

目的：

- 先确认两条主链路都没死

## 第 2 轮：交互与观测

再测：

- locator 全类型
- tab
- frame
- storage
- console capture
- network capture

## 第 3 轮：增强能力

再测：

- upload / download
- recipe
- batch workflow
- cookies
- 指纹专项

## 第 4 轮：带代理 / 带随机指纹 / 带复杂配置回归

最后再测：

- 代理 profile
- 开启随机指纹 profile
- 指定固定简单指纹字段 profile

---

# 9. 通过 / 失败判定标准

## 9.1 通过

必须同时满足：

- MCP 调用成功
- 返回值结构正确
- 页面真实结果正确
- Chromium / Camoufox 都能完成

## 9.2 失败

以下任一情况都记失败：

- 工具直接报错
- 工具返回成功，但页面实际没变化
- locator 找错对象
- wait 逻辑不稳定
- frame / tab 上下文错乱
- console / network 抓不到应有内容
- 指纹配置写入了，但运行态没体现

## 9.3 允许差异但不一定算 bug

以下需要“记录差异”，不一定直接算缺陷：

- 两内核的默认 `userAgent` / `platform` 风格不同
- 两内核部分指纹字段表现不同
- 某些浏览器原生能力差异（只要功能不坏）

---

# 10. 最终报告格式

建议 AI 最后按这个格式输出：

```text
# MCP 双内核测试报告

## 测试环境
- MCP 地址：
- JnmBrowser 版本：
- Chromium profile：
- Camoufox profile：

## 总结
- P0 通过数 / 失败数
- P1 通过数 / 失败数
- P2 通过数 / 失败数
- 是否可以认定“双内核 MCP 主线可用”

## Chromium 测试结果
- 逐项结果

## Camoufox 测试结果
- 逐项结果

## 差异对比
- 同项能力差异列表

## 缺陷清单
- 编号
- 现象
- 复现步骤
- 影响范围
- 优先级

## 建议修复顺序
- 先修哪些
- 哪些可以后放
```

---

# 11. 当前不建议纳入阻塞项的内容

以下内容目前**不建议作为本轮双内核 MCP 是否可用**的阻塞标准：

- `donut-sync` 云端链路
- 图表 UI warning
- Radix Dialog 无障碍 warning
- 某些商业/团队功能边缘路径
- 外部站点偶发网络波动

这些可以记录，但不要把它们和“MCP 自动化主线失败”混为一谈。

---

# 12. 一句话执行建议

如果你现在就想让另一个 AI 开始测，最实用的顺序是：

1. 先拿这份文档 + `MCP_当前功能测试文档.md`
2. 先做 **Chromium 冒烟**
3. 再做 **Camoufox 冒烟**
4. 然后补 **console/network/frame/tab/storage**
5. 最后做 **指纹专项 + batch workflow**

这样最省时间，也最容易先把真正阻塞问题抓出来。
