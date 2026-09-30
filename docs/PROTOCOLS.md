# PROTOCOLS.md — the three wire dialects we own

Everything below is hand-rolled on purpose — the protocol stack is the
differentiator (SPEC §10). When adding a fourth dialect, keep the same shape:
provider-specific types → shared `StreamDelta` vocabulary → one agent loop.

## OpenAI-compatible (`crates/llm/src/oai.rs`)

`POST {base}/chat/completions`, SSE stream.

- `delta.content` → `StreamDelta::Content`
- `delta.reasoning_content` (DeepSeek) → `StreamDelta::Reasoning`
- `delta.tool_calls[i]` fragments → `ToolCallAssembler` slots keyed by `index`
- `finish_reason` → `StreamDelta::Finish`
- SSE: LF or CRLF both legal; `:` comment lines = keepalives; `[DONE]` sentinel

Retry: connection failure / HTTP 429 / 5xx → backoff 300ms, 600ms, then give
up — *request-establishment only*. Once a delta has flown, never replay (a
partial assistant message + re-request = duplicated content).

## Anthropic Messages (`crates/llm/src/anthropic.rs`)

`POST {base}/messages`, `x-api-key` + `anthropic-version` headers,
`max_tokens` required (default 8192 if caller omits).

Message mapping (our flat `Message` → their content blocks):

| Ours | Anthropic |
|---|---|
| `Role::System` | top-level `system` field (concatenated) |
| `Role::User` text | `content: [{type:"text"}]` |
| `Role::Assistant` + tool_calls | `content: [text?, tool_use blocks]` |
| `Role::Tool` result | user turn containing `{type:"tool_result"}` |

SSE events:

| `type` | mapping |
|---|---|
| `content_block_start` (tool_use) | `ToolCallFragment{id,name}` |
| `content_block_delta` (text_delta) | `Content` |
| `content_block_delta` (thinking_delta) | `Reasoning` |
| `content_block_delta` (input_json_delta) | `ToolCallFragment{arguments}` |
| `message_delta` (stop_reason) | `Finish` |

## MCP client (`crates/core/src/mcp.rs`, rmcp 3.5)

Two transports from one `ServerSpec`:

- `{"command","args","env"}` → `TokioChildProcess` (stderr → null)
- `{"url": "https://…"}` → `StreamableHttpClientTransport::from_uri`

Tools surface as `mcp__{server}__{tool}`, schema/description passed through.
Merge order: `.sunmao/mcp.json` then `plugin.json`/`plugins/*/plugin.json` —
each malformed entry warns and continues; a dead server bricks only itself.

## ACP server (`crates/cli/src/acp.rs`, agent-client-protocol 2.2 + `unstable_protocol_v2`)

stdio JSON-RPC, stdout is protocol-only (diagnostics → stderr/tracing).

| Method | Handler |
|---|---|
| `initialize` | protocol version + capabilities |
| `session/new` | fresh Context + session log in `cwd/.sunmao/sessions/` |
| `session/list` | scans session dir → `SessionInfo[]` |
| `session/resume` | reopens `{id}.jsonl`, events fold back into context |
| `session/prompt` | runs the turn; emits `AgentMessageChunk`, `AgentThoughtChunk`, `ToolCallUpdate`, idle state |
| `session/cancel` (notification) | sets `ctx.cancelled`; loop exits cooperatively |
| `session/close` | drops the live session handle |
| `session/request_permission` (agent→client) | approval gate surfaces as a native permission prompt |

Rule that has to stay true: **stdout = JSON-RPC frames, nothing else** — a
stray println! bricks the whole protocol.

## Hook dialects (`crates/core/src/hooks.rs` + `hooks/{cursor,dialect}.rs`)

One event space, three file dialects. The *Claude* spelling is the wire
canonical: stdin payload `{session_id, transcript_path, cwd, hook_event_name,
prompt?, source?, tool_name?, tool_use_id?, tool_input?, tool_response?}`,
stdout reply `continue/stopReason`, `systemMessage`, `hookSpecificOutput.
{permissionDecision, permissionDecisionReason, additionalContext,
updatedInput}` or legacy `decision:"block"`; exit 2 = block with stderr.

| File | Dialect | Normalization |
|---|---|---|
| `.sunmao/hooks.json`, `.claude/settings*.json`, plugin manifests | Claude | none — canonical |
| `.codex/hooks.json`, `~/.codex/hooks.json` | Claude | none — Codex bundles (rtk `init --codex`) use the identical shape |
| `.cursor/hooks.json`, `~/.cursor/hooks.json` | Cursor | `hooks/cursor.rs` rewrites both directions |

Cursor file shape is flat: `{"version":1,"hooks":{"preToolUse":[{"command",
"matcher","timeout"}]}}` — each entry becomes a single-command matcher group.
camelCase events with a native counterpart map (`sessionStart`/`sessionEnd`,
`preToolUse`/`postToolUse`, `subagentStart`/`subagentStop`,
`beforeSubmitPrompt`→UserPromptSubmit, `preCompact`, `stop`); cursor-only
events (beforeShellExecution, afterFileEdit, Tab hooks, …) are skipped at
load rather than mis-fired. Matchers
filter on *cursor* tool names (Bash→Shell, Edit→Write, `mcp__s__t`→
`MCP:<t>`). Payloads keep cursor spellings (`hook_event_name` echoes the
config's camelCase name; `conversation_id`, `workspace_roots` added).

Cursor replies normalize into the canonical outcome: `permission`
(allow/ask/deny) → `permissionDecision`, `user_message`+`agent_message` →
`permissionDecisionReason`, `updated_input` → `updatedInput`,
`additional_context`/`followup_message` → `additionalContext`,
`continue:false`+`user_message` → `continue:false`+`stopReason`, per-entry
`timeout` overrides the hook budget. Exit 2 keeps the same veto semantics.

**Gemini is deliberately not normalized yet** — its settings shape and
`hookSpecificOutput.tool_input` nesting were never confirmed against a real
bundle; landing it blind would violate the "compatibility is measured, not
claimed" rule.

## Extension protocol (`sunmao` JSON-RPC)

SPEC §4.8: the contract below is backed by a first-party host —
`crates/core/src/ext/` spawns children, handshakes, routes `ext/event`
through `HookEngine::fire`, and tears down with the session. JS extension
modules ride the same protocol via the sidecar
(`node tools/extension-host.mjs` — "JS host sidecar" below). Design rule
inherited from hooks/MCP: **a host process is a process boundary** —
spawn per session, die with it, never hot-plug.

### Lifecycle

Declared in a plugin bundle's `plugin.json`:

```json
{"name": "demo", "extensions": [{"command": "node", "args": ["${CLAUDE_PLUGIN_ROOT}/ext.mjs"]}]}
```

The kernel spawns one child per session at startup (cold-plug: the child is
chosen by files, effective at process start — swapping extensions = restart).
stdout = protocol frames only; stderr is free for logging. On session end the
kernel sends `ext/shutdown` then closes the pipe; a child that doesn't exit
within 2s is killed — a dying extension **degrades to a warning**, never a
session abort (same rule as `mcp.rs`: one bad server bricks only itself).

### Frames

JSON-RPC 2.0, one object per line on the child's stdin/stdout. Kernel →
extension calls carry `"id"`; extension → kernel replies carry the same `id`;
notifications omit it.

| Direction | Method | Semantics |
|---|---|---|
| kernel → ext | `ext/initialize` | `{protocol: 1, cwd, session_id, transcript_path}` → ext replies `{name, version, capabilities: {tools: bool, events: ["PreToolUse", ...]}}` |
| kernel → ext | `ext/tools/list` | only sent when `capabilities.tools` → reply `{tools: [{name, description, input_schema}]}` — tools surface namespaced `ext__{plugin}__{tool}` like `mcp__` |
| kernel → ext | `ext/tools/call` | `{name, arguments}` → reply `{content: string, is_error?: bool}` or a JSON-RPC error — errors fold to failed `ToolResult`, never turn abort |
| kernel → ext | `ext/event` (request — replies carry effects) | `{event, payload}` where payload is the hooks dialect (tool_name/tool_input/source/…); ext may reply `{extra_context?: [..], block?: "reason"}` — same semantics as hook stdout: `block` vetoes, `extra_context` appends session facts |
| kernel → ext | `ext/shutdown` | notification; then close stdin, wait ≤2s, kill |

### Contract notes

- **Event surface = the hooks union** (`HookEvent` variants) — extensions get
  the same dialect fields, so an extension can do anything a hook can:
  context-mode-style `source` reads, rtk-style argument rewrites, audit taps.
- `PreToolUse` replies additionally accept `{updatedInput}` — the rtk rewrite
  mechanism is part of the dialect, not hook-specific.
- v1 has **no reverse channel** (extensions can't query session history or
  call kernel services) — an extension is a passive responder. A `ext/lookup`
  family is deliberately deferred: every reverse method is a SemVer promise.
- Tool schemas pass through verbatim to the model — a bad `input_schema`
  means the tool doesn't exist to the LLM (same rule as native `Tool::decl`).

### JS host sidecar

`tools/extension-host.mjs` is a zero-dependency Node script that fronts
this exact protocol for **JS extension modules** — a plugin ships
`.mjs`/`.js` files instead of a bespoke binary:

```json
{"command": "node", "args": ["${CLAUDE_PLUGIN_ROOT}/extension-host.mjs",
                             "${CLAUDE_PLUGIN_ROOT}/ext"]}
```

The sidecar file is **copied into the plugin bundle** — a bundle is
self-contained, `${CLAUDE_PLUGIN_ROOT}` only resolves inside it. In-repo
development may instead point `args` straight at `tools/extension-host.mjs`
plus any module dir. See `examples/js-extension/` (a real bundled copy +
`ext/wordcount.mjs` module + install notes).

Module contract — each `*.mjs` / `*.js` file in the scanned dir
(non-recursive, sorted by name) exports a default function invoked once
with an `api` object, plus an optional `dispose()` export run on
`ext/shutdown`:

```js
export default function (api) {
  api.registerTool({ name, description, input_schema, handler });
  //   handler: async (args) => ({content} | {content, is_error: true})
  api.on("<HookEvent>", (payload) => ({ extra_context: ["…"] }));
  api.log("…");                       // stderr — stdout is protocol-only
}
```

- `*.ts` files are skipped with a warning — no toolchain ships, and
  Node's type-stripping only covers erasable syntax; ship compiled JS.
- Modules load **before** `ext/initialize` is answered so
  `capabilities` is truthful: `tools` = any `registerTool` across all
  modules, `events` = union of all `api.on()` names. Early frames queue.
- `ext/tools/list` aggregates every module's registrations;
  `ext/tools/call` dispatches by `name` (handler throw →
  `{content: <err>, is_error: true}`, unknown name → JSON-RPC error).
- `ext/event` replies merge across subscribers in load order, mirroring
  hook aggregation: `extra_context` concatenates; `block`,
  `updatedInput`, `permissionDecision` are last-non-null-wins.
- Degrade philosophy matches the Rust host: a module that throws on
  import/registration warns on stderr and is skipped — one bad module
  never takes the bridge down. Frames are handled serially, so replies
  keep request order and `ext/shutdown` can't exit ahead of a pending
  reply.

#### pi dialect (oh-my-pi / pi-mono compat)

Modules written against pi's `ExtensionAPI` run against a **documented
subset** — the sidecar normalizes both directions rather than asking
plugins to learn our names:

- `api.on(pi_name, handler)` accepts pi's snake_case events:
  `session_start`/`session_shutdown`, `session_before_compact`/
  `session_compact`, `tool_call`, `tool_result`, `input`,
  `agent_start`/`agent_end`, `turn_end` → the canonical events.
  Handlers get **pi-shaped payloads** (`tool_call` sees
  `{toolName, toolCallId, input, cwd, sessionId}`; `tool_result` adds
  `result`; `input` sees `{prompt, cwd, sessionId}`); other events get
  the canonical payload (pi ignores the extra fields).
- `tool_call` replies: `{block: true, reason}` → our `block` reason;
  `extra_context`/`additionalContext` and `updatedInput` pass through.
- `api.registerTool` accepts pi's spec shape `{name, description,
  parameters, execute}` — `parameters` must be a real JSON schema; a
  **zod object is warn-and-skip** (call `z.toJSONSchema()` first —
  pi's zero-dep schema shim is not this host's job). `execute`'s
  `{content:[{type:"text",text}]}` reply folds to our `{content}`.
- **Not this host's surface**: `sendMessage`, `registerCommand`,
  `ui`/renderers, providers, settings, `registerProvider`, timers —
  a module that needs them is a pi-native plugin, not a sunmao one.
  Unknown pi event names simply never fire (the honest skip).
