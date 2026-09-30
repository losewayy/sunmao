# CODE-ARCHITECTURE.md — the code's shape and its upkeep rules

This is the maintainer's contract for the code's *structure*: what the layers
are, what lives where, and the rules that keep it clean as it grows.
Companion to `AGENTS.md` (operational invariants) — this file owns *shape*,
`AGENTS.md` owns *correctness invariants*.

## Hard rules

1. **No god files.** A file past ~600 lines gets split — **enforced**:
   `cargo run -p xtask -- arch` fails CI. A file that needs a table of
   contents to navigate is already too big. Split by *responsibility*,
   not arbitrarily — `tool.rs` became `tool/{fs,shell,search,artifact,webmod}`.
   Test modules move to sibling `tests.rs` files, which do count toward
   the budget — the point is navigability, not hiding bulk.
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
6. **Cold-plug, not hot-plug.** Adopted from dsh: everything that *can* be a
   replaceable unit *should* be one — but swaps happen at the **file/config
   layer, effective at process start**, never hot-reloaded mid-session.
   Product semantics (prompt text, policy tables) are **data files, never
   string literals in `.rs`**: kernel-owned text lives under
   `crates/*/assets/` and ships via `include_str!`; user- and project-level
   files replace or extend it by name (`prompt.d/identity.md` replaces the
   `identity` section). The same-named-file replacement rule IS the
   cold-plug mechanism. When you add prose the user might want to change,
   ask "why is this a string in code?" — a hardcoded prompt or policy table
   is a seam violation even though it compiles.
   Counter-examples that must stay in code: API surfaces (`Tool::decl`
   schemas/descriptions ARE the wire dialect), private-API mirrors
   (`DENO_BUILTINS` shadows upstream's crate-private list — a comment says
   so), and spec data tied to verifier code.
   The gate: `cargo run -p xtask -- arch` flags prose-shaped string
   literals (≥80 chars, ≥5 spaces) outside `#[cfg(test)]` and
   `Tool::function(...)` call sites — the two exemptions encoded.

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
               ├── agent/        AgentLoop — the one concrete loop
               │   ├── mod.rs    orchestration + Context wiring
               │   ├── turn.rs   per-turn loop (LLM call, tool dispatch, events)
               │   ├── gate.rs   dispatch gate — rules, grants, classifier, ask
               │   ├── bare.rs   the bare LoopDriver — straight circuit, no seam
               │   └── tests/    loop fixtures (mock providers, session replays)
               ├── session.rs    SessionEvent + SessionLog fold
               ├── hooks.rs      lifecycle hook dispatcher (Claude contract)
               │   hooks/        contract fixtures (tests.rs, live_tests.rs)
               ├── permissions.rs deny/ask/allow rule engine
               ├── approval.rs   Approver trait + risk classifier
               ├── mcp.rs        MCP client (stdio + streamable-HTTP)
               ├── ext/          extension host — spawned extension children
               │   ├── mod.rs    plugin manifest `extensions` spec scan
               │   ├── registry.rs child spawn, request/reply correlation,
               │   │             ExtTool, ExtRegistry + shutdown/Drop
               │   ├── proto.rs  JSON-RPC frame vocabulary (line-delimited)
               │   └── tests.rs  unit + node-gated live fixtures
               ├── agents.rs     named sub-agent definitions loader
               ├── task.rs       Task tool — nested AgentLoop, depth cap
               │   task/         nested-loop fixtures (tests.rs)
               ├── preflight.rs  shell/preflight — spawnfate advisory pass
               ├── prompt.rs     PromptAssembler — sectioned prompt layering
               ├── assets/       kernel-owned data files (prompt/*.md,
               │               risky-patterns.txt) — include_str!, not literals
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
               ├── tui/          ratatui TUI (CJK-native)
               │   ├── mod.rs    event loop, driver task, focus machine
               │   ├── app.rs    App state — blocks, composer, approval, menu
               │   ├── input.rs  composer/key input handling
               │   ├── replay.rs session replay → transcript blocks
               │   ├── blocks.rs transcript blocks (band/panel/fold/copy)
               │   ├── render.rs draw — transcript/menu/card/input/status
               │   ├── md.rs     pulldown-cmark → styled lines
               │   ├── theme.rs  semantic palette + legacy-glyph fallbacks
               │   ├── slash.rs  command discovery/resolution
               │   └── *_tests.rs / tests.rs — state + render fixtures
               ├── acp/         ACP v2 server — mod.rs wire, observer.rs outbound adapters
               ├── dataflow.rs   session-log → audit report
               ├── eval.rs       `sunmao eval` case runner + assertion pass
               └── doctor.rs     env self-check
```

## Growth rules (when adding things)

| Adding a… | Goes in | Not allowed to |
|---|---|---|
| provider dialect | `llm/` new file | touch `agent/turn.rs`'s loop |
| native tool | `core/src/tool/<family>.rs` | know about sessions directly (use `ctx`) |
| hook event | `HookEvent` variant + fire site | bypass the matcher/dispatch path |
| config file | documented in `docs/CONFIG.md` | read files outside the documented set |
| session fact | `SessionEvent` variant | skip the fold — `messages()` must handle it |
| frontend | `cli/` new file + `Observer` impl | import agent internals |

## Doc discipline

- `SPEC.md` = intent (what we want), `ARCHITECTURE.md` = state (what exists),
  this file = shape rules. Disagreement → fix whichever is wrong, not both.
- `AGENTS.md` stays lean — deep structure rules live here, it links out.
