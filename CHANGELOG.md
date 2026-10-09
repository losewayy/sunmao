# Changelog

All notable changes to sunmao. Dates are commit-era, not release dates.

## [0.2.1] — current

Kernel is a real runtime: two provider dialects, ten native tools, hooks,
permissions, approvals across three frontends, MCP client (stdio + HTTP),
ACP v2 server, event-sourced sessions with resume/fork/dataflow, CJK-native
TUI.

**post-0.2 additions**
- **gui**: the bundle icon is two blocks locked into one — a literal 榫卯 joint — replacing the placeholder; the serve page's favicon follows it, and the README picks up the mark plus serve screenshots.
- **tools**: a foreground command that hits its timeout is handed to the background instead of being killed. Jobs have a registry, a durable completion message folded into the conversation, and `JobList`/`JobStop`/`JobOutput`; the panel lists background runs only, and a job that finishes in the foreground leaves nothing behind.
- **im**: the deeper audit's findings, each with a test that fails without the fix. The Telegram bot token no longer reaches a log (reqwest prints the URL it failed on, and the token was in the base URL), QQ advances its reply sequence per chunk instead of reusing one pair, cursor keys are scoped to the credential, token TTLs and frame sizes are bounded, and a partly delivered message is never resent from the top.
- **web**: fetch no longer panics on any page with a non-ASCII dash or quote, a panic that left the tool call without a result and hung the turn. It decodes by the response's charset, caps the body at 2 MiB, reuses one HTTP client, and refuses loopback and private addresses unless `SUNMAO_WEBFETCH_ALLOW_PRIVATE=1`.
- **cli**: a `channels.json` that does not parse is shown as it is and can never be overwritten by the fallback, and a dirty cached preference repairs itself at boot instead of taking the page with it.
- **models**: saving the settings page merges rather than replacing, so hand-written keys and fields from a newer build survive, and adding a provider onto a name that already exists is refused instead of overwriting it.
- **cli**: a queue drag ends when the pointer is released, wherever that happens, and a failed steer keeps its quote cards and pasted text.
- **docs**: the IM and configuration pages read as one line per paragraph.
- **im**: five channels. Telegram, feishu, QQ, dingtalk and wechat are all outbound-initiated (websocket or long poll), so no public ingress, no callback URL and no signature verification is needed. Each platform is a spec in `channels.json` plus an adapter and a pure protocol module, and a credential is always a source (`<field>_env` or `<field>_file`), never plaintext. Groups are dropped on every platform: answering an unpaired sender with a pairing code in a room is the one thing a pairing flow must never do. WeChat's QR login and media, and message editing on dingtalk and wechat, are not shipped.
- **cli**: the IM channels page lists every platform as a fixed card in a row (wechat, QQ, feishu, dingtalk, telegram) with no kind picker, no add and no delete, because a second channel of one kind cannot run anyway: routing keys on the kind name and the cursor is shared.
- **cli**: quoting a passage is a card above the composer now (quote icon, first-line summary, remove, hover for the whole passage) instead of the passage pasted into the textarea. The message still leaves with the quote block ahead of whatever was typed.
- **cli**: queued prompts are a column above the composer, one row each, with a drag grip at the left of every row wired to the existing relative `input_move` step, and Alt+Up/Down on the grip for the keyboard.
- **models**: `default_model` in `.sunmao/models.json` pins the model a new conversation starts on. An explicit `--model` wins, and a session already under way keeps the model from its own log.
- **agent**: stopping is remembered. `Notify::notify_waiters()` stores no permit, so a cancel whose waiter had not registered yet was lost and a running command ignored it; `CancelSignal` is a flag plus a wake, a summary stream in flight is cancelled instead of waited out, the stop button acknowledges on the same frame, and a turn that ignores a cancel is force-ended after ten seconds with a `force_stop` event.
- **llm**: a pasted full endpoint is stripped before the dialect's path is appended (`/v1/chat/completions` no longer becomes `/v1/chat/completions/chat/completions`). The version segment and any gateway prefix stay exactly as written.
- **tools**: `Bash`'s `timeout_secs` clamps to 1..600 seconds instead of trusting the request.
- **cli**: the reasoning fold closes again. It looked for its body inside the button while the stylesheet keys on the next sibling, so it opened and could never close.
- **security change**: trust pinning extends to spawn surfaces — plugin
  `extensions` children and MCP `command:` stdio servers join hook
  commands under the same `.sunmao/trusted-hooks.json` ledger (digest =
  `sha256(canonical source + serialized {command,args,env})`; `url` MCP
  transports don't spawn and aren't gated). Untrusted specs fail closed
  at connect: skipped, with `ext.untrusted`/`mcp.untrusted` audit rows —
  no approval prompt exists that early (ACP/serve sessions come up
  unattended). `/hooks` lists hooks + spawn rows under one numbering and
  `trust`/`untrust` pins either kind
- **security change**: hook trust pinning — project/plugin/preset hook
  commands no longer execute on load. Each command needs a pin in
  `.sunmao/trusted-hooks.json` keyed on `sha256(canonical source path +
  command)`; editing the command or moving the file invalidates the pin.
  User-level sources (`~/.claude`, `~/.codex`, `~/.cursor`) are implicitly
  trusted. Skips are durable `hook.untrusted` audit facts plus live ⚙
  lines; `/hooks` lists the roster (`user`/`pinned`/`untrusted`) and
  `/hooks trust <n>` / `untrust <n>` manage the ledger — identical in
  REPL, TUI, and serve (`GET /hooks` REST read face). A cloned repo can
  no longer exec code at SessionStart
- **behavior change** (Windows): `Bash` defaults to real PowerShell 7 when
  `pwsh` is on PATH — no config needed; explicit `SUNMAO_SHELL` /
  `.sunmao/shell.txt` values still win (`posix`/`bash`/`deno` force the
  embedded interpreter back on, `auto` re-arms detection). New user-level
  pin `~/.sunmao/shell.txt`; `sunmao doctor` reports the effective
  backend, its source layer, and `pwsh --version`. Non-Windows unchanged.
- **behavior change**: `-p`/`--print` no longer pins `full_access`. With no
  `--mode` flag a piped run denies every approval prompt (new
  `PipedApprover`) — headless sessions can't wait on cards — and the
  denial reason lands in the failed `ToolResult` so the model sees it.
  `--mode full_access` or a `permissions.json` allow rule stays the
  escape hatch. A resumed log's `full_access` clamps to `auto` under `-p`
  unless `--mode` says otherwise. New `--mode <stance>` flag also sets the
  approval mode for interactive sessions
- `LiveEvent::ToolStart` carries the parsed `args` (post-hook-rewrite) —
  the GUI renders Edit calls as line-level diffs and Write calls as new-
  file previews; synthetic events (compact, `!` shell) carry `null`
- `/mcp` (connected server roster: name · transport · tool count · liveness)
  and `/status` (model · provider · cwd · session id · approval mode ·
  token totals) builtins — parsed in `commands.rs`, executed by all three
  frontends; `commands/` note-text builders split out to `notes.rs`
- `sunmao-gui` — GUI phase 2 (GUI.md §8): a Tauri v2 shell embedding the
  SAME multi-session host in-process (`sunmao::serve_host` — the new
  transport-free entry; `serve/` split into host/client/request +
  axum http/ws adapters) with ZERO listening sockets. The page rides a
  `sunmao` custom URI scheme (`http://sunmao.localhost/` on WebView2) —
  every REST endpoint answers through the scheme handler via
  `HostHandle::request`; the ws channel degrades to a Tauri `Channel`
  (`session_events` outbound / `host_call` inbound — same JSON frames),
  and the MCP Apps sandbox proxy gets its own `sunmao-sandbox` scheme —
  SEP-1865's double-iframe still needs a second origin. Custom-scheme
  pages never get Tauri's injected globals, so the shell seam is an
  `__sunmaoShell` init script — caption/drag verbs plus a minimal
  `Channel` class — with capability `local: true` (WebView2 classifies
  `<scheme>.localhost` as a local origin). The DESIGN.md titlebar lives
  in the page; verbs cross IPC. `crates/cli` became lib+bin so the shell
  reuses `Cli` and the serve assembly verbatim — one host
  implementation, four frontends
- artifact CSP declarations (GUI.md §3): `GET /artifacts/{name}` emits a
  default-deny CSP (`default-src 'none'`); frontmatter
  `csp.resourceDomains`/`csp.connectDomains`/`csp.frameDomains` lines
  whitelist origins for the island
- `sunmao serve` — GUI phase 1 (GUI.md): the web frontend on
  127.0.0.1 HTTP+WebSocket. One ws channel carries `LiveEvent` verbatim
  (hello/replay/approval/note/session/model/busy wrappers); REST serves
  sessions, artifacts (sandboxed iframe islands + annotate writeback),
  and the dataflow report. Approval cards ride the same Approver seam
  as the TUI
- `/fork` mid-session in the TUI and REPL; `swap_session` retargets
  hook `session_id`/`transcript_path` so resume doesn't report the
  abandoned session's identity
- session fold fixes: auto-compaction runs before the prompt lands in
  the log (it used to summarize the question away), `Compacted` keeps
  the system message (sub-agent identity survived no longer depends on
  luck), `open_path` seals a crash-truncated tail line
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
- **cache-hit discipline** — the todos snapshot moved from head-of-
  request injection to tail: inserting at index 0 invalidated the
  provider's whole prompt prefix every time the plan moved; appended
  after the last message it follows pairing rules and leaves the
  entire history prefix byte-stable. The rest of the cache story:
  `BTreeMap` tool declarations (stable ordering), sorted skill/
  command indexes, and the Anthropic dialect's `cache_control`
  breakpoints
- **`/resume` session picker** — a third arg-completing builtin joins
  `/model`: after `/resume ` (or `/sessions `) the composer completes
  against the newest-first session list (rescanned on menu open so
  sessions sub-agents spawned mid-turn show up). Enter on a candidate
  submits `/resume <id>` — picking a session IS the command; Tab fills
  for a `--fork`-style edit first. Bare `/sessions`+Enter opens the same
  picker; `recent_sessions(cwd, limit)` is the single session scan the
  picker and the bare `/resume` list share
- **slash command arg substitution** — `/name args` expands `$ARGUMENTS`
  and positional `$1`..`$9` where the command body placed them (the
  Claude Code convention); placeholder-free bodies keep the appended
  behavior
- **live MCP fixture** — `tests/fixtures/mcp_server.rs` (rustc-compiled
  JSONL server) proves the v0.2 crash-tolerance bar on the MCP side:
  `tools/list` survives, a real call round-trips, and a child that dies
  after listing degrades subsequent calls to failures instead of hanging
  the loop. `compile_fixture` in lib.rs shares the rustc path between
  the ext and mcp live tests
- **self-audit fixes** (a dogfooded `sunmao -p` review of its own loop
  surfaced these): a UserPromptSubmit veto used to skip `TurnEnd`
  entirely — frontends hung with the queue stuck — and `cancelled` only
  reset on the success tail, so an Err-path turn poisoned the next.
  Both moved to `run_turn`'s wrapper tail: every exit emits exactly one
  TurnEnd and clears the flag. The gate's `hook allow` short-circuit no
  longer launders deny-scoped Bash segments, Session grants now apply
  per-segment, a detached child's `TaskDone` append takes the parent's
  `turn_lock` (no more tool_use/tool_result splits mid-turn), and
  malformed-args calls emit ToolStart/ToolDone + PostToolUseFailure like
  every other settled failure
- acp: the permission prompt's description names the specifier a
  session grant would cover
- `LiveEvent::ToolStart/ToolDone` carry `call_id` — the provider's
  tool_call id is the exact start↔done join key. Same-name calls in one
  turn used to pair by FIFO position (GUI) or recency (TUI) and could
  cross-wire; ACP tool-call ids now key on the real id, frontends fall
  back to (name, depth, lane) only for id-less synthetic events
- **approval modes** (SPEC §4.6) — `ApprovalMode` is session state on
  `Context`: `always_ask` prompts on every mutating call, `auto` (the
  default) is rule-driven as before, `read_only` refuses mutations
  outright, `full_access` never asks — `deny` rules stay a hard refusal
  in every mode. Mutation detection is structural, not substring: Bash
  walks the deno_task_shell AST (output redirects, command substitutions,
  dynamic verbs all count), a per-verb flag table catches `find -exec`/
  `fd -x`/`sort -o`, and `git` reads by subcommand (`status`/`log` read;
  `branch`/`tag`/`remote` only in bare listing shape). The allowlist is
  a cold-plug file (`assets/readonly-verbs.txt`, extendable per project).
  Switching is durable: `set_approval_mode` writes a `mode_change`
  SessionEvent under `turn_lock`, resume reseeds the stance. Entries on
  every frontend — `/mode` (REPL+TUI menu), the GUI composer chip, and
  ACP `session/set_config_option` ("mode"); `-p` pins `full_access`
- **TUI↔GUI replay parity** — one canonical transcript is the golden
  test for the seam contract: `App::replay` and index.html's
  `renderReplay` fold the same 30-event fixture (fake-DOM driver at
  `serve/replay_parity.mjs`, node-skipped when absent). It caught real
  drift the day it landed — GUI kept pre-compaction DOM, stray
  tool_results stole pending rows, `<local-shell>` leaked as user
  bubbles — all fixed to mirror the TUI's audit-row semantics

- **artifact version chains** (GUI.md §3) — a same-name `HtmlArtifact`
  rewrite archives the old file as `{name}.v{rev-1}.html` instead of
  losing it; `Artifact` session+live events carry `rev`, the log sequence
  is the authoritative version list. `GET /artifacts/{name}?rev=k`
  serves history (rev 0 or latest → the live file), `/revs` reports the
  count; GUI islands get ◀ ▶ nav (`v{cur}/{max}` chip, open-in-browser
  honors the pinned rev), TUI/REPL notes show `· rev N`, `/artifacts`
  lists `·N revs` and hides the archives. ACP resource titles say
  `name.html (rev N)`. Legacy events deserialize with `rev: 0`
- **`sunmao serve` is a multi-session host** (GUI.md §7 made real) —
  every adopted session runs its own AgentLoop + input queue + approval
  map + busy counter (`serve::Host`), built per-log by a
  `SessionFactory` main injects (provider, registry, fresh MCP tool
  impls sharing process-wide connections, extensions, approver, model
  routes). resume/fork/new adopt a new host instead of swapping a
  shared Context — the previous session keeps running and rendering in
  other tabs. Every outbound frame carries `sess`; the tab only
  renders its viewed session while the rail reads every session's
  run/wait/done dot. ws gains `view`/`resume`/`fork`/`new`,
  `approval_done`, `sessions_changed`; `hello` carries `busy_sessions`
  and pending approval cards so a tab arriving mid-ask re-renders them.
  serve.rs split: `serve/ws.rs` (client channel), `serve/artifacts.rs`
  (REST read surface), `serve/driver.rs` (per-session FIFO)
- **MCP Apps host** (SEP-1865, stable 2026-01-26) — a tool declaring
  `_meta.ui.resourceUri` renders its result as a sandboxed island:
  the call fetches the `ui://` resource, lands it as
  `artifacts/mcp-{server}-{tool}.html` (riding the same version chain)
  plus a `{name}.ui.json` sidecar with the call args, raw result and
  declared CSP. `_meta.ui.visibility` splits the surface — `["app"]`
  tools never reach the model registry, `["model"]` tools are refused
  from the island. The GUI runs the spec's double-iframe sandbox proxy
  on a second loopback origin (CSP translated from the resource's
  `connectDomains`/`resourceDomains`/`frameDomains`/`baseUriDomains`);
  the island's `tools/call`, `resources/read` and `ui/message` proxy
  over the ws bridge and pass through the SAME dispatch gate — the UI
  is never a permissions bypass, and every island action lands on the
  audit spine (`mcp.app_call`/`mcp.app_read`/`mcp.ui_message` facts).
  `ui/resource-teardown` and host-context `styles.variables` are
  deferred — v1 covers the stable surface

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
