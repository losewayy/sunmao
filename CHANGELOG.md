# Changelog

All notable changes to sunmao. Dates are commit-era, not release dates.

## [0.2.0] — current

Kernel is a real runtime: two provider dialects, ten native tools, hooks,
permissions, approvals across three frontends, MCP client (stdio + HTTP),
ACP v2 server, event-sourced sessions with resume/fork/dataflow, CJK-native
TUI.

**post-0.2 additions**
- `sunmao plugin install|list|remove` — bundle management for
  `.sunmao/plugins/<name>/` (the dir all five consumers already scan);
  name sanitization keeps `plugins/` an airtight root
- `--preset <name>` — plugin bundles layered on demand from
  `.sunmao/presets/<name>/` or `~/.sunmao/presets/<name>/` (`+name` = the
  SPEC's layering notation). Presets merge last into hooks/mcp/agents/
  skills/commands; sub-agent contexts inherit them; ACP resolves
  per-session against the request cwd
- `sunmao eval <file>` — case-driven runner: JSON/JSONL cases assert
  `final_contains`/`tool_called`/`tool_not_called`/`max_tool_calls`
  against the session's own facts — the eval surface reads what the
  audit path wrote, no shadow transcript. Per-case fresh Context,
  `--report` writes the result array
- `xtask arch` — the shape rules got teeth: god-file budget (600),
  layer direction (cli→core→llm only), prose-in-code — runs in CI
- **extension host** (`crates/core/src/ext/`) — plugin.json `extensions`
  specs spawn one JSON-RPC child per session (the `sunmao` dialect in
  PROTOCOLS.md): `ext__{plugin}__{tool}` namespaced tools share the
  `tools:` whitelist; `ext/event` runs inside HookEngine after command
  hooks, folding `block`/`extra_context`/`updatedInput` into the same
  HookOutcome; dead children degrade to failed calls, never aborts.
  The v0.5 seam landed early
- **JS extension-host sidecar** (`tools/extension-host.mjs`) — a
  zero-dep Node bridge fronting the same `ext/*` protocol for `.mjs`/`.js`
  extension modules (`api.registerTool` / `api.on` / `api.log`); event
  replies merge with hooks semantics (extra_context concat, effect
  scalars last-non-null-wins), `.ts` files warn-and-skip. Bundled-copy
  example at `examples/js-extension/`
- cold-plug reach: `.sunmao/risky-patterns.txt` replaces the shipped
  approval table outright; preset dirs' same-named file merges additively
- model routing: `.sunmao/models.json` names providers + routes; agent
  defs' `model:` frontmatter resolves through it; `/model` switches
  mid-session (TUI + REPL + ACP)
- Task `run_in_background: true` — detached sub-agents push `TaskDone`
  facts into the parent's log; `spawns:`/`tools:` whitelist in agent
  frontmatter; self-recursion blocked
- crash tolerance: corrupt session lines skip instead of aborting
  resume, dangling tool_calls get a synthesized `[interrupted]` result,
  hook subprocesses die at a 60s ceiling
- rtk/context-mode conformance is *measured*, not claimed: a live test
  hangs the real `rtk` binary on PreToolUse and asserts the rewrite;
  SessionStart carries `source` for context-mode-style sidecars
- **hook dialects: Codex + Cursor** — `.codex/hooks.json` rides the Claude
  path unchanged (rtk `init --codex` bundles load verbatim);
  `.cursor/hooks.json` flat `{command, matcher, timeout}` entries under
  camelCase events are parsed by `hooks/cursor.rs`: matchers filter on
  cursor tool names (Bash→Shell, `mcp__s__t`→`MCP:<t>`), payloads keep
  cursor spellings, snake_case replies normalize into `HookOutcome`
  (`permission`/`updated_input`/`additional_context`/`continue:false`).
  Gemini deliberately deferred — its real bundle shape is unverified
- test hygiene: pid-keyed scratch dirs recycled under Windows PID wrap —
  every suite now carves unique dirs via `fresh_test_dir` (flakes it
  caused: write-allows-new-file, bg-task delivery, batch lanes)

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
