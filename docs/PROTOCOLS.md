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
