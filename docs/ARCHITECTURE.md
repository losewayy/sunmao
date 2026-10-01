# ARCHITECTURE.md — the map, not the aspiration

What the code actually is today. `SPEC.md` holds the design contract and
planned milestones; this file is the current-state module map. When the two
disagree, **this file is right** — update SPEC or fix the code.

## Crate layout

```text
sunmao-llm   crates/llm/   provider dialects + wire primitives
sunmao-core  crates/core/  the kernel — Context, AgentLoop, sessions, tools
sunmao       crates/cli/   every user-facing surface (lib+bin — the GUI
                          shell reuses Cli + the serve host verbatim)
sunmao-gui   crates/gui/   Tauri v2 desktop shell — zero TCP: custom
                          `sunmao`/`sunmao-sandbox` URI schemes answer
                          REST via `HostHandle::request`; the ws channel
                          degrades to `session_events` Channel +
                          `host_call` invoke (same JSON frames)
```

## Data path (one prompt, end to end)

```text
stdin/TUI/ACP
  └─ AgentLoop::run_turn(prompt, observer)            core/agent/turn.rs
       ├─ ctx.hooks.fire(UserPromptSubmit)            core/hooks.rs
       ├─ ctx.sessions.messages()                     core/session.rs
       │     └─ fold SessionEvent[] → Message[]       (file or in-mem)
       ├─ ctx.llm.stream(ChatRequest)                 llm/{oai|anthropic}.rs
       │     └─ SSE bytes → SseParser → StreamDelta   llm/sse.rs
       ├─ assembler.push() / finish_lenient()         llm/assemble.rs
       ├─ per tool_call:
       │     ├─ ctx.permissions.check()               core/permissions.rs
       │     ├─ Bash: shell_segments() → per-segment  core/preflight.rs
       │     │   rules + classifier (pipelines stay    — the &&/;/|| split
       │     │   whole; | sh family needs the join)    is the structural gate
       │     ├─ ctx.approval.approve()                core/approval.rs
       │     ├─ hooks.fire(PreToolUse)  → exit-2 veto possible
       │     ├─ registry.call(name,args,ctx)          core/tool/*
       │     ├─ SessionEvent::ToolCall + ToolResult   (audit trail)
       │     └─ hooks.fire(PostToolUse)
       ├─ SessionEvent::Message{assistant} persisted  (durable before next turn)
       └─ cancelled flag consumed at turn end         (see AGENTS.md invariants)
```

## The seams (where contracts live)

| Seam | Type | Why it exists |
|---|---|---|
| `ProviderAdapter` | trait object | two live dialects (OAI, Anthropic) — earned its existence |
| `Approver` | trait object | three frontends: REPL stdin, TUI y/n, ACP `request_permission` |
| `Observer` | trait | REPL writer / TUI channel / ACP notifications — same events, three sinks |
| `ToolRegistry` | concrete map | built-ins + boxed MCP tools + ext tools + `Task` share one dispatch table |
| `SessionLog` | concrete | file vs ephemeral are the same fold — replay is the test |
| `Hooks` | concrete dispatcher | one code path, any number of `command` handlers + extension children |
| `ExtRegistry` | concrete (`ctx.ext`) | one spawned child per extension spec — tools surface `ext__{plugin}__{name}`, `ext/event` replies fold into `HookOutcome` |
| `PromptAssembler` | concrete | one layering order — REPL/TUI/ACP/Task can't drift |
| process boundary | MCP child / ext child / ACP peer / hook proc / rg | everything that can fail alone is its own process |

The **cold-plug principle** (CODE-ARCHITECTURE rule 6): replaceable units
swap at the file/config layer, effective at process start — never hot.
Prompt sections are named files (`assets/prompt/*.md` ← `~/.sunmao/` ←
`.sunmao/`); same-named files replace earlier sections. The risk table is
a text asset too (`assets/risky-patterns.txt`).

## Native tool surface (10)

```text
fs.rs      Read (line numbers) / Write / Edit (whitespace-normalized match)
shell.rs   Bash (deno_task_shell — POSIX on Windows) + JobOutput (jobs are
           files under .sunmao/jobs/{id}/, inspectable while running)
search.rs  Glob (200-entry cap) + Grep (managed rg child, no shell)
artifact.rs HtmlArtifact — emits durable Artifact session facts
todo.rs    TodoWrite — the model's task list; durable Todos events feed a
           head-of-request injection so compaction/resume can't lose it
webmod.rs  WebFetch — naive tag-strip → readable text, ~24KB cap
task.rs     Task — nested AgentLoop, depth-capped at 2, own session log,
           subagent_type selects .claude/agents/*.md definitions; defs carry
           model:/tools:/spawns: frontmatter (route the adapter, trim the
           registry, whitelist what the child may itself spawn); a call-site
           `model` selector (@route/provider<id>) overrides both for
           multi-model orchestration; run_in_background detaches — the
           finished child appends TaskDone into the parent's session log
           (push delivery)
models.rs  ModelResolver — .sunmao/models.json providers + @routes;
           agent model: selectors and /model swaps resolve through it
```

`ctx.sessions` is `Arc<Mutex<SessionLog>>` — background Task children
outlive their spawn call and append into the parent log directly.

## Extension surfaces (what loads at startup)

```text
.sunmao/            .claude/              ~/.claude  ~/.agents
├── hooks.json      ├── settings.json     ├── settings.json  └── skills/
├── mcp.json        ├── settings.local    └── skills/
├── permissions.json├── commands/*.md     (same three dirs read unmodified)
├── plugin.json     ├── agents/*.md
├── prompt.md       ├── skills/*/SKILL.md
├── prompt.d/*.md   └── plugins/*/        (plugin dirs: commands/skills/agents scanned;
├── skills/                               manifest fields merge from every
├── agents/                               plugin.json — top-level + plugins/*/
└── plugin/         (the "this project is a plugin" dir)
    └── hooks|commands|skills|agents/
```

A bundle's `plugin.json` may also carry `"extensions": [{command, args,
env}]` — each spec spawns a JSON-RPC extension child per session
(`crates/core/src/ext/`, protocol in `PROTOCOLS.md`); its tools register
as `ext__{plugin}__{tool}` and its `ext/event` replies fold into the same
`HookOutcome` command hooks produce. Preset dirs carry extensions the
same way (extra plugin roots); `examples/extensions/` is the reference.

`prompt.md` + `prompt.d/*.md` also load from `~/.sunmao/` (user layer, before
the project layer). Prompt sections order: built-in assets → user → project
→ project context; a file named like a built-in section replaces it.

## Events — the audit-native spine

```rust
SessionEvent::Started | Message | ToolCall | ToolResult
                  | Compacted | Artifact | Usage | Hook | LocalShell
                  | TaskDone | Todos
```

Append-only JSONL; the visible transcript is a pure fold over them. `messages()`
implements that fold — `Compacted` clears and re-seeds; `ToolResult` becomes
`Role::Tool` messages; `Usage` is accounting, not content; `Hook` records
auditor-visible facts (input rewrites, vetoes, injected context, session
grants) and stays out of the model-facing fold — rewrites are transparent to
the model, durable for the auditor. `LocalShell` is the `!` companion: a
user-run command folds in as a tagged `<local-shell>` user message, so the
next turn sees the evidence. `TaskDone` is the background-sub-agent twin: a
finished detached child folds in as a `<task-result>` user message carrying
its capped output (full transcript in its own `sessions/<id>.jsonl`).

The fold is also the resilience layer: corrupt lines warn-and-skip, and an
assistant tool_call stranded without its result (crash mid-turn) gains a
synthetic `[interrupted]` result — providers reject unpaired tool_use.

The *transient* vocabulary going the other way is `LiveEvent` (`Content`,
`Reasoning`, `ToolStart{name, summary}`, `ToolDone{name, ok, output}`,
`Hook{event, detail}`, `Usage`, `TurnEnd`) — what `Observer` sinks see live.
It never persists; frontends that want full tool output read the session log.

## Frontends

```text
sunmao              stdin/stdout REPL — /compact, /model, /resume, /rewind,
                    /tasks, /todos, /artifacts, /<command>
sunmao -p "..."     one-shot; exit code encodes outcome
sunmao --tui        ratatui: block transcript (virtualized draw,
                    fold-by-cap + `e` expand, copy OSC52, select via
                    Tab+j/k, Enter opens a full-screen viewer), 3-option
                    approval card (once / session / deny, parkable Esc),
                    `/` slash popup (commands / model selectors /
                    session picker), `@` file mentions with dir descent,
                    paste placeholders [paste #N], `!` local bash (runs
                    through deno_task_shell directly, folds into
                    context), markdown-rendered assistant text,
                    multiline composer, grapheme-cluster wrapping,
                    CSI ?2026 synced frames, two-line footer (branch +
                    live context tokens); Esc is layered
                    (park/clear/hint), Ctrl-C cancels or quits.
                    Semantic theme (tokyonight), user-prompt band, tool
                    blocks with arg digest + output panel, same-tool
                    verb-grouping, ⚙ audit lines for hook facts
sunmao --acp        ACP v2 stdio server: initialize, session/{new,list,
                    resume,prompt,close}, cancel, session/request_permission
sunmao serve        multi-session web host — loopback HTTP+WebSocket;
                    `serve/` holds the transport-free core
                    (Host/HostHandle::request + Client hello/replay)
                    under thin axum adapters; sessions, artifacts
                    (CSP-sandboxed islands + annotate), dataflow
sunmao-gui          Tauri v2 frameless window over the SAME host
                    in-process — no TCP at all (scheme + IPC above)
```

## What's *not* in code (spec-only for later)

- MCP resources/prompts subscriptions
- OTel export — usage events land in the session log already; export is later
- Gemini hook dialect — `.codex` + `.cursor` normalize already; Gemini's
  real bundle shape is unverified, so it stays out (compatibility is
  measured, not claimed)
