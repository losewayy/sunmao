# AGENTS.md — build, verify, don't-break list

Read this before touching code. sunmao is a Rust agent harness kernel where
the *seams are the architecture* — changes that violate a seam invariant are
bugs even if they compile.

## Workspace

```text
crates/llm/    provider dialects (OAI, Anthropic), hand-rolled SSE parser,
               tool_calls fragment reassembly — we own the wire
crates/core/   Context seam assembly, AgentLoop, session log (event-sourced
               JSONL), tool registry + 10 native tools, hooks dispatcher,
               permissions, approval gate, MCP client, sub-agent Task
crates/cli/    `sunmao` binary — REPL / -p / TUI / ACP / --dataflow / doctor
docs/          SPEC.md (design contract), ARCHITECTURE.md (current state),
               CODE-ARCHITECTURE.md (shape rules), CONFIG.md, PROTOCOLS.md,
               TESTING.md
examples/      starter hooks/mcp/permissions/agents/commands files
```

## Commands

```bash
cargo build                          # dev build
cargo test --workspace               # 20+ unit tests incl. 4 full loop tests
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
| Claude-contract hooks | `crates/core/src/hooks.rs` |
| MCP client (stdio + HTTP) | `crates/core/src/mcp.rs` |
| tools | `crates/core/src/tool/{mod,fs,shell,search,artifact,webmod}.rs` |
| sub-agents | `crates/core/src/task.rs` + `agents.rs` |
| SSE parser | `crates/llm/src/sse.rs` |
| OAI dialect | `crates/llm/src/oai.rs` |
| Anthropic dialect | `crates/llm/src/anthropic.rs` |
| fragment reassembly | `crates/llm/src/assemble.rs` |
| REPL/flags | `crates/cli/src/main.rs` |
| TUI (CJK-native) | `crates/cli/src/tui.rs` |
| ACP server | `crates/cli/src/acp.rs` |
| dataflow report | `crates/cli/src/dataflow.rs` |
| env self-check | `crates/cli/src/doctor.rs` |
