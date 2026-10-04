//! sunmao-core — the kernel: event-sourced sessions, tool registry, agent loop.
//!
//! Design contract: `docs/SPEC.md`. The seam-first rule is "earn your seam" —
//! an interface appears only once a second real implementation demands it.

pub mod agent;
pub mod agents;
pub mod approval;
pub mod checkpoints;
pub mod console;
pub mod context;
pub mod ext;
pub mod hooks;
pub mod mcp;
pub mod model_knowledge;
pub mod models;
pub mod permissions;
pub mod plugin;
pub mod preflight;
pub mod presets;
pub mod prompt;
pub mod session;
pub mod task;
pub mod tool;
pub mod web;

pub use agent::{AgentLoop, TurnOutcome};
pub use context::Context;
pub use session::{SessionEvent, SessionLog};
pub use tool::{ToolRegistry, ToolResult};

/// Directory enumeration in **sorted path order** — `read_dir` order is
/// filesystem-dependent, and every scan that feeds the serialized request
/// (skills index, agent defs, hook/plugin/extension manifests, command
/// files) would otherwise churn the prompt/tools prefix and defeat
/// provider-side prompt caching. Deterministic enumeration is a
/// cache-hit invariant: apply it anywhere the order reaches the wire.
pub(crate) fn sorted_entries(dir: &std::path::Path) -> Vec<std::fs::DirEntry> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().collect())
        .unwrap_or_default();
    v.sort_by_key(|e| e.path());
    v
}

/// pid-keyed temp dirs recycle (Windows PIDs wrap fast) — a second test run
/// landing on a recycled pid inherited leftover files and flaked. Nanos makes
/// each caller's scratch dir unique. Test-only; production code keeps its own
/// naming.
#[cfg(test)]
pub(crate) fn fresh_test_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("sunmao-test-{tag}-{}-{nanos}", std::process::id()))
}

/// Compile a `tests/fixtures/*.rs` child-process binary once per test run
/// (each fixture gets its own `OnceLock` cache). `rustc` rides with the
/// toolchain cargo came from — PATH first, CARGO's sibling as fallback.
/// Returns None when rustc isn't on this box — live tests degrade to a
/// skip, the contract fixtures have always carried.
#[cfg(test)]
pub(crate) fn compile_fixture(name: &str, stem: &str) -> Option<std::path::PathBuf> {
    let rustc = std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path).find_map(|dir| {
                ["rustc", "rustc.exe"]
                    .iter()
                    .map(|n| dir.join(n))
                    .find(|c| c.is_file())
            })
        })
        .or_else(|| {
            let sib = std::path::Path::new(env!("CARGO"))
                .parent()?
                .join(if cfg!(windows) { "rustc.exe" } else { "rustc" });
            sib.is_file().then_some(sib)
        })?;
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let out = std::env::temp_dir().join(if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    });
    let tmp = out.with_extension("tmp");
    let status = std::process::Command::new(&rustc)
        .args(["--edition", "2021", "-O"])
        .arg(&src)
        .arg("-o")
        .arg(&tmp)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    // another test may have finished first — either way `out` ends up
    // whole (rename over an existing dest replaces it on both OSes).
    let _ = std::fs::rename(&tmp, &out);
    out.is_file().then_some(out)
}
