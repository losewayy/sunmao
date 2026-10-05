//! IM gateway — `sunmao im`: channel adapters (Telegram first) feed an
//! inbound stream into the same `Shared`/`Host` machinery `serve` uses;
//! replies flow back through a durable delivery ledger. Layers:
//!
//! ```text
//! channels/*   transport only — poll, send_text, edit_text, send_typing
//! config.rs    ~/.sunmao/channels.json (cold-plug, credentials by ref)
//! store.rs     ~/.sunmao/im/state.db — routing index / pairing / ledger
//! authz.rs     pairing | allowlist | open | disabled admission
//! route.rs     build_session_key — the single derivation point
//! progress.rs  LiveEvent → throttled draft edits + final fan-out
//! deliver.rs   pending → attempting → delivered, startup replay
//! ```
//!
//! Hard rules the design pinned (owner §7): dmScope=main merges every DM
//! into one session; a busy session steers (never queues) at the next
//! request boundary; `/stop` stops the main agent only — sub-agents keep
//! running; approval mode is `full_access` end-to-end (no IM buttons) —
//! pairing/allowlist is the gate and `deny` permission rules are the net.

pub mod authz;
pub mod channels;
pub mod config;
mod deliver;
mod messages;
pub mod progress;
mod redact;
pub mod route;
mod scope;
pub mod store;

mod runtime;
pub use runtime::PairingArgs;
pub(crate) use runtime::{pairing, run};

/// A scratch directory no other test can claim, removed if a previous run
/// left it behind. The process id is not enough on its own: tests run
/// concurrently inside one process, and a coarse clock can hand two of them
/// the same timestamp, which leaves both fighting over one SQLite file.
#[cfg(test)]
pub(crate) fn test_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "sunmao-im-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}
