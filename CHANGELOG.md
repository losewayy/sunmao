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
- `LiveEvent` carries richer payload for frontends:
  `ToolStart{name, summary}` (one-line arg digest) and
  `ToolDone{name, ok, output}` (≤8 KiB preview) — the TUI draws header +
  output panel from them, the REPL prints the digest inline, ACP ignores
  the new fields

**cli/TUI (latest)**
- `!` **local bash mode** — `!` on the empty composer (or a literal
  `!cmd` line) runs the command through the same deno_task_shell path
  the Bash tool uses, with zero LLM tokens and no approval; the run
  lands as `SessionEvent::LocalShell` and folds into the next turn's
  context as a tagged `<local-shell>` message
- approval card is **3-option**: allow once / allow for session / deny —
  session grants live on the Context (`session_grants`), are shared with
  Task sub-agents, and audit as `SessionEvent::Hook{"approval.session"}`
- `⚙` **audit blocks** — `LiveEvent::Hook` surfaces hook rewrites,
  vetoes, injected context, and session grants in the transcript (REPL
  prints them, ACP forwards them as agent text)
- **two-line footer**: `● state · model · cwd (branch) · mode · ctx Nk`
  over the hints/toast line; branch probed at startup, tokens from
  `LiveEvent::Usage`
- **full-screen viewer**: scrollback `Enter` expands the block (wrapped
  body, j/k/PgUp/PgDn/g, `y` copy, Esc/q back)
- slash menu matches the shared design language: `─` rules, title +
  `(type to search)`, Search row, `❯` pointer, windowed scroll —
  and `Enter` applies the highlighted command
- rendering mechanics: CSI `?2026` synchronized frames, bulk bracketed
  paste, grapheme-cluster wrapping (`unicode-segmentation` — ZWJ emoji
  and combining marks never tear), CJK width-correct cursor
- transcript is **block-structured**: user/assistant/thinking/tool/note are
  distinct blocks — `Tab` enters block browse, `j/k` select, `e` folds
  (thinking starts folded), `y` copies via OSC 52, `g/G` jump ends
- `/` opens a **slash menu** above the composer: builtins + convention-dir
  `.md` commands fuzzy-filtered, `↑↓` walk, `Tab` completes
- assistant blocks render **markdown** (headings/emphasis/`code`/fenced
  blocks/lists/quotes) via pulldown-cmark — raw text stays copyable
- composer: `/multiline` toggles Enter=newline vs send (Alt/Shift+Enter
  sends when multiline is on); double-`Esc` stashes the draft (`Ctrl+S`
  restores); Esc never cancels a turn — `Ctrl-C` cancels, quits when idle
- `Esc` no longer exits the TUI outright (was a footgun mid-typing)
- Windows double-typing fixed: `KeyEventKind::Release` filtered at the
  event source — crossterm reports Press+Release on Windows
- `tui/` split into `mod/app/blocks/md/render/slash` (god-file rule)
- **visual pass** (design study of grok-build's scrollback, implemented
  independently): semantic tokyonight theme (`tui/theme.rs`), your prompts
  render as a `❯` full-width band (long ones collapse), tool blocks show
  `✓ Name <arg digest>` headers with a dim output-preview panel ("N more
  lines"), consecutive same-name calls verb-group into `×N`, status bar
  shows model · cwd · mode; glyphs degrade on legacy ConHost

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
