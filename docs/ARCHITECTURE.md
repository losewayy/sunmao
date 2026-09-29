# ARCHITECTURE.md — the map, not the aspiration

What the code actually is today. `SPEC.md` holds the design contract and
planned milestones; this file is the current-state module map. When the two
disagree, **this file is right** — update SPEC or fix the code.

## Crate layout

```text
sunmao-llm   crates/llm/   provider dialects + wire primitives
sunmao-core  crates/core/  the kernel — Context, AgentLoop, sessions, tools
sunmao       crates/cli/   every user-facing surface
```

## Data path (one prompt, end to end)

```text
stdin/TUI/ACP
  └─ AgentLoop::run_turn(prompt, observer)            core/agent.rs
       ├─ ctx.hooks.fire(UserPromptSubmit)            core/hooks.rs
       ├─ ctx.sessions.messages()                     core/session.rs
       │     └─ fold SessionEvent[] → Message[]       (file or in-mem)
       ├─ ctx.llm.stream(ChatRequest)                 llm/{oai|anthropic}.rs
       │     └─ SSE bytes → SseParser → StreamDelta   llm/sse.rs
       ├─ assembler.push() / finish_lenient()         llm/assemble.rs
       ├─ per tool_call:
       │     ├─ ctx.permissions.check()               core/permissions.rs
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
| `ToolRegistry` | concrete map | built-ins + boxed MCP tools + `Task` share one dispatch table |
| `SessionLog` | concrete | file vs ephemeral are the same fold — replay is the test |
| `Hooks` | concrete dispatcher | one code path, any number of `command` handlers |
| process boundary | MCP child / ACP peer / hook proc / rg | everything that can fail alone is its own process |

## Native tool surface (10)

```text
fs.rs      Read (line numbers) / Write / Edit (whitespace-normalized match)
shell.rs   Bash (deno_task_shell — POSIX on Windows) + JobOutput (jobs are
           files under .sunmao/jobs/{id}/, inspectable while running)
search.rs  Glob (200-entry cap) + Grep (managed rg child, no shell)
artifact.rs HtmlArtifact — emits durable Artifact session facts
webmod.rs  WebFetch — naive tag-strip → readable text, ~24KB cap
task.rs    Task — nested AgentLoop, depth-capped at 2, own session log,
           subagent_type selects .claude/agents/*.md definitions
```

## Extension surfaces (what loads at startup)

```text
.sunmao/            .claude/              ~/.claude  ~/.agents
├── hooks.json      ├── settings.json     ├── settings.json  └── skills/
├── mcp.json        ├── settings.local    └── skills/
├── permissions.json├── commands/*.md     (same three dirs read unmodified)
├── plugin.json     ├── agents/*.md
├── commands/       ├── skills/*/SKILL.md
├── skills/         └── plugins/*/        (plugin bundles: hooks+mcp+skills+commands+agents)
├── agents/
└── plugin/         (the "this project is a plugin" dir)
    └── hooks|commands|skills|agents/
```

## Events — the audit-native spine

```rust
SessionEvent::Started | Message | ToolCall | ToolResult
                  | Compacted | Artifact | Usage
```

Append-only JSONL; the visible transcript is a pure fold over them. `messages()`
implements that fold — `Compacted` clears and re-seeds; `ToolResult` becomes
`Role::Tool` messages; `Usage` is accounting, not content.

## Frontends

```text
sunmao              stdin/stdout REPL — /compact, /skills, /<command>
sunmao -p "..."     one-shot; exit code encodes outcome
sunmao --tui        ratatui: char-indexed CJK input, unicode-width cursor,
                    approval prompts, PgUp/PgDn scroll, Ctrl-C = cancel
sunmao --acp        ACP v2 stdio server: initialize, session/{new,list,
                    resume,prompt,close}, cancel, session/request_permission
```

## What's *not* in code (spec-only for later)

- JS extension host (spec §"sidecar", v0.5+) — intended for the pi/TS ecosystem
- Electron/Web frontend — the runtime is the seam; UIs are replaceable
- MCP resources/prompts subscriptions
- OTel export — usage events land in the session log already; export is later
