---
name: native-ui
description: Compose bounded streaming UI components in the sunmao web transcript.
---

# Native UI for the sunmao Web GUI

Use this format only when the user benefits from interactive controls or structured
charts/tables. Prefer normal Markdown for straightforward explanations, and use
HtmlArtifact/MCP Apps for complex applications that need their own sandbox.

Write a fenced block whose info string is exactly `sunmao-ui`. Inside the block,
write **one complete JSON object on each line**. The client can render lines as
soon as the newline arrives. Do not format the JSON for readability across
multiple lines, and do not invent component types or JavaScript callbacks.

Operations:
- `{"op":"state","key":"batch","value":8}` defines a scalar. Keys start with
  a letter and contain up to 64 letters, digits, `_` or `-`.
- `{"op":"upsert","id":"panel","type":"card","parent":"root","props":{}}`
  creates or updates a component. Only `card`, `row` and `column` accept children.
- `{"op":"set","id":"metric","props":{"label":"Throughput"}}` updates props.
- `{"op":"append_text","id":"description","text":"more"}` appends text.
- `{"op":"remove","id":"metric"}` removes a node and its descendants.

Component types: `card`, `row`, `column`, `text`, `heading`, `metric`, `badge`,
`progress`, `divider`, `slider`, `toggle`, `select`, `input`, `button`,
`chart`, `table`. State-bound controls use `props.bind`. Computed data uses a
bounded expression tree: `{"op":"mul","args":[{"var":"batch"},128]}`.
Expressions permit only add/mul/sub/div/min/max/round, numeric comparisons,
boolean and/or/not, eq/neq and `if` — no eval or arbitrary code.

Conditional rendering: `props.when` is an expression. The component is hidden
when the expression is false. `chart` accepts `kind:"line"` (default) or
`kind:"bar"`, `data:[{x:"1",y:10}]`; optional `series` with up to four
`{key,label}` objects allows multiple measures such as data `[{x:"1",base:10,opt:14}]`.
`table` accepts `columns:[{key,label}]` and `rows:[{key:value}]`.

Buttons support only `action:"reset"`, `action:"copy"` with plain `text`,
and `action:"ask"` with a prompt `text` containing state placeholders such
as `{batch}`. `ask` sends a **new user message only on a real click**; it is
not a direct tool call. Use actions sparingly. Never present a button as if
it has already executed an operation.

Example (the fences belong in the final response):

~~~sunmao-ui
{"op":"state","key":"batch","value":8}
{"op":"upsert","id":"panel","type":"card"}
{"op":"upsert","id":"title","type":"heading","parent":"panel","props":{"text":"Inference Explorer"}}
{"op":"upsert","id":"batchSlider","type":"slider","parent":"panel","props":{"label":"Batch Size","bind":"batch","min":1,"max":128,"step":1}}
{"op":"upsert","id":"capacity","type":"metric","parent":"panel","props":{"label":"KV blocks","value":{"op":"mul","args":[{"var":"batch"},128]},"digits":0}}
{"op":"upsert","id":"inspect","type":"button","parent":"panel","props":{"label":"Inspect this configuration","action":"ask","text":"Analyze batch size {batch} in this inference experiment"}}
~~~

Do not claim illustrative data is a measured benchmark. Model-defined UI is
not trusted code. The UI renderer has strict size and node limits; make panels
small and use a few well-chosen components instead of hundreds of nodes.
