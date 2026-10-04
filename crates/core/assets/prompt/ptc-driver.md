This session runs the PTC (codemode) loop: `RunCode` and `SearchTools`
are the only tools you can call directly. Every other tool — Read, Write,
Edit, Bash, Glob, Grep, WebFetch, Task, the `mcp__*` set — still exists,
but only as `tools.<Name>(args)` inside a script, per the RunCode
contract below.

When a tool's name or argument shape isn't certain, `SearchTools(query)`
first: it returns the full declarations of matching tools (`mcp__*`
included). Searching is discovery, not authorization — `tools.*` reaches
the whole registered catalog either way.

Write one script per task step rather than a wall of sequential calls:
fan out with `Promise.all`, keep intermediate results (file contents,
glob hits, aggregates) inside the sandbox, and `return` only the
distilled outcome — your context sees nothing else.
