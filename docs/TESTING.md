# TESTING.md — what "verified" means here

Two tiers. Unit tests are the regression floor; **live dogfood is the bar** —
the point of this project is that it actually runs, so every landed feature
has (or should gain) a live pass against the real provider stack.

## Unit tests

```bash
cargo test --workspace    # ~136 tests
cargo run -p xtask -- arch # shape gate — god files, layer direction, prose-in-code
```

Current coverage:

| Suite | Proves |
|---|---|
| `llm::sse` | incremental SSE framing (split events, CRLF, keepalives, `[DONE]`) |
| `llm::assemble` | tool_call fragment reassembly incl. malformed-JSON rejection |
| `llm::anthropic` | block mapping (system fold, tool_use/tool_result, stream events) |
| `core::agent` (MockProvider) | full loop: plain turn, tool round-trip, cancel, malformed-args feedback, hook veto mid-loop, model routing + `/model` override; **loop drivers**: manifest `loop:` resolution + preset layering win, `bare` dispatches ungated (deny rule + vetoing hook both bypassed), `full` keeps enforcing |
| `core::task` | detached `run_in_background` → TaskDone push into parent log, unique spawn ids, spawns-whitelist + self-recursion guard |
| `core::tool` | read-before-write gate (deny→read→allow), Edit normalization, dying tool backend → failed result not turn abort |
| `core::hooks` | matcher semantics, live exit-2 veto via real subprocess, **rtk binary rewrite** + SessionStart/source contract, **Cursor dialect live-fire** (flat file → Shell matcher → `updated_input` rewrite), Codex file loading |
| `core::permissions` | deny>ask>allow>default matrix, glob specifiers |
| `core::approval` | risk classifier catches destructive patterns |
| `core::session` | event fold: messages, tool results, compaction boundary, corrupt-line skip, dangling tool_call synthesis |
| `core::prompt` | section layering: built-ins order, `--system` complete, `prompt.d` replace-by-name, AGENTS.md merge, subagent default/override |
| `core::plugin` | install/list/remove roundtrip, name sanitization, overwrite-then-force, self-install refusal |
| `core::presets` | name resolution (`+` strip), CLI order layering, unknown-name error lists searched dirs, preset hook actually fires |
| `core::ext` | frame parse, reply fold into HookOutcome, parked-id correlation, live Node fixture roundtrip (handshake → `ext__*` tool call → ext/event merge), dead-child fast-fail, **pi dialect live**: `api.on("tool_call")` veto + pi-spec registerTool |
| `core::preflight` | AST extraction (pipelines, booleans, dynamic-skip), fatal-note advisory (spawnfate) |
| `cli::eval` | case-file parsing (object/array/JSONL), assertion checks against session facts |
| `cli::tui` | keymap dispatch, slash-menu completion (`@route`, `provider/`), render-cache wrap invariants |

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
| TUI | CJK input renders; block browse (Tab/j/k/e/y), approval card (1-2/Esc park), `/` popup, markdown render, `❯` prompt band, tool digest headers + output panels, `×N` verb-grouping — manual smoke in `sunmao --tui` |
| Task | `subagent_type` loaded `.claude/agents/*.md`, independent count returned |
| Task run_in_background | detached child appended `TaskDone` to the parent log; `agent::tests::subagents` + `task::tests` cover the push path |
| `/model` + models.json | TUI `/model` arg completion + override adapter install; `agent::tests::models` covers routing + swap |
| `--preset` | `examples/presets/strict-audit/` live-smoked: `--preset nope` fails startup with searched dirs; a SessionStart hook in the preset fired only when named |
| `sunmao plugin` | install/list/remove roundtrip incl. `owner/repo` git-source classification; real clone path shells `git` |
| `sunmao eval` | live run against the local provider: PASS/FAIL lines, exit code, `--report` JSON |
| `--dataflow` | JSON report incl. token totals from `Usage` events |
| `--doctor` | provider probe + rg + session dir + config inventory + prompt assembly preview |
| `shell/preflight` | `-p` ran `totallynotreal-xyz`: advisory predicted FileNotFound (936 candidates), shell answered 127 — model reported both honestly |
| PromptAssembler | `-p` echoed the assembled first line verbatim; `--doctor --system X` shows the complete override |

## Adding a feature — the checklist

1. Unit test for the deterministic part (pure functions, event folds,
   malformed-input paths).
2. If it touches the agent loop, drive it with `MockProvider` — never a live
   provider in tests.
3. Live dogfood at least once against a real provider before pushing.
4. `cargo clippy --workspace --all-targets -- -D warnings` must be clean —
   CI is strict-mode on windows-latest and emails on failure.
