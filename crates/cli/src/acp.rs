//! `sunmao --acp` — Agent Client Protocol server over stdio (v2 schema).
//!
//! Any ACP client (Zed, JetBrains plugins, …) can drive a sunmao session.
//! JSON-RPC owns stdout; diagnostics go to stderr.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v2;
use agent_client_protocol::{Agent, Client, Error, Responder, Result, Stdio, V2ConnectionTo};
use sunmao_core::agent::{AgentLoop, LiveEvent, Observer, TurnOutcome};
use sunmao_core::tool::builtin_registry;
use sunmao_core::{Context, SessionEvent, SessionLog};
use sunmao_llm::types::Message;
use sunmao_llm::OaiClient;

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
}

struct AcpObserver {
    connection: V2ConnectionTo<Client>,
    session_id: v2::SessionId,
    msg_counter: std::sync::atomic::AtomicU64,
}

impl AcpObserver {
    fn send(&self, update: v2::SessionUpdate) {
        let _ = self
            .connection
            .send_notification(v2::UpdateSessionNotification::new(
                self.session_id.clone(),
                update,
            ));
    }

    fn next_id(&self, kind: &str) -> v2::MessageId {
        let n = self.msg_counter.fetch_add(1, Ordering::Relaxed);
        v2::MessageId::new(format!("{kind}-{n}"))
    }
}

impl Observer for AcpObserver {
    fn on_event(&self, ev: &LiveEvent) {
        match ev {
            LiveEvent::Content(c) => self.send(v2::SessionUpdate::AgentMessageChunk(
                v2::ContentChunk::new(c.clone().into(), self.next_id("msg")),
            )),
            LiveEvent::Reasoning(r) => self.send(v2::SessionUpdate::AgentThoughtChunk(
                v2::ContentChunk::new(r.clone().into(), self.next_id("thought")),
            )),
            LiveEvent::ToolStart {
                name,
                summary,
                depth,
                lane,
            } => {
                let label = if *depth > 0 {
                    format!("↳{name}")
                } else {
                    name.clone()
                };
                // ID keys on depth+lane so a sub-agent's Read doesn't collide
                // with the parent's Read — or a parallel sibling's.
                self.send(v2::SessionUpdate::ToolCallUpdate(
                    v2::ToolCallUpdate::new(v2::ToolCallId::new(format!("{depth}:{lane}:{name}")))
                        .name(label.clone())
                        .title(format!("{label} {summary}"))
                        .status(v2::ToolCallStatus::InProgress),
                ));
            }
            LiveEvent::ToolDone {
                name,
                ok,
                output,
                depth,
                lane,
            } => {
                self.send(v2::SessionUpdate::ToolCallUpdate(
                    v2::ToolCallUpdate::new(v2::ToolCallId::new(format!("{depth}:{lane}:{name}")))
                        .status(if *ok {
                            v2::ToolCallStatus::Completed
                        } else {
                            v2::ToolCallStatus::Failed
                        })
                        .content(vec![v2::ToolCallContent::Content(Box::new(
                            v2::Content::new(v2::ContentBlock::Text(v2::TextContent::new(
                                output.clone(),
                            ))),
                        ))]),
                ));
            }
            LiveEvent::TurnEnd { .. } => {}
            LiveEvent::Usage(_) => {}
            // audit facts: visible in local frontends; ACP clients get them
            // as agent message text so the rewrite/veto is never silent
            LiveEvent::Hook { event, detail } => {
                self.send(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                    format!("[⚙ {event}: {detail}]\n").into(),
                    self.next_id("msg"),
                )));
            }
        }
    }
}

/// Approval over ACP: risky tool calls become `session/request_permission`
/// prompts in the client (Zed etc.) — the approval seam's third frontend.
struct AcpApprover {
    cx: V2ConnectionTo<Client>,
    session_id: v2::SessionId,
}

#[async_trait::async_trait]
impl sunmao_core::approval::Approver for AcpApprover {
    async fn approve(
        &self,
        tool: &str,
        detail: &str,
        why: &str,
    ) -> sunmao_core::approval::Approval {
        use sunmao_core::approval::Approval;
        let req = v2::RequestPermissionRequest::new(
            self.session_id.clone(),
            format!("{tool}: {why}"),
            vec![
                v2::PermissionOption::new(
                    v2::PermissionOptionId::new("allow"),
                    "Allow once",
                    v2::PermissionOptionKind::AllowOnce,
                ),
                v2::PermissionOption::new(
                    v2::PermissionOptionId::new("allow-session"),
                    "Allow for this session",
                    v2::PermissionOptionKind::AllowAlways,
                ),
                v2::PermissionOption::new(
                    v2::PermissionOptionId::new("deny"),
                    "Deny",
                    v2::PermissionOptionKind::RejectOnce,
                ),
            ],
        )
        .description(detail.to_string());
        match self.cx.send_request(req).block_task().await {
            Ok(resp) => match resp.outcome {
                v2::RequestPermissionOutcome::Selected(ref s)
                    if s.option_id.to_string() == "allow" =>
                {
                    Approval::Once
                }
                v2::RequestPermissionOutcome::Selected(ref s)
                    if s.option_id.to_string() == "allow-session" =>
                {
                    Approval::Session
                }
                _ => Approval::Deny,
            },
            Err(_) => Approval::Deny,
        }
    }
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
}

fn invalid_params(msg: impl ToString) -> Error {
    Error::invalid_params().data(msg.to_string())
}

pub async fn run(base_url: &str, api_key: &str, model: &str, provider: &str) -> Result<()> {
    let agent = Arc::new(SunmaoAgent {
        sessions: Mutex::new(HashMap::new()),
        base_url: base_url.to_string(),
        api_key: api_key.to_string(),
        model: model.to_string(),
        provider: provider.to_string(),
    });

    Agent
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
                    let llm = agent.new_llm();
                    let sessions_dir = cwd.join(".sunmao").join("sessions");
                    let log = match SessionLog::open(&sessions_dir, &id).await {
                        Ok(l) => l,
                        Err(e) => {
                            return responder
                                .respond_with_error(invalid_params(format!("session log: {e:#}")))
                        }
                    };
                    let mut registry = builtin_registry();
                    for t in sunmao_core::mcp::connect_all(&cwd).await {
                        registry.register_boxed(t);
                    }
                    let session_id = v2::SessionId::new(id.clone());
                    let mut ctx_raw = Context::new(llm, log, registry, cwd.clone());
                    ctx_raw.approval = Arc::new(AcpApprover {
                        cx: cx.clone(),
                        session_id: session_id.clone(),
                    });
                    ctx_raw.models = Some(agent.new_resolver(&cwd));
                    let ctx = Arc::new(ctx_raw);
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
                                    sunmao_core::prompt::PromptAssembler::new(&cwd).assemble(None),
                                ),
                            })
                            .await;
                    }
                    agent.sessions.lock().unwrap().insert(
                        id,
                        Arc::new(Mutex::new(SessionState {
                            agent: AgentLoop::new(ctx.clone()),
                            ctx,
                            next_msg: 0,
                        })),
                    );
                    responder.respond(v2::NewSessionResponse::new(session_id.clone()))?;
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
                async move |_req: v2::ListSessionsRequest,
                            responder: Responder<v2::ListSessionsResponse>,
                            _cx: V2ConnectionTo<Client>| {
                    let map = agent.sessions.lock().unwrap();
                    let infos: Vec<_> = map
                        .keys()
                        .map(|id| {
                            v2::SessionInfo::new(
                                v2::SessionId::new(id.clone()),
                                v2::AbsolutePath::new(map[id].lock().unwrap().ctx.cwd.clone()),
                            )
                        })
                        .collect();
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
                    let llm = agent.new_llm();
                    let log = match SessionLog::open_path(&log_path).await {
                        Ok(l) => l,
                        Err(e) => {
                            return responder
                                .respond_with_error(invalid_params(format!("open log: {e:#}")))
                        }
                    };
                    let mut registry = builtin_registry();
                    for t in sunmao_core::mcp::connect_all(&cwd).await {
                        registry.register_boxed(t);
                    }
                    let mut ctx_raw = Context::new(llm, log, registry, cwd);
                    ctx_raw.approval = Arc::new(AcpApprover {
                        cx: cx.clone(),
                        session_id: req.session_id.clone(),
                    });
                    ctx_raw.models = Some(agent.new_resolver(&ctx_raw.cwd.clone()));
                    let ctx = Arc::new(ctx_raw);
                    agent.sessions.lock().unwrap().insert(
                        id,
                        Arc::new(Mutex::new(SessionState {
                            agent: AgentLoop::new(ctx.clone()),
                            ctx,
                            next_msg: 0,
                        })),
                    );
                    responder.respond(v2::ResumeSessionResponse::new())
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
                    agent
                        .sessions
                        .lock()
                        .unwrap()
                        .remove(&req.session_id.to_string());
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
        .await
}
