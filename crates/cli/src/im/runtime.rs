//! `sunmao im` — the daemon wiring. Owns the inbound loop: adapter →
//! authz → session → reply. Reuses `serve`'s Host/Shared so a session's
//! queue, driver, live bus and approval seam are the same objects the
//! GUI drives — IM is another frontend, not a parallel kernel.
//!
//! Inbound pipeline per message:
//!   adapter InboundMsg
//!     → control commands inline (/stop, /pairing mgmt — never the FIFO:
//!       Hermes #4926 — a busy session's control path must not block
//!       behind the turn it's meant to steer)
//!     → authz (pairing code offer / allowlist / silent)
//!     → route (session key) → pending-reply registration
//!     → busy? push_steer : queue push (steer folds at the next request
//!       boundary; leftovers drain as follow-up input by the driver)

use std::sync::Arc;

use anyhow::{Context as _, Result};
use tokio::sync::mpsc;

use sunmao_core::agent::ApprovalMode;

use crate::Cli;
use crate::serve::host::Host;

use super::authz::Verdict;
use super::channels::{self, InboundMsg};
use super::config::ChannelsConfig;
use super::deliver::{self, Delivery};
use super::progress::{self, ChatKey};
use super::route::{self, ImSource};
use super::store::Store;

/// The daemon's session id under dmScope=main — stable across restarts so
/// `/resume`/`im:main` routing always lands on the same log.
// `im:main`'s deterministic session id resolves through session_id_for —
// no separate constant, the route key is the single source.
/// Sender slot on queued inputs — IM never switches GUI tabs, so the
/// ticket's client field just needs a stable non-client marker.
const IM_CLIENT: u64 = u64::MAX;

/// `sunmao pairing <op>` — manage the admission ledger from the operator
/// console. `approve` consumes a live code and admits its sender; `list`
/// prints pending codes + the allowlist; `revoke` drops a sender.
pub(crate) fn pairing(op: &PairingOp) -> Result<()> {
    let store = Store::open(&super::config::state_dir())?;
    match op {
        PairingOp::List => {
            for row in store.pairing_all() {
                let mins = ((row.expires - super::store::now()) / 60).max(0);
                println!(
                    "{}\t{}:{} \t({mins}m left)",
                    row.code, row.channel, row.sender
                );
            }
            for (ch, sender, role) in store.allow_list() {
                println!("{ch}:{sender}\t{role}");
            }
        }
        PairingOp::Approve { code } => match store.pairing_approve(code)? {
            Some((ch, sender, role)) => {
                println!("approved {ch}:{sender} — role {role}");
            }
            None => anyhow::bail!("no live pairing code {code:?}"),
        },
        PairingOp::Revoke { sender } => {
            // `sender` may be "channel:id" or bare id — a bare id resolves
            // only when exactly one channel is enabled
            let (ch, id) = match sender.split_once(':') {
                Some((c, s)) => (c.to_string(), s.to_string()),
                None => (sole_channel()?, sender.clone()),
            };
            anyhow::ensure!(
                store.allow_remove(&ch, &id)?,
                "no allowlist entry for {sender}"
            );
            println!("revoked {ch}:{id}");
        }
    }
    Ok(())
}

/// The channel a bare `pairing revoke <id>` means — the sole enabled kind
/// in channels.json. Zero or several kinds must be spelled `channel:id`;
/// guessing would revoke the wrong channel's entry.
fn sole_channel() -> Result<String> {
    let mut kinds: Vec<&'static str> = Vec::new();
    if let Some(cfg) = ChannelsConfig::load(&super::config::config_path())? {
        for spec in cfg.enabled_specs() {
            if !kinds.contains(&spec.kind_name()) {
                kinds.push(spec.kind_name());
            }
        }
    }
    match kinds.as_slice() {
        [only] => Ok((*only).to_string()),
        _ => anyhow::bail!("sender id is ambiguous — write channel:id"),
    }
}

/// The `sunmao pairing` subcommand's arg shape — defined next to the code
/// that runs it (plugin.rs re-exports it into `Cmd`).
#[derive(clap::Args, Clone)]
pub struct PairingArgs {
    #[command(subcommand)]
    pub op: PairingOp,
}

#[derive(clap::Subcommand, Clone)]
pub enum PairingOp {
    /// List pending pairing codes and admitted senders.
    List,
    /// Approve a pairing code — admits its sender (first approval = owner).
    Approve {
        /// The 8-char code the sender received.
        code: String,
    },
    /// Remove a sender from the allowlist (`channel:id` or bare id).
    Revoke {
        /// Sender to remove — `telegram:12345`, or a bare `12345` when
        /// exactly one channel is enabled.
        sender: String,
    },
}

/// One live session lane — the host plus the progress state its turn_end
/// replies flow through. `main` scope has exactly one; `per_channel_peer`
/// spawns one lazily per session_key.
struct HostLane {
    host: Arc<Host>,
    progress: progress::Shared,
}

/// Everything the daemon needs once channels.json parsed.
struct Gateway {
    cfg: ChannelsConfig,
    store: Arc<Store>,
    /// the serve registry — adopts per-peer hosts lazily
    shared: Arc<crate::serve::host::Shared>,
    workspace: std::path::PathBuf,
    model: String,
    roots: Vec<std::path::PathBuf>,
    driver: Option<sunmao_core::agent::LoopDriver>,
    /// session_key → live lane. Under main scope it only ever holds
    /// `im:main`; under per_channel_peer each first contact binds one.
    lanes: tokio::sync::Mutex<std::collections::HashMap<String, HostLane>>,
    /// One entry per running channel — adapter and ledger are paired
    /// inside `Delivery`, so a lane iterates endpoints instead of zipping
    /// two positional vectors.
    endpoints: Vec<Arc<Delivery>>,
}

/// A session_key's deterministic session id — `im:telegram:dm:123` →
/// `im-telegram-dm-123`. Same resume-safety rule as im-main: the id
/// survives restarts because it's derived, not generated.
fn session_id_for(session_key: &str) -> String {
    session_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Open (or seed) a DM session log under the IM workspace. Deterministic
/// ids make logs resume-safe; `seed` runs exactly once (the Started+system
/// pair a fresh log needs before a turn can fold).
async fn open_session_log(
    workspace: &std::path::Path,
    session_id: &str,
    model: &str,
    roots: &[std::path::PathBuf],
    driver: Option<sunmao_core::agent::LoopDriver>,
) -> Result<sunmao_core::SessionLog> {
    let dir = workspace.join(".sunmao/sessions");
    let path = dir.join(format!("{session_id}.jsonl"));
    let fresh = !path.exists();
    // open() creates — open_path's contract is existing logs only
    let mut log = sunmao_core::SessionLog::open(&dir, session_id).await?;
    if fresh {
        log.append(&sunmao_core::SessionEvent::Started {
            model: model.to_string(),
            cwd: workspace.display().to_string(),
            driver: Some(
                driver
                    .unwrap_or_else(|| sunmao_core::agent::LoopDriver::resolve(workspace, roots))
                    .as_str()
                    .into(),
            ),
        })
        .await?;
        let mut asm = sunmao_core::prompt::PromptAssembler::new(workspace).with_extra_roots(roots);
        if let Some(d) = driver {
            asm = asm.with_driver(d);
        }
        let prompt = asm.assemble(None);
        log.append(&sunmao_core::SessionEvent::Message {
            message: sunmao_llm::types::Message::system(prompt),
        })
        .await?;
    }
    Ok(log)
}

/// `sunmao im` entry — build the host on the IM workspace, adopt the
/// main session, spawn one adapter per enabled channel, then run the
/// inbound loop until the process exits.
pub(crate) async fn run(cli: &Cli) -> Result<()> {
    let cfg = ChannelsConfig::load(&super::config::config_path())?
        .context("no ~/.sunmao/channels.json — nothing to run")?;
    anyhow::ensure!(
        cfg.enabled_specs().next().is_some(),
        "channels.json has no enabled channels"
    );

    // IM sessions live under the fixed workspace — not the launch cwd
    let workspace = super::config::state_dir().join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let workspace = workspace.canonicalize().unwrap_or(workspace);

    // reuse the serve host assembly, re-rooted at the workspace — prompt
    // sections read user-level files plus any `.sunmao/` under it
    let mut cli2 = cli.clone();
    cli2.cwd = workspace.clone();
    cli2.session_dir = workspace.join(".sunmao/sessions");
    let mut spec = crate::host_spec(&cli2).await?;
    spec.first_log = None; // the gateway owns session bootstrap, not --resume
    let (spec_roots, spec_driver) = (spec.roots.clone(), spec.driver_override);
    let handle = crate::serve::spawn_host(spec, 0).await?;
    let shared = &handle.s;

    let store = Arc::new(Store::open(&super::config::state_dir())?);

    // per-channel: adapter + delivery lane (progress lanes bind per
    // session lane — one under main, one per peer under per_channel_peer)
    let (tx, mut rx) = mpsc::channel::<InboundMsg>(256);
    let mut endpoints: Vec<Arc<Delivery>> = Vec::new();
    let mut names: Vec<&'static str> = Vec::new();

    for spec in cfg.enabled_specs() {
        let kind = spec.kind_name();
        // one adapter per channel id — routing, authz and the ledger all
        // key on the kind name, so a second block of a kind already
        // running would fight the first over the same channel
        if names.contains(&kind) {
            tracing::warn!("im channel {kind} declared twice — extra block skipped");
            continue;
        }
        let adapter = match channels::build(spec, store.clone()) {
            Ok(a) => a,
            // one broken channel block must not take the daemon down —
            // the rest keep polling and the failure lands in the log
            Err(e) => {
                tracing::warn!("im channel {kind} skipped: {e:#}");
                continue;
            }
        };
        let delivery = Arc::new(Delivery::new(store.clone(), adapter.clone()));
        delivery.resend_outstanding().await;
        let a = adapter.clone();
        let tx = tx.clone();
        tokio::spawn(async move { a.poll(tx).await });
        endpoints.push(delivery);
        names.push(kind);
    }
    anyhow::ensure!(!endpoints.is_empty(), "no channel adapter started");
    // connection liveness lands in the status file the serve page reads
    write_status(&names);
    // the loop below ends when the last adapter drops its sender; without
    // this the daemon's own handle would keep the inbound stream open
    drop(tx);

    let gw = Gateway {
        cfg: cfg.clone(),
        store: store.clone(),
        shared: shared.clone(),
        workspace: workspace.clone(),
        model: cli.model.clone(),
        roots: spec_roots.clone(),
        driver: spec_driver,
        lanes: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        endpoints,
    };
    // dmScope=main eagerly binds the shared lane — same warm session the
    // daemon has always started with; per_channel_peer binds lazily on
    // each chat's first message
    if matches!(gw.cfg.dm_scope, super::config::DmScope::Main) {
        gw.lane_for("im:main").await?;
    }
    eprintln!(
        "sunmao im — {} polling, dmScope={:?}, full_access",
        names.join("+"),
        gw.cfg.dm_scope
    );
    while let Some(msg) = rx.recv().await {
        if let Err(e) = gw.dispatch(&msg).await {
            tracing::warn!("im inbound: {e:#}");
        }
    }
    Ok(())
}

/// `~/.sunmao/im/status.json` — the serve page's channels section reads
/// this; written at startup and per reconnect-worthy event.
fn write_status(channels: &[&str]) {
    let v = serde_json::json!({
        "updated": super::store::now(),
        "channels": channels,
    });
    let _ = std::fs::write(
        super::config::state_dir().join("status.json"),
        v.to_string(),
    );
}

impl Gateway {
    /// Resolve (or lazily bind) the live lane for a session_key — one
    /// shared host under `main`, one host per chat under
    /// `per_channel_peer`. Binding adopts the deterministic session id so
    /// restarts re-open the same transcript.
    async fn lane_for(&self, session_key: &str) -> Result<(Arc<Host>, progress::Shared)> {
        if let Some(l) = self.lanes.lock().await.get(session_key) {
            return Ok((l.host.clone(), l.progress.clone()));
        }
        let session_id = session_id_for(session_key);
        let log = open_session_log(
            &self.workspace,
            &session_id,
            &self.model,
            &self.roots,
            self.driver,
        )
        .await?;
        let host = self.shared.adopt(log, "startup").await?;
        // pinned at adopt — IM sessions are full_access end to end (no
        // buttons on the channel side); `deny` rules still bind in the gate.
        host.agent
            .set_approval_mode(
                ApprovalMode::FullAccess,
                &crate::serve::host::WsObserver::new(self.shared.live.clone(), host.id.clone()),
            )
            .await;
        let progress = progress::new_shared();
        // ONE progress subscriber per lane — it owns the lane's final
        // text and fans the reply out to every channel on the lane
        tokio::spawn(progress::run(
            host.id.clone(),
            self.shared.live.subscribe(),
            progress.clone(),
            self.endpoints.clone(),
        ));
        self.lanes.lock().await.insert(
            session_key.to_string(),
            HostLane {
                host: host.clone(),
                progress: progress.clone(),
            },
        );
        Ok((host, progress))
    }

    /// One inbound DM through the whole pipeline. Control commands resolve
    /// HERE, before the FIFO — `/stop` on a busy session must trip the
    /// cancel flag immediately, not queue behind the turn it's stopping.
    async fn dispatch(&self, msg: &InboundMsg) -> Result<()> {
        let src = &msg.source;
        let text = msg.text.trim();

        // ── control commands — inline, never queued ──
        if let Some(cmd) = text.strip_prefix('/') {
            // admission first: /pairing mgmt needs owner, the rest needs a
            // seat at all — a stranger's "/stop" must not cancel a session
            if !super::authz::admitted(&self.cfg, &self.store, src) {
                return self.authz_reply(src).await;
            }
            return self.control(src, cmd).await;
        }

        // ── admission ──
        if !super::authz::admitted(&self.cfg, &self.store, src) {
            return self.authz_reply(src).await;
        }

        // ── route + dispatch into the session ──
        let route = route::route_for(self.cfg.dm_scope, src);
        let (host, progress) = self.lane_for(&route.session_key).await?;
        // first contact on this key binds the route to the live session —
        // rewrites are no-ops once bound
        if self.store.route_get(&route.session_key).as_deref() != Some(&host.id) {
            self.store.route_put(&route.session_key, &host.id)?;
        }
        let prompt = route::prompt_text(src, text);
        let busy = host.busy.load(std::sync::atomic::Ordering::Relaxed) > 0;
        progress::expect_reply(
            &progress,
            ChatKey {
                channel: src.channel.clone(),
                chat_id: src.chat_id.clone(),
            },
            busy,
        );
        if busy {
            // steer — the running turn folds this at its next request
            // boundary; the running tool and any sub-agents are untouched
            host.agent.push_steer(IM_CLIENT, prompt);
        } else {
            // idle → FIFO; the driver's dispatch_input paints the bubble
            // and runs the turn
            let id = host
                .queue_next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            host.queue
                .lock()
                .unwrap()
                .push_back(crate::serve::host::Input {
                    id,
                    client: IM_CLIENT,
                    text: prompt,
                    attachments: Vec::new(),
                });
            host.agent
                .context()
                .input_pending
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            host.queue_notify.notify_one();
        }
        Ok(())
    }

    /// Control commands — `/stop` and the owner's `/pairing` mgmt. All
    /// replies are ledgered final-style sends (plain text).
    async fn control(&self, src: &ImSource, cmd: &str) -> Result<()> {
        let reply = match cmd.split_whitespace().next().unwrap_or("") {
            "stop" => {
                // arming cancel while IDLE poisons the next turn — the
                // flag only clears at a run_turn end that never comes.
                // main-agent stop only — sub-agents keep running (owner
                // decision §7-5; the cascade form is `agent.cancel()`).
                // The lane must already exist — a /stop doesn't create one.
                let route = route::route_for(self.cfg.dm_scope, src);
                let host = self
                    .lanes
                    .lock()
                    .await
                    .get(&route.session_key)
                    .map(|l| l.host.clone());
                if let Some(host) = host
                    && host.busy.load(std::sync::atomic::Ordering::Relaxed) > 0
                {
                    host.agent.cancel_main();
                    super::messages::get("stopped")
                } else {
                    super::messages::get("stopped_idle")
                }
            }
            "pairing" => {
                if !super::authz::is_owner(&self.cfg, &self.store, src) {
                    return Ok(());
                }
                let mut out = super::messages::get("pairing_pending");
                for row in self.store.pairing_all() {
                    let mins = ((row.expires - super::store::now()) / 60).max(0);
                    out.push_str(
                        &super::messages::get("pairing_row")
                            .replace("{code}", &row.code)
                            .replace("{sender}", &row.sender)
                            .replace("{mins}", &mins.to_string()),
                    );
                    out.push('\n');
                }
                out
            }
            "mode" => super::messages::get("mode_fixed"),
            _ => super::messages::get("help"),
        };
        if !reply.is_empty()
            && let Some(d) = deliver::for_channel(&self.endpoints, &src.channel)
        {
            let _ = d.send_final(&src.chat_id, &reply).await;
        }
        Ok(())
    }

    /// What a stranger gets back — the code offer, a cooldown note, or
    /// silence, per `unauthorized_dm_behavior`.
    async fn authz_reply(&self, src: &ImSource) -> Result<()> {
        if let Verdict::Reply(text) = super::authz::authorize(&self.cfg, &self.store, src)
            && let Some(d) = deliver::for_channel(&self.endpoints, &src.channel)
        {
            let _ = d.send_final(&src.chat_id, &text).await;
        }
        Ok(())
    }
}
