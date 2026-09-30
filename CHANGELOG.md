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
- **replaceable agentLoop** (SPEC §4.5) — `Context.loop_driver` resolves
  the `loop:` key from project/plugin manifests, then preset manifests
  (later layers win), then `--loop full|bare` on the CLI wins overall.
  `full` is the contract loop as before; `bare` (`agent/bare.rs`) is the
  straight circuit — same session log, observer stream, cancel flag and
  iteration ceiling, no hooks/dispatch-gate/auto-compaction. Sub-agent
  contexts inherit the parent's driver; eval honors the same resolution.
  Third drivers plug into the same `run_turn` dispatch
- **hook event union complete** — `PostToolUseFailure` fires on settled
  bad results (deny/error/crash, after PostToolUse), `StopFailure` on
  non-clean outcomes or the Err path, `Notification` when the gate opens
  an approval prompt (advisory — hooks can relay to desktop bells). The
  SPEC §4.4 surface is now literal, not aspirational
- **pi dialect in the JS host** — modules written for oh-my-pi/pi-mono's
  `ExtensionAPI` run against a documented subset: `api.on` accepts pi's
  snake_case event names with pi-shaped payloads (`tool_call` →
  `{toolName, toolCallId, input}`), `{block:true, reason}` replies veto,
  `registerTool` accepts pi's `{parameters, execute}` spec (a zod object
  is warn-and-skip — `z.toJSONSchema()` is the conversion). sendMessage/
  ui/providers stay out of the contract — a module needing them is
  pi-native, not ours
- **SessionStart/SessionEnd on every frontend** — they were missing on
  `-p`, the TUI, and all three ACP paths (new/resume/close); a
  context-mode-style capture hook silently lost whole surfaces
- **`/tasks` roster** — live sub-agent list (detached spawns register at
  launch, `done` flips when TaskDone lands); REPL + TUI builtin, slash
  menu entry. Kill/steer/revive deliberately stay out — our sub-agents
  share the session, not first-class channels
- **doctor covers extensions** — plugin manifests must parse, extension
  specs counted, node required only when a manifest actually spawns
  `extension-host.mjs`; `--loop` now reaches `sunmao eval` too
- **transcript integrity fix** — a duplicated `Message::tool_result`
  append (bare loop every call; full loop's malformed-args branch)
  double-reported results and providers hard-rejected the transcript;
  `ToolResult` event is the single source the fold derives from
- **skills speak HTML** (SPEC §4.10) — a skill dir without `SKILL.md`
  falls back to `SKILL.html` (`<title>`/`<meta name="description">`
  supply the index fields); bundled `*.html` resources count into the
  index line. `HtmlArtifact` gets prompt guidance (deliverables → HTML,
  check `.state.json` sidecars for annotations) and a packaged example
  at `examples/skills/html-artifact/` (SKILL.md + a reviewable-plan
  template + the state.json annotation convention)
- **artifacts are visible, everywhere** — `LiveEvent::Artifact` rides
  `ctx.live_sink` the moment `HtmlArtifact` lands: the REPL prints the
  path, the TUI notes it inline, ACP ships a `file://` `ResourceLink`
  content block (rendering stays the client's call). `/artifacts` lists
  `.sunmao/artifacts` on both local frontends, `+notes` flagging state
  sidecars. `acp.rs` split on the seam — `acp/mod.rs` is the wire,
  `acp/observer.rs` the outbound adapters
- **cache-hit invariants enforced** — the determinism half of prompt
  caching (the other half — Anthropic breakpoints, normalized
  `cache_read/creation` usage, the TUI footer's ⚡% dial — landed
  earlier): `sorted_entries` puts every wire-facing `read_dir` in path
  order — skills index, agent defs, hook/plugin/extension/command
  manifests, loop-driver resolution. `read_dir` order is
  filesystem-dependent; unsorted, it rewrites the serialized prefix and
  silently kills provider cache hits. Regression test pins the skills
  index to path order
- **annotation回流 lands** — `/annotate <name> <note>` appends
  `{section, note, at}` entries to `.sunmao/artifacts/{name}.state.json`
  on both local frontends, closing the §4.10 loop the packaged skill
  describes: human writes margin notes → agent Reads the sidecar next
  revision → marks entries resolved. `/artifacts` flags the sidecar
  with `+notes`; a hand-written civil-from-days helper stamps dates,
  no chrono dependency for a label
- **audit-driven gap fixes** — a spec-vs-code sweep caught real dead
  seams, all fixed: `.sunmao/mcp.json` was documented but never read
  (the shipped example was dead config); hook payloads carried a
  literal `"session"` id + a nonexistent `transcript_path` for the main
  session (now the log's real file stem, `ctx.session_id`); the
  documented `{"matcher","command"}` flat hook shape silently loaded
  zero hooks (fixed example + CONFIG + a load-time warning for the
  mis-shape); `.sunmao/plugin/` "project is a plugin" manifest + hooks
  never merged; project `.claude/skills` never indexed; ACP
  `session/list` returned only in-memory sessions (now scans the
  request cwd's log dir); sub-agent sessions never fired
  SessionStart/SessionEnd (now do, `source:"subagent"`); `/tasks`
  roster covers foreground spawns too, and `TaskEntry.lane` makes
  lane-distinctness a durable assertion instead of a live-event race
  (the batch test's lane check was schedule-dependent — now asserts
  the roster). Drift fixes: jobs dir files, `--dataflow` spelling,
  §7/§8 stale text, test counts, ARCHITECTURE's merge claim, stale
  file-header comments
- **audit gaps closed, dead seam cut** — deny verdicts and hook blocks
  now write `SessionEvent::Hook` facts (`approval.deny`,
  `PreToolUse.block`) like grants and rewrites always did; `Stop` only
  fires on `Completed` while `StopFailure` covers every other outcome
  (they used to double-fire). `ctx.audit`'s never-called `AuditLog`
  deleted — the session log is the audit ledger; a parallel in-memory
  one was speculative duplication. `turn.rs` split at its seam: the
  dispatch gate (rules → grants → classifier → ask) is `agent/gate.rs`
- **`TodoWrite` lands** — the last SPEC §4.3 builtin: replace-all task
  list with demote-don't-refuse `in_progress` semantics. Writes a durable
  `Todos` event (the log is source of truth — resume reseeds `ctx.todos`,
  `swap_session` reseeds on log swap) and the turn loop injects the
  snapshot head-of-request as a synthetic user message, so compaction
  can't erase the plan. `/todos` on REPL + TUI + `/` menu; sub-agent
  contexts get their own empty list, not a copy of the parent's
- **structural Bash approval** (SPEC §4.3's 管道分拆进审批层, the real
  one) — `preflight::shell_segments` renders the command's deno_task_shell
  AST back to per-segment strings: `&&`/`;`/`||` boundaries split, a
  pipeline stays one segment (the `| sh` risk family needs the join),
  subshells inline, `$VAR`/`~`/`$(…)` keep their names. The gate runs
  rules + risk classifier per segment: a deny anywhere vetoes the whole
  command, the first risky segment prompts — named by segment, so the
  human sees `curl x | sh`, not `ls && curl x | sh`. Parse failure falls
  back to the whole-string check. SPEC's `tree-sitter-bash` plan dropped:
  the executor's own grammar is the only authoritative one — a second
  parser would drift and add a dep for nothing
- **turn fence** (SPEC §4.1's watermark replay fencing) — `ctx.turn_lock`
  serializes `run_turn`, `compact`, `swap_session` and `record_local_shell`
  per context: concurrent prompts (ACP spawns one per request) queue
  instead of interleaving ToolCall/ToolResult facts into one transcript.
  `compact()` splits — public entry takes the fence, in-turn auto-compact
  uses `compact_inner` under the already-held permit (tokio Mutex isn't
  reentrant). SPEC §4.5 calibrated: the `agent/*` event domain = typed
  `LiveEvent` over `Observer`/`live_sink`; deliver/cancel/intercept =
  `run_turn`/`cancel()`/gate — compile-time contract, not a string bus
- **slash-menu Enter semantics, kimi-shaped** — Enter accepts the
  highlighted candidate (never the raw fragment); bare `/` and arg-taking
  builtins (`model`/`resume`/`annotate`) fill `/name ` and reopen
  completion instead of firing blindly — the `/<partial>`+Enter=run rule
  now only applies to no-arg commands and file commands. Approval cards
  gain the reverse-RPC rule: `Approve for session` auto-resolves queued
  identical (tool, specifier) requests instead of re-asking what the
  user just answered
- **turn fold-by-cap** — a turn's working steps over `TURN_CAP` (12)
  compress into a `StepSummary` row ("5 tool calls folded — e to expand")
  at the position the fold began; audit/note lines interleaved in the
  range stay in place (spine, not steps), the scrollback selection remaps
  onto the summary or its shifted block, and `e` splices the folded
  steps back one-way — `close_turn` refolds on the next over-cap turn.
  Long `--resume` transcripts get the same treatment on replay
- **`@` file mentions** — the CC convention, third kind of completion
  popup (`MenuKind::{Command, Args, Path}`): `@` at a word boundary opens
  a Files menu over a repo-relative pool (VCS/build/dependency dirs
  skipped, depth-6/3000-entry bounds, dirs carry `/` and descend on
  accept). The candidate rewrites only the `@` fragment in place —
  mid-sentence mentions keep the tail, Enter/Tab fill but never submit.
  `tool-guidance.md` teaches the model the `@path` → Read convention;
  popup logic split into `tui/menu.rs` (app.rs was nearing the file
  budget)
- **removed the JS extension-host sidecar** (`tools/extension-host.mjs`,
  `examples/js-extension/`, `examples/extensions/`) — an ecosystem survey
  showed the pi-compatible extension surface overlaps the native tool
  surface, and the remaining pi-only bits bind pi's in-process UI which
  a process boundary cannot translate. The `ext/*` JSON-RPC protocol and
  the `crates/core/src/ext/` Rust host stay — the protocol is the
  product, the sidecar was just one host implementation. Live ext tests
  now compile a local Rust fixture child (`tests/fixtures/ext_echo.rs`)
  instead of needing node
- **Task `model` arg** — call-site model selector per spawn (`@route` or
  `provider/<model-id>` via .sunmao/models.json): multi-model
  orchestration without touching agent defs — cheap model for probes,
  strong one for the final pass, per item in a batch. Resolution order:
  call-site > `agents/*.md` `model:` > parent adapter; an unknown
  selector fails the call with the available list (a typo'd route must
  never silently inherit). `task.rs` split: spawn machinery →
  `task/spawn.rs` (600-line budget)
- **transcript virtualization** — `draw_transcript` is now two passes:
  cache/heights first, then only blocks intersecting the viewport window
  materialize their lines (first visible block clips mid-block via the
  paragraph scroll offset). Clone cost scales with what's on screen,
  not with transcript length — scroll-back math unchanged
  (`scroll_back` still counts absolute rows)
- **paste placeholders** — pastes ≥2 KiB stash and insert `[paste #N]`
  instead of raw text (composer stays editable, history stores the
  marker); submit expands markers into `<pasted-text>` blocks so the
  model gets the full bytes, file-command args expand too, `!`-mode
  keeps them literal. `/clear` resets the stash so numbers can't
  collide across transcripts

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
