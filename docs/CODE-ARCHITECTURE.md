# CODE-ARCHITECTURE.md — the code's shape and its upkeep rules

This is the maintainer's contract for the code's *structure*: what the layers
are, what lives where, and the rules that keep it clean as it grows.
Companion to `AGENTS.md` (operational invariants) — this file owns *shape*,
`AGENTS.md` owns *correctness invariants*.

## Hard rules

1. **No god files.** A file past ~600 lines gets split. A file that needs a
   table of contents to navigate is already too big. Split by *responsibility*,
   not arbitrarily — `tool.rs` became `tool/{fs,shell,search,artifact,webmod}`.
2. **No history rewrites.** `main` history is immutable: no force-push, no
   amend of pushed commits, no rebase of published history. A wrong commit
   gets a follow-up commit, not a rewrite. The public timeline is an asset.
3. **Less is more.** Smallest change that correctly does the thing. No
   speculative abstraction, no "just in case" branches, no dead code kept
   "for later" — delete it.
4. **Layers run one way.** `cli → core → llm`, never backwards. `llm` must
   not know sessions exist; `core` must not know TUI exists.
5. **Seams earn existence.** A trait gets introduced only when a second real
   implementation needs it. Concrete types until then.

## Layer map

```text
crates/llm     the wire. protocols in, StreamDelta out. Nothing here may
               reference sessions, tools, or UI.
               ├── types.rs      shared vocabulary (Message, ToolCall, Usage)
               ├── sse.rs        SSE frame parser
               ├── assemble.rs   tool_calls fragment reassembly
               ├── oai.rs        OpenAI-compatible dialect
               └── anthropic.rs  Anthropic dialect

crates/core    the kernel. Owns state, dispatch, policy. May not know
               any frontend exists.
               ├── context.rs    Context — the seam assembly struct
               ├── agent.rs      AgentLoop — the one concrete loop
               ├── session.rs    SessionEvent + SessionLog fold
               ├── hooks.rs      lifecycle hook dispatcher (Claude contract)
               ├── permissions.rs deny/ask/allow rule engine
               ├── approval.rs   Approver trait + risk classifier
               ├── mcp.rs        MCP client (stdio + streamable-HTTP)
               ├── agents.rs     named sub-agent definitions loader
               ├── task.rs       Task tool — nested AgentLoop, depth cap
               ├── web.rs        fetch → readable text
               └── tool/         the native tool surface
                   ├── mod.rs    ToolResult, ToolImpl, ToolRegistry, builtin_registry
                   ├── fs.rs     Read / Write / Edit (+ read-before-write gate)
                   ├── shell.rs  Bash + background jobs + JobOutput
                   ├── search.rs Glob + Grep
                   ├── artifact.rs HtmlArtifact
                   └── webmod.rs WebFetch

crates/cli     every frontend + flag plumbing. Thin by design — heavy logic
               belongs in core.
               ├── main.rs       flags, dispatch, REPL, Observer/Approver impls
               ├── tui.rs        ratatui TUI (CJK-native)
               ├── acp.rs        ACP v2 server
               ├── dataflow.rs   session-log → audit report
               └── doctor.rs     env self-check
```

## Growth rules (when adding things)

| Adding a… | Goes in | Not allowed to |
|---|---|---|
| provider dialect | `llm/` new file | touch agent.rs's loop |
| native tool | `core/src/tool/<family>.rs` | know about sessions directly (use `ctx`) |
| hook event | `HookEvent` variant + fire site | bypass the matcher/dispatch path |
| config file | documented in `docs/CONFIG.md` | read files outside the documented set |
| session fact | `SessionEvent` variant | skip the fold — `messages()` must handle it |
| frontend | `cli/` new file + `Observer` impl | import agent internals |

## Doc discipline

- `SPEC.md` = intent (what we want), `ARCHITECTURE.md` = state (what exists),
  this file = shape rules. Disagreement → fix whichever is wrong, not both.
- `AGENTS.md` stays lean — deep structure rules live here, it links out.
