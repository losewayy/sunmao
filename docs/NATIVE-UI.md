# Streaming Native UI (development prototype)

This is a new **declarative message rendering format**, not ChatGPT's private
Intelligent UI protocol. The web transcript accepts `sunmao-ui` fenced blocks,
where each physical line is a complete JSON operation. An incomplete final line
is buffered; every completed and validated line can update an existing tree.
The session's original assistant text remains the durable fact, so normal
session replay recompiles the same UI. Existing Markdown, HtmlArtifact and
MCP Apps continue to use their previous rendering paths.

Open `demo.html` from the extracted bundle to see a completely offline,
model-free mock stream. The demo simulates Agent requests rather than calling
a real server. It is not a substitute for application E2E verification.

## Protocol and components

Supported operations:

| Operation | Fields | Meaning |
|---|---|---|
| `state` | `key`, scalar `value` | Set initial reactive state; later model updates do not override user-touched values |
| `upsert` | `id`, `type`, optional `parent`, `props` | Create/update a node, reusing its DOM element while type is stable |
| `set` | `id`, `props` | Apply props to an existing node |
| `append_text` | `id`, `text` | Append a bounded text fragment |
| `remove` | `id` | Remove a node and its children |

`card`, `row`, `column` are containers; their children can be inserted later.
Other nodes: `text`, `heading`, `metric`, `badge`, `progress`, `divider`,
`slider`, `toggle`, `select`, `input`, `button`, `chart` and `table`. All
user-controlled text uses `textContent` or equivalent safe DOM setters; the
renderer never executes HTML, inline scripts or arbitrary model-generated JS.

State controls use `props.bind`, e.g. `{ "bind": "batch", "min": 1, "max": 64 }`.
A `value` or `when` property may reference state through `{ "var": "batch" }`
or a bounded expression tree using `op` + `args`. Operators: `add`, `mul`,
`sub`, `div`, `min`, `max`, `round`, `gt`, `gte`, `lt`, `lte`, `eq`, `neq`,
`and`, `or`, `not`, `if`. No arbitrary evaluation or property paths.

For charts, `props.data` contains at most 64 rows with `x` and `y`, or with
up to four named series described by `props.series:[{key,label}]`.
`props.kind` supports `line` (default) or `bar`. A table accepts
`columns:[{key,label}]` and `rows:[{key:value}]` (up to eight columns,
64 rows). These are intentionally small, browser-native building blocks.

Buttons only support `reset`, `copy`, and `ask`. `ask` interpolates state into
an ordinary text prompt and sends it to the existing `ui_message` path **on
click**, where the normal Agent turn and tools remain subject to permissions,
approvals and audit. It does not create a new permission or invoke a tool by
itself. Local slider, text and select updates remain ephemeral; replay shows
the original values unless a later durable assistant message contains an
updated UI description.

## Example

A normal assistant response can contain prose, this fenced block and more
prose after it:

~~~sunmao-ui
{"op":"state","key":"batch","value":8}
{"op":"upsert","id":"box","type":"card"}
{"op":"upsert","id":"h","type":"heading","parent":"box","props":{"text":"Inference capacity"}}
{"op":"upsert","id":"b","type":"slider","parent":"box","props":{"label":"Batch size","bind":"batch","min":1,"max":64,"step":1}}
{"op":"upsert","id":"m","type":"metric","parent":"box","props":{"label":"KV blocks","value":{"op":"mul","args":[{"var":"batch"},128]},"digits":0}}
{"op":"upsert","id":"a","type":"button","parent":"box","props":{"label":"Analyze","action":"ask","text":"Analyze batch {batch}"}}
~~~

The optional skill `examples/skills/native-ui/SKILL.md` teaches a compatible
model this format. New browser/desktop sessions now include optional native UI guidance in the
system prompt. Models can choose a UI when helpful without an explicit user
request, but this is model-dependent, not guaranteed. Existing sessions and
an explicit --system override keep their original prompt.

## Streaming and safety invariants

- The source is UTF-8-budgeted to **128 KiB per UI block**, at most **256 patch
  lines**, **96 nodes**, **8 KiB per line**, **4,096 characters per text append**,
  and at most 32 UI islands per message. The renderer tolerates malformed
  individual lines and never interprets incomplete JSON.
- Rewriting/retrying a source prefix resets its compiled tree. Appending
  normal new tokens applies only the new complete lines. Forward-parent
  references wait until the parent exists; non-container parenting and
  parent cycles are rejected.
- User-touched state survives later streaming state declarations. Input
  elements have stable node identities, and text input keeps focus and its
  current value while more patches arrive (subject to browser validation).
- A component type change creates a new element and drops the old type's
  props, including actions. Rendering uses a fixed set of components and
  bounded expressions. There is no arbitrary file or network access.
- HTML Artifact/MCP Apps remain distinct sandboxed surfaces; no automatic
  conversion from HTML is attempted. The MCP Apps outer-window origin is
  verified before accepting bridge messages.

## Validation and remaining work

From an applied checkout:

~~~sh
node --test crates/cli/src/serve/native-ui.test.cjs
# Optional: requires a working local Chromium; unavailable environments must
# not be counted as a passed browser test.
SUNMAO_BROWSER_TEST=1 node --test crates/cli/src/serve/native-ui.browser.test.cjs
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p xtask -- arch
~~~

The development bundle can be applied once to a *clean checkout* using
`python sunmao_ui_patch/apply.py /path/to/sunmao`. Review the diff before
committing. The script does not contact GitHub or make commits.

**Unverified:** complete Rust/Windows checks, live Browser/Tauri E2E,
MCP Apps compatibility in the integrated application, performance at large
transcript sizes, exact parity with all replay paths, and privacy compliance
of actual Git commit metadata. Native UI does not yet have cross-turn
server-originated patch events, persisted user widget state, generalized
Agent actions or deterministic model-independent UI selection. Prompt guidance enables
best-effort UI choice in new serve sessions; cross-turn patch events need
separate auditable contracts and tests.


## Cross-turn panel patches (development)

An optional first JSONL line `{"op":"surface","id":"bench"}` gives a panel a
stable, session-scoped identity. A later assistant message can reuse that ID
and emit incremental `set`, `upsert`, `remove` or `state` patches, updating
the original panel instead of creating a duplicate. All patches remain normal
assistant text in the append-only session log, so replay reconstructs them in
order. Stream rewrites are rejected for shared panels; re-open to replay.
User-touched widget values are still local and do not persist after reopening.


## Durable user control values

For panels declaring a stable `surface` header, slider/input/select/toggle
changes are debounced and sent as bounded `ui_state` transport frames.
The host validates names and scalar values before adding a `UiState` fact to
the session log; sibling tabs receive a scoped live notification. On replay,
those facts restore the last user-touched values. They never call tools or
expand permissions. Unscoped panels remain ephemeral. Restore-after-fork
and multiple-tab conflict rules still need full live dogfood testing.

Plain freeform text inputs are deliberately **not** auto-persisted to
session logs, to reduce the chance of recording a secret or sensitive note.
Slider, toggle and select values on scoped panels are persisted. New native
input fields remain local unless the user explicitly sends their contents
through a normal prompt. All session logs may contain user-provided data;
review their privacy before exporting them.

Native UI fences accept either three backticks or three tildes, so models
copying the standalone skill's Markdown example render correctly. The closing
fence must use the same marker as the opening fence.

The Native UI reset button also records the restored defaults for scoped
slider/select/toggle controls. Freeform text fields stay ephemeral.

The automated CI now runs the Chrome browser smoke test on Windows, covering
incremental rendering, controls, real user-click action gating, and reuse of a
stable surface across assistant turns.
