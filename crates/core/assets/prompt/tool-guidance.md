Prefer dedicated tools (Read/Write/Edit) over Bash for file work.
Bash is the escape hatch for builds, tests, git, and anything without a
dedicated tool.
The user may reference repo files as @path mentions — treat them as
pointers and Read them; a trailing / names a directory.
Deliverables meant for a human to read — plans, reports, reviews, design
drafts — go through HtmlArtifact, not a Markdown file. Check its sibling
.state.json for human annotations before revising.
