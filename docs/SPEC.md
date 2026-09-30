# sunmao — Harness Kernel Spec

> This file is the design contract — what we build, and the lines we
> deliberately don't cross.

> 状态：v0.2 在库进行中。已落地：SSE/协议栈（OAI + Anthropic 双方言）、事件溯源会话（resume/fork/dataflow）、十原生工具 + Task 子代理、Read-before-Write 闸门、deno_task_shell Bash + 后台 job、compaction、Claude 契约 hooks（含 `.claude/settings*.json` 兼容加载）、MCP client（stdio + HTTP）、ACP v2 server、TUI（CJK 原生、块式 transcript、审批卡、slash 菜单、markdown、tokyonight 主题）、审批闸（风险命令分级问询）、PromptAssembler 分层提示词、shell/preflight（spawnfate 预判）、AGENTS.md/skills 生态加载。
> **sunmao（榫卯）** — joinery by seams; the repo is the contract.

## 1. 定位

**一句话**：一个 Rust 写的 agent harness 内核——**可审计性是地基而不是补丁**，全部扩展面走进程边界，对外说主流方言（Claude 契约），对内一切皆接缝。

**不是什么**：
- 不是"又一个 agent demo"——它的竞品对照物是 dsh / pi / Claude Code 的架构层，不是某个聊天壳
- 不是框架——它是能跑的真实 agent，harness 性质是产品立场
- 不是对任何既有项目的重写——第一阶段独立存在

**立场陈述（README 用）**：
> 别人的 agent 把扩展当住户，我们把扩展当边界。每一个扩展是独立进程：崩溃隔离、语言无关、每一次交互可拦截可审计。会话的唯一事实源是仅追加的事件日志——transcript、回放、数据流审计全是它的免费推论。

## 2. 三条设计纲领

| 纲领 | 内涵 | 出处/对立面 |
|---|---|---|
| **① 解耦核心（seam-first）** | 内核退化为接缝总线：session、tools、agents、agentLoop、systemPrompt、llm、hooks、audit 全是 `ctx` 上可替换的服务，连 agent loop 本体都是插件；**接缝面是我们的自有契约**——dsh 怎么变与我们无关，我们只守自己发布的 SemVer。**冷拔插原则**：能解耦的都解耦，但替换发生在**文件/配置层、进程启动时生效**——不做运行时热拔插。产品语义（prompt 段、策略表）是数据文件不是代码字面量；同名文件替换内置段即拔插机制 | 学 dsh（Cordis）；反经典主流"单体核心+边缘开口" |
| **② 超集兼容（dialect-native）** | hook 引擎 + 方言插件：事件面取各方言并集，Claude JSON-stdio 契约是默认出厂方言；工具命名对齐主流词表 | 反 pi/dsh"灵活但生态薄"（桥少、无事实标准）；目标：rtk/context-mode 的 claude 适配层开箱即跑 |
| **③ 审计地基（audit-native）** | SessionEvent 仅追加日志做唯一事实源；能力缝上挂审批/审计策略；telemetry 默认物理缺席 | 学 dsh 的事件溯源；立场来自此前 harness 项目里摘 telemetry、处理事件错序的教训 |

## 3. 架构总览

```text
┌────────────────────────── frontends ──────────────────────────┐
│  TUI(内置, ratatui)   Electron GUI(后)   第三方 ACP client(白送) │
└──────────────┬───────────────────┬────────────────────────────┘
               │ 同一协议           │ ACP server (JSON-RPC/stdio)
┌──────────────▼───────────────────▼──────── seams(ctx 服务) ────┐
│ ctx.sessions    仅追加 SessionEvent 日志 = 唯一事实源           │
│ ctx.systemPrompt 提示词段+工具schema 组装                       │
│ ctx.llm         provider 适配缝（OAI 兼容首发）                 │
│ ctx.tools       作用域工具注册表 + 受控执行管线                 │
│ ctx.hooks       hook 引擎 + 方言插件注册表                      │
│ ctx.agents      Agent 句柄/注册表 + agent/* 实时事件            │
│ ctx.agentLoop   默认 driver——本身是插件，可替换                  │
│ ctx.systemPrompt PromptAssembler——分段层叠装配（文件即接缝）     │
│ ctx.audit       审批闸门 + 审计策略（tools/*、fs/*、hooks/* 缝） │
└──────────────┬───────────────────┬────────────────────────────┘
               │ 进程边界（扩展面）  │
┌──────────────▼───────────────────▼────────────────────────────┐
│ MCP client │ 子进程hooks(Claude契约) │ JSON-RPC扩展协议(预留)     │
│ 声明式包: SKILL.md / commands / agents / AGENTS.md / plugin.json│
└──────────────────────────────────────────────────────────────┘
```

**组合而非分支**：preset = 插件层叠（`base` → `+tui` → `+acp` → `+strict-audit`…），不是 `--flag` 分支。参考 dsh 的 profile 模型。

## 4. 核心子系统

### 4.1 `sessions` — 事件溯源会话日志

- 唯一事实源：**仅追加** `SessionEvent`，持久化到 session 文件
- 事件词汇（持久事实域）：`turn/*`、`step/*`、`system/message`、`user/message`、`assistant/message`、`assistant/attempt`、`tool/call`、`tool/result`
- 推论免费品：transcript 落盘（hook 生态契约要求 `transcript_path`，真实文件）、回放、fork、审计数据流图
- 既有项目经验输入：取消语义、watermark replay fencing、迟到的结果按身份丢弃


**实现校准（dogfood 实测后）**：事件集收敛为 `Started / Message / ToolCall / ToolResult / Compacted / Artifact` 六型——比草案精简，replay 语义等价（assistant 消息内嵌 tool_calls）。Hook 事件面实现为 `SessionStart / SessionEnd / UserPromptSubmit / PreToolUse / PostToolUse / Stop / SubagentStart / SubagentStop` 八点。

### 4.2 `llm` — provider 适配缝

- 首发方言：**OpenAI Chat Completions 兼容**（DeepSeek/Kimi/国产端点一套通吃）
- 手写 SSE 解析 + delta 累积 + `tool_calls` 碎片重组（本项目立身之本：裸写协议栈）
- 缝定义：`trait ProviderAdapter { stream(req) -> Stream<Chunk> }`——第二 provider（Anthropic）是 v0.4+ 的另一个方言插件
- 宽容序列化：第三方端点的脏 payload（`tool_calls` 收 null、`finish_reason` 未知值兜底）——实测教训沉淀

### 4.3 `tools` — 作用域注册表 + 受控执行管线

- 内置工具命名**强制对齐主流词表**：`Bash`、`Read`、`Write`、`Edit`、`Grep`、`Glob`、`WebFetch`、`JobOutput`、`HtmlArtifact`、`Task`——hook matcher 免费命中
- MCP 工具命名空间：`mcp__{server}__{tool}`——`mcp__*` matcher 免费命中
- 执行管线串缝：`pre`(hooks+审批) → `exec` → `post`(hooks) —— 拦截点全部公开给 `ctx.audit` 与 `ctx.hooks`

**能力分档（实现层纪律）**：

| 档 | 机制 | 审计形态 |
|---|---|---|
| 进程内原生 | `Read/Write/Edit/Glob/TodoWrite` = 内核函数调用 | 结构化 `tool_input`，零 shell |
| 受管子进程 | `Grep`→spawn `rg.exe` 式直接进程，不经 shell | 结构化输入 + 可控环境 |
| Shell 逃生门 | `Bash`——模型写命令字符串 | 不透明字符串，**重点审计对象**（管道分拆进审批层） |

规矩：**能做成原生的全部原生，shell 是刻意保留的逃生门而非地基**。

**Edit 成熟细节（抄 grok-build 作业）**：归一化匹配吃空白漂移（`find_normalized_match_positions` 模式）；`old_string` 为空=建文件；Read 行号锚前缀；Read-before-Write 闸门（改已有文件必须先读过）。

**Windows 立场（差异点）**：`Bash` 首选内嵌 `deno_task_shell`（Rust 跨平台 bash 方言解释器）——模型写的 bash 在 Windows 上原样跑，行为逐字节确定、不依赖 Git Bash；覆盖不了的外部命令落回真 shell。工具描述向模型声明当前 shell 方言。

**吸收 FastCtx（yc-duan/fastctx，Apache-2.0）的设计模式——吸模式不吸源码**：

| 它的机制 | 我们的内化 |
|---|---|
| 后台 job + `output.idx` 索引输出（可寻址增量读） | `Bash` 后台模式原生支持：长跑命令输出=可寻址工件，模型按偏移翻页 |
| 许可按能力域分闸（file/shell/replace permits） | `ctx.audit` 的 policy 粒度=能力域而非全局 |
| context-efficient 工具输出（截断/分块为 token 预算设计） | 所有工具返回的内建纪律 |
| 字节级保真 replace | `Edit` 的正确性基线 |

它本体继续以 MCP 形态作为 ② 档受管子进程共存（法律：Apache-2.0+NOTICE 要求署名保留；内核保持自证清白）。顺带：`rmcp` 选型被它实战背书。

### 4.4 `hooks` — 引擎 + 方言插件（兼容宣称的全部根据）

```text
hook engine（核心）
├── 事件面（各方言并集）: PreToolUse / PostToolUse / PostToolUseFailure /
│   UserPromptSubmit / SessionStart / SessionEnd / PreCompact / PostCompact /
│   Stop / StopFailure / SubagentStart / SubagentStop / Notification
├── 方言注册表（缝: hooks/dialect/*）
│   └── claude-dialect（默认出厂）: Claude JSON-stdio 契约
│       ├── 注册源: hooks.json / settings.json hooks 键 / .claude-plugin/plugin.json
│       ├── stdin payload: tool_name/tool_input/session_id/cwd/transcript_path/hook_event_name
│       ├── stdout 协议: permissionDecision / updatedInput / hookSpecificOutput / 退出码
│       └── matcher: 工具名子串+regex
│   └── (预留) gemini-dialect / cursor-dialect / copilot-dialect——字段归一化层
└── 工件: transcript_path 真实落盘; fail-open 语义+审计记录每条 hook 决策
```

**验收试金石**：拿真实生态插件当 conformance fixture——`rtk init` 后跑一次 `Bash`，断言命令被 rewrite；context-mode 的 `hooks.json` 挂上后断言 `PreToolUse` 拦截生效。**兼容不是声称的，是测出来的。**

### 4.5 `agents` + `agentLoop` — 可替换的循环

- `Agent` 公开契约：deliver/cancel/intercept；`agent/*` 实时事件域（`agent/assistant-stream`、`agent/status`、`agent/request`）
- `agentLoop` 默认 driver：input → sessions 开 turn → systemPrompt 组装 → llm 流式 → tools 分发 → 事实追加回日志——**它是插件，不是内核特权层**
- 这保留了 dsh 的"换循环即换产品"能力（极简 loop、PTC code-mode loop、审计严格 loop 都是预设层）

### 4.6 `audit` — 审批与审计

- 审批缝：`tools/*` 执行前可挂 policy（always_ask / auto / full-access 模式；`tool_input` 级规则）
- **`shell/preflight`（差异化原语）**：Bash 命令执行前过 spawnfate 引擎——模拟 Windows spawn 各层（deno_task_shell 的 which 解析→CreateProcess→argv 序列化→目标 argv 拆分），预测命令会死在哪层/被怎么改写；结果进审计日志+警告回模型。spawnfate 同时以 MCP 形态对外发布（MCP client 的 dogfood + "我们给生态供货"的证明）
- 审计流：每一次 hook 决策、每一次外部动作（exec/网络/写盘）、每一次上下文注入——全部进 SessionEvent 日志
- **data-flow 文档是一等功能**：`sunmao dataflow --emit` 从日志生成"什么数据去了哪"的机器可读报告

### 4.7 ACP server — 前端桥 + 生态出口

- 实现 ACP typed contract：`session/new`、`session/prompt`、`session/update` 通知、`session/request_permission` 反向请求
- 白送的生态位：Zed / 任何 ACP client 开箱驱动我们
- TUI 走同一会话 API——壳只是另一种 client

### 4.8 `plugins` — 进程边界扩展协议（预留）

- v1 只定义契约：`sunmao` JSON-RPC extension protocol（stdin/stdout，类似 ACP 但面向扩展能力：注册工具/监听事件/注入上下文）
- v0.5+ 落一个**通用 JS extension host 侧车**：`node extension-host.mjs` 加载 TS/JS 扩展，对本协议暴露——届时 pi-style 生态（含 pi 扩展兼容尝试）由此进入，而不是为 pi 定制

### 4.9 声明式格式加载器

- `SKILL.md`（Agent Skills）、`.claude/commands/*` 风格斜杠命令、`agents/*.md` subagent 定义（frontmatter `model:` 走 `models.json` 路由——角色化模型分配）、`AGENTS.md`、`plugin.json`（含 skills+mcp+hooks 打包）
- 自家 `.devin/skills/` 六件套是**第一天的真实测试集**

### 4.10 `html` — 一等公民的产物格式（"HTML is the new Markdown"）

采纳 Anthropic Claude Code 工程师 Thariq Shihipar 2026-05 提出的范式：**agent 面向人的产出默认是 HTML 工件而非 Markdown**——计划、spec、报告、清单、设计稿。人机同一份文件：agent 读写结构化 DOM，人打开就是可扫读/可交互的页面。

- **三层落地**：
  - **产出约定**：内置工具集含 `HtmlArtifact`（或经 `Write`+约定）——模型产出 `*.html` 工件而非 `plan.md`；skill 可打包 `.html` 资源（SKILL.md 调用面不变，HTML 是载荷）
  - **渲染面**：Electron/web 前端用 **sandboxed iframe** 渲染 artifact（对齐 MCP Apps 的沙箱模型：CSP、无 node 访问、默认禁外网）；TUI 降级为"浏览器打开"/文本提取；ACP 通道传 artifact content block，渲染归 client
  - **交互回流**：HTML 工件可带表单/批注机制（参考 linkc-skills/HTML-Plan：inline annotation + state.json 回写）——人对计划的批注成为下一轮 agent 输入
- **安全**：模型产出的 HTML 是不可信内容——渲染一律 sandboxed，禁用任意 script 权限，默认无外网；审计日志记录 artifact 生成/修改
- **文档政策（本项目自身）**：**调用面文档（SKILL.md、AGENTS.md）保持 Markdown 不动**（生态契约）；**协作面文档（计划、清单、spec、状态页）用 HTML**——本仓的 spec/路线图本身就是第一个 HTML 工件
- **MCP Apps 兼容**：GUI 渲染层预留 iframe-resource 支持——别家 MCP server 返回的 UI 组件我们也能嵌（又一面免费生态）

## 5. 兼容矩阵

| 生态面 | 我方对应物 | 状态 |
|---|---|---|
| MCP servers（消费） | `ctx.mcp` client（stdio+SSE） | v0.3 |
| Claude hooks（契约+注册格式） | `claude-dialect` 默认插件 | ✅ rtk/context-mode 实测（`hooks/live_tests.rs`：真 rtk 二进制改写 + SessionStart/source 载荷） |
| Claude skills/commands/agents | 格式加载器 | v0.3 |
| AGENTS.md | 惯例加载 | v0.3 |
| plugin.json 打包（skills+mcp+hooks） | 加载器 | v0.4 |
| ACP（被别人驱动） | acp-server | v0.3 |
| MCP Apps / UI resources（渲染别家 UI） | sandboxed iframe 渲染层 | v0.5+ |
| Codex/Gemini/Cursor hook 方言 | 归一化层 | v0.4 |
| OpenCode/pi TS 扩展 | JS extension host 侧车 | v0.5+（协议预留，不定制） |
| 自身扩展协议 | `sunmao` JSON-RPC | v0.3 契约，v0.5 宿主 |

## 6. 里程碑

| 版本 | 名称 | 内容 | 验收 | 状态（v0.2 时点） |
|---|---|---|---|---|
| v0.1 | kernel walks | OAI 适配 + SSE + sessions + tools(3个) + agentLoop + TUI REPL | 跑通真实任务 | ✅ 超预期完成（tools 到 10、TUI 也落了） |
| v0.2 | trustworthy | 审批缝 + transcript 工件 + 上下文窗口管理 + 错误恢复/中断续跑 + 审计流 | 子进程崩溃不炸 agent | ✅ 大部分落地——审批闸三前端、compaction、resume/fork、审计事件流；进程崩溃容忍只验过 hook veto |
| v0.3 | ecosystem citizen | MCP client + hooks 引擎 + claude-dialect + 格式加载器 + ACP server | rtk/context-mode 实测 | ✅ 落地（MCP 双 transport、hooks 8 事件、agents/commands/skills/plugin.json、ACP v2）；rtk/context-mode 实测通过 |
| v0.4 | distributed | plugin.json 安装 + 扩展协议宿主 + presets + eval + HTML 工件 | 发布 | 🟡 plugin.json 清单已读；宿主/presets/eval 未做 |
| v0.5+ | open frontier | JS 扩展宿主、第二 provider 方言、GUI、subagents | — | 🟡 Anthropic 方言提前落地（v0.2）、Task 子代理已上线（`spawns:`/`tools:` 白名单 + `run_in_background` 异步扇出 + `model:` 路由）；JS 宿主/GUI 未动 |

## 7. 非目标（v1 明确不做）

- in-process 脚本扩展（那是 pi 的路，与进程边界立场冲突）
- GUI（Electron 前端属 v0.5+，且需干净重写）
- MCP server 模式（让我们被别的 agent 调用——v0.5+ 再议）
- subagents / plan mode（学 pi 的纪律：能做扩展的不进核心；但 `SubagentStart/Stop` 事件面先留好）
- 自研 marketplace（装插件=git clone/路径，不做中心化商店）

## 8. 技术选型

- **语言**：Rust（定盘）——tokio + reqwest + serde + clap；TUI 用 ratatui
- MCP client：优先 `rmcp`（官方 Rust SDK），不达标再手写 JSON-RPC
- ACP：参照 `agent-client-protocol` Rust crate
- 仓库形态：单仓 cargo workspace——`crates/core`（seams+engine）、`crates/dialect-claude`、`crates/acp-server`、`crates/tui`、`crates/mcp-client`、`crates/extension-proto`
- 发布纪律：README+GIF+LICENSE(MIT)+CHANGELOG+CI+`docs/design/*` 设计决策留痕——沿用本家发布标准

## 9. 实现纪律（Less is more，操作化）

seam-first 的最大风险是自反的：**接缝本身就是抽象税**。纪律：

1. **缝要挣出自己的存在**——只有真实存在或近期确定会出现的第二个实现，才配开缝；绝不预开空缝"以备将来"。YAGNI 对架构同样适用
2. **核心代码预算制**：v0.1 内核（协议+sessions+tools+loop+TUI）目标 ~3-5k 行；每加一个抽象必须回答"它省掉的代码比它自己多吗"
3. **单事实源**：任何状态只有一个 owner（session 事实只在日志里，工具注册只有一张表）——同一份事实两份存储 = 未来 bug 仓库
4. **组合层叠 > 配置开关**：preset 是插件组合而非 `--flag` 森林
5. **删除是最便宜的维护**：删得掉的代码不写；写过的死路代码当场删不留注释化石
6. **依赖预算**：每个外部 crate 进仓前要过"我们能不能 200 行自己写"的质询——能就自研（协议栈本来就是我们的练手场），不能才引依赖
7. **接缝面即公开契约**：我们仓自己养缝，dsh 的 API 漂移与我们无关；但我们自己的扩展契约一旦发布要守 SemVer——生态信任是攒出来的

## 10. 自研 / 复用清单

> 哪些是手写的、哪些是借的依赖——这张表是代码预算的对账表。

**自研（立身之本，全部手写）**：

| 模块 | 为什么自研 |
|---|---|
| OAI 协议客户端 + SSE 流式解析 + tool_calls 碎片重组 | 本项目的存在理由——"懂底下那台机器"的证明。注：codex 都借 `eventsource-stream`——我们选择写是因为帧层 ≤200 行且需要 tool_calls 碎片重组的控制权；立论记诚实账 |
| SessionEvent 追加日志 + 回放/fork/审计派生 | 审计地基，核心 IP |
| agentLoop driver | 纲领①：循环本体是插件 |
| hook 引擎 + 方言归一化 + claude-dialect | 兼容宣称的全部根据，没有现成轮子可借 |
| 工具集（Read/Edit/Write/Grep/Glob/WebFetch/TodoWrite）+ 归一化匹配 + Read-before-Write | 正确性细节是差异化 |
| 审批/审计策略 + `shell/preflight` + dataflow 报告 | 立场所在；preflight 直接链接自家 spawnfate crate |
| 声明式格式加载器（SKILL.md/commands/agents/plugin.json/hooks.json） | 格式解析本来就不大，自研换零依赖 |
| 上下文管理/压缩 | 可靠性语义是我们的，库给不了 |
| ACP typed contract server | 传输层可借 `agent-client-protocol` crate，语义层自研 |

**复用（不重复造轮）**：

| 依赖 | 用在哪 |
|---|---|
| `tokio` / `reqwest` / `serde` / `clap` | async runtime / HTTP / 序列化 / CLI 解析——基础设施层不造 |
| `rmcp`（官方 Rust MCP SDK） | MCP client——fastctx 实战背书 |
| `deno_task_shell` | `Bash` 工具的跨平台方言解释器 |
| `ratatui` | TUI 渲染 |
| `rg.exe`（ripgrep 二进制） | `Grep` 后端，RG_BIN_PATH 式注入（受管子进程②档） |
| `tree-sitter-bash` | `Bash` 命令结构化解析——审批缝把不透明命令字符串变成可分析 AST（抄 codex 作业：它的 `execpolicy` 同款） |
| `agent-client-protocol` crate | ACP 传输/类型基元 |
| 现有生态整体 | MCP servers、Claude 契约 hooks、skills、plugins——**消费，不实现** |

**边界规矩**：上表之外冒出"想自己写"的冲动 → 先过纪律 6；冒出"想加依赖"的冲动 → 先过同一问。两侧对称执法。

## 11. 设计主张速览

| 设计点 | 它证明什么 |
|---|---|
| 裸写 SSE/协议栈 | 懂 agent 底下那台机器 |
| seam-first 内核 | dsh 式可组合架构的 Rust 实现 |
| Claude 方言默认插件 | 让全世界的 claude 适配器免费工作 |
| 事件溯源+审计 | 可靠性当地基的 harness |
| ACP 出口 | 生态位玩家：Zed 开箱驱动 |
| HTML 一等公民 | 产出即 UI |
