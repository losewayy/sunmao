<div align="center">

# sunmao（榫卯）

**A Rust agent harness where auditability is the foundation, not an afterthought.**

[![CI](https://github.com/losewayy/sunmao/actions/workflows/ci.yml/badge.svg)](https://github.com/losewayy/sunmao/actions/workflows/ci.yml)

</div>

榫卯 (sǔnmǎo) is the Chinese joinery tradition where timber members connect
through precisely-cut interlocking joints — no nails, no welding. That's the
architecture: components joined by seams, every seam a contract.

> **Status: v0.2** — the kernel walks, talks to real models, calls real tools,
> speaks MCP and ACP, and runs a CJK-native TUI. See [`docs/SPEC.md`](docs/SPEC.md)
> for the design contract.

## What works today

- **Agent loop** — streaming OpenAI-compatible dialect (hand-rolled SSE
  parser), tool_calls fragment reassembly, reasoning-channel support
  (DeepSeek-style `reasoning_content`), auto-compaction at a token ceiling
- **Event-sourced sessions** — append-only `*.jsonl` per session; the message
  list is a pure fold over facts. `--resume` to continue, `--dataflow` to
  audit "what data went where"
- **Six native tools + sub-agents** — `Read` / `Write` / `Edit`
  (whitespace-normalized match) / `Bash` (embedded POSIX shell — identical
  syntax on Windows) / `Glob` / `Grep` (managed `rg` subprocess, no shell) /
  `JobOutput` (filesystem-state background jobs) / `HtmlArtifact` /
  `Task` (depth-capped nested agents)
- **Read-before-Write gate** — overwriting a file the model hasn't read is
  refused
- **Hooks** — lifecycle dispatcher speaking the dominant hook contract
  (JSON stdin/stdout, matchers, exit-2 veto). Loads `.sunmao/hooks.json`,
  `.claude/settings.json`, `.claude/settings.local.json`, `~/.claude/settings.json`
  unmodified — events fired: `UserPromptSubmit`, `PreToolUse`, `PostToolUse`
- **MCP client** — `.sunmao/mcp.json` (Claude `mcpServers` shape), stdio
  transport, tools surface as `mcp__{server}__{tool}`
- **ACP server** — `sunmao --acp`: ACP v2 over stdio (initialize, session/new,
  prompt, cancel) so Zed and other clients can drive sunmao sessions
- **Frontends** — REPL (`sunmao`), one-shot (`sunmao -p`), TUI
  (`sunmao --tui`, ratatui, CJK-native width-correct input), ACP

## Usage

```bash
# configure via flags or env
sunmao --base-url http://127.0.0.1:7863/v1 --api-key KEY --model MODEL

# inside the REPL
/compact   # force context compaction

sunmao --tui            # terminal UI
sunmao -p "task"        # one-shot, scriptable
sunmao --resume s-…     # continue a session
sunmao --dataflow .sunmao/sessions/s-….jsonl   # audit report
sunmao --acp            # ACP server (stdio) for Zed etc.
```

## Layout

```text
crates/
├── llm/    # provider dialects: hand-rolled SSE, tool_calls reassembly
├── core/   # sessions (event log), tools, hooks, MCP client, agent loop
└── cli/    # `sunmao` binary — REPL / -p / TUI / ACP / dataflow
```

## License

Dual-licensed under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE).
