# Changelog

All notable changes to sunmao. Dates are commit-era, not release dates.

## [0.2.0] — current

Kernel is a real runtime: two provider dialects, ten native tools, hooks,
permissions, approvals across three frontends, MCP client (stdio + HTTP),
ACP v2 server, event-sourced sessions with resume/fork/dataflow, CJK-native
TUI.

**core (latest)**
- `shell/preflight`: Bash commands are predicted by the `spawnfate` engine
  (`Producer::WinSpawn` — the which-resolve + CreateProcess path
  deno_task_shell actually takes) before they run; advisories ride in the
  tool result so the model can self-correct. Advisory only, never blocks
- `PromptAssembler`: the system prompt is assembled from named section
  files — built-in `assets/prompt/*.md` ← `~/.sunmao/prompt{,.d}` ←
  `.sunmao/prompt{,.d}` → project context → `--system` (complete override).
  Same-named files replace built-in sections (cold-plug). Fixes a real
  divergence: ACP previously carried its own hardcoded prompt without
  project context — all frontends plus `Task` now share one path
- risk table moved to `assets/risky-patterns.txt` — policy is a data file
- `assets/` convention: kernel-owned prose/policy ships as files via
  `include_str!`, never string literals in `.rs` (CODE-ARCHITECTURE rule 6)

**llm**
- OAI dialect: hand-rolled SSE, tool_calls fragment reassembly, reasoning
  channel, transient retry (connect/429/5xx, 300/600ms backoff)
- Anthropic dialect: content-block mapping, thinking→Reasoning,
  input_json_delta→fragment reassembly — second `ProviderAdapter` impl
- malformed tool-call args feed back as failed `ToolResult` instead of
  aborting the turn

**core**
- event-sourced `SessionLog` (file-backed + ephemeral share one fold)
- `Compacted` event: auto-compaction past token budget, model-generated
  summary replaces transcript while facts stay durable
- `Usage` event per finish → `--dataflow` reports token totals
- hooks dispatcher: 8 lifecycle events, Claude contract, exit-2 veto
- permissions: deny/ask/allow glob rules from `.sunmao/permissions.json` +
  `.claude/settings*.json`
- approval seam: risk classifier + interactive/TUI/ACP three frontends
- MCP client: stdio child + streamable-HTTP; plugin.json manifests merge in
- Task sub-agent: depth-2 cap, own session log, `subagent_type` → `agents/*.md`
- tools: Read/Write/Edit/Bash/Glob/Grep/JobOutput/HtmlArtifact/WebFetch/Task

**cli**
- REPL (`/compact`, `/<command>`), `-p` one-shot, `--tui`,
  `--acp`, `--resume`, `--fork`, `--sessions`, `--dataflow`, `--doctor`
- TUI: char-indexed CJK input, unicode-width cursor, approval prompts,
  cooperative Ctrl-C cancel

**ecosystem**
- AGENTS.md / CLAUDE.md auto-injected into system prompt
- skills indexed from `.sunmao/skills`, `~/.claude/skills`, `~/.agents/skills`
- slash commands from `commands/*.md`; sub-agent defs from `agents/*.md`
- plugin bundles: `plugin.json` + `plugins/*/`

## [0.1.0] — v0.1 "kernel walks"

workspace skeleton, OAI streaming, 4 native tools, REPL, session JSONL —
one real task end-to-end against the local provider.
