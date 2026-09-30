# GUI — 第四个前端的设计契约

> 状态：设计稿（未实现）。SPEC §4.10 渲染面 + v0.5+ GUI 条目的展开。
> 原则不变：**壳只是另一种 client**——GUI 与 REPL/TUI/ACP 共用同一套
> `Context`/`Observer` 接缝，不为它开特权通道。

## 1. 路线：先 serve，再套壳

```
阶段 1  sunmao serve        localhost HTTP+WebSocket → 浏览器即 GUI
阶段 2  同一套 web 前端      塞进 Tauri v2 壳 → 独立桌面 app
```

- 前端是纯 web 资产（HTML/JS/CSS），两个阶段零重写。
- Electron 已否：Node 主进程与"内核 Rust、壳也 Rust"的立场冲突，
  且只能当外部 client。Tauri 阶段可直接 `use sunmao_core` 同进程
  嵌内核——GUI 真正变成第四个前端。
- 反向贡献面：Tauri 生态实践中若踩到上游 bug（IPC/webview 层），
  修复可 PR 回 tauri-apps——按对外贡献纪律走。

## 2. 聊天区：Markdown 流，不是 HTML

流式物理决定了格式：模型逐 token 吐字，**部分渲染必须优雅降级**。

- 半截 Markdown 仍是合法文本（顶多星号未闭合）；
- 半截 HTML 是畸形 DOM，浏览器强行闭合标签 → 中间态布局不可控地闪。

所以聊天正文走 Markdown→DOM 增量渲染（与 TUI `md.rs` 同语义）。
内联 ` ```html ` 代码块**默认显示为代码**——富内容一律走
HtmlArtifact 正式通道（可审计、可批注、进事件日志），不开内联
渲染旁路。

## 3. 岛屿：sandboxed iframe（MCP Apps 对齐）

富内容以**岛屿**嵌在聊天流中，模式对齐 SEP-1865 / Apps SDK：

```
[Markdown 文字流]
[工具调用卡片 — GUI 原生组件]
┌─ <iframe sandbox> ─┐   artifact 岛屿：加载 .sunmao/artifacts/{n}.html
│  完整 HTML 工件      │   sandbox=CSP 禁网/无同源/无脚本桥出
└─────────────────────┘
[Markdown 继续流]
```

- **资源传递走路径不走载荷**：artifact 本来就是文件，前端经
  `GET /artifacts/{name}`（serve 阶段）或自定义协议（Tauri 阶段）
  拉取——不复制 HTML 进 IPC。
- **沙箱声明制**（借 MCP Apps `_meta.ui.csp` 模型）：默认全禁——
  无外网、无父页 DOM、无本地存储。工件要请求外部域须在 frontmatter
  声明，GUI 按声明放 CSP。
- **版本化**：同名 `HtmlArtifact` 覆盖写产生新版本；岛屿可回看
  历史版本（会话日志里的 Artifact 事件序列即版本链）。

## 4. 批注回流：postMessage → state.json（文件仍是事实源）

对齐 MCP Apps `ui/message` 的语义，但**真相留在文件**：

```
iframe 内用户批注 → postMessage → GUI host → 追加
.sunmao/artifacts/{name}.state.json → agent 下轮 Read
```

- 与 `/annotate` 命令、`+notes` 标记同一文件协议——GUI 只是给
  它加了个好看的输入口，不引入第二个事实源。
- 批注全部可审计（state.json 纯文件 + 会话事件）。

## 5. MCP Apps host：又一面免费生态

在 iframe bridge 上实现 `ui/*` JSON-RPC 子集后，**第三方 MCP
server 返回的 `ui://` 资源可直接嵌入**（SPEC §4.10 预留）：

- 最小子集：`ui/initialize`、`ui/notifications/tool-input`、
  `ui/notifications/tool-result`、`tools/call`（转回 `ctx.mcp`）、
  `ui/message`（→ 会话注入）。
- `tools/call` 从岛屿发出时**复用同一权限/审批闸**——UI 不是
  绕过闸门的后门。
- 我们的 artifact 机制比 MCP Apps 简单（模型产文档而非 server
  产模板），但 host 面按标准做 = 双向都赢。

## 6. 信息架构

```
┌──────────────────────────────────────────┐
│ 会话列表   │  transcript 流（markdown+岛屿）│ dataflow │
│ (sessions) │  ─────────────────────────── │  ⚡% 缓存  │
│            │  工具块 · 审批卡 · artifact    │  tokens   │
│            │  ─────────────────────────── │  成本     │
│            │  ❯ 输入区（slash 菜单同 TUI）  │          │
└──────────────────────────────────────────┘
```

- 工具块/审批卡/slash 菜单**语义与 TUI 对齐**——前端换皮不换模型，
  同一份 `LiveEvent` 流喂两个壳。
- dataflow 面板复用 `dataflow.rs` 的聚合（tokens/cache_hit/成本）。
- 会话面板 = 事件日志列表 + resume/fork（同一 SessionLog API）。

## 7. serve 协议草案

```
WS  /session/{id}/events    ← LiveEvent 流（observer 扇出）
POST /session/{id}/prompt   → 用户输入
POST /session/{id}/approval → 审批裁决回执
GET  /artifacts/{name}      → artifact 文件（含历史版本 ?rev=）
POST /artifacts/{name}/annotate → state.json 追加
GET  /sessions              → 会话清单
GET  /dataflow              → 聚合仪表数据
```

WebSocket 只载 `LiveEvent`（与 TUI/ACP 同源）——**不发明第三套
事件方言**。大对象一律路径引用（见 §3）。

## 8. Tauri 阶段的接缝

- `sunmao-core` 作为 crate 嵌入 Tauri core 进程——GUI 不再过
  HTTP，直接 `Context` + `Observer`；serve 的 WS 协议退化为
  `Channel` 消息（同形 JSON）。
- capability 白名单收紧：前端只有 invoke 白名单命令的权限；
  artifact iframe 依旧 sandboxed。
- 已知坑备忘：Tauri IPC 为 JSON，大载荷慢 → 大对象坚持文件路径
  引用（§3 已是此设计）；WebView2 高并发缓冲问题与我们的单会话
  流量不在同一量级。

## 9. 明确不做

- 不做内联 HTML 聊天流（§2 的物理约束）。
- 不做 iframe→内核的直连通道——一切经 GUI host 中转进闸门。
- 不做 pi 式 `ctx.ui` 扩展面（进程边界外的东西不给 UI 特权）。
- 阶段 1 不做账号/多用户——localhost 单用户，绑定 127.0.0.1。
