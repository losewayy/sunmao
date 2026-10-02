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
use super::channels::{ChannelAdapter, InboundMsg, TelegramAdapter};
use super::config::ChannelsConfig;
use super::deliver::Delivery;
use super::progress::{self, ChatKey};
use super::route::{self, ImSource};
use super::store::Store;

/// The daemon's session id under dmScope=main — stable across restarts so
/// `/resume`/`im:main` routing always lands on the same log.
const MAIN_SESSION_ID: &str = "im-main";
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
        PairingOp::Approve { code } => match store.pairing_consume(code) {
            Some((ch, sender)) => {
                let role = store.allow_add(&ch, &sender)?;
                println!("approved {ch}:{sender} — role {role}");
            }
            None => anyhow::bail!("no live pairing code {code:?}"),
        },
        PairingOp::Revoke { sender } => {
            // `sender` may be "channel:id" or bare id — try both spellings
            let (ch, id) = sender
                .split_once(':')
                .map(|(c, s)| (c.to_string(), s.to_string()))
                .unwrap_or_else(|| ("telegram".to_string(), sender.clone()));
            anyhow::ensure!(
                store.allow_remove(&ch, &id)?,
                "no allowlist entry for {sender}"
            );
            println!("revoked {ch}:{id}");
        }
    }
    Ok(())
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
        /// Sender to remove — `telegram:12345` or `12345`.
        sender: String,
    },
}

/// Everything the daemon needs once channels.json parsed.
struct Gateway {
    cfg: ChannelsConfig,
    store: Arc<Store>,
    host: Arc<Host>,
    progress: progress::Shared,
}

/// Open (or seed) the shared DM session log under the IM workspace.
/// `im-main.jsonl` is the dmScope=main anchor — resume-safe because the
/// id is deterministic; `seed` runs exactly once (the Started+system pair
/// a fresh log needs before a turn can fold).
async fn open_main_log(
    workspace: &std::path::Path,
    model: &str,
) -> Result<sunmao_core::SessionLog> {
    let dir = workspace.join(".sunmao/sessions");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{MAIN_SESSION_ID}.jsonl"));
    let fresh = !path.exists();
    let mut log = sunmao_core::SessionLog::open_path(&path).await?;
    if fresh {
        log.append(&sunmao_core::SessionEvent::Started {
            model: model.to_string(),
            cwd: workspace.display().to_string(),
        })
        .await?;
        let prompt = sunmao_core::prompt::PromptAssembler::new(workspace).assemble(None);
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
        cfg.telegram().is_some(),
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
    let handle = crate::serve::spawn_host(spec, 0).await?;
    let shared = &handle.s;

    let log = open_main_log(&workspace, &cli.model).await?;
    let host = shared.adopt(log, "startup").await?;
    // pinned at adopt — IM sessions are full_access end to end (no
    // buttons on the channel side); `deny` rules still bind in the gate.
    // set_approval_mode writes a durable ModeChange so restarts keep it.
    host.agent
        .set_approval_mode(
            ApprovalMode::FullAccess,
            &crate::serve::host::WsObserver::new(shared.live.clone(), host.id.clone()),
        )
        .await;

    let store = Arc::new(Store::open(&super::config::state_dir())?);
    store.route_put("im:main", &host.id)?;

    // per-channel: adapter + delivery lane + progress subscriber
    let (tx, mut rx) = mpsc::channel::<InboundMsg>(256);
    let state = progress::new_shared();
    let mut deliveries: Vec<Arc<Delivery>> = Vec::new();

    if let Some(tg) = cfg.telegram() {
        let adapter: Arc<dyn ChannelAdapter> = Arc::new(TelegramAdapter::new(tg, store.clone())?);
        let delivery = Arc::new(Delivery::new(store.clone(), adapter.clone()));
        delivery.resend_outstanding().await;
        let prog = progress::run(
            host.id.clone(),
            shared.live.subscribe(),
            state.clone(),
            adapter.clone(),
            delivery.clone(),
        );
        tokio::spawn(prog);
        let a = adapter.clone();
        tokio::spawn(async move { a.poll(tx).await });
        deliveries.push(delivery);
        // connection liveness lands in the status file the serve page reads
        write_status(&["telegram"]);
    }

    let gw = Gateway {
        cfg: cfg.clone(),
        store,
        host,
        progress: state,
    };
    eprintln!("sunmao im — telegram polling, dmScope=main, full_access");
    while let Some(msg) = rx.recv().await {
        if let Err(e) = gw.dispatch(&msg, &deliveries).await {
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

/// `&deliveries[0]` lookup by channel name — multi-channel lands in v0.4.
fn lane<'a>(deliveries: &'a [Arc<Delivery>], channel: &str) -> Option<&'a Arc<Delivery>> {
    deliveries.iter().find(|d| d.channel() == channel)
}

impl Gateway {
    /// One inbound DM through the whole pipeline. Control commands resolve
    /// HERE, before the FIFO — `/stop` on a busy session must trip the
    /// cancel flag immediately, not queue behind the turn it's stopping.
    async fn dispatch(&self, msg: &InboundMsg, deliveries: &[Arc<Delivery>]) -> Result<()> {
        let src = &msg.source;
        let text = msg.text.trim();

        // ── control commands — inline, never queued ──
        if let Some(cmd) = text.strip_prefix('/') {
            // admission first: /pairing mgmt needs owner, the rest needs a
            // seat at all — a stranger's "/stop" must not cancel a session
            if !super::authz::admitted(&self.cfg, &self.store, src) {
                return self.authz_reply(src, deliveries).await;
            }
            return self.control(src, cmd, deliveries).await;
        }

        // ── admission ──
        if !super::authz::admitted(&self.cfg, &self.store, src) {
            return self.authz_reply(src, deliveries).await;
        }

        // ── route + dispatch into the session ──
        let route = route::route_for(self.cfg.dm_scope, src);
        // first contact on this key binds the route to the live session —
        // rewrites are no-ops once bound
        if self.store.route_get(&route.session_key).as_deref() != Some(&self.host.id) {
            self.store.route_put(&route.session_key, &self.host.id)?;
        }
        progress::expect_reply(
            &self.progress,
            ChatKey {
                channel: src.channel.clone(),
                chat_id: src.chat_id.clone(),
            },
        );
        let prompt = route::prompt_text(src, text);
        let busy = self.host.busy.load(std::sync::atomic::Ordering::Relaxed) > 0;
        if busy {
            // steer — the running turn folds this at its next request
            // boundary; the running tool and any sub-agents are untouched
            self.host.agent.push_steer(IM_CLIENT, prompt);
        } else {
            // idle → FIFO; the driver's dispatch_input paints the bubble
            // and runs the turn
            let id = self
                .host
                .queue_next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.host
                .queue
                .lock()
                .unwrap()
                .push_back(crate::serve::host::Input {
                    id,
                    client: IM_CLIENT,
                    text: prompt,
                    attachments: Vec::new(),
                });
            self.host
                .agent
                .context()
                .input_pending
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.host.queue_notify.notify_one();
        }
        Ok(())
    }

    /// Control commands — `/stop` and the owner's `/pairing` mgmt. All
    /// replies are ledgered final-style sends (plain text).
    async fn control(&self, src: &ImSource, cmd: &str, deliveries: &[Arc<Delivery>]) -> Result<()> {
        let reply = match cmd.split_whitespace().next().unwrap_or("") {
            "stop" => {
                // main-agent stop only — sub-agents keep running (owner
                // decision §7-5; the cascade form is `agent.cancel()`)
                self.host.agent.cancel_main();
                super::messages::get("stopped")
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
            && let Some(d) = lane(deliveries, &src.channel)
        {
            let _ = d.send_final(&src.chat_id, &reply).await;
        }
        Ok(())
    }

    /// What a stranger gets back — the code offer, a cooldown note, or
    /// silence, per `unauthorized_dm_behavior`.
    async fn authz_reply(&self, src: &ImSource, deliveries: &[Arc<Delivery>]) -> Result<()> {
        if let Verdict::Reply(text) = super::authz::authorize(&self.cfg, &self.store, src)
            && let Some(d) = lane(deliveries, &src.channel)
        {
            let _ = d.send_final(&src.chat_id, &text).await;
        }
        Ok(())
    }
}
