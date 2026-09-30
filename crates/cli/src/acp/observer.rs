//! Outbound adapters — live events → `session/update` notifications and
//! the approval gate → `session/request_permission`. Kept out of `mod.rs`
//! so the file split lands on the seam: mod is the wire, this is the UI.

use std::sync::atomic::Ordering;

use agent_client_protocol::schema::v2;
use agent_client_protocol::{Client, V2ConnectionTo};
use sunmao_core::agent::{LiveEvent, Observer};

pub(super) struct AcpObserver {
    pub connection: V2ConnectionTo<Client>,
    pub session_id: v2::SessionId,
    pub msg_counter: std::sync::atomic::AtomicU64,
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
            // SPEC §4.10 — artifacts ride the ACP channel as resource links;
            // the client decides how to render (sandboxed webview or not)
            LiveEvent::Artifact { name, path, bytes } => {
                let uri = format!("file:///{}", path.replace('\\', "/"));
                let link = v2::ResourceLink::new(name.clone(), uri)
                    .title(format!("{name}.html"))
                    .mime_type("text/html".to_string())
                    .size(*bytes as i64);
                self.send(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                    v2::ContentBlock::ResourceLink(link),
                    self.next_id("artifact"),
                )));
            }
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
pub(super) struct AcpApprover {
    pub cx: V2ConnectionTo<Client>,
    pub session_id: v2::SessionId,
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
