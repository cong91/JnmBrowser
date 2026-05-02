# Wayfern 遗留逻辑清理准备书

目标：为后续**彻底删除 Wayfern 遗留逻辑**做准备，把当前项目中残留的 Wayfern 命名、兼容层、付费/条款/token、前端文案、测试和文档，拆成可执行清理阶段。

> 本文档当前是“准备书”，不是立即全删执行记录。  
> 现阶段重点是：**先盘点、先定边界、先决定替代命名，再按阶段删。**

---

## 一、当前判断

当前项目里，Wayfern 已经不是实际自动化主内核：

- 实际自动化主链路：`fingerprint-chromium 142`
- Wayfern 现在主要还残留在：
  - **命名**
  - **配置结构**
  - **兼容层**
  - **云端 token / terms**
  - **前端 UI 文案**
  - **MCP / API 描述**
  - **测试与文档**

所以后续“彻底删除 Wayfern”本质上不是删一个小模块，而是做一次：

1. **内核命名统一**
2. **兼容层下沉/移除**
3. **云端 Wayfern 专属能力摘除**
4. **前后端 schema 改名**
5. **旧 profile 数据迁移或兼容收口**

---

## 二、建议的最终命名目标

这是后续动手前必须先定的事情。

### 建议方案

- 内部 canonical browser key：`chromium`
- 内部 config 名：`chromium_config`
- 内部 manager 名：`chromium_manager.rs`
- UI 展示名：`Chromium`

### 为什么不继续用 `wayfern`

因为继续保留 `wayfern`：

- 会让代码语义长期失真
- 会把旧付费/terms/token 误导继续带着
- 会让后续维护者误以为仍依赖 Wayfern 服务端

### 为什么不建议把最终 canonical key 直接定成 `fingerprint-chromium`

因为它太长，后续会污染：

- profile schema
- MCP/API 参数
- 前端类型
- 表格/筛选/文案

更适合：

- **安装目录/下载目录**
- **实现层**

不适合作为全局业务 browser key。

### 结论

建议：

- **业务层叫 `chromium`**
- **实现层仍可指向 `fingerprint-chromium`**

---

## 三、当前遗留点盘点

下面只列后续清理最关键的部分。

### A. 后端运行时 / 兼容层

高优先级文件：

- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/browser.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/browser_runner.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/wayfern_manager.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/profile/manager.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/profile/types.rs`

当前状态：

- `BrowserType::Wayfern` 仍存在
- `WayfernBrowser` 仍是 Chromium runtime 的包装名
- `wayfern_config` 仍是 profile 元数据字段
- `WayfernManager` 实际上已经在管理 fingerprint-chromium
- `JNM_USE_FINGERPRINT_CHROMIUM` 开关还在兼容 legacy 路径

后续要做：

- `WayfernBrowser` → `ChromiumBrowser`
- `WayfernManager` → `ChromiumManager`
- `wayfern_config` → `chromium_config`
- `BrowserType::Wayfern` → `BrowserType::Chromium`

---

### B. 云端 token / 条款 / 旧付费链路

高优先级文件：

- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/cloud_auth.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/wayfern_terms.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/api_server.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/lib.rs`

当前状态：

- `CloudAuthManager` 里还有 `wayfern_token`
- API 里还有：
  - `/v1/wayfern-token`
  - `/v1/wayfern-token/refresh`
- 本地还有 Wayfern 条款检查 / 接受逻辑
- 前端还有 terms hook / terms dialog

后续要做：

- 删除 `wayfern_token` 状态与刷新接口
- 删除 `wayfern_terms.rs`
- 删除相关 tauri commands
- 删除 Wayfern terms 弹窗、hook、设置入口

注意：

- 这部分是**真正“彻底删 Wayfern”**的核心标志之一
- 但要在确认**再也不依赖 Wayfern 云接口**之后再删

---

### C. MCP / API / 自动化能力描述

高优先级文件：

- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/mcp_server.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/api_server.rs`

当前状态：

- MCP 描述里大量写着：
  - “Wayfern and Camoufox profiles”
  - `browser must be 'wayfern' or 'camoufox'`
- schema 里还暴露 `wayfern`
- fingerprint 读写仍走 `wayfern_config`

后续要做：

- MCP / API 改成：
  - `chromium` / `camoufox`
- 文案统一去掉 Wayfern
- 若需要兼容旧调用：
  - 短期可接受传入 `wayfern`
  - 运行时映射到 `chromium`
  - 但响应输出逐步统一为 `chromium`

---

### D. 前端类型 / 表单 / 组件 / hook

高优先级文件：

- `/Users/lijinmu/Documents/New project/donutbrowser/src/types.ts`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/wayfern-config-form.tsx`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/wayfern-terms-dialog.tsx`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/create-profile-dialog.tsx`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/app/page.tsx`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/hooks/use-wayfern-terms.ts`

当前状态：

- `WayfernConfig`
- `WayfernFingerprintConfig`
- `WayfernOS`
- `WayfernConfigForm`
- `WayfernTermsDialog`
- `useWayfernTerms`

后续要做：

- 类型改名：
  - `WayfernConfig` → `ChromiumConfig`
  - `WayfernFingerprintConfig` → `ChromiumFingerprintConfig`
  - `WayfernOS` → `ChromiumOS`
- 组件改名：
  - `wayfern-config-form.tsx` → `chromium-config-form.tsx`
  - `wayfern-terms-dialog.tsx` 删除
- hook 改名/删除：
  - `use-wayfern-terms.ts` 删除

---

### E. 前端文案 / 多语言

高优先级文件：

- `/Users/lijinmu/Documents/New project/donutbrowser/src/i18n/locales/en.json`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/i18n/locales/es.json`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/i18n/locales/fr.json`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/i18n/locales/ja.json`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/i18n/locales/pt.json`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/i18n/locales/ru.json`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/i18n/locales/zh.json`

当前状态：

- 多语言里大量直接显示 `Wayfern`
- terms 文案仍存在
- “Powered by Wayfern” 类文案仍存在
- “仅支持 Wayfern/Camoufox” 类文案仍存在

后续要做：

- 全量替换为：
  - `Chromium`
  - `Chromium / Camoufox`
- 删除 `wayfernTerms` 整个命名空间
- 删除 `updateWayfernConfigFailed` 等旧 key
- 改成新的 `chromium` 相关 key

---

### F. 下载 / 安装 / 解压 / 版本管理

高优先级文件：

- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/api_client.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/browser_version_manager.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/downloader.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/extraction.rs`

当前状态：

- 有不少“Wayfern manifest / Wayfern version / wayfern install dir”式命名
- 实际逻辑已经偏向 fingerprint-chromium
- 但变量名、测试名、错误文案还没完全清理

后续要做：

- 内部命名统一改成：
  - `chromium`
  - `fingerprint_chromium`
- 测试名去掉 `wayfern_*`
- 错误文案去掉 Wayfern

---

### G. Cookie / 导入导出 / 同步 / 扩展 / 资料展示

中优先级文件：

- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/cookie_manager.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/profile_importer.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src-tauri/src/synchronizer.rs`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/profile-info-dialog.tsx`
- `/Users/lijinmu/Documents/New project/donutbrowser/src/components/import-profile-dialog.tsx`

当前状态：

- 部分逻辑还是写“Wayfern/Camoufox”
- 浏览器兼容性描述还没统一

后续要做：

- 全部改成：
  - `Chromium/Camoufox`
- 导入、展示、复制 cookie 等说明同步更新

---

## 四、建议删除顺序

不要一口气大改，建议按下面顺序。

### Phase A：先改名字，不删功能

目标：

- 先把**业务名**从 `wayfern` 切到 `chromium`
- 暂时保留兼容输入

动作：

- 后端类型/枚举/响应增加 `chromium`
- 前端类型和组件重命名
- UI 文案全部改成 Chromium
- MCP/API 文案改成 Chromium

这一步结束后，用户界面和新代码里就不该再看到 Wayfern。

---

### Phase B：保留兼容层，只做旧数据映射

目标：

- 允许旧 profile 继续读
- 但新写入尽量使用新字段

动作：

- 读旧字段：
  - `wayfern_config`
  - `browser = "wayfern"`
- 写新字段：
  - `chromium_config`
  - `browser = "chromium"`

必要时加一次 profile 元数据迁移脚本。

---

### Phase C：删除 Wayfern token / terms / 旧云接口

目标：

- 把 Wayfern 付费/条款/云接口彻底摘掉

动作：

- 删 `wayfern_terms.rs`
- 删 `useWayfernTerms`
- 删 `WayfernTermsDialog`
- 删 `/v1/wayfern-token` 与 refresh
- 删 `CloudAuthManager` 里的 `wayfern_token`

这一步完成后，项目就不该再存在“Wayfern 条款/Wayfern token”概念。

### Phase C：当前推进记录（2026-05-02）

- [x] 删除后端 `wayfern_terms.rs`
- [x] 删除 Tauri commands：`check_wayfern_terms_accepted` / `check_wayfern_downloaded` / `accept_wayfern_terms`
- [x] 删除 REST API：`/v1/wayfern-token` 与 `/v1/wayfern-token/refresh`
- [x] 删除 `CloudAuthManager` 中的 `wayfern_token` 状态与刷新逻辑
- [x] 删除前端 `useChromiumTerms` / `useWayfernTerms`
- [x] 删除前端 `ChromiumTermsDialog` / `WayfernTermsDialog`
- [x] MCP / API 开关逻辑不再被 terms/token 阻断
- [ ] 文案/翻译中的历史 `Wayfern terms/token` 残留清理，放到后续文档/翻译清扫阶段

---

### Phase D：删除 runtime 兼容层

目标：

- 不再通过 `WayfernBrowser` / `WayfernManager` 这些旧壳子运行

动作：

- `WayfernBrowser` → `ChromiumBrowser`
- `WayfernManager` → `ChromiumManager`
- 删除 `JNM_USE_FINGERPRINT_CHROMIUM` / `DONUT_USE_FINGERPRINT_CHROMIUM` 回滚开关
- 删除旧 `wayfern` 安装目录解析逻辑

这一步完成后，runtime 彻底只剩 fingerprint-chromium 主线。

---

### Phase E：删旧 schema / 旧测试 / 旧文档

目标：

- 清理最后残留

动作：

- 删除 `browser="wayfern"` 兼容输入
- 删除旧测试名
- 删除旧文档/计划书中的 Wayfern 术语
- 清理翻译残留 key

---

## 五、风险点

### 1. 旧 profile 读不出来

风险最大。

因为现在历史 profile 很可能仍是：

- `browser = "wayfern"`
- `wayfern_config = {...}`

如果直接硬删，不做迁移，旧数据会失效。

### 2. MCP / API 调用方还在发 `wayfern`

如果外部 AI 工作流、测试脚本、MCP recipe 里还写着：

```json
{ "browser": "wayfern" }
```

那直接删会导致调用全挂。

### 3. 前端翻译 key 改名会牵连很多组件

必须全语言同步，不然会出现：

- 缺 key
- 旧 key 未删
- 文案混用

### 4. 云端接口删除过早

如果还有任何地方偷偷依赖 Wayfern token / refresh，就会出“看起来已经脱离，实际还会调用”的隐患。

---

## 六、建议的执行策略

### 建议原则

- **先重命名，后删除**
- **先兼容迁移，后切断旧 schema**
- **先删 UI，后删 runtime 兼容**
- **最后再删旧 API/MCP 输入兼容**

### 不建议

- 一次性全局替换 `wayfern -> chromium`
- 不做迁移直接删旧字段
- 没跑 MCP / API / 前端全链路回归就删 token/terms

---

## 七、建议新增的清理任务书

后续真正执行时，建议再拆成一份任务清单，例如：

1. 命名迁移（前后端类型/组件/文案）
2. profile schema 兼容迁移
3. MCP/API `wayfern` → `chromium`
4. Wayfern terms/token 删除
5. runtime manager/browser 重命名
6. 回归测试
7. 旧兼容层最终摘除

---

## 八、当前结论

当前已经可以确认：

- **Wayfern 不再是自动化主内核**
- **Wayfern 现在主要是“历史命名 + 兼容层 + 云端遗留”**
- **彻底删除是可做的**
- 但必须分阶段推进，不能一把梭

### 当前最合理的下一步

如果正式开删，建议先从：

**Phase A：统一命名（Wayfern → Chromium）**

开始。

这一步风险最低，收益最大，也最容易验证。

---

## 九、当前推进记录（2026-05-02）

> 这里先记录“已经落地的阶段性收口”，避免后续重复排查；不代表整份清理计划已经完成。

### Phase A：统一命名（进行中）

- [x] 前端主入口组件已切到 `chromium-config-form.tsx` / `chromium-terms-dialog.tsx` / `use-chromium-terms.ts`
- [x] 旧 `wayfern-*` 前端入口已降级为兼容 shim
- [x] 前端浏览器显示名 / 图标 / Chromium 判定已统一走 canonical `chromium`
- [x] 前端 cross-OS 与配置弹窗已统一读取 `chromium_config`
- [x] MCP 工具描述与错误文案已改成 `Chromium / Camoufox`
- [x] MCP `list_profiles` / `get_profile` / `get_profile_fingerprint` 对外输出已优先显示 `browser = "chromium"`
- [ ] 仍有部分后端内部类型/文件名保留 `Wayfern*` 命名，暂未进入 runtime 重命名阶段

### Phase B：schema 兼容迁移（进行中）

- [x] `BrowserProfile` 序列化已改为写出 `chromium_config`
- [x] `BrowserProfile` 已停止接受旧字段 `wayfern_config`
- [x] `BrowserProfile.browser` 已改为对外统一序列化为 `chromium`
- [x] 旧 metadata 中 `browser = "wayfern"` 已不再兼容，加载时会直接报错
- [x] 已补单测覆盖 `chromium_config` 写新 / `wayfern` 旧 shape 拒绝
- [x] REST `create_profile` 现在只接收 `chromium_config`
- [x] REST `ApiProfile` 已开始返回 `chromium_config`
- [x] 新 profile / clone / importer 写入的 `browser` 已切到 `chromium`
- [x] runtime / MCP / sync 对 Chromium profile 的判断已切到 canonical `chromium` 主链
- [x] 旧 `browser = "wayfern"` 输入兼容已摘除

### Phase D：runtime 兼容层清理（进行中）

- [x] `src-tauri/src/chromium_manager.rs` 已成为 Chromium runtime 的主实现文件
- [x] `src-tauri/src/wayfern_manager.rs` shim 已删除，runtime 入口不再保留该模块
- [x] `WayfernBrowser` 类型别名已删除，主运行时 struct 统一为 `ChromiumBrowser`
- [x] `WayfernManager` / `WayfernConfig` / `WayfernLaunchResult` 类型别名已删除
- [x] `launch_wayfern` / `stop_wayfern` / `find_wayfern_by_profile` / `launch_wayfern_profile` wrapper 已删除，主调用统一走 `launch_chromium` / `stop_chromium` / `find_chromium_by_profile`
- [x] `JNM_USE_FINGERPRINT_CHROMIUM` / `DONUT_USE_FINGERPRINT_CHROMIUM` 回滚开关已删除，runtime 固定走 fingerprint-chromium
- [x] Browser executable 目录已直接解析到 `binaries/fingerprint-chromium/<version>`，旧 `wayfern -> fingerprint-chromium` 安装目录映射 helper 已删除
- [x] `browser_runner.rs` / `profile/manager.rs` / `profile_importer.rs` 主调用字段已切到 `chromium_manager`
- [x] `BrowserType::Wayfern` 已改成 `BrowserType::Chromium`，且 `from_str("wayfern")` 已删除
- [x] `normalize_browser_name("wayfern")` 兼容归一已删除
- [x] `wayfern_config` 这类 legacy schema 输入壳已从后端兼容边界移除

### Phase E：旧测试 / 旧文案 / 旧文档清扫（进行中）

- [x] 7 个 locale 中残留的 `Wayfern terms` / `Wayfern` 展示文案已改为 `Chromium`
- [x] profile importer 的浏览器展示名已从 `Wayfern` 改成 `Chromium`
- [x] MCP 用户提示已改为只接受 `chromium` / `camoufox`
- [x] 一批 Rust 单测名已去掉 `wayfern_*` 命名，改成 `chromium_*` / `legacy_*`
- [x] `/Users/lijinmu/Documents/New project/donutbrowser/完整内核替换计划书.md` 中引用的旧测试名已同步更新
- [x] 后端 `BrowserProfile` 主字段已切到 `chromium_config`，旧 `wayfern_config` 不再读取
- [x] 下载/版本/自动下载主线已完全切到 canonical `chromium`
- [x] 前端 `BrowserProfile` / cross-OS / 指纹配置弹窗等 fallback 已不再读取 `wayfern_config`
- [x] MCP 工具 schema 展示已改成只暴露 `chromium` / `camoufox`
- [x] `update_wayfern_config` command / method wrapper 已删除，主链只保留 `update_chromium_config`
- [x] `WayfernVersionInfo` 与 `fetch/get/has_wayfern_*` wrapper 已删除，版本下载主线只保留 `chromium_*`
- [x] `wayfern_manager` backend shim 模块已删除，前端 `WayfernConfigForm` shim 也已删除
- [x] `WayfernOS` / `WayfernFingerprintConfig` / `WayfernLaunchResult` 等前端类型别名已删除
- [x] `BrowserVersionManager` 的缓存 / 下载 / 版本查询主线已先做 canonical 归一，legacy `wayfern` 不再写出独立 cache key
- [x] 前端 `config.wayfern.*` 翻译 key 路径已切到 `config.chromium.*`
- [x] 一批仅做功能校验的测试数据已从 legacy `wayfern` 样例切回 canonical `chromium`
- [x] runtime / extraction / process scan 中分散的 `wayfern` 可执行名判断已彻底移除，主链只认 Chromium 新内核
- [x] `browser = "wayfern"` 输入兼容与 `wayfern_config` legacy schema alias 已全部移除
- [x] 旧 profile / 旧配置文件兼容已明确放弃；历史 `wayfern` metadata 不再保证可读
- [ ] `Wayfern遗留逻辑清理准备书.md` / 其他历史设计文档中的术语还未全量重写，避免影响阶段语义，后续再收尾
