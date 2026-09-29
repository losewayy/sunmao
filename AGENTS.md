# AGENTS.md — build, verify, don't-break list

Read this before touching code. sunmao is a Rust agent harness kernel where
the *seams are the architecture* — changes that violate a seam invariant are
bugs even if they compile.

## 隐私红线（最高优先级——先于一切其它规矩）

**开源开源的是代码，不是人生。** 这个仓是公开仓；任何进入它的内容——
commit、git 历史、`.crate` 包体、文档、注释、CI 产物——**一律不得携带
作者私人信息**。违规即事故，无例外。

禁止入库的（出现即红线）：
- 个人动机/履历/求职/面试类文档（`docs/WHY.md` 已因此永久 gitignore）
- 交接底稿/内部工作笔记（`docs/HANDOFF.md` 同上——它们存在本地，永不进仓）
- 本机绝对路径（`F:\projects\`、`D:\worktable\`、`C:\Users\oooo`——
  含代码注释、文档、错误示例、CI 配置）
- 真实姓名、私人邮箱（GitHub noreply 别名除外）、电话、住址
- 未公开的内部项目代号、内部事件复盘
- 任何凭据：API key、token、cookie、`.env`、SSH 私钥

**push 或 `cargo publish` 前强制闸**（一项不过就不许推）：
1. `git log --all --name-only | sort -u` 全历史文件名过一遍——敏感文件名零命中
2. `git grep` 全历史扫：绝对路径盘符、私人代号、履历类词汇
3. `.gitignore` 已覆盖私人文档；**"先删文件再 commit"不算修复**——历史仍携带
4. `cargo package --list` 确认打进 `.crate` 的每个文件都过了闸——
   包体上传后**不可删除**，只能 yank
5. 仓内出现过的隐私内容，唯一根治是**重写历史/删库重建**；追加删除 commit 无效

发现泄露：立即上报，说清泄露面+根治方案，不许"删一个文件就算修好"。

## Workspace

```text
crates/llm/    provider dialects (OAI, Anthropic), hand-rolled SSE parser,
               tool_calls fragment reassembly — we own the wire
crates/core/   Context seam assembly, AgentLoop, session log (event-sourced
               JSONL), tool registry + 10 native tools, hooks dispatcher,
               permissions, approval gate, MCP client, sub-agent Task
crates/cli/    `sunmao` binary — REPL / -p / TUI / ACP / --dataflow / doctor
docs/          SPEC.md (design contract — read first),
               ARCHITECTURE.md (current state),
               CODE-ARCHITECTURE.md (shape rules), CONFIG.md, PROTOCOLS.md,
               TESTING.md
               (WHY.md + HANDOFF.md are maintainer-private, gitignored —
               ask the maintainer, don't recreate them in-tree)
examples/      starter hooks/mcp/permissions/agents/commands files
```

## Commands

```bash
cargo build                          # dev build
cargo test --workspace               # 40 unit tests incl. 4 full loop tests
cargo fmt --all                      # before every commit
cargo clippy --workspace --all-targets -- -D warnings   # CI's strict gate —
                                       # must be zero warnings before push
./target/debug/sunmao --doctor       # env self-check (provider/rg/session dir)
```

CI on every push to main = fmt check + clippy -D warnings + workspace test on
`windows-latest`. A red run emails the maintainer; do not push with lint debt.

## Hard invariants (breaking any of these is a bug)

- **`std::process::Stdio::null()` on MCP child stderr** — protocol stdout must
  stay clean; diagnostics go to tracing/stderr only.
- **ACP stdout is JSON-RPC only** — never println!/eprintln!-into-stdout
  inside `acp.rs`. Logs → `tracing` (stderr).
- **No `MutexGuard` across `.await`** — clone what you need out of the lock,
  drop the guard, then await.
- **Ephemeral vs file-backed SessionLog have identical fold semantics** — both
  reduce `SessionEvent`s into the same `Message` list; keep `session.rs`'s
  two `messages()` paths in lockstep.
- **`deno_task_shell` internals are `!Send`** (`Rc<Cell>` exit codes) — every
  `ShellState`/pipe must be constructed *inside* `tokio::spawn_blocking` +
  `Handle::block_on`, never moved in.
- **Edit tool requires read-before-write**: the `read_paths` ledger on
  `Context` is the enforcement point; new files exempt, existing files must
  have been `Read` first.
- **Permission verdicts are ordered deny > ask > allow > default** —
  `permissions.rs::check()`; a `deny` is a hard refusal, not a prompt.
- **Cancelled flag resets at turn END, not start** — a cancel issued before
  `run_turn` still must take effect; a mid-turn cancel is consumed and the
  next turn starts clean.
- **Tool surface is a dialect too** — `declarations()` shapes go to the
  model verbatim; a bad JSON schema = the tool doesn't exist to the LLM.
- **Product semantics are files, not literals** — prompt sections live in
  `assets/prompt/*.md`, policy tables in `assets/*.txt` (cold-plug rule 6 in
  CODE-ARCHITECTURE.md). Never embed new prose/policy in `.rs` string
  literals.

## Code shape

Structure rules (layering, file-size limits, no-history-rewrite, less-is-more)
live in [`docs/CODE-ARCHITECTURE.md`](docs/CODE-ARCHITECTURE.md). Follow them.

## Commit conventions

- Author: `losewayy <104015127+losewayy@users.noreply.github.com>` — never
  commit agent/tool identity.
- Message shape: `area: short imperative` (`llm:`, `core:`, `cli:`, `acp:`,
  `mcp:`, `hooks:`, `tools:`, `sessions:`, `tui:`, `test+fix:`, `chore:`,
  `docs:`, `refactor:`).
- One logical change per commit; push after every green commit — public
  history is part of the project.

## Provider quirks worth knowing

- DeepSeek emits a separate `reasoning_content` channel → `StreamDelta::Reasoning`.
- Anthropic maps `thinking_delta`→Reasoning, `input_json_delta`→ToolCallFragment
  (arguments accumulate as partial_json, reassembled by `assemble.rs`).
- Malformed tool-call args: `ToolCallAssembler::finish_lenient()` returns the
  call + error map; the loop converts bad calls into failed `ToolResult`s so
  the model can self-correct instead of aborting the turn.
- Retry only on request-establishment failures (connect / 429 / 5xx), backoff
  300/600ms; once deltas flow, no replay.

## Where things live (jump table)

| Looking for | File |
|---|---|
| tool-call dispatch loop | `crates/core/src/agent.rs` |
| session event fold | `crates/core/src/session.rs` |
| permission rules | `crates/core/src/permissions.rs` |
| approval seam | `crates/core/src/approval.rs` |
| prompt assembly | `crates/core/src/prompt.rs` + `assets/prompt/` |
| shell preflight | `crates/core/src/preflight.rs` (spawnfate) |
| risk pattern table | `crates/core/assets/risky-patterns.txt` |
| Claude-contract hooks | `crates/core/src/hooks.rs` |
| MCP client (stdio + HTTP) | `crates/core/src/mcp.rs` |
| tools | `crates/core/src/tool/{mod,fs,shell,search,artifact,webmod}.rs` |
| sub-agents | `crates/core/src/task.rs` + `agents.rs` |
| SSE parser | `crates/llm/src/sse.rs` |
| OAI dialect | `crates/llm/src/oai.rs` |
| Anthropic dialect | `crates/llm/src/anthropic.rs` |
| fragment reassembly | `crates/llm/src/assemble.rs` |
| REPL/flags | `crates/cli/src/main.rs` |
| TUI (CJK-native) | `crates/cli/src/tui/` — `mod` event loop + focus, `app` state, `blocks` transcript, `render` draw, `md` markdown, `theme` palette, `slash` commands |
| ACP server | `crates/cli/src/acp.rs` |
| dataflow report | `crates/cli/src/dataflow.rs` |
| env self-check | `crates/cli/src/doctor.rs` |
