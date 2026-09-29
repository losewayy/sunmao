# Contributing

Small surface, strict invariants. Read `AGENTS.md` first — the invariants
list is the real contributor guide; this file is the human frame around it.

## Ground rules

- Seams earn their existence: don't add an abstraction without a second real
  implementation wanting it. Concrete types until then.
- Audit is the floor: every tool call produces durable `SessionEvent`s;
  features that bypass the log are bugs.
- No speculative generality. If a compat layer exists, it's because a real
  file format/ecosystem demanded it (Claude settings, `mcpServers`,
  `agents/*.md` — all observable contracts).
- Windows is a first-class citizen: tools can't assume a POSIX shell,
  `Bash` runs embedded POSIX via `deno_task_shell`, `Grep` spawns `rg`
  directly.

## Workflow

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # zero warnings
cargo test --workspace
```

Then commit `area: imperative`, push. CI runs the same three on
`windows-latest` — a red run emails the maintainer.

## Docs discipline

- `docs/SPEC.md` = design contract (what we intend)
- `docs/ARCHITECTURE.md` = current state (what exists)
- `docs/CONFIG.md` = every file sunmao reads
- `docs/PROTOCOLS.md` = the wire dialects
- `docs/TESTING.md` = how to prove a feature works
- `docs/HANDOFF.md` = maintainer-private state delta (gitignored; not in
  the public repo)
- `CHANGELOG.md` = what landed, per version

If code and docs disagree, fix whichever is wrong — never both claim
different facts.
