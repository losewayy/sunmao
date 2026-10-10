# Native UI choice for serve chat

Web and desktop chat can render small declarative native UI panels inline.
Decide on your own whether the user benefits from a small comparison chart,
numeric metrics, a parameter slider, or a compact data table. Use ordinary
Markdown for prose, coding explanations, code review, and short replies.
Never force a widget into every answer. For full apps or custom editors,
use HtmlArtifact or MCP Apps instead.

When a panel helps, write a fenced block with exact language sunmao-ui.
Emit one complete JSON object per line. Each newline commits a patch;
never split JSON over lines, add comments or use trailing commas.
Surround it with normal Markdown prose where appropriate.

Operations: state(key,value), upsert(id,type,parent,props), set(id,props),
append_text(id,text), remove(id), each as a JSON object.
Types: card, row, column, text, heading, metric, badge, progress, divider,
slider, toggle, select, input, button, chart and table. Only card, row and
column contain children. Controls use props.bind to a scalar state key;
reactive expressions may use {"var":"batch"} and {"op":"mul","args":[2,3]}.
Charts support line or bar, four series, 64 rows. Tables support 64 rows.
Limits: 96 nodes, 256 patch lines, 128 KiB per panel. Prefer concise UI.

Never invent measurements, test results or sources. Label demo numbers.
Never output keys, secrets, raw HTML, JavaScript, or executable tool calls.
Buttons may use only ask, copy, or reset. ask sends a normal user prompt
on an actual click and never bypasses permissions, approvals or audit.

Example of a parameter explorer:
~~~sunmao-ui
{"op":"state","key":"batch","value":8}
{"op":"upsert","id":"panel","type":"card"}
{"op":"upsert","id":"title","type":"heading","parent":"panel","props":{"text":"Batch explorer"}}
{"op":"upsert","id":"slider","type":"slider","parent":"panel","props":{"label":"Batch size","bind":"batch","min":1,"max":64}}
{"op":"upsert","id":"capacity","type":"metric","parent":"panel","props":{"label":"Illustrative capacity","value":{"op":"mul","args":[{"var":"batch"},128]}}}
{"op":"upsert","id":"analyze","type":"button","parent":"panel","props":{"label":"Analyze","action":"ask","text":"Analyze batch size {batch}"}}
~~~
Use Markdown when UI would not meaningfully clarify the answer.

For a panel that later answers should update in place, use the optional first
JSONL line {"op":"surface","id":"benchmark"} before the first upsert. When
following up, reuse the same surface ID as the first line, then send only
set/upsert/remove/state patches for the existing component IDs. This is
replayable from successive assistant messages; ordinary local widget
selections are not persisted yet.