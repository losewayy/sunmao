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
    /// Shared with the session, not owned per observer — a fresh observer
    /// (next `session/prompt`, config set, resumed session) minting its own
    /// `msg-0` collides with chunks the client already deduped.
    pub msg_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
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
        next_id(&self.msg_counter, kind)
    }
}

fn next_id(counter: &std::sync::atomic::AtomicU64, kind: &str) -> v2::MessageId {
    let n = counter.fetch_add(1, Ordering::Relaxed);
    v2::MessageId::new(format!("{kind}-{n}"))
}

impl Observer for AcpObserver {
    fn on_event(&self, ev: &LiveEvent) {
        match ev {
            LiveEvent::Content { text } => self.send(v2::SessionUpdate::AgentMessageChunk(
                v2::ContentChunk::new(text.clone().into(), self.next_id("msg")),
            )),
            LiveEvent::Reasoning { text } => self.send(v2::SessionUpdate::AgentThoughtChunk(
                v2::ContentChunk::new(text.clone().into(), self.next_id("thought")),
            )),
            LiveEvent::ToolStart {
                name,
                summary,
                depth,
                lane,
                call_id,
                ..
            } => {
                let label = if *depth > 0 {
                    format!("↳{name}")
                } else {
                    name.clone()
                };
                // the provider's call id is the exact join key; synthetic
                // events (compact/local shell, no call_id) fall back to
                // depth+lane+name — still unique enough for them.
                let key = call_id
                    .clone()
                    .unwrap_or_else(|| format!("{depth}:{lane}:{name}"));
                self.send(v2::SessionUpdate::ToolCallUpdate(
                    v2::ToolCallUpdate::new(v2::ToolCallId::new(key))
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
                call_id,
                ..
            } => {
                let key = call_id
                    .clone()
                    .unwrap_or_else(|| format!("{depth}:{lane}:{name}"));
                self.send(v2::SessionUpdate::ToolCallUpdate(
                    v2::ToolCallUpdate::new(v2::ToolCallId::new(key))
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
            LiveEvent::Artifact {
                name,
                path,
                bytes,
                rev,
            } => {
                let uri = format!("file:///{}", path.replace('\\', "/"));
                let title = if *rev > 1 {
                    format!("{name}.html (rev {rev})")
                } else {
                    format!("{name}.html")
                };
                let link = v2::ResourceLink::new(name.clone(), uri)
                    .title(title)
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
            // transcript-folding facts — durable on the log; an ACP client
            // rebuilds its own view, so a one-line notice is the whole job
            LiveEvent::Compacted { summary } => {
                self.send(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                    format!("[context compacted] {summary}\n").into(),
                    self.next_id("msg"),
                )));
            }
            LiveEvent::Todos { .. } => {} // the tool's ToolDone output covers it
            // the goal chain's progress marker — round bumps would spam
            // chunks, so only objective/status transitions speak
            LiveEvent::Goal { goal } => {
                use sunmao_core::tool::GoalStatus;
                if goal.status != GoalStatus::InProgress || goal.rounds == 0 {
                    self.send(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                        format!(
                            "[goal {} — {} (round {}/{})]\n",
                            sunmao_core::tool::status_name(goal.status),
                            goal.objective,
                            goal.rounds,
                            goal.max_rounds
                        )
                        .into(),
                        self.next_id("msg"),
                    )));
                }
            }
            // serve-only live mirror of the durable user message — ACP
            // never runs through client.rs's Input lane, so it never lands
            LiveEvent::UserMessage { .. } => {}
            LiveEvent::TurnBoundary { .. } => {} // rewind ordinals are a serve concern
            LiveEvent::TaskDone { id, ok, .. } => {
                self.send(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                    format!("[sub-agent {id} {}]\n", if *ok { "done" } else { "failed" }).into(),
                    self.next_id("msg"),
                )));
            }
            LiveEvent::JobDone { id, ok, .. } => {
                self.send(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                    format!("[job {id} {}]\n", if *ok { "done" } else { "failed" }).into(),
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
        // the client sees what gets granted: "Allow for this session"
        // covers this exact tool+specifier for the rest of the session —
        // without it the user can't tell what they just approved.
        .description(format!(
            "{detail}\n\nsession grant would cover: {tool}: {detail}"
        ));
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
                _ => Approval::Deny { reason: None },
            },
            Err(_) => Approval::Deny { reason: None },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counter is session-owned: every observer draws from the same
    /// sequence, so a second turn's first chunk is `msg-N`, never `msg-0`
    /// again — a client deduping on MessageId would drop the collision.
    #[test]
    fn ids_share_one_monotonic_sequence() {
        let counter = std::sync::atomic::AtomicU64::new(0);
        assert_eq!(next_id(&counter, "msg").to_string(), "msg-0");
        assert_eq!(next_id(&counter, "artifact").to_string(), "artifact-1");
        assert_eq!(next_id(&counter, "msg").to_string(), "msg-2");
    }
}
