<div align="center">

# sunmao（榫卯）

**A Rust agent harness where auditability is the foundation, not an afterthought.**

</div>

> **Status: early development (v0.1 in progress).** The kernel is being built in
> public — see [`docs/SPEC.md`](docs/SPEC.md) for the design contract.

sunmao is an agent harness kernel built on three convictions:

- **Seams, not monoliths.** Sessions, tools, the agent loop itself — every
  capability sits behind a replaceable seam (`ctx.*`). Swapping a subsystem is a
  configuration act, not a fork.
- **Compatibility is a dialect, not a favor.** The hook engine speaks the
  dominant lifecycle contract natively, with dialect plugins for everything
  else. Third-party tooling written for the mainstream contract works
  unmodified.
- **Every extension is a boundary.** Extensions live across process boundaries:
  crash-isolated, language-agnostic, and fully auditable. The session's single
  source of truth is an append-only event log — transcripts, replay, and
  data-flow reports are free corollaries.

The name: 榫卯 (sǔnmǎo) is the Chinese joinery tradition where timber members
connect through precisely-cut interlocking joints — no nails, no welding.
That's the architecture: components joined by seams, every seam a contract.

## Layout

```text
crates/
├── llm/    # provider protocol adapters (OpenAI-compatible first), hand-rolled SSE
├── core/   # sessions (event-sourced log), tools, agent loop, seams
└── cli/    # `sunmao` binary — REPL first, TUI later
```

## License

Dual-licensed under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE).
