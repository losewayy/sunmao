# TESTING.md — what "verified" means here

Two tiers. Unit tests are the regression floor; **live dogfood is the bar** —
the point of this project is that it actually runs, so every landed feature
has (or should gain) a live pass against the real provider stack.

## Unit tests

```bash
cargo test --workspace    # 20+ tests
```

Current coverage:

| Suite | Proves |
|---|---|
| `llm::sse` | incremental SSE framing (split events, CRLF, keepalives, `[DONE]`) |
| `llm::assemble` | tool_call fragment reassembly incl. malformed-JSON rejection |
| `llm::anthropic` | block mapping (system fold, tool_use/tool_result, stream events) |
| `core::agent` (MockProvider) | full loop: plain turn, tool round-trip, cancel, malformed-args feedback |
| `core::tool` | read-before-write gate (deny→read→allow), Edit normalization |
| `core::hooks` | matcher semantics, live exit-2 veto via real subprocess |
| `core::permissions` | deny>ask>allow>default matrix, glob specifiers |
| `core::session` | event fold (message/tool_result/compaction boundary) |

The `MockProvider` in `agent::tests` is the pattern to reuse: `ProviderAdapter`
is a trait, so scripted `Vec<StreamDelta>` queues drive the whole agent loop
deterministically — no network.

## Live dogfood (the bar)

Provider fixture for local runs:

```bash
--base-url http://127.0.0.1:7863/v1 --api-key your-api-key-here \
--model global:deepseek-v4.1-flash        # or SUNMAO_* env vars
```

Verified live as of v0.2:

| Feature | How it was proven |
|---|---|
| REPL + tools | model listed files, edited files, answered repo questions |
| MCP (stdio) | `spawnfate mcp` spawned, `mcp__spawnfate__analyze_spawn` invoked |
| Hook veto | exit-2 hook blocked a real `Bash` call; model reported the denial |
| Permissions | `curl *` denied via `.claude/settings.json` deny glob |
| `--resume` | code word memorized in session A recalled in session B |
| `--fork` | copied log resumed independently, original untouched |
| ACP | raw JSON-RPC smoke: initialize→session/new→prompt→streamed chunks |
| TUI | CJK input renders; approval prompt suspends on y/n |
| Task | `subagent_type` loaded `.claude/agents/*.md`, independent count returned |
| `--dataflow` | JSON report incl. token totals from `Usage` events |
| `--doctor` | provider probe + rg + session dir + config inventory |

## Adding a feature — the checklist

1. Unit test for the deterministic part (pure functions, event folds,
   malformed-input paths).
2. If it touches the agent loop, drive it with `MockProvider` — never a live
   provider in tests.
3. Live dogfood at least once against a real provider before pushing.
4. `cargo clippy --workspace --all-targets -- -D warnings` must be clean —
   CI is strict-mode on windows-latest and emails on failure.
