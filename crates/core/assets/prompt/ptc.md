# Scripted tool orchestration (RunCode)

`RunCode` executes a JavaScript program in a sandboxed QuickJS engine. The
sandbox has **no filesystem, network, `import`, `require`, or `process`** —
the only capabilities are the ones you invoke:

- `await tools.<Name>(args)` — call any registered tool (Bash, Read, Write,
  Edit, Glob, Grep, WebFetch, Task, JobOutput, TodoWrite, MCP `mcp__*` …).
  `args` is that tool's normal argument object. The call goes through the
  same permission/approval pipeline as a direct tool call — a denied call
  resolves `{ok: false, output}` instead of throwing; engine-level failures
  throw. Each call resolves `{ok, output}` — check `ok` before parsing.
- `await Promise.all([...])` — real parallel fan-out; concurrent tool calls
  overlap instead of serializing.
- `await describe()` / `await describe(name)` — the callable tool catalog
  (names + argument schemas) when you need to look up a signature.
- `await tools.SearchTools({query})` — the same catalog filtered by query
  terms (name/description match, `mcp__*` included); resolves the full
  declarations as `{ok, output}` like any other nested call.
- `await store(key, value)` / `await load(key)` — a session-scoped,
  durable JSON key-value store shared across `RunCode` calls and surviving
  resume; `load` returns `undefined` for missing keys.

Write `code` so its completion value is what you want back — end with an
expression, or wrap the body in `(async () => { ... })()` and `return` the
result. Intermediate computation (globs, greps, bulk reads, aggregation)
stays in the sandbox and never enters this transcript — prefer RunCode when
a task fans out over many files or chains tool outputs, instead of
dispatching calls one at a time.
