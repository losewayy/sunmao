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
- Windows is a first-class citizen: tools can't assume a POSIX shell.
  Without a shell pin, `Bash` auto-selects PowerShell 7 on Windows when
  `pwsh` is available; otherwise it uses embedded POSIX. `deno_task_shell`
  remains an explicit portable option. `Grep` spawns `rg` directly.

## Workflow

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # zero warnings
cargo test --workspace
```

Then commit `area: imperative`, push. CI runs the same three on
`windows-latest` — a red run emails the maintainer.

## Packaging

```bash
cargo tauri build     # NSIS installer; version lives in crates/gui/tauri.conf.json
```

An installer that leaves your machine needs one extra step. A release binary
embeds panic locations, and those carry absolute source paths from the
toolchain and the crate registry — so an un-remapped build ships your home
directory as plain strings inside the exe. `--remap-path-prefix` rewrites
them at compile time (it changes `RUSTFLAGS`, so expect a full rebuild):

```bash
# Windows (Git Bash; $USERPROFILE is already in native form)
RUSTFLAGS="--remap-path-prefix=$USERPROFILE=/build --remap-path-prefix=$USERPROFILE\\.cargo=/cargo --remap-path-prefix=$USERPROFILE\\.rustup=/rustup" cargo tauri build
# macOS / Linux
RUSTFLAGS="--remap-path-prefix=$HOME=/build --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup" cargo tauri build
```

Verify the artifact, not the intent: search the built exe for your own home
path / user name (want 0 hits), and run `cargo package --list` before
publishing the crate. Nothing here should ever be committed with a literal
builder path in it.

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
