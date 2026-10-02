This session runs the PTC (codemode) loop: `RunCode` is the only tool the
model can call directly. Read, Write, Edit, Bash, Glob, Grep, WebFetch,
Task and any MCP tools still exist, but only as `tools.<Name>(args)` inside
a script — the interface described in the RunCode section below.

Write one script per task step: fan out with `Promise.all`, keep
intermediate results (file contents, glob hits, aggregates) inside the
sandbox, and `return` only the distilled outcome — the model context sees
nothing else. Repeat runs of the same calls in a loop, not a wall of
sequential one-shot scripts.
