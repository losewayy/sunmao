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

## Extension protocol (`sunmao` JSON-RPC)

SPEC §4.8: the contract below is backed by a first-party host —
`crates/core/src/ext/` spawns children, handshakes, routes `ext/event`
through `HookEngine::fire`, and tears down with the session. A generic
JS extension-host sidecar (`node extension-host.mjs`) can still front
this same protocol for TS/JS ecosystems later. Design rule inherited
from hooks/MCP: **a host process is a process boundary** — spawn per
session, die with it, never hot-plug.

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
