//! `SendMessage` — the child's uplink back to its spawning session.
//!
//! A sub-agent pushes a text onto `ctx.parent_steer` (the parent's steer
//! queue, cloned into the child's Context at `build_sub_ctx`). The parent's
//! turn loop drains it at the next request boundary and appends it as a
//! tagged user message — the child can therefore ask the parent a question,
//! report an intermediate finding, or request permission mid-run, and the
//! parent sees it in-band instead of only at `TaskDone` completion.
//! The interactive session has no parent: the tool exists but returns an
//! explanatory failure rather than silently dropping the text.

use serde::Deserialize;
use serde_json::{Value, json};
use sunmao_llm::types::Tool;

use crate::context::{Context, MutexRecover, RwLockRecover};
use crate::tool::{ToolImpl, ToolResult};

/// The wire tag a child's uplink message carries — the parent's fold renders
/// it as a user message with this provenance marker. Locked shape: frontends
/// pattern-match on it for the "sub-agent spoke" chip styling.
pub(crate) const UPLINK_TAG: &str = "sub-agent-message";

pub struct SendMessageTool;

#[async_trait::async_trait]
impl ToolImpl for SendMessageTool {
    fn name(&self) -> &'static str {
        "SendMessage"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "SendMessage",
            "Send a message back to the session that spawned you — a question, \
             a progress note, or a finding worth stopping for. The parent folds \
             it as a user message at its next request boundary, so it arrives \
             in-band, not just at your final result. Only meaningful inside a \
             sub-agent; on the main session it reports there is no parent.",
            json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string", "description": "The text to deliver to the parent session"}
                },
                "required": ["message"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &Context) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            message: String,
        }
        let a: Args = serde_json::from_value(args)?;
        let Some(queue) = &ctx.parent_steer else {
            return Ok(ToolResult {
                output: "no parent session — the interactive agent has nothing to message".into(),
                ok: false,
            });
        };
        // client id = u64::MAX marks "came from a child, not a user client" —
        // frontends can distinguish the provenance chip if they read ids.
        queue.lock_or_recover().push_back((
            u64::MAX,
            format!(
                "<{UPLINK_TAG} id=\"{}\" lane=\"{}\">\n{}\n</{UPLINK_TAG}>",
                ctx.session_id.read_or_recover(),
                ctx.lane,
                a.message
            ),
        ));
        Ok(ToolResult {
            output: "message queued for the parent session".into(),
            ok: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Minimal provider — the uplink tool never calls the LLM, but
    /// `Context::new` requires the seam populated.
    struct NullProvider;
    #[async_trait::async_trait]
    impl sunmao_llm::ProviderAdapter for NullProvider {
        async fn stream(
            &self,
            _req: sunmao_llm::ChatRequest<'_>,
        ) -> anyhow::Result<sunmao_llm::DeltaStream> {
            Ok(Box::pin(futures_util::stream::empty()))
        }
    }

    /// A bare context pointing its uplink at a queue the test can read —
    /// mirrors what `build_sub_ctx` wires for a real child.
    fn ctx_with_uplink() -> (Context, crate::context::SteerQueue) {
        let queue = crate::context::SteerQueue::default();
        let ctx = Context {
            parent_steer: Some(queue.clone()),
            ..Context::new(
                Arc::new(NullProvider),
                crate::session::SessionLog::ephemeral(),
                crate::tool::builtin_registry(),
                crate::fresh_test_dir("sendmsg"),
            )
        };
        (ctx, queue)
    }

    #[tokio::test]
    async fn child_message_lands_on_parent_steer_queue() {
        let (ctx, queue) = ctx_with_uplink();
        let r = SendMessageTool
            .call(json!({"message": "halfway — found 3 call sites"}), &ctx)
            .await
            .unwrap();
        assert!(r.ok);
        let items: Vec<String> = queue
            .lock_or_recover()
            .iter()
            .map(|(_, t)| t.clone())
            .collect();
        assert_eq!(items.len(), 1);
        assert!(items[0].contains(UPLINK_TAG), "tag: {}", items[0]);
        assert!(
            items[0].contains("halfway — found 3 call sites"),
            "body: {}",
            items[0]
        );
        assert!(items[0].contains("lane=\"0\""), "lane tag: {}", items[0]);
    }

    #[tokio::test]
    async fn interactive_session_reports_no_parent() {
        let ctx = Context::new(
            Arc::new(NullProvider),
            crate::session::SessionLog::ephemeral(),
            crate::tool::builtin_registry(),
            crate::fresh_test_dir("sendmsg-top"),
        );
        assert!(ctx.parent_steer.is_none());
        let r = SendMessageTool
            .call(json!({"message": "hello"}), &ctx)
            .await
            .unwrap();
        assert!(!r.ok);
        assert!(r.output.contains("no parent"), "{}", r.output);
    }
}
