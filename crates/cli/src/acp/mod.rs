//! `sunmao --acp` — Agent Client Protocol server over stdio (v2 schema).
//!
//! Any ACP client (Zed, JetBrains plugins, …) can drive a sunmao session.
//! JSON-RPC owns stdout; diagnostics go to stderr.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v2;
use agent_client_protocol::{Agent, Client, Error, Responder, Result, Stdio, V2ConnectionTo};
use sunmao_core::agent::{AgentLoop, TurnOutcome};
use sunmao_core::tool::builtin_registry;
use sunmao_core::{Context, SessionEvent, SessionLog};
use sunmao_llm::OaiClient;
use sunmao_llm::types::Message;

mod observer;
use observer::{AcpApprover, AcpObserver};

struct SessionState {
    agent: AgentLoop,
    ctx: Arc<Context>,
    next_msg: u64,
}

struct SunmaoAgent {
    sessions: Mutex<HashMap<String, Arc<Mutex<SessionState>>>>,
    base_url: String,
    api_key: String,
    model: String,
    provider: String,
    /// `--preset` args, resolved per session — the client's cwd arrives in
    /// the request, not on our CLI.
    preset_names: Vec<String>,
}

impl SunmaoAgent {
    fn new_llm(&self) -> Arc<dyn sunmao_llm::ProviderAdapter> {
        match self.provider.as_str() {
            "anthropic" => Arc::new(sunmao_llm::AnthropicClient::new(
                self.base_url.clone(),
                self.api_key.clone(),
                self.model.clone(),
            )),
            _ => Arc::new(OaiClient::new(
                self.base_url.clone(),
                self.api_key.clone(),
                self.model.clone(),
            )),
        }
    }

    /// Model routing seam — same resolution as every other frontend.
    fn new_resolver(&self, cwd: &std::path::Path) -> Arc<sunmao_core::models::ModelResolver> {
        Arc::new(sunmao_core::models::ModelResolver::load(
            cwd,
            sunmao_core::models::ProviderDef {
                base_url: self.base_url.clone(),
                api_key_env: None,
                api_key: Some(self.api_key.clone()),
                dialect: self.provider.clone(),
            },
            "default",
        ))
    }

    /// Resolve this session's presets against the request cwd. An unknown
    /// name must fail the session — running looser than requested is worse
    /// than refusing.
    fn presets(
        &self,
        cwd: &std::path::Path,
    ) -> std::result::Result<Vec<std::path::PathBuf>, Error> {
        sunmao_core::presets::resolve(cwd, &self.preset_names)
            .map_err(|e| invalid_params(format!("presets: {e:#}")))
    }
}

fn invalid_params(msg: impl ToString) -> Error {
    Error::invalid_params().data(msg.to_string())
}

/// The approval-mode config option every ACP session advertises — the
/// client's `session/set_config_option` selects map onto SPEC §4.6 stances.
fn mode_config(current: sunmao_core::agent::ApprovalMode) -> v2::SessionConfigOption {
    v2::SessionConfigOption::select(
        "mode",
        "Approval mode",
        current.as_str(),
        sunmao_core::agent::ApprovalMode::ALL
            .iter()
            .map(|m| {
                v2::SessionConfigSelectOption::new(
                    m.as_str(),
                    match m {
                        sunmao_core::agent::ApprovalMode::AlwaysAsk => "Ask for approval",
                        sunmao_core::agent::ApprovalMode::Auto => "Auto",
                        sunmao_core::agent::ApprovalMode::ReadOnly => "Read only",
                        sunmao_core::agent::ApprovalMode::FullAccess => "Full access",
                    },
                )
            })
            .collect::<Vec<_>>(),
    )
    .category(v2::SessionConfigOptionCategory::Mode)
    .description("Gate stance per SPEC §4.6 — always_ask/auto/read_only/full_access; deny rules apply in every mode")
}

pub async fn run(
    base_url: &str,
    api_key: &str,
    model: &str,
    provider: &str,
    preset_names: &[String],
) -> Result<()> {
    let agent = Arc::new(SunmaoAgent {
        sessions: Mutex::new(HashMap::new()),
        base_url: base_url.to_string(),
        api_key: api_key.to_string(),
        model: model.to_string(),
        provider: provider.to_string(),
        preset_names: preset_names.to_vec(),
    });

    let result = Agent
        .v2()
        .name("sunmao")
        .on_receive_request(
            async |req: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _cx: V2ConnectionTo<Client>| {
                responder.respond(
                    v2::InitializeResponse::new(
                        req.protocol_version,
                        v2::Implementation::new("sunmao", env!("CARGO_PKG_VERSION")),
                    )
                    .capabilities(
                        v2::AgentCapabilities::new().session(v2::SessionCapabilities::new()),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: v2::NewSessionRequest,
                            responder: Responder<v2::NewSessionResponse>,
                            cx: V2ConnectionTo<Client>| {
                    let id = format!(
                        "s-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis()
                    );
                    let cwd = req.cwd.clone().into_inner();
                    let preset_roots = match agent.presets(&cwd) {
                        Ok(r) => r,
                        Err(e) => return responder.respond_with_error(e),
                    };
                    let llm = agent.new_llm();
                    let sessions_dir = cwd.join(".sunmao").join("sessions");
                    let log = match SessionLog::open(&sessions_dir, &id).await {
                        Ok(l) => l,
                        Err(e) => {
                            return responder
                                .respond_with_error(invalid_params(format!("session log: {e:#}")));
                        }
                    };
                    let mut registry = builtin_registry();
                    let mcp = sunmao_core::mcp::connect_all(&cwd, &preset_roots).await;
                    for t in mcp.tools {
                        registry.register_boxed(t);
                    }
                    let session_id = v2::SessionId::new(id.clone());
                    let mut ctx_raw = Context::new(llm, log, registry, cwd.clone())
                        .with_extra_plugin_roots(preset_roots.clone());
                    ctx_raw.mcp_servers = mcp.servers;
                    ctx_raw.connect_extensions().await;
                    ctx_raw.approval = Arc::new(AcpApprover {
                        cx: cx.clone(),
                        session_id: session_id.clone(),
                    });
                    ctx_raw.models = Some(agent.new_resolver(&cwd));
                    let ctx = Arc::new(ctx_raw);
                    // sub-agent lifecycle + bg task results relay to the
                    // client as session updates, same as the REPL's sink.
                    let _ = ctx.live_sink.set(Arc::new(AcpObserver {
                        connection: cx.clone(),
                        session_id: session_id.clone(),
                        msg_counter: std::sync::atomic::AtomicU64::new(0),
                    }));
                    {
                        let mut l = ctx.sessions.lock().await;
                        let _ = l
                            .append(&SessionEvent::Started {
                                model: agent.model.clone(),
                                cwd: cwd.display().to_string(),
                            })
                            .await;
                        // same assembled prompt as every other frontend —
                        // the ACP path no longer drifts from the REPL's.
                        let _ = l
                            .append(&SessionEvent::Message {
                                message: Message::system(
                                    sunmao_core::prompt::PromptAssembler::new(&cwd)
                                        .with_extra_roots(&preset_roots)
                                        .assemble(None),
                                ),
                            })
                            .await;
                    }
                    // SessionStart is a session fact, not a frontend
                    // courtesy — fire after extensions are up so they can
                    // answer it, same ordering every frontend keeps.
                    ctx.hooks
                        .fire(
                            sunmao_core::hooks::HookEvent::SessionStart,
                            &ctx.cwd,
                            &sunmao_core::hooks::HookInput {
                                source: Some("startup"),
                                ..Default::default()
                            },
                        )
                        .await;
                    let mode = *ctx.approval_mode.read().unwrap();
                    agent.sessions.lock().unwrap().insert(
                        id,
                        Arc::new(Mutex::new(SessionState {
                            agent: AgentLoop::new(ctx.clone()),
                            ctx,
                            next_msg: 0,
                        })),
                    );
                    responder.respond(
                        v2::NewSessionResponse::new(session_id.clone())
                            .config_options(vec![mode_config(mode)]),
                    )?;
                    let _ = cx.send_notification(v2::UpdateSessionNotification::new(
                        session_id,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                            v2::IdleStateUpdate::new(),
                        )),
                    ));
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: v2::ListSessionsRequest,
                            responder: Responder<v2::ListSessionsResponse>,
                            _cx: V2ConnectionTo<Client>| {
                    // live sessions first; a fresh server's map is empty, so
                    // also scan the request cwd's session dir — resumable
                    // logs are sessions too
                    let map = agent.sessions.lock().unwrap();
                    let mut infos: Vec<_> = map
                        .keys()
                        .map(|id| {
                            v2::SessionInfo::new(
                                v2::SessionId::new(id.clone()),
                                v2::AbsolutePath::new(map[id].lock().unwrap().ctx.cwd.clone()),
                            )
                        })
                        .collect();
                    drop(map);
                    if let Some(cwd) = req.cwd {
                        let base = cwd.into_inner().clone();
                        let dir = base.join(".sunmao").join("sessions");
                        let mut disk: Vec<_> = std::fs::read_dir(&dir)
                            .map(|rd| {
                                rd.flatten()
                                    .filter_map(|e| {
                                        let p = e.path();
                                        if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                                            let m = e.metadata().ok()?.modified().ok()?;
                                            Some((m, p.file_stem()?.to_string_lossy().to_string()))
                                        } else {
                                            None
                                        }
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        disk.sort_by_key(|b| std::cmp::Reverse(b.0));
                        let live: std::collections::HashSet<String> =
                            infos.iter().map(|i| i.session_id.to_string()).collect();
                        for (_, id) in disk.into_iter().take(50) {
                            if !live.contains(&id) {
                                infos.push(v2::SessionInfo::new(
                                    v2::SessionId::new(id),
                                    v2::AbsolutePath::new(base.clone()),
                                ));
                            }
                        }
                    }
                    responder.respond(v2::ListSessionsResponse::new(infos))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: v2::ResumeSessionRequest,
                            responder: Responder<v2::ResumeSessionResponse>,
                            cx: V2ConnectionTo<Client>| {
                    let id = req.session_id.to_string();
                    // already live in this process?
                    if agent.sessions.lock().unwrap().contains_key(&id) {
                        return responder.respond(v2::ResumeSessionResponse::new());
                    }
                    // reopen the on-disk log — events fold back into messages
                    let log_path = req
                        .cwd
                        .clone()
                        .into_inner()
                        .join(".sunmao")
                        .join("sessions")
                        .join(format!("{id}.jsonl"));
                    if !log_path.exists() {
                        return responder.respond_with_error(invalid_params(format!(
                            "no session log for {id}"
                        )));
                    }
                    let cwd = req.cwd.clone().into_inner();
                    let preset_roots = match agent.presets(&cwd) {
                        Ok(r) => r,
                        Err(e) => return responder.respond_with_error(e),
                    };
                    let llm = agent.new_llm();
                    let log = match SessionLog::open_path(&log_path).await {
                        Ok(l) => l,
                        Err(e) => {
                            return responder
                                .respond_with_error(invalid_params(format!("open log: {e:#}")));
                        }
                    };
                    let mut registry = builtin_registry();
                    let mcp = sunmao_core::mcp::connect_all(&cwd, &preset_roots).await;
                    for t in mcp.tools {
                        registry.register_boxed(t);
                    }
                    let mut ctx_raw =
                        Context::new(llm, log, registry, cwd).with_extra_plugin_roots(preset_roots);
                    ctx_raw.mcp_servers = mcp.servers;
                    ctx_raw.connect_extensions().await;
                    ctx_raw.approval = Arc::new(AcpApprover {
                        cx: cx.clone(),
                        session_id: req.session_id.clone(),
                    });
                    ctx_raw.models = Some(agent.new_resolver(&ctx_raw.cwd.clone()));
                    let ctx = Arc::new(ctx_raw);
                    let _ = ctx.live_sink.set(Arc::new(AcpObserver {
                        connection: cx.clone(),
                        session_id: req.session_id.clone(),
                        msg_counter: std::sync::atomic::AtomicU64::new(0),
                    }));
                    // SessionStart(source=resume) — same fact a --resume
                    // startup would record; extensions must be up first.
                    ctx.hooks
                        .fire(
                            sunmao_core::hooks::HookEvent::SessionStart,
                            &ctx.cwd,
                            &sunmao_core::hooks::HookInput {
                                source: Some("resume"),
                                ..Default::default()
                            },
                        )
                        .await;
                    // the mode the reopened log was seeded with — the
                    // response advertises it so the client's selector
                    // shows the resumed stance, not a default
                    let resumed_mode = *ctx.approval_mode.read().unwrap();
                    agent.sessions.lock().unwrap().insert(
                        id,
                        Arc::new(Mutex::new(SessionState {
                            agent: AgentLoop::new(ctx.clone()),
                            ctx,
                            next_msg: 0,
                        })),
                    );
                    responder.respond(
                        v2::ResumeSessionResponse::new()
                            .config_options(vec![mode_config(resumed_mode)]),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: v2::SetSessionConfigOptionRequest,
                            responder: Responder<v2::SetSessionConfigOptionResponse>,
                            cx: V2ConnectionTo<Client>| {
                    let session = {
                        let map = agent.sessions.lock().unwrap();
                        map.get(&req.session_id.to_string()).cloned()
                    };
                    let Some(session) = session else {
                        return responder.respond_with_error(invalid_params("unknown session"));
                    };
                    if req.config_id.to_string() != "mode" {
                        return responder.respond_with_error(invalid_params(format!(
                            "unknown config option: {}",
                            req.config_id
                        )));
                    }
                    let wanted = match &req.value {
                        v2::SessionConfigOptionValue::Id { value } => value.to_string(),
                        _ => {
                            return responder
                                .respond_with_error(invalid_params("mode expects an id value"));
                        }
                    };
                    let Some(m) = sunmao_core::agent::ApprovalMode::parse(&wanted) else {
                        return responder
                            .respond_with_error(invalid_params(format!("unknown mode: {wanted}")));
                    };
                    let (agent_loop, ctx, session_id) = {
                        let st = session.lock().unwrap();
                        (st.agent.clone(), st.ctx.clone(), req.session_id.clone())
                    };
                    // durable + live: the audit line rides the same observer
                    // the client already subscribes to
                    agent_loop
                        .set_approval_mode(
                            m,
                            &observer::AcpObserver {
                                connection: cx.clone(),
                                session_id: session_id.clone(),
                                msg_counter: std::sync::atomic::AtomicU64::new(0),
                            },
                        )
                        .await;
                    responder.respond(v2::SetSessionConfigOptionResponse::new(vec![mode_config(
                        *ctx.approval_mode.read().unwrap(),
                    )]))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: v2::CloseSessionRequest,
                            responder: Responder<v2::CloseSessionResponse>,
                            _cx: V2ConnectionTo<Client>| {
                    let session = agent
                        .sessions
                        .lock()
                        .unwrap()
                        .remove(&req.session_id.to_string());
                    // closed session = dead extensions — graceful shutdown
                    // before the state drops (Drop would only detach).
                    // Clone the ctx out of the lock: guards never cross await.
                    let ctx = session.map(|s| s.lock().unwrap().ctx.clone());
                    if let Some(ctx) = ctx {
                        // SessionEnd is a session fact, not a frontend
                        // courtesy — fire before the children die so they
                        // can still answer it.
                        ctx.hooks
                            .fire(
                                sunmao_core::hooks::HookEvent::SessionEnd,
                                &ctx.cwd,
                                &sunmao_core::hooks::HookInput::default(),
                            )
                            .await;
                        ctx.ext.shutdown().await;
                    }
                    responder.respond(v2::CloseSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: v2::PromptRequest,
                            responder: Responder<v2::PromptResponse>,
                            cx: V2ConnectionTo<Client>| {
                    let session = {
                        let map = agent.sessions.lock().unwrap();
                        map.get(&req.session_id.to_string()).cloned()
                    };
                    let Some(session) = session else {
                        return responder.respond_with_error(invalid_params("unknown session"));
                    };

                    let prompt_text = req
                        .prompt
                        .iter()
                        .filter_map(|b| match b {
                            v2::ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    let user_msg_id = {
                        let mut st = session.lock().unwrap();
                        st.next_msg += 1;
                        v2::MessageId::new(format!("user-{}", st.next_msg))
                    };
                    responder.respond(v2::PromptResponse::new(user_msg_id))?;

                    let session_id = req.session_id.clone();
                    cx.spawn({
                        let session = session.clone();
                        let cx = cx.clone();
                        async move {
                            let obs = AcpObserver {
                                connection: cx.clone(),
                                session_id: session_id.clone(),
                                msg_counter: std::sync::atomic::AtomicU64::new(0),
                            };
                            let _ = cx.send_notification(v2::UpdateSessionNotification::new(
                                session_id.clone(),
                                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                                    v2::RunningStateUpdate::new(),
                                )),
                            ));
                            // clone the loop handle out from under the lock —
                            // turns on one session serialize via ACP anyway
                            let agent_loop = session.lock().unwrap().agent.clone();
                            let outcome = agent_loop.run_turn(&prompt_text, &obs).await;
                            let reason = match outcome {
                                Ok(TurnOutcome::Completed) => v2::StopReason::EndTurn,
                                _ => v2::StopReason::Cancelled,
                            };
                            let _ = cx.send_notification(v2::UpdateSessionNotification::new(
                                session_id,
                                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                                    v2::IdleStateUpdate::new().stop_reason(reason),
                                )),
                            ));
                            Ok(())
                        }
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let agent = agent.clone();
                async move |notif: v2::CancelSessionNotification, _cx: V2ConnectionTo<Client>| {
                    let map = agent.sessions.lock().unwrap();
                    if let Some(s) = map.get(&notif.session_id.to_string()) {
                        s.lock()
                            .unwrap()
                            .ctx
                            .cancelled
                            .store(true, Ordering::Relaxed);
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await;

    // server going down = every live session's extensions go down with it
    let exts: Vec<_> = {
        let map = agent.sessions.lock().unwrap();
        map.values()
            .map(|s| s.lock().unwrap().ctx.ext.clone())
            .collect()
    };
    for ext in exts {
        ext.shutdown().await;
    }
    result
}
