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
            LiveEvent::ToolStart { name } => self.send(v2::SessionUpdate::ToolCallUpdate(
                v2::ToolCallUpdate::new(v2::ToolCallId::new(name.clone())),
            )),
            LiveEvent::ToolDone { .. } | LiveEvent::TurnEnd { .. } => {}
        }
    }
}

fn invalid_params(msg: impl ToString) -> Error {
    Error::invalid_params().data(msg.to_string())
}

pub async fn run(base_url: &str, api_key: &str, model: &str) -> Result<()> {
    let agent = Arc::new(SunmaoAgent {
        sessions: Mutex::new(HashMap::new()),
        base_url: base_url.to_string(),
        api_key: api_key.to_string(),
        model: model.to_string(),
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
                    let llm = Arc::new(OaiClient::new(
                        agent.base_url.clone(),
                        agent.api_key.clone(),
                        agent.model.clone(),
                    ));
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
                    let ctx = Arc::new(Context::new(llm, log, registry, cwd.clone()));
                    {
                        let mut l = ctx.sessions.lock().await;
                        let _ = l
                            .append(&SessionEvent::Started {
                                model: agent.model.clone(),
                                cwd: cwd.display().to_string(),
                            })
                            .await;
                        let _ = l
                            .append(&SessionEvent::Message {
                                message: Message::system(
                                    "You are sunmao, a coding agent. Use tools to act on the \
                                     filesystem. Prefer dedicated tools over Bash.",
                                ),
                            })
                            .await;
                    }
                    let session_id = v2::SessionId::new(id.clone());
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
