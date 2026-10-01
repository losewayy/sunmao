//! `sunmao serve` — GUI 阶段 1：localhost HTTP + WebSocket，浏览器即前端；
//! 阶段 2 起同一宿主（`spawn_host`/`HostHandle`）也喂 Tauri 壳的
//! scheme/Channel 适配层（GUI.md §8）。契约见 docs/GUI.md §7：WS 只载
//! `LiveEvent`（与 TUI/ACP 同源，不发明第三套方言），大对象走
//! `GET /artifacts/{name}` 路径引用。绑 127.0.0.1，无账号——单用户
//! 本地前端。
//!
//! 协议（单一事件通道 + REST 拉资源 — ws 或 Tauri Channel，同形 JSON）：
//!   host → client: {"type":"live","event":<LiveEvent>} | {"type":"replay",
//!                  "events":[<SessionEvent>]} | {"type":"approval","id",
//!                  "tool","detail","why"} | {"type":"note","text"} |
//!                  {"type":"session","id"} | {"type":"model","label"} |
//!                  {"type":"busy","busy":bool}
//!   client → host: {"type":"prompt","text"} | {"type":"cancel"} |
//!                  {"type":"approval","id","verdict":"once|session|deny"} |
//!                  {"type":"resume","id"} | {"type":"fork","id"} |
//!                  {"type":"annotate","name","note"} | {"type":"model","sel"}
//!   REST: GET /sessions · GET /session · POST /session/new ·
//!         POST /session/{id}/resume|fork|rename · DELETE /session/{id} ·
//!         GET /session/{id}/events · GET /session/{id}/turns ·
//!         POST /session/{id}/rewind · GET /artifacts/{name} ·
//!         GET /artifacts/{name}/revs|notes|ui ·
//!         POST /artifacts/{name}/annotate · GET /dataflow[/{id}]

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use sunmao_core::{Context, SessionLog};
use tokio::sync::{broadcast, mpsc};

mod artifacts;
mod client;
mod driver;
mod host;
mod http;
mod request;
mod ws;

use host::SessionOp;

pub use client::Client;
pub use host::HostHandle;
pub use request::HostResponse;

/// Index page — the workbench prototype adapted as the product frontend
/// (docs/DESIGN-SYSTEM.md tokens are its source of truth).
const INDEX: &str = include_str!("serve/assets/index.html");

/// Design tokens (custom properties + keyframes) and component styles —
/// split per docs/DESIGN-SYSTEM.md §1; served on the shared route table
/// so the Tauri `sunmao` scheme answers them identically.
const TOKENS_CSS: &str = include_str!("serve/assets/tokens.css");
const APP_CSS: &str = include_str!("serve/assets/app.css");

/// The page's script surface — index.html's inline script split by
/// responsibility (plain `<script src>` classic scripts, not modules: the
/// replay-parity harness evals them in one shared scope). Order in
/// index.html is load order.
const STATE_JS: &str = include_str!("serve/assets/state.js");
const WALLPAPER_JS: &str = include_str!("serve/assets/wallpaper.js");
const SETTINGS_JS: &str = include_str!("serve/assets/settings.js");
const DIFF_JS: &str = include_str!("serve/assets/diff.js");
const TRANSCRIPT_JS: &str = include_str!("serve/assets/transcript.js");
const ISLANDS_JS: &str = include_str!("serve/assets/islands.js");
const CONNECTION_JS: &str = include_str!("serve/assets/connection.js");
const COMPOSER_JS: &str = include_str!("serve/assets/composer.js");
const PALETTE_JS: &str = include_str!("serve/assets/palette.js");
const FIND_JS: &str = include_str!("serve/assets/find.js");
const MENUS_JS: &str = include_str!("serve/assets/menus.js");
const BOOT_JS: &str = include_str!("serve/assets/boot.js");

/// MCP Apps sandbox proxy — a separate origin serving a single static
/// page (`serve/assets/sandbox.html`); see `http::sandbox_page`/`ui/`
/// bridge. `pub` for the Tauri shell's `sunmao-sandbox` scheme handler —
/// same bytes, no HTTP listener under the shell.
pub const SANDBOX_PAGE: &str = include_str!("serve/assets/sandbox.html");

/// A factory `main.rs` installs at startup: `build` reproduces the exact
/// Context assembly a session needs (provider, registry, MCP tools,
/// extensions, approver, model routes, `--loop`) so every session the host
/// adopts — startup, new, resume, fork — gets a fully-seeded kernel, not a
/// borrowed one. `cwd` is the *session's* project dir — adopted logs carry
/// their own root (a session resumed from a different project keeps its
/// tools/sessions dir, not the launch dir).
/// The closure `SessionFactory.make` wraps — (log, approver, session cwd)
/// → Context.
pub(crate) type MakeFn = dyn Fn(SessionLog, Arc<dyn sunmao_core::approval::Approver>, std::path::PathBuf) -> SessionBuild
    + Send
    + Sync;

pub(crate) struct SessionFactory {
    pub make: Box<MakeFn>,
}

/// The future `SessionFactory::make` returns (boxed so the closure stays
/// object-safe — the log/approver/cwd move in, nothing is borrowed).
pub(crate) type SessionBuild =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Context>> + Send>>;

impl SessionFactory {
    pub async fn build(
        &self,
        log: SessionLog,
        approver: Arc<dyn sunmao_core::approval::Approver>,
        cwd: std::path::PathBuf,
    ) -> Result<Arc<Context>> {
        Ok(Arc::new((self.make)(log, approver, cwd).await?))
    }
}

/// Everything a host needs to come up — `lib.rs` assembles it once from
/// `Cli` and hands it to `spawn_host` (Tauri scheme path) or
/// `http::run` (loopback HTTP path). `first_log` is the `--resume`/`--fork`
/// target resolved by main — `None` boots a fresh seeded session.
pub(crate) struct HostSpec {
    pub factory: SessionFactory,
    pub cwd: std::path::PathBuf,
    pub roots: Vec<std::path::PathBuf>,
    /// `--system` override (already assembled); `None` → each session's
    /// prompt is assembled from *its* project dir
    pub prompt_override: Option<String>,
    pub model_label: String,
    pub first_log: Option<(SessionLog, &'static str)>,
}

/// Build the multi-session host without binding any listener: `Shared`
/// state, the mgmt lane, and the bootstrap session adopt. `sandbox_port`
/// is what hello advertises for the MCP Apps double-iframe's second
/// origin — the HTTP path fills in its sandbox listener's port, the Tauri
/// shell passes 0 (the frontend substitutes the `sunmao-sandbox` scheme).
pub(crate) async fn spawn_host(spec: HostSpec, sandbox_port: u16) -> Result<HostHandle> {
    let (live, _) = broadcast::channel::<serde_json::Value>(512);
    let (mgmt_tx, mgmt_rx) = mpsc::unbounded_channel::<SessionOp>();
    let shared = Arc::new(host::Shared {
        cwd: spec.cwd,
        roots: spec.roots,
        live,
        sessions: Mutex::new(HashMap::new()),
        factory: spec.factory,
        prompt_override: spec.prompt_override,
        model_label: spec.model_label,
        sandbox_port,
        approval_ids: Arc::new(AtomicU64::new(0)),
        mgmt: mgmt_tx,
    });
    tokio::spawn(host::mgmt_loop(shared.clone(), mgmt_rx));

    // bootstrap session: `first_log` carries --resume/--fork; otherwise a
    // fresh seeded log — the host, not main.rs, owns session assembly
    match spec.first_log {
        Some((log, source)) => {
            shared.adopt(log, source).await?;
        }
        None => {
            host::new_session(&shared, None).await?;
        }
    }
    Ok(HostHandle { s: shared })
}

/// `sunmao serve` — bind-loopback HTTP host, runs until the process exits.
/// Thin shell over `spawn_host` + `http::run`.
pub(crate) async fn run(spec: HostSpec, listener: std::net::TcpListener) -> Result<()> {
    http::run(spec, listener).await
}
